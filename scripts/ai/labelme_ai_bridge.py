"""Optional LabelMe/OSAM AI bridge used by the Qt client.

The Qt side sends one compact JSON object on stdin and receives one JSON object
on stdout.  OSAM is deliberately optional: a missing model runtime is returned
as a structured error instead of being treated as an empty annotation.
"""

from __future__ import annotations

import base64
from collections import OrderedDict
import io
import json
import math
import sys
from pathlib import Path


def _with_metadata(shape: dict, *, label: str | None, description: str | None) -> dict:
    if label is not None:
        shape["label"] = label
    if description is not None:
        shape["description"] = description
    return shape


def _sort_annotations_by_score(annotations):
    """Match LabelMe's greedy suppression input order: highest score first."""
    return sorted(
        annotations,
        key=lambda annotation: annotation.score if annotation.score is not None else 0,
        reverse=True,
    )


def _local_mask_from_annotation(annotation):
    """Convert an OSAM annotation to the local mask used by LabelMe NMS."""
    import numpy as np

    bbox = getattr(annotation, "bounding_box", None)
    if bbox is None:
        return None
    xmin, ymin, xmax, ymax = np.round(
        [bbox.xmin, bbox.ymin, bbox.xmax, bbox.ymax]
    ).astype(int).tolist()
    expected_shape = (ymax - ymin + 1, xmax - xmin + 1)
    if expected_shape[0] <= 0 or expected_shape[1] <= 0:
        raise ValueError(f"invalid bounding box extent {expected_shape}")

    mask = getattr(annotation, "mask", None)
    if mask is None:
        local_mask = np.ones(expected_shape, dtype=bool)
    else:
        local_mask = np.asarray(mask).astype(bool)
        if local_mask.shape != expected_shape:
            raise ValueError(
                f"mask shape {local_mask.shape} does not match "
                f"bbox-derived extent {expected_shape}"
            )
    return local_mask, (xmin, ymin), int(np.count_nonzero(local_mask))


def _mask_intersection_area(left, right) -> int:
    import numpy as np

    left_mask, (left_xmin, left_ymin), _ = left
    right_mask, (right_xmin, right_ymin), _ = right
    left_h, left_w = left_mask.shape
    right_h, right_w = right_mask.shape
    xmin = max(left_xmin, right_xmin)
    ymin = max(left_ymin, right_ymin)
    xmax = min(left_xmin + left_w, right_xmin + right_w)
    ymax = min(left_ymin + left_h, right_ymin + right_h)
    if xmax <= xmin or ymax <= ymin:
        return 0
    left_slice = left_mask[
        ymin - left_ymin : ymax - left_ymin,
        xmin - left_xmin : xmax - left_xmin,
    ]
    right_slice = right_mask[
        ymin - right_ymin : ymax - right_ymin,
        xmin - right_xmin : xmax - right_xmin,
    ]
    return int(np.count_nonzero(left_slice & right_slice))


def _is_redundant_annotation(left, right, iou_threshold: float) -> bool:
    intersection = _mask_intersection_area(left, right)
    if intersection == 0:
        return False
    left_area = left[2]
    right_area = right[2]
    if left_area <= 0 or right_area <= 0:
        return False
    iou = intersection / (left_area + right_area - intersection)
    containment = intersection / min(left_area, right_area)
    return iou >= iou_threshold or containment >= 0.85


def _suppress_annotations(annotations, iou_threshold: float):
    """Greedily suppress same-class overlapping responses like LabelMe."""
    if not annotations:
        return []
    kept = []
    local_masks_by_label = {}
    for annotation in annotations:
        local_mask = _local_mask_from_annotation(annotation)
        if local_mask is None:
            kept.append(annotation)
            continue
        label = getattr(annotation, "text", None)
        peers = local_masks_by_label.setdefault(label, [])
        if any(
            _is_redundant_annotation(local_mask, peer, iou_threshold)
            for peer in peers
        ):
            continue
        kept.append(annotation)
        peers.append(local_mask)
    return kept


def _load_model(model_name: str, progress_callback=None):
    """Prepare and instantiate an OSAM model like LabelMe's download flow."""
    import osam

    model_type = osam.apis.get_model_type_by_name(model_name)
    if model_type.get_size() is None:
        if progress_callback is None:
            model_type.pull()
        else:
            blobs = getattr(model_type, "_blobs", {})
            file_count = sum(
                1 + len(getattr(blob, "attachments", ()))
                for blob in getattr(blobs, "values", lambda: ())()
            )
            file_count = max(1, file_count)
            file_index = -1
            current_filename = None

            def on_progress(filename, bytes_so_far, bytes_total):
                nonlocal file_index, current_filename
                if filename != current_filename:
                    file_index += 1
                    current_filename = filename
                progress_callback(
                    {
                        "event": "progress",
                        "model": model_name,
                        "file_index": file_index,
                        "file_count": file_count,
                        "filename": filename,
                        "bytes_done": int(bytes_so_far),
                        "bytes_total": -1 if bytes_total is None else int(bytes_total),
                    }
                )

            model_type.pull(progress=on_progress)
    return model_type()


class _BridgeSession:
    """Small equivalent of LabelMe's OsamSession for the JSON bridge."""

    def __init__(self, model_name: str, progress_callback=None):
        self.model_name = model_name
        self.model = _load_model(model_name, progress_callback=progress_callback)
        self._embedding_cache = OrderedDict()

    def image_embedding(self, image, image_id: str):
        if image_id in self._embedding_cache:
            embedding = self._embedding_cache.pop(image_id)
            self._embedding_cache[image_id] = embedding
            return embedding
        try:
            embedding = self.model.encode_image(image=image)
        except NotImplementedError:
            return None
        self._embedding_cache[image_id] = embedding
        while len(self._embedding_cache) > 3:
            self._embedding_cache.popitem(last=False)
        return embedding


def _circle_from_mask(mask):
    import numpy as np

    ys, xs = np.nonzero(mask)
    if len(xs) == 0:
        return None
    return float(xs.mean()), float(ys.mean()), float(np.sqrt(len(xs) / np.pi))


def _oriented_rectangle_from_mask(mask):
    import numpy as np
    from scipy.spatial import ConvexHull
    from scipy.spatial import QhullError

    ys, xs = np.nonzero(mask)
    if len(xs) < 3:
        return None
    points = np.stack([xs, ys], axis=1).astype(np.float64)
    try:
        hull = points[ConvexHull(points=points).vertices]
    except QhullError:
        return None

    best_area = float("inf")
    best_corners = None
    for index in range(len(hull)):
        edge = hull[(index + 1) % len(hull)] - hull[index]
        length = float(np.linalg.norm(edge))
        if length == 0:
            continue
        axis = edge / length
        perpendicular = np.array([-axis[1], axis[0]])
        axis_coordinates = hull @ axis
        perpendicular_coordinates = hull @ perpendicular
        axis_min = float(axis_coordinates.min())
        axis_max = float(axis_coordinates.max())
        perpendicular_min = float(perpendicular_coordinates.min())
        perpendicular_max = float(perpendicular_coordinates.max())
        axis_extent = axis_max - axis_min
        perpendicular_extent = perpendicular_max - perpendicular_min
        area = axis_extent * perpendicular_extent
        if area >= best_area:
            continue
        best_area = area
        center = ((axis_min + axis_max) / 2 * axis +
                  (perpendicular_min + perpendicular_max) / 2 * perpendicular)
        if axis_extent >= perpendicular_extent:
            long_axis = axis
            half_long = axis_extent / 2
            half_short = perpendicular_extent / 2
        else:
            long_axis = perpendicular
            half_long = perpendicular_extent / 2
            half_short = axis_extent / 2
        if long_axis[0] < 0 or (long_axis[0] == 0 and long_axis[1] < 0):
            long_axis = -long_axis
        short_axis = np.array([-long_axis[1], long_axis[0]])
        best_corners = np.array([
            center - long_axis * half_long - short_axis * half_short,
            center + long_axis * half_long - short_axis * half_short,
            center + long_axis * half_long + short_axis * half_short,
            center - long_axis * half_long + short_axis * half_short,
        ])
    return None if best_corners is None else best_corners


def _polygon_from_mask(mask):
    import numpy as np
    from skimage import measure

    contours = measure.find_contours(np.pad(mask.astype(bool), 1))
    if not contours:
        return None

    def contour_length(contour):
        return float(np.linalg.norm(contour[1:] - contour[:-1], axis=1).sum())

    contour = max(contours, key=contour_length)
    tolerance = np.ptp(contour, axis=0).max() * 0.004
    polygon = measure.approximate_polygon(contour, tolerance=tolerance)
    polygon = np.clip(polygon, (0, 0), (mask.shape[0] - 1, mask.shape[1] - 1))
    polygon = polygon[:-1]
    if len(polygon) < 3:
        return None
    return polygon[:, ::-1]


def _shape_from_annotation(
    annotation,
    output_format: str,
    *,
    label: str | None = None,
    description: str | None = None,
) -> dict | None:
    bbox = annotation.bounding_box
    bbox_values = None
    if bbox is not None:
        bbox_values = [float(bbox.xmin), float(bbox.ymin), float(bbox.xmax), float(bbox.ymax)]

    mask = getattr(annotation, "mask", None)
    if mask is not None:
        import numpy as np

        mask = np.asarray(mask).astype(bool)
    if output_format == "rectangle":
        if bbox_values is None:
            return None
        points = [[bbox_values[0], bbox_values[1]], [bbox_values[2], bbox_values[3]]]
        return _with_metadata(
            {"shape_type": "rectangle", "points": points},
            label=label,
            description=description,
        )

    if output_format == "circle":
        if mask is not None and mask.any():
            cx, cy, radius = _circle_from_mask(mask)
            offset_x, offset_y = (bbox_values[:2] if bbox_values is not None else (0.0, 0.0))
            return _with_metadata(
                {
                    "shape_type": "circle",
                    "points": [
                        [cx + offset_x, cy + offset_y],
                        [cx + offset_x + radius, cy + offset_y],
                    ],
                },
                label=label,
                description=description,
            )
        if bbox_values is None:
            return None
        xmin, ymin, xmax, ymax = bbox_values
        radius = min(xmax - xmin, ymax - ymin) / 2
        if radius <= 0:
            return None
        return _with_metadata(
            {
                "shape_type": "circle",
                "points": [[(xmin + xmax) / 2, (ymin + ymax) / 2],
                           [(xmin + xmax) / 2 + radius, (ymin + ymax) / 2]],
            },
            label=label,
            description=description,
        )

    if output_format == "oriented_rectangle":
        if mask is not None and mask.any():
            corners = _oriented_rectangle_from_mask(mask)
            if corners is not None:
                offset_x, offset_y = (bbox_values[:2] if bbox_values is not None else (0.0, 0.0))
                return _with_metadata(
                    {
                        "shape_type": "oriented_rectangle",
                        "points": [[float(point[0] + offset_x), float(point[1] + offset_y)]
                                   for point in corners],
                    },
                    label=label,
                    description=description,
                )
        if bbox_values is None:
            return None
        xmin, ymin, xmax, ymax = bbox_values
        return _with_metadata(
            {
                "shape_type": "oriented_rectangle",
                "points": [[xmin, ymin], [xmax, ymin], [xmax, ymax], [xmin, ymax]],
            },
            label=label,
            description=description,
        )

    if mask is None:
        return None
    mask = mask.astype("uint8")
    if not mask.any():
        return None

    if output_format == "mask":
        if bbox_values is None:
            return None
        from PIL import Image

        image = Image.fromarray(mask * 255, mode="L")
        payload = io.BytesIO()
        image.save(payload, format="PNG")
        return _with_metadata(
            {
                "shape_type": "mask",
                "points": [[bbox_values[0], bbox_values[1]], [bbox_values[2], bbox_values[3]]],
                "mask_data": base64.b64encode(payload.getvalue()).decode("ascii"),
            },
            label=label,
            description=description,
        )

    if bbox_values is None:
        return None
    polygon = _polygon_from_mask(mask)
    if polygon is None:
        return None
    points = [[float(point[0] + bbox_values[0]), float(point[1] + bbox_values[1])] for point in polygon]
    if len(points) < 3:
        return None
    return _with_metadata(
        {"shape_type": "polygon", "points": points},
        label=label,
        description=description,
    )


def _process_request(
    request: dict, model_cache: dict | None = None, progress_callback=None
) -> dict:
        image_path = Path(request["image_path"])
        model_name = str(request.get("model", "sam2:latest"))
        output_format = str(request.get("output_format", "polygon"))
        if output_format not in {"rectangle", "polygon", "mask", "oriented_rectangle", "circle"}:
            raise ValueError(f"unsupported output_format: {output_format}")

        import numpy as np
        import osam
        from PIL import Image

        image = np.asarray(Image.open(image_path).convert("RGB"))
        if model_cache is None:
            model = _load_model(model_name, progress_callback=progress_callback)
            embedding = None
        else:
            session = model_cache.get(model_name)
            if session is None:
                session = _BridgeSession(model_name, progress_callback=progress_callback)
                model_cache[model_name] = session
            model = session.model
            embedding = session.image_embedding(image, str(image_path.resolve()))
        prompt_type = str(request.get("prompt_type", "points")).strip().lower()
        shapes = []
        if prompt_type == "text":
            raw_texts = request.get("texts", [])
            if not isinstance(raw_texts, list):
                raise ValueError("texts must be a list")
            texts = []
            for value in raw_texts:
                text = str(value).strip()
                if text and text not in texts:
                    texts.append(text)
            if not texts:
                raise ValueError("texts must contain at least one non-empty prompt")

            score_threshold = float(request.get("score_threshold", 0.1))
            iou_threshold = float(request.get("iou_threshold", 0.5))
            if not math.isfinite(score_threshold) or not 0 <= score_threshold <= 1:
                raise ValueError("score_threshold must be between 0 and 1")
            if not math.isfinite(iou_threshold) or not 0 <= iou_threshold <= 1:
                raise ValueError("iou_threshold must be between 0 and 1")
            response = model.generate(
                request=osam.types.GenerateRequest(
                    model=model.name,
                    image=image,
                    image_embedding=embedding,
                    prompt=osam.types.Prompt(
                        texts=texts,
                        iou_threshold=iou_threshold,
                        score_threshold=score_threshold,
                        max_annotations=1000,
                    ),
                )
            )
            filtered_annotations = []
            for annotation in _sort_annotations_by_score(response.annotations):
                text = str(getattr(annotation, "text", "")).strip()
                if text not in texts:
                    raise ValueError(f"Unexpected text {text!r} found in the response")
                score = float(getattr(annotation, "score", 0.0))
                if not math.isfinite(score) or score < score_threshold:
                    continue
                filtered_annotations.append(annotation)
            for annotation in _suppress_annotations(
                filtered_annotations, iou_threshold=iou_threshold
            ):
                text = str(getattr(annotation, "text", "")).strip()
                description = json.dumps(
                    {
                        "score": float(getattr(annotation, "score", 0.0)),
                        "text": text,
                    },
                    ensure_ascii=False,
                )
                shape = _shape_from_annotation(
                    annotation,
                    output_format,
                    label=text,
                    description=description,
                )
                if shape is not None:
                    shapes.append(shape)
        elif prompt_type == "points":
            points = np.asarray(request.get("points", []), dtype=np.float32)
            point_labels = np.asarray(request.get("point_labels", []), dtype=np.intp)
            if len(points) == 0 or len(points) != len(point_labels):
                raise ValueError("points and point_labels must have the same non-zero length")
            response = model.generate(
                request=osam.types.GenerateRequest(
                    model=model.name,
                    image=image,
                    image_embedding=embedding,
                    prompt=osam.types.Prompt(points=points, point_labels=point_labels),
                )
            )
            for annotation in _suppress_annotations(
                _sort_annotations_by_score(response.annotations),
                iou_threshold=0.5,
            ):
                shape = _shape_from_annotation(annotation, output_format)
                if shape is not None:
                    shapes.append(shape)
        else:
            raise ValueError(f"unsupported prompt_type: {prompt_type}")
        return {"ok": True, "shapes": shapes}


def _json_error(exc: Exception) -> dict:
    return {"ok": False, "error": f"{type(exc).__name__}: {exc}"}


def _server_main() -> int:
    # Keep the model cache in the long-lived process so repeated prompts do
    # not re-import and re-instantiate OSAM. Each request remains one JSON
    # line, making the protocol safe for Qt's asynchronous readyRead path.
    model_cache: dict = {}

    def emit_progress(event: dict) -> None:
        print(json.dumps(event, ensure_ascii=False, separators=(",", ":")), flush=True)

    for raw_line in sys.stdin.buffer:
        if not raw_line.strip():
            continue
        try:
            request = json.loads(raw_line.decode("utf-8-sig"))
            response = _process_request(request, model_cache, progress_callback=emit_progress)
        except Exception as exc:  # noqa: BLE001 - bridge errors are returned to Qt.
            response = _json_error(exc)
        print(json.dumps(response, ensure_ascii=False, separators=(",", ":")), flush=True)
    return 0


def main() -> int:
    try:
        if "--server" in sys.argv[1:]:
            return _server_main()
        request = json.loads(sys.stdin.buffer.read().decode("utf-8-sig"))
        print(json.dumps(_process_request(request), ensure_ascii=False))
        return 0
    except Exception as exc:  # noqa: BLE001 - bridge errors are returned to Qt.
        print(json.dumps(_json_error(exc), ensure_ascii=False))
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

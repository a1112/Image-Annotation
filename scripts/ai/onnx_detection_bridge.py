"""Local YOLO detection via ONNX Runtime; JSON-lines protocol for Qt.

Only explicit detection layouts are accepted. No model downloads or OSAM needed.
"""
from __future__ import annotations

import ast
import json
import math
from pathlib import Path
import sys


def class_names(value):
    if isinstance(value, str):
        value = ast.literal_eval(value)
    if isinstance(value, dict):
        value = [value[i] if i in value else value[str(i)] for i in range(len(value))]
    if not isinstance(value, list) or not value:
        raise ValueError("Class names must be a non-empty list in class ID order")
    if any(not isinstance(name, str) or not name.strip() for name in value):
        raise ValueError("Class names must be non-empty strings")
    if len(set(value)) != len(value):
        raise ValueError("Class names must be unique")
    return value


class Detector:
    def __init__(self, path):
        try:
            import onnxruntime as ort
        except ImportError as exc:
            raise RuntimeError("Install dependencies: python -m pip install -r requirements-onnx.txt") from exc
        if not Path(path).is_file() or Path(path).suffix.lower() != ".onnx":
            raise ValueError("Select an existing .onnx model file")
        self.session = ort.InferenceSession(str(path), providers=["CPUExecutionProvider"])
        inputs = self.session.get_inputs()
        if len(inputs) != 1 or len(inputs[0].shape) != 4:
            raise ValueError("Expected one NCHW image input [1, 3, height, width]")
        self.input = inputs[0]
        batch, channels, _, _ = self.input.shape
        if (isinstance(batch, int) and batch != 1) or channels != 3:
            raise ValueError("Only batch 1, RGB NCHW inputs are supported")
        if self.input.type not in ("tensor(float)", "tensor(float16)"):
            raise ValueError("Only float32/float16 image inputs are supported")
        if len(self.session.get_outputs()) != 1:
            raise ValueError("Expected one detection output; segmentation/pose models are not supported")
        metadata = self.session.get_modelmeta().custom_metadata_map
        if metadata.get("task", "detect") != "detect":
            raise ValueError("Only object detection models are supported")
        self.names = class_names(metadata["names"]) if "names" in metadata else []

    def infer(self, request):
        import numpy as np
        from PIL import Image, ImageOps

        names = class_names(request.get("classes") or self.names)
        if self.names and names != self.names:
            raise ValueError("Class names/order differ from model metadata; use the model's class order")
        size = int(request.get("input_size", 640))
        if not 32 <= size <= 4096:
            raise ValueError("Dynamic input size must be between 32 and 4096")
        height, width = [d if isinstance(d, int) and d > 0 else size for d in self.input.shape[2:]]
        if not (0 < height <= 4096 and 0 < width <= 4096):
            raise ValueError("Model image dimensions must be between 1 and 4096")
        with Image.open(request["image_path"]) as source:
            image = ImageOps.exif_transpose(source).convert("RGB")
        original_width, original_height = image.size
        ratio = min(width / original_width, height / original_height)
        resized = (max(1, round(original_width * ratio)), max(1, round(original_height * ratio)))
        left, top = (width - resized[0]) // 2, (height - resized[1]) // 2
        padded = Image.new("RGB", (width, height), (114, 114, 114))
        padded.paste(image.resize(resized, Image.Resampling.BILINEAR), (left, top))
        dtype = np.float16 if self.input.type == "tensor(float16)" else np.float32
        tensor = np.asarray(padded).transpose(2, 0, 1)[None].astype(dtype) / dtype(255)
        output = self.session.run(None, {self.input.name: np.ascontiguousarray(tensor)})[0]
        return decode(output, request, names, ratio, left, top, original_width, original_height)


def decode(output, request, names, ratio, left, top, width, height):
    import numpy as np

    score_threshold = float(request.get("score_threshold", 0.25))
    iou_threshold = float(request.get("iou_threshold", 0.45))
    if not all(math.isfinite(v) and 0 <= v <= 1 for v in (score_threshold, iou_threshold)):
        raise ValueError("Thresholds must be finite values between 0 and 1")
    output = np.asarray(output)
    if output.ndim != 3 or output.shape[0] != 1:
        raise ValueError(f"Expected batch-1 rank-3 detection output, got {output.shape}")
    layout = request.get("layout", "yolo8")
    rows = output[0]
    if layout == "yolo8":
        if rows.shape[0] != 4 + len(names):
            raise ValueError(f"YOLOv8/11 requires [1, {4 + len(names)}, N]; got {output.shape}")
        rows = rows.T
        classes = rows[:, 4:].argmax(axis=1)
        scores = rows[np.arange(len(rows)), classes + 4]
    elif layout == "yolo5":
        if rows.shape[1] != 5 + len(names):
            raise ValueError(f"YOLOv5 requires [1, N, {5 + len(names)}]; got {output.shape}")
        classes = rows[:, 5:].argmax(axis=1)
        scores = rows[:, 4] * rows[np.arange(len(rows)), classes + 5]
    elif layout == "xyxy":
        if rows.shape[1] != 6:
            raise ValueError(f"End-to-end/NMS output requires [1, N, 6]; got {output.shape}")
        classes, scores = rows[:, 5], rows[:, 4]
    else:
        raise ValueError("Unsupported detection layout")
    selected = np.isfinite(rows).all(axis=1) & (scores >= score_threshold) & (scores > 0)
    rows, classes, scores = rows[selected], classes[selected], scores[selected]
    if np.any((scores < 0) | (scores > 1)):
        raise ValueError("Expected probability scores in [0, 1]")
    if np.any((classes < 0) | (classes >= len(names)) | (classes != np.floor(classes))):
        raise ValueError("Detection class ID is outside the supplied class list")
    boxes = rows[:, :4].astype(np.float64).copy()
    if layout != "xyxy":
        centers, sizes = boxes[:, :2].copy(), boxes[:, 2:].copy()
        boxes[:, :2], boxes[:, 2:] = centers - sizes / 2, centers + sizes / 2
    boxes[:, [0, 2]] = np.clip((boxes[:, [0, 2]] - left) / ratio, 0, width)
    boxes[:, [1, 3]] = np.clip((boxes[:, [1, 3]] - top) / ratio, 0, height)
    valid = (boxes[:, 2] > boxes[:, 0]) & (boxes[:, 3] > boxes[:, 1])
    boxes, classes, scores = boxes[valid], classes[valid].astype(int), scores[valid]
    order = np.argsort(-scores, kind="stable")[:30000]
    areas = (boxes[:, 2:] - boxes[:, :2]).prod(axis=1)
    kept = []
    while order.size and len(kept) < 300:
        index = int(order[0])
        kept.append(index)
        rest = order[1:]
        intersection_size = np.maximum(0, np.minimum(boxes[index, 2:], boxes[rest, 2:]) -
                                       np.maximum(boxes[index, :2], boxes[rest, :2]))
        intersection = intersection_size.prod(axis=1)
        iou = intersection / (areas[index] + areas[rest] - intersection)
        order = rest[~((classes[rest] == classes[index]) & (iou > iou_threshold))]
    return [{"shape_type": "rectangle", "label": names[classes[i]],
             "points": [boxes[i, :2].tolist(), boxes[i, 2:].tolist()],
             "description": json.dumps({"score": float(scores[i])}), "flags": {}}
            for i in kept]


def process(request, cache):
    path = str(Path(request["model_path"]).resolve())
    stat = Path(path).stat()
    key = (path, stat.st_mtime_ns, stat.st_size)
    if key not in cache:
        detector = Detector(path)
        cache.clear()
        cache[key] = detector
    detector = cache[key]
    if request.get("operation") == "load":
        return {"ok": True, "classes": detector.names, "input_shape": detector.input.shape}
    if request.get("operation") != "infer":
        raise ValueError("Unknown operation")
    return {"ok": True, "shapes": detector.infer(request)}


def main():
    # Use UTF-8 explicitly on Windows, including non-ASCII labels and paths.
    sys.stdout.reconfigure(encoding="utf-8")
    cache = {}
    lines = sys.stdin.buffer if "--server" in sys.argv else [sys.stdin.buffer.read()]
    for line in lines:
        if not line.strip():
            continue
        try:
            response = process(json.loads(line.decode("utf-8-sig")), cache)
        except Exception as exc:
            response = {"ok": False, "error": f"{type(exc).__name__}: {exc}"}
        print(json.dumps(response, ensure_ascii=False, allow_nan=False), flush=True)


if __name__ == "__main__":
    main()

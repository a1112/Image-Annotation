import type { AnnotationObject, DatasetImage, Point } from "../../types/domain";
import { bboxHandles, normalizeBox, pointDistance } from "./geometry";
import type { CanvasSize, CanvasViewport, ToolMode } from "./types";

export function drawAnnotationCanvas({
  activeImage,
  canvas,
  ctx,
  draftBox,
  draftPolygon,
  aiPromptPoints,
  maskStroke,
  maskBrushSize,
  maskImages,
  brightness,
  imageElement,
  imageReady,
  mode,
  objects,
  hiddenObjectIds,
  selectedObjectIds,
  size,
  viewport,
}: {
  activeImage: DatasetImage | undefined;
  canvas: HTMLCanvasElement;
  ctx: CanvasRenderingContext2D;
  draftBox: { start: { x: number; y: number }; end: { x: number; y: number } } | null;
  draftPolygon: Point[];
  aiPromptPoints: Array<{ point: Point; label: number }>;
  maskStroke: Point[];
  maskBrushSize: number;
  maskImages: ReadonlyMap<string, HTMLImageElement>;
  brightness: number;
  imageElement: HTMLImageElement | null;
  imageReady: boolean;
  mode: ToolMode;
  objects: AnnotationObject[];
  hiddenObjectIds: ReadonlySet<string>;
  selectedObjectIds: ReadonlySet<string>;
  size: CanvasSize;
  viewport: CanvasViewport;
}) {
  const dpr = window.devicePixelRatio || 1;
  const width = Math.max(1, Math.round(size.width));
  const height = Math.max(1, Math.round(size.height));
  const pixelWidth = Math.round(width * dpr);
  const pixelHeight = Math.round(height * dpr);
  if (canvas.width !== pixelWidth || canvas.height !== pixelHeight) {
    canvas.width = pixelWidth;
    canvas.height = pixelHeight;
  }

  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, width, height);
  ctx.fillStyle = "#111827";
  ctx.fillRect(0, 0, width, height);
  ctx.strokeStyle = "rgba(255, 255, 255, 0.05)";
  ctx.lineWidth = 1;
  for (let x = 0; x < width; x += 48) {
    ctx.beginPath();
    ctx.moveTo(x, 0);
    ctx.lineTo(x, height);
    ctx.stroke();
  }
  for (let y = 0; y < height; y += 48) {
    ctx.beginPath();
    ctx.moveTo(0, y);
    ctx.lineTo(width, y);
    ctx.stroke();
  }

  const imageWidth = activeImage?.width || 640;
  const imageHeight = activeImage?.height || 480;
  ctx.save();
  ctx.translate(viewport.offsetX, viewport.offsetY);
  ctx.scale(viewport.scale, viewport.scale);
  ctx.imageSmoothingEnabled = true;
  ctx.imageSmoothingQuality = "high";

  if (imageReady && imageElement) {
    ctx.filter = `brightness(${brightness}%)`;
    ctx.drawImage(imageElement, 0, 0, imageWidth, imageHeight);
    ctx.filter = "none";
  } else {
    ctx.fillStyle = "#253044";
    ctx.fillRect(0, 0, imageWidth, imageHeight);
    ctx.fillStyle = "#94a3b8";
    ctx.font = `${Math.max(13 / viewport.scale, 10)}px sans-serif`;
    ctx.fillText("加载图片", 18 / viewport.scale, 28 / viewport.scale);
  }

  ctx.strokeStyle = "rgba(255, 255, 255, 0.42)";
  ctx.lineWidth = 1 / viewport.scale;
  ctx.strokeRect(0, 0, imageWidth, imageHeight);

  function drawBox(box: NonNullable<AnnotationObject["bbox"]>, label: string, selected: boolean, draft = false, color = "#1fa7ff") {
    ctx.save();
    ctx.lineWidth = (selected ? 3 : 2) / viewport.scale;
    ctx.strokeStyle = draft ? "#1769e0" : color;
    ctx.fillStyle = selected ? "rgba(23, 105, 224, 0.16)" : "rgba(31, 167, 255, 0.08)";
    if (draft) {
      ctx.setLineDash([6 / viewport.scale, 4 / viewport.scale]);
      ctx.fillStyle = "rgba(23, 105, 224, 0.1)";
    }
    ctx.fillRect(box.x, box.y, box.width, box.height);
    ctx.strokeRect(box.x, box.y, box.width, box.height);
    ctx.setLineDash([]);

    if (!draft) {
      const fontSize = Math.max(14 / viewport.scale, 9);
      const labelX = box.x + 4 / viewport.scale;
      const labelY = Math.max(fontSize + 2 / viewport.scale, box.y - 4 / viewport.scale);
      ctx.font = `700 ${fontSize}px sans-serif`;
      ctx.lineJoin = "round";
      ctx.strokeStyle = "rgba(17, 24, 39, 0.86)";
      ctx.lineWidth = 4 / viewport.scale;
      ctx.strokeText(label, labelX, labelY);
      ctx.fillStyle = "#ffffff";
      ctx.fillText(label, labelX, labelY);
    }

    if (selected) {
      const handleSize = 8 / viewport.scale;
      ctx.fillStyle = "#ffffff";
      ctx.strokeStyle = "#1769e0";
      ctx.lineWidth = 2 / viewport.scale;
      bboxHandles(box).forEach((handle) => {
        ctx.fillRect(handle.x - handleSize / 2, handle.y - handleSize / 2, handleSize, handleSize);
        ctx.strokeRect(handle.x - handleSize / 2, handle.y - handleSize / 2, handleSize, handleSize);
      });
    }
    ctx.restore();
  }

  objects.forEach((object) => {
    if (hiddenObjectIds.has(object.id)) return;
    const color = typeof object.attributes.lineColor === "string" && /^#[0-9a-fA-F]{6}$/.test(object.attributes.lineColor)
      ? object.attributes.lineColor : "#cc54d8";
    if (object.type === "bbox" && object.bbox) {
      drawBox(object.bbox, object.label, selectedObjectIds.has(object.id), false, color);
    } else if (object.polygon) {
      ctx.save();
      ctx.beginPath();
      object.polygon.forEach((point, index) => {
        if (index === 0) ctx.moveTo(point.x, point.y);
        else ctx.lineTo(point.x, point.y);
      });
      ctx.closePath();
      ctx.fillStyle = "rgba(204, 84, 216, 0.16)";
      const selected = selectedObjectIds.has(object.id);
      ctx.strokeStyle = color;
      ctx.lineWidth = (selected ? 3 : 2) / viewport.scale;
      ctx.fill();
      ctx.stroke();
      if (selected) {
        const handleRadius = 5 / viewport.scale;
        ctx.fillStyle = "#ffffff";
        ctx.strokeStyle = color;
        ctx.lineWidth = 2 / viewport.scale;
        object.polygon.forEach((point) => {
          ctx.fillRect(
            point.x - handleRadius,
            point.y - handleRadius,
            handleRadius * 2,
            handleRadius * 2,
          );
          ctx.strokeRect(
            point.x - handleRadius,
            point.y - handleRadius,
            handleRadius * 2,
            handleRadius * 2,
          );
        });
      }
      ctx.restore();
    } else if (object.type === "mask" && object.points?.length === 2 && object.maskData) {
      const maskImage = maskImages.get(object.maskData);
      ctx.save();
      if (maskImage?.complete && maskImage.naturalWidth > 0) {
        ctx.globalAlpha = 0.55;
        ctx.drawImage(maskImage, object.points[0].x, object.points[0].y,
          object.points[1].x - object.points[0].x, object.points[1].y - object.points[0].y);
      }
      ctx.strokeStyle = color;
      ctx.lineWidth = 2 / viewport.scale;
      ctx.strokeRect(object.points[0].x, object.points[0].y,
        object.points[1].x - object.points[0].x, object.points[1].y - object.points[0].y);
      ctx.restore();
    } else if (object.points && object.points.length) {
      ctx.save();
      ctx.beginPath();
      const selected = selectedObjectIds.has(object.id);
      ctx.strokeStyle = color;
      ctx.fillStyle = ctx.strokeStyle;
      ctx.lineWidth = (selected ? 3 : 2) / viewport.scale;
      if (object.type === "circle" && object.points.length >= 2) {
        ctx.arc(object.points[0].x, object.points[0].y, pointDistance(object.points[0], object.points[1]), 0, Math.PI * 2);
      } else if (object.type === "point" || object.type === "points") {
        object.points.forEach((point) => {
          ctx.moveTo(point.x + 5 / viewport.scale, point.y);
          ctx.arc(point.x, point.y, 5 / viewport.scale, 0, Math.PI * 2);
        });
      } else {
        object.points.forEach((point, index) => {
          if (index === 0) ctx.moveTo(point.x, point.y);
          else ctx.lineTo(point.x, point.y);
        });
      }
      if (object.type === "oriented_rectangle") {
        ctx.closePath();
        ctx.fillStyle = "rgba(204, 84, 216, 0.16)";
        ctx.fill();
      }
      if (object.type === "point" || object.type === "points") ctx.fill();
      else ctx.stroke();
      if (selected) {
        ctx.fillStyle = "#ffffff";
        object.points.forEach((point) => {
          ctx.fillRect(point.x - 4 / viewport.scale, point.y - 4 / viewport.scale, 8 / viewport.scale, 8 / viewport.scale);
        });
      }
      ctx.restore();
    }
  });

  if (draftBox) {
    if (mode === "line" || mode === "circle") {
      ctx.save();
      ctx.beginPath();
      ctx.strokeStyle = "#1769e0";
      ctx.lineWidth = 2 / viewport.scale;
      ctx.setLineDash([6 / viewport.scale, 4 / viewport.scale]);
      if (mode === "circle") {
        ctx.arc(draftBox.start.x, draftBox.start.y, pointDistance(draftBox.start, draftBox.end), 0, Math.PI * 2);
      } else {
        ctx.moveTo(draftBox.start.x, draftBox.start.y);
        ctx.lineTo(draftBox.end.x, draftBox.end.y);
      }
      ctx.stroke();
      ctx.restore();
    } else {
      drawBox(normalizeBox(draftBox.start, draftBox.end), "", false, true);
    }
  }

  if (draftPolygon.length) {
    ctx.save();
    ctx.beginPath();
    draftPolygon.forEach((point, index) => {
      if (index === 0) ctx.moveTo(point.x, point.y);
      else ctx.lineTo(point.x, point.y);
    });
    ctx.strokeStyle = "#f0abfc";
    ctx.lineWidth = 2 / viewport.scale;
    ctx.setLineDash([6 / viewport.scale, 4 / viewport.scale]);
    ctx.stroke();
    ctx.setLineDash([]);
    const handleRadius = 5 / viewport.scale;
    draftPolygon.forEach((point, index) => {
      ctx.fillStyle = index === 0 && draftPolygon.length >= 3 ? "#cc54d8" : "#ffffff";
      ctx.strokeStyle = "#cc54d8";
      ctx.fillRect(
        point.x - handleRadius,
        point.y - handleRadius,
        handleRadius * 2,
        handleRadius * 2,
      );
      ctx.strokeRect(
        point.x - handleRadius,
        point.y - handleRadius,
        handleRadius * 2,
        handleRadius * 2,
      );
    });
    ctx.restore();
  }

  if (aiPromptPoints.length) {
    ctx.save();
    ctx.lineWidth = 2 / viewport.scale;
    aiPromptPoints.forEach(({ point, label }, index) => {
      if (label === 3 && aiPromptPoints[index - 1]?.label === 2) {
        const first = aiPromptPoints[index - 1].point;
        ctx.strokeStyle = "#22c55e";
        ctx.strokeRect(first.x, first.y, point.x - first.x, point.y - first.y);
      }
      ctx.beginPath();
      ctx.arc(point.x, point.y, 5 / viewport.scale, 0, Math.PI * 2);
      ctx.fillStyle = label === 0 ? "#ef4444" : "#22c55e";
      ctx.fill();
    });
    ctx.restore();
  }

  if (maskStroke.length) {
    ctx.save();
    ctx.beginPath();
    ctx.moveTo(maskStroke[0].x, maskStroke[0].y);
    maskStroke.slice(1).forEach((point) => ctx.lineTo(point.x, point.y));
    ctx.strokeStyle = mode === "mask_erase" ? "#ef4444" : "#22c55e";
    ctx.lineWidth = maskBrushSize;
    ctx.lineCap = "round";
    ctx.lineJoin = "round";
    ctx.stroke();
    ctx.restore();
  }

  if (mode === "pan") {
    ctx.restore();
    ctx.fillStyle = "rgba(17, 24, 39, 0.62)";
    ctx.fillRect(12, height - 36, 120, 24);
    ctx.fillStyle = "#ffffff";
    ctx.font = "12px sans-serif";
    ctx.fillText("拖拽平移画布", 24, height - 20);
    return;
  }

  ctx.restore();
}

import type { AnnotationObject, BBox, Point } from "../../types/domain";
import type { CanvasViewport, ResizeHandle } from "./types";

const annotationPalette = ["#0b84f3", "#18a77c", "#e58a12", "#d14fd7", "#e14f5a", "#5969e8", "#00a5b8", "#8a62d3"];

export function annotationColor(label: string) {
  let hash = 0;
  for (let index = 0; index < label.length; index += 1) hash = (hash * 31 + label.charCodeAt(index)) >>> 0;
  return annotationPalette[hash % annotationPalette.length];
}

export function clamp(value: number, min: number, max: number) { return Math.max(min, Math.min(max, value)); }

export function normalizeBox(start: Point, end: Point): BBox {
  return { x: Number(Math.min(start.x, end.x).toFixed(1)), y: Number(Math.min(start.y, end.y).toFixed(1)), width: Number(Math.abs(end.x - start.x).toFixed(1)), height: Number(Math.abs(end.y - start.y).toFixed(1)) };
}

export function pointDistance(left: Point, right: Point) { return Math.hypot(left.x - right.x, left.y - right.y); }

export function translateShapePoints(object: AnnotationObject, points: Point[], dx: number, dy: number, width: number, height: number): Point[] {
  if (!points.length) return points;
  let minX = Math.min(...points.map((point) => point.x));
  let maxX = Math.max(...points.map((point) => point.x));
  let minY = Math.min(...points.map((point) => point.y));
  let maxY = Math.max(...points.map((point) => point.y));
  if (object.type === "circle" && points.length === 2) {
    const radius = pointDistance(points[0], points[1]);
    minX = points[0].x - radius; maxX = points[0].x + radius;
    minY = points[0].y - radius; maxY = points[0].y + radius;
  }
  const boundedDx = clamp(dx, -minX, width - maxX);
  const boundedDy = clamp(dy, -minY, height - maxY);
  return points.map((point) => ({ x: Number((point.x + boundedDx).toFixed(1)), y: Number((point.y + boundedDy).toFixed(1)) }));
}

export function resizeBox(original: BBox, dx: number, dy: number, handle: ResizeHandle, imageWidth: number, imageHeight: number): BBox {
  const minSize = 3;
  let left = original.x; let top = original.y; let right = original.x + original.width; let bottom = original.y + original.height;
  if (handle.includes("w")) left = clamp(left + dx, 0, right - minSize);
  if (handle.includes("e")) right = clamp(right + dx, left + minSize, imageWidth);
  if (handle.includes("n")) top = clamp(top + dy, 0, bottom - minSize);
  if (handle.includes("s")) bottom = clamp(bottom + dy, top + minSize, imageHeight);
  return { x: Number(left.toFixed(1)), y: Number(top.toFixed(1)), width: Number((right - left).toFixed(1)), height: Number((bottom - top).toFixed(1)) };
}

export function bboxHandles(box: BBox) {
  return [{ handle: "nw" as const, x: box.x, y: box.y }, { handle: "ne" as const, x: box.x + box.width, y: box.y }, { handle: "sw" as const, x: box.x, y: box.y + box.height }, { handle: "se" as const, x: box.x + box.width, y: box.y + box.height }];
}

export function isEditableShortcutTarget(target: EventTarget | null) {
  if (!(target instanceof HTMLElement)) return false;
  return Boolean(target.closest("input, textarea, select, [contenteditable='true']"));
}

export function fitCanvasViewport(imageWidth: number, imageHeight: number, canvasWidth: number, canvasHeight: number): CanvasViewport {
  const padding = 36;
  const scale = clamp(Math.min(Math.max(1, canvasWidth - padding * 2) / imageWidth, Math.max(1, canvasHeight - padding * 2) / imageHeight, 1), 0.1, 8);
  return { scale, offsetX: Math.round((canvasWidth - imageWidth * scale) / 2), offsetY: Math.round((canvasHeight - imageHeight * scale) / 2) };
}

export function zoomCanvasViewport(viewport: CanvasViewport, nextScale: number, focalPoint: Point): CanvasViewport {
  const scale = clamp(nextScale, 0.1, 8);
  const imageX = (focalPoint.x - viewport.offsetX) / viewport.scale;
  const imageY = (focalPoint.y - viewport.offsetY) / viewport.scale;
  return { scale, offsetX: focalPoint.x - imageX * scale, offsetY: focalPoint.y - imageY * scale };
}

export function pointToSegmentDistance(point: Point, start: Point, end: Point) {
  const dx = end.x - start.x; const dy = end.y - start.y; const lengthSquared = dx * dx + dy * dy;
  if (lengthSquared === 0) return pointDistance(point, start);
  const ratio = clamp(((point.x - start.x) * dx + (point.y - start.y) * dy) / lengthSquared, 0, 1);
  return pointDistance(point, { x: start.x + ratio * dx, y: start.y + ratio * dy });
}

export function pointInPolygon(point: Point, polygon: Point[]) {
  let inside = false;
  for (let current = 0, previous = polygon.length - 1; current < polygon.length; previous = current++) {
    const currentPoint = polygon[current]; const previousPoint = polygon[previous];
    const crosses = (currentPoint.y > point.y) !== (previousPoint.y > point.y) && point.x < ((previousPoint.x - currentPoint.x) * (point.y - currentPoint.y)) / (previousPoint.y - currentPoint.y) + currentPoint.x;
    if (crosses) inside = !inside;
  }
  return inside;
}

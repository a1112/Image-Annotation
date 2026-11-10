export type PointerPosition = { x: number; y: number };

export type PointerViewport = {
  scale: number;
  offsetX: number;
  offsetY: number;
};

export function pointerDistance(left: PointerPosition, right: PointerPosition): number {
  return Math.hypot(left.x - right.x, left.y - right.y);
}

export function pointerCenter(left: PointerPosition, right: PointerPosition): PointerPosition {
  return { x: (left.x + right.x) / 2, y: (left.y + right.y) / 2 };
}

function zoomAt(viewport: PointerViewport, nextScale: number, focalPoint: PointerPosition): PointerViewport {
  const scale = Math.min(8, Math.max(0.1, nextScale));
  const imageX = (focalPoint.x - viewport.offsetX) / viewport.scale;
  const imageY = (focalPoint.y - viewport.offsetY) / viewport.scale;
  return {
    scale,
    offsetX: focalPoint.x - imageX * scale,
    offsetY: focalPoint.y - imageY * scale,
  };
}

/**
 * Apply a two-pointer gesture to a canvas viewport. The image point under the
 * initial pinch centre stays under that centre while the midpoint translates,
 * so the same helper handles pinch zoom and two-finger pan.
 */
export function pinchCanvasViewport(
  viewport: PointerViewport,
  startLeft: PointerPosition,
  startRight: PointerPosition,
  currentLeft: PointerPosition,
  currentRight: PointerPosition,
): PointerViewport {
  const startDistance = Math.max(pointerDistance(startLeft, startRight), 1);
  const currentDistance = Math.max(pointerDistance(currentLeft, currentRight), 1);
  const startCenter = pointerCenter(startLeft, startRight);
  const currentCenter = pointerCenter(currentLeft, currentRight);
  const zoomed = zoomAt(viewport, viewport.scale * currentDistance / startDistance, startCenter);
  return {
    ...zoomed,
    offsetX: zoomed.offsetX + currentCenter.x - startCenter.x,
    offsetY: zoomed.offsetY + currentCenter.y - startCenter.y,
  };
}

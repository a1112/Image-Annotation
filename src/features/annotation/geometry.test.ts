import { describe, expect, it } from "vitest";
import { fitCanvasViewport, normalizeBox, pointInPolygon, resizeBox, zoomCanvasViewport } from "./geometry";

describe("annotation geometry", () => {
  it("normalizes boxes and keeps resize handles inside the image", () => {
    expect(normalizeBox({ x: 90, y: 70 }, { x: 10, y: 20 })).toEqual({ x: 10, y: 20, width: 80, height: 50 });
    expect(resizeBox({ x: 10, y: 20, width: 80, height: 50 }, -100, -100, "nw", 100, 100)).toEqual({
      x: 0, y: 0, width: 90, height: 70,
    });
  });

  it("zooms around a focal point without moving the focal image coordinate", () => {
    const viewport = fitCanvasViewport(1000, 500, 500, 400);
    const focal = { x: 250, y: 200 };
    const next = zoomCanvasViewport(viewport, viewport.scale * 2, focal);
    const before = { x: (focal.x - viewport.offsetX) / viewport.scale, y: (focal.y - viewport.offsetY) / viewport.scale };
    const after = { x: (focal.x - next.offsetX) / next.scale, y: (focal.y - next.offsetY) / next.scale };
    expect(after.x).toBeCloseTo(before.x);
    expect(after.y).toBeCloseTo(before.y);
  });

  it("selects polygon interior but not exterior points", () => {
    const polygon = [{ x: 0, y: 0 }, { x: 100, y: 0 }, { x: 100, y: 100 }, { x: 0, y: 100 }];
    expect(pointInPolygon({ x: 40, y: 60 }, polygon)).toBe(true);
    expect(pointInPolygon({ x: 140, y: 60 }, polygon)).toBe(false);
  });
});

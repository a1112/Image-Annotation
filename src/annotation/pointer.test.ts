import { describe, expect, it } from "vitest";
import { pinchCanvasViewport, pointerCenter, pointerDistance } from "./pointer";

describe("annotation pointer gestures", () => {
  it("computes distance and midpoint for a touch pair", () => {
    expect(pointerDistance({ x: 0, y: 0 }, { x: 3, y: 4 })).toBe(5);
    expect(pointerCenter({ x: 0, y: 4 }, { x: 10, y: 8 })).toEqual({ x: 5, y: 6 });
  });

  it("keeps the pinch anchor stable while zooming and translating", () => {
    const viewport = { scale: 1, offsetX: 10, offsetY: 20 };
    const next = pinchCanvasViewport(
      viewport,
      { x: 100, y: 100 },
      { x: 200, y: 100 },
      { x: 120, y: 80 },
      { x: 320, y: 80 },
    );

    expect(next.scale).toBe(2);
    // Initial midpoint (150, 100) moves to (220, 80); the image coordinate
    // under the midpoint is therefore preserved before that translation.
    expect(next.offsetX).toBeCloseTo(-60);
    expect(next.offsetY).toBeCloseTo(-80);
  });
});

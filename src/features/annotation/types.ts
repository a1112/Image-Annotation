import type { MouseEvent, PointerEvent as ReactPointerEvent, WheelEvent } from "react";

export type ToolMode = "select" | "bbox" | "polygon" | "pan" | "point" | "line" | "circle" | "linestrip" | "points" | "oriented_rectangle" | "mask" | "mask_erase" | "ai_point_positive" | "ai_point_negative" | "ai_box";
export type CanvasViewport = { scale: number; offsetX: number; offsetY: number };
export type CanvasSize = { width: number; height: number };
export type CanvasPointEvent = MouseEvent<HTMLCanvasElement> | ReactPointerEvent<HTMLCanvasElement>;
export type CanvasLocalEvent = CanvasPointEvent | WheelEvent<HTMLCanvasElement>;
export type ResizeHandle = "nw" | "ne" | "sw" | "se";

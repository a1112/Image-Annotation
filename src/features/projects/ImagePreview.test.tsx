import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ImagePreviewDialog, ThumbnailAnnotationOverlay } from "./ImagePreview";
import type { AnnotationObject, DatasetImage } from "../../types/domain";

const image: DatasetImage = {
  id: "one", fileName: "one.jpg", width: 640, height: 480, split: "train",
  status: "completed", qaStatus: "", reviewNote: null, tags: [],
};
const box: AnnotationObject = {
  id: "box", classId: 1, label: "car", type: "bbox",
  bbox: { x: 10, y: 20, width: 30, height: 40 }, attributes: {},
};

describe("project image previews", () => {
  it("renders supported thumbnail annotations", () => {
    render(<ThumbnailAnnotationOverlay image={image} objects={[box]} />);
    expect(screen.getByLabelText("one.jpg 标注框 car")).toHaveAttribute("width", "30");
  });

  it("toggles a class in the preview and closes on Escape", () => {
    const onClose = vi.fn();
    render(<ImagePreviewDialog image={image} imageUrl={undefined} images={[image]} imageUrls={{}} objects={[box]} onAnnotate={vi.fn()} onClose={onClose} onSelectImage={vi.fn()} />);
    expect(screen.getByLabelText("one.jpg 标注框 car")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /car 1/ }));
    expect(screen.queryByLabelText("one.jpg 标注框 car")).not.toBeInTheDocument();
    fireEvent.keyDown(window, { key: "Escape" });
    expect(onClose).toHaveBeenCalledOnce();
  });
});

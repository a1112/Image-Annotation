import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ImageTransformsPanel } from "./ImageTransformsPanel";

const transformApi = vi.hoisted(() => ({
  parseWebDatasetIdentity: vi.fn((datasetId: string) => {
    const separator = datasetId.indexOf("::");
    return separator > 0
      ? { dataProjectId: datasetId.slice(0, separator), datasetKey: datasetId.slice(separator + 2) }
      : null;
  }),
  listWebDataProjectImages: vi.fn(),
  createWebDataProjectTransform: vi.fn(),
  getWebImageTransform: vi.fn(),
  getWebImageTransformDownloadUrl: vi.fn((taskId: string) => `/api/v1/image-transforms/${taskId}/download`),
  getWebImageContentUrl: vi.fn((datasetId: string, imageId: string) => `/api/v1/web/datasets/${datasetId}/images/${imageId}/content`),
  listWebDataProjectTransforms: vi.fn(),
  resolveWebResourceUrl: vi.fn((url: string) => url),
}));

vi.mock("../../api/web", () => transformApi);

const sourceImage = {
  id: "image-1",
  fileName: "road.png",
  width: 1000,
  height: 600,
  split: "train",
  status: "raw",
  qaStatus: "pending",
  reviewNote: null,
  tags: [],
  imageHash: "a".repeat(64),
  imageUrl: "https://workspace.example.test/image-annotation/api/v1/web/projects/project-uuid/objects/image",
  annotationUrl: "https://workspace.example.test/image-annotation/api/v1/web/projects/project-uuid/objects/annotation",
};

beforeEach(() => {
  vi.clearAllMocks();
  transformApi.listWebDataProjectImages.mockResolvedValue({ items: [sourceImage], nextCursor: null });
  transformApi.createWebDataProjectTransform.mockResolvedValue({ taskId: "task-1", status: "queued", created: true });
  transformApi.listWebDataProjectTransforms.mockResolvedValue([]);
  transformApi.getWebImageTransform.mockResolvedValue({
    taskId: "task-1",
    status: "succeeded",
    outputHash: "b".repeat(64),
    outputMimeType: "image/png",
    outputByteSize: 1234,
    outputWidth: 320,
    outputHeight: 600,
  });
});

describe("ImageTransformsPanel", () => {
  it("submits the server source hash and exposes a new output download", async () => {
    const user = userEvent.setup();
    render(<ImageTransformsPanel datasetId="project-uuid::road" imageId="image-1" />);

    expect(screen.getByRole("button", { name: "展开转换面板" })).toHaveAttribute("aria-expanded", "false");
    await user.click(screen.getByRole("button", { name: "展开转换面板" }));
    await screen.findByText("源文件：road.png");
    expect(screen.queryByText(/project-uuid|sourceHash/)).not.toBeInTheDocument();
    expect(screen.getByRole("link", { name: /下载原图/ })).toHaveAttribute("href", sourceImage.imageUrl);
    expect(screen.getByRole("link", { name: /下载标签/ })).toHaveAttribute("href", sourceImage.annotationUrl);
    await user.type(screen.getByLabelText("宽度（可选）"), "320");
    await user.type(screen.getByLabelText("裁剪 x"), "0");
    await user.type(screen.getByLabelText("裁剪 y"), "0");
    await user.type(screen.getByLabelText("裁剪宽度"), "640");
    await user.type(screen.getByLabelText("裁剪高度"), "480");
    await user.click(screen.getByRole("button", { name: "开始转换" }));

    await waitFor(() => expect(transformApi.createWebDataProjectTransform).toHaveBeenCalledWith("project-uuid", expect.objectContaining({
      sourceHash: "a".repeat(64),
      datasetKey: "road",
      width: 320,
      format: "png",
      rotate: 0,
      flip: "none",
      crop: { x: 0, y: 0, width: 640, height: 480 },
    })));
    expect(await screen.findByText("转换完成，原图和标签已保留。")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /下载新图像/ })).toHaveAttribute("href", "/api/v1/image-transforms/task-1/download");
  });

  it("does not offer a transform when the server omitted sourceHash", async () => {
    transformApi.listWebDataProjectImages.mockResolvedValue({ items: [{ ...sourceImage, imageHash: undefined }], nextCursor: null });
    const user = userEvent.setup();
    render(<ImageTransformsPanel datasetId="project-uuid::road" />);

    await user.click(screen.getByRole("button", { name: "展开转换面板" }));
    expect(await screen.findByText("暂无可转换图像。")).toBeInTheDocument();
    expect(transformApi.createWebDataProjectTransform).not.toHaveBeenCalled();
  });

  it("restores durable history and matches its source hash to the image filename", async () => {
    transformApi.listWebDataProjectTransforms.mockResolvedValueOnce([{
      taskId: "task-history",
      status: "succeeded",
      sourceHash: sourceImage.imageHash,
      outputHash: "b".repeat(64),
      outputMimeType: "image/png",
      outputByteSize: 12,
      downloadUrl: "/api/v1/image-transforms/task-history/download",
    }]);
    const user = userEvent.setup();
    render(<ImageTransformsPanel datasetId="project-uuid::road" />);

    await user.click(screen.getByRole("button", { name: "展开转换面板" }));
    expect(await screen.findByText("转换历史（1）")).toBeInTheDocument();
    expect(within(screen.getByRole("article")).getByText("road.png")).toBeInTheDocument();
    expect(within(screen.getByRole("article")).getByRole("link", { name: /下载/ })).toHaveAttribute("href", "/api/v1/image-transforms/task-history/download");
    expect(transformApi.listWebDataProjectTransforms).toHaveBeenCalledWith("project-uuid", "road", { limit: 50 });
  });

  it("keeps durable downloads visible when the current image list is empty", async () => {
    transformApi.listWebDataProjectImages.mockResolvedValueOnce({ items: [], nextCursor: null });
    transformApi.listWebDataProjectTransforms.mockResolvedValueOnce([{
      taskId: "task-orphan",
      status: "succeeded",
      sourceHash: "c".repeat(64),
      outputHash: "d".repeat(64),
    }]);
    const user = userEvent.setup();
    render(<ImageTransformsPanel datasetId="project-uuid::road" />);

    await user.click(screen.getByRole("button", { name: "展开转换面板" }));
    expect(await screen.findByText("转换历史（1）")).toBeInTheDocument();
    expect(within(screen.getByRole("article")).getByText("原图")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /下载/ })).toHaveAttribute("href", "/api/v1/image-transforms/task-orphan/download");
  });

  it("does not let a previous dataset history request repopulate the new dataset", async () => {
    let resolveOld: ((value: unknown) => void) | undefined;
    transformApi.listWebDataProjectImages.mockImplementation((datasetId: string) => Promise.resolve({
      items: [{ ...sourceImage, fileName: datasetId.includes("old") ? "old.png" : "new.png" }],
      nextCursor: null,
    }));
    transformApi.listWebDataProjectTransforms.mockImplementation((_projectId: string, datasetKey: string) => {
      if (datasetKey === "old") return new Promise((resolve) => { resolveOld = resolve; });
      return Promise.resolve([{ taskId: "task-new", status: "succeeded", sourceHash: sourceImage.imageHash }]);
    });
    const user = userEvent.setup();
    const view = render(<ImageTransformsPanel datasetId="project-uuid::old" />);
    await waitFor(() => expect(resolveOld).toBeDefined());
    await user.click(screen.getByRole("button", { name: "展开转换面板" }));
    view.rerender(<ImageTransformsPanel datasetId="project-uuid::new" />);
    await user.click(screen.getByRole("button", { name: "展开转换面板" }));

    await waitFor(() => expect(screen.getAllByText("new.png").length).toBeGreaterThan(0));
    expect(await screen.findByText("转换历史（1）")).toBeInTheDocument();
    resolveOld?.([{ taskId: "task-old", status: "succeeded", sourceHash: sourceImage.imageHash }]);
    await waitFor(() => expect(screen.queryByText("old.png")).not.toBeInTheDocument());
    expect(screen.getByRole("article")).toHaveTextContent("new.png");
  });
});

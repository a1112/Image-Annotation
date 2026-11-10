import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { AnnotationWorkspace } from "./AnnotationWorkspace";
import {
  getFileAssetUrl,
  getImageAnnotationState,
  getProjectDetail,
  listProjectImages,
} from "../../api/tauri";

vi.mock("../../api/tauri", () => ({
  cancelAiAnnotation: vi.fn(),
  exportAnnotationFile: vi.fn(),
  getAiJobStatus: vi.fn(),
  getFileAssetUrl: vi.fn(),
  getImageAnnotationState: vi.fn(),
  getProjectDetail: vi.fn(),
  isBackendUnavailableError: vi.fn(() => false),
  listProjectImages: vi.fn(),
  loadExternalAnnotations: vi.fn(),
  pickDataSource: vi.fn(),
  runAiAnnotation: vi.fn(),
  saveImageAnnotations: vi.fn(),
  setImageVerified: vi.fn(),
  submitImageAnnotations: vi.fn(),
}));

vi.mock("../shared/useImageAssetUrls", () => ({
  useImageAssetUrls: () => ({}),
}));

vi.mock("../../platform/desktop", () => ({
  beginDesktopWindowDrag: vi.fn(),
  runDesktopCommand: vi.fn(),
}));

vi.mock("../../shared/navigation", () => ({
  navigate: vi.fn(),
}));

const image = {
  id: "image-1",
  fileName: "cat.jpg",
  width: 640,
  height: 480,
  split: "train",
  status: "未标注",
  qaStatus: "",
  reviewNote: null,
  tags: [],
};

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(getProjectDetail).mockResolvedValue({
    project: {
      id: "project-1",
      name: "移动项目",
      description: "测试项目",
      annotationTypes: ["BBox"],
      imageCount: 1,
      annotatedPercent: 0,
      reviewCount: 0,
      issueCount: 0,
      classCount: 0,
      tagGroupCount: 0,
      status: "进行中",
      tags: [],
    },
    tagGroups: [],
    classes: [],
    tasks: [],
    qualityChecks: [],
    exportPresets: [],
  });
  vi.mocked(listProjectImages).mockResolvedValue([image]);
  vi.mocked(getFileAssetUrl).mockResolvedValue("");
  vi.mocked(getImageAnnotationState).mockResolvedValue({
    imageId: image.id,
    revision: null,
    objects: [],
    status: "未标注",
    verified: false,
    updatedAt: null,
  });
  if (typeof window.requestAnimationFrame !== "function") {
    window.requestAnimationFrame = ((callback: FrameRequestCallback) => {
      callback(0);
      return 0;
    }) as typeof window.requestAnimationFrame;
  }
  if (typeof window.cancelAnimationFrame !== "function") {
    window.cancelAnimationFrame = (() => undefined) as typeof window.cancelAnimationFrame;
  }
});

describe("AnnotationWorkspace mobile layout", () => {
  it("renders a closed inspector drawer and opens it through the mobile control", async () => {
    render(
      <AnnotationWorkspace
        imageId={image.id}
        mobileRuntime
        projectId="project-1"
        registerLeaveGuard={vi.fn()}
        showWindowControls={false}
      />,
    );

    await waitFor(() => expect(screen.getByTestId("annotation-canvas")).toBeInTheDocument());
    const inspector = document.querySelector(".mobile-inspector");
    expect(document.querySelector(".mobile-annotation-page")).toBeInTheDocument();
    expect(inspector).toBeInTheDocument();
    expect(inspector).not.toHaveClass("is-mobile-open");
    expect(screen.queryByText("project-1 / cat.jpg")).not.toBeInTheDocument();

    const toggle = screen.getByRole("button", { name: "对象属性" });
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    fireEvent.click(toggle);

    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(document.querySelector(".mobile-inspector")).toHaveClass("is-mobile-open");
    expect(screen.getByRole("button", { name: "关闭属性" })).toBeInTheDocument();
    const closeInspector = screen.getByRole("button", { name: "关闭对象属性" });
    expect(closeInspector).toBeInTheDocument();

    const actions = document.querySelector(".annotation-actions");
    expect(actions?.querySelector(".mobile-submit-action")).toHaveClass("mobile-primary-action");
    expect(actions?.querySelector(".mobile-save-action")).toHaveClass("mobile-primary-action");
    const actionButtons = Array.from(actions?.querySelectorAll("button") ?? []);
    const inspectorIndex = actionButtons.findIndex((button) => button.classList.contains("mobile-inspector-toggle"));
    const submitIndex = actionButtons.findIndex((button) => button.classList.contains("mobile-submit-action"));
    const saveIndex = actionButtons.findIndex((button) => button.classList.contains("mobile-save-action"));
    expect(inspectorIndex).toBeGreaterThanOrEqual(0);
    expect(submitIndex).toBeGreaterThan(inspectorIndex);
    expect(saveIndex).toBeGreaterThan(inspectorIndex);
    expect(submitIndex).toBeGreaterThan(saveIndex);
    expect(actions?.querySelector(".mobile-submit-action")?.closest("details")).toHaveClass("annotation-more-actions");

    fireEvent.click(closeInspector);
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(inspector).not.toHaveClass("is-mobile-open");
  });

  it("does not mark the mobile toolbar as a desktop window drag region", async () => {
    render(
      <AnnotationWorkspace
        imageId={image.id}
        mobileRuntime
        projectId="project-1"
        registerLeaveGuard={vi.fn()}
        showWindowControls={false}
      />,
    );

    await waitFor(() => expect(screen.getByTestId("annotation-canvas")).toBeInTheDocument());
    expect(document.querySelector(".annotation-toolbar")).not.toHaveAttribute("data-tauri-drag-region");
  });

  it("keeps one visible leave prompt and resolves pending guards when unmounted", async () => {
    vi.mocked(getImageAnnotationState).mockResolvedValueOnce({
      imageId: image.id,
      revision: "rev-1",
      objects: [{
        id: "object-1",
        classId: 0,
        label: "person",
        type: "bbox",
        bbox: { x: 10, y: 10, width: 20, height: 20 },
        attributes: {},
      }],
      status: "已标注",
      verified: false,
      updatedAt: null,
    });
    const registerLeaveGuard = vi.fn();
    const view = render(
      <AnnotationWorkspace
        imageId={image.id}
        projectId="project-1"
        registerLeaveGuard={registerLeaveGuard}
        showWindowControls={false}
      />,
    );

    await screen.findByRole("button", { name: "person bbox" });
    fireEvent.click(screen.getByRole("button", { name: "复制对象" }));
    const guard = registerLeaveGuard.mock.calls.at(-1)?.[0] as (() => Promise<boolean>) | undefined;
    expect(guard).toEqual(expect.any(Function));

    const first = guard!();
    const second = guard!();
    expect(await screen.findAllByRole("dialog", { name: "未保存的标注" })).toHaveLength(1);
    view.unmount();

    await expect(first).resolves.toBe(false);
    await expect(second).resolves.toBe(false);
  });
});

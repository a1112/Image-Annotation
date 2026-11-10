import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ProjectWorkspace } from "./ProjectWorkspace";
import { createAnnotationTask, getProjectDetail, listProjectImages, openAnnotationWindow } from "../../api/tauri";
import { flushOfflineQueue } from "../../api/mobile";
import { navigate } from "../../shared/navigation";

vi.mock("../../api/tauri", () => ({
  createAnnotationTask: vi.fn(),
  createDatasetSnapshot: vi.fn(),
  createProjectFolder: vi.fn(),
  deleteProjectFolder: vi.fn(),
  exportDataset: vi.fn(),
  getImageAnnotations: vi.fn().mockResolvedValue([]),
  getProjectDetail: vi.fn(),
  isBackendUnavailableError: vi.fn(() => false),
  listClassSamples: vi.fn().mockResolvedValue([]),
  listExports: vi.fn().mockResolvedValue([]),
  listProjectFolders: vi.fn().mockResolvedValue({ folders: [], members: [] }),
  listProjectImages: vi.fn(),
  listSnapshots: vi.fn().mockResolvedValue([]),
  migrateLegacyProjectFolders: vi.fn().mockResolvedValue({ folders: [], members: [] }),
  moveImageToProjectFolder: vi.fn(),
  openAnnotationWindow: vi.fn(),
  renameProjectFolder: vi.fn(),
  upgradeDatasetSnapshotBridge: vi.fn(),
}));

vi.mock("../../api/mobile", () => ({
  flushOfflineQueue: vi.fn(),
}));

vi.mock("../../shared/navigation", () => ({
  navigate: vi.fn(),
}));

vi.mock("../shared/useImageAssetUrls", () => ({
  useImageAssetUrls: () => ({}),
}));

vi.mock("./HybridProjectPanel", () => ({
  HybridProjectPanel: () => null,
}));

const project = {
  id: "project-1",
  name: "移动项目",
  description: "测试项目",
  annotationTypes: ["BBox"],
  imageCount: 2,
  annotatedPercent: 0,
  reviewCount: 0,
  issueCount: 0,
  classCount: 1,
  tagGroupCount: 1,
  status: "已导入",
  tags: [],
};

const images = [
  { id: "image-1", fileName: "one.jpg", width: 640, height: 480, split: "train", status: "未标注", qaStatus: "", reviewNote: null, tags: [] },
  { id: "image-2", fileName: "two.jpg", width: 640, height: 480, split: "train", status: "未标注", qaStatus: "", reviewNote: null, tags: [] },
];

const detail = {
  project,
  tagGroups: [],
  classes: [],
  tasks: [],
  qualityChecks: [],
  exportPresets: [],
};

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(getProjectDetail).mockResolvedValue(detail);
  vi.mocked(listProjectImages).mockResolvedValue(images);
  vi.mocked(createAnnotationTask).mockResolvedValue({
    id: "task-1",
    name: "第一轮标注",
    status: "进行中",
    createdAt: "",
    updatedAt: "",
  });
  vi.mocked(flushOfflineQueue).mockResolvedValue({
    online: true,
    queuedOperations: 0,
    conflictCount: 0,
    cachedAssets: 0,
    lastSyncAt: null,
    lastError: null,
  });
  const tauri = vi.mocked(getProjectDetail);
  tauri.mockResolvedValueOnce(detail).mockResolvedValueOnce({
    ...detail,
    tasks: [{ name: "第一轮标注", owner: "本地工作台", status: "进行中", progress: 0 }],
  });
});

describe("ProjectWorkspace task creation", () => {
  it("creates a task with the selected images, flushes the queue, and refreshes tasks", async () => {
    render(
      <ProjectWorkspace
        projectId="project-1"
        routeTab="任务"
        onProjectContextChange={vi.fn()}
      />,
    );

    await screen.findByRole("heading", { name: "创建任务" });
    const firstImage = await screen.findByRole("checkbox", { name: "选择 one.jpg" });
    const secondImage = await screen.findByRole("checkbox", { name: "选择 two.jpg" });
    expect(firstImage).toBeChecked();
    expect(secondImage).toBeChecked();

    fireEvent.change(screen.getByLabelText("任务名称"), { target: { value: "第一轮标注" } });
    fireEvent.click(secondImage);
    fireEvent.click(screen.getByRole("button", { name: "创建任务" }));

    await waitFor(() => expect(createAnnotationTask).toHaveBeenCalledWith("project-1", "第一轮标注", ["image-1"]));
    expect(flushOfflineQueue).toHaveBeenCalledWith("project-1");
    expect(await screen.findByText("任务已创建并同步。")).toBeInTheDocument();
    expect(await screen.findByText("第一轮标注")).toBeInTheDocument();
  });

  it("keeps selections across image pages and creates from the full selected set", async () => {
    const firstPage = Array.from({ length: 48 }, (_, index) => ({
      id: `image-${index + 1}`,
      fileName: `page-${String(index + 1).padStart(2, "0")}.jpg`,
      width: 640,
      height: 480,
      split: "train",
      status: "未标注",
      qaStatus: "",
      reviewNote: null,
      tags: [],
    }));
    const secondPage = [49, 50].map((number) => ({
      id: `image-${number}`,
      fileName: `page-${number}.jpg`,
      width: 640,
      height: 480,
      split: "train",
      status: "未标注",
      qaStatus: "",
      reviewNote: null,
      tags: [],
    }));
    const allImageIds = [...firstPage, ...secondPage].map((image) => image.id);
    const manyImageProject = { ...project, imageCount: allImageIds.length };
    const manyImageDetail = { ...detail, project: manyImageProject };
    vi.mocked(getProjectDetail).mockReset();
    vi.mocked(getProjectDetail).mockResolvedValue(manyImageDetail);
    vi.mocked(listProjectImages).mockImplementation(async (_projectId, _groupId, page) => (
      page?.offset === 48 ? secondPage : firstPage
    ));

    render(
      <ProjectWorkspace
        projectId="project-1"
        routeTab="任务"
        onProjectContextChange={vi.fn()}
      />,
    );

    const firstImage = await screen.findByRole("checkbox", { name: "选择 page-01.jpg" });
    await waitFor(() => expect(firstImage).toBeChecked());
    fireEvent.click(firstImage);
    await waitFor(() => expect(firstImage).not.toBeChecked());
    expect(screen.getByText("已选 49 / 50 张")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "图片", exact: true }));
    fireEvent.click(await screen.findByRole("button", { name: "下一页" }));
    await screen.findByText("page-49.jpg");
    fireEvent.click(screen.getByRole("button", { name: "任务", exact: true }));

    const secondPageImage = await screen.findByRole("checkbox", { name: "选择 page-49.jpg" });
    expect(secondPageImage).toBeChecked();
    fireEvent.change(screen.getByLabelText("任务名称"), { target: { value: "跨页任务" } });
    fireEvent.click(screen.getByRole("button", { name: "创建任务" }));

    await waitFor(() => expect(createAnnotationTask).toHaveBeenCalledWith(
      "project-1",
      "跨页任务",
      allImageIds.slice(1),
    ));
    expect(flushOfflineQueue).toHaveBeenCalledWith("project-1");
  });
});

describe("ProjectWorkspace mobile navigation", () => {
  it("navigates the current mobile window to annotation instead of opening a native window", async () => {
    vi.mocked(navigate).mockClear();
    vi.mocked(openAnnotationWindow).mockClear();

    render(
      <ProjectWorkspace
        projectId="project-1"
        mobileRuntime
        routeTab="概览"
        onProjectContextChange={vi.fn()}
      />,
    );

    fireEvent.click(await screen.findByRole("button", { name: "继续标注" }));

    await waitFor(() => expect(navigate).toHaveBeenCalledWith("#/annotate/project-1/image-1"));
    expect(openAnnotationWindow).not.toHaveBeenCalled();
  });
});

describe("ProjectWorkspace image browser", () => {
  it("shows Chinese status labels while filtering with the original status values", async () => {
    const statusImages = [
      { ...images[0], status: "inProgress" },
      { ...images[1], status: "pendingReview" },
    ];
    vi.mocked(getProjectDetail).mockReset().mockResolvedValue(detail);
    vi.mocked(listProjectImages).mockResolvedValue(statusImages);

    render(
      <ProjectWorkspace
        projectId="project-1"
        routeTab="图片"
        onProjectContextChange={vi.fn()}
      />,
    );

    expect(await screen.findByRole("heading", { name: "图片" })).toBeInTheDocument();
    const inProgressFilter = screen.getByRole("button", { name: "处理中" });
    expect(inProgressFilter).toHaveAttribute("aria-pressed", "false");
    expect(screen.getByRole("option", { name: /one\.jpg/ })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: /two\.jpg/ })).toBeInTheDocument();

    fireEvent.click(inProgressFilter);

    expect(inProgressFilter).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByRole("option", { name: /one\.jpg/ })).toBeInTheDocument();
    expect(screen.queryByRole("option", { name: /two\.jpg/ })).not.toBeInTheDocument();
  });
});

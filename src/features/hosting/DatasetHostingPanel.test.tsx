import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { DatasetHostingPanel } from "./DatasetHostingPanel";
import {
  addHostedMember,
  captureHostedProject,
  createHostedDataset,
  createHostedVersion,
  getHostedDataset,
  getHostingQuota,
  downloadHostedExport,
  listHostingJobs,
  listHostedMembers,
  listHostedDatasets,
  listHostedVersions,
  openHostedExport,
  reuseHostedVersion,
  startHostedExport,
  updateHostedDataset,
} from "../../api/hosting";
import { authStatus, listRemoteProjects, remoteProjectOpen } from "../../api/mobile";

vi.mock("../../api/hosting", () => ({
  addHostedMember: vi.fn(),
  archiveHostedDataset: vi.fn(),
  beginStableHostingOperation: vi.fn(() => ({ key: "operation-key", operationId: "operation-test" })),
  captureHostedProject: vi.fn(),
  completeStableHostingOperation: vi.fn(),
  createHostedDataset: vi.fn(),
  createHostedVersion: vi.fn(),
  downloadHostedExport: vi.fn(),
  getHostedDataset: vi.fn(),
  getHostedVersion: vi.fn(),
  getHostingJob: vi.fn(),
  getHostingQuota: vi.fn(),
  isHostingConflict: vi.fn(() => false),
  listHostingJobs: vi.fn(),
  listHostedMembers: vi.fn(),
  listHostedDatasets: vi.fn(),
  listHostedVersions: vi.fn(),
  newHostingOperationId: vi.fn(() => "operation-test"),
  readHostingArchive: vi.fn(),
  removeHostedMember: vi.fn(),
  reuseHostedVersion: vi.fn(),
  startHostedExport: vi.fn(),
  openHostedExport: vi.fn(),
  updateHostedDataset: vi.fn(),
  updateHostingQuota: vi.fn(),
  uploadHostedArchive: vi.fn(),
}));

vi.mock("../../api/mobile", () => ({
  authStatus: vi.fn(),
  listRemoteProjects: vi.fn(),
  remoteProjectOpen: vi.fn(),
}));

const dataset = {
  id: "dataset-1",
  name: "道路数据集",
  description: "用于协同标注",
  revision: 2,
  itemCount: 12,
  totalBytes: 4096,
  role: "owner",
  latestVersionId: "version-1",
  createdAt: "2026-10-25T01:00:00Z",
  updatedAt: "2026-10-25T02:00:00Z",
  status: "active",
};

const version = {
  id: "version-1",
  datasetId: "dataset-1",
  name: "首个版本",
  versionNumber: 1,
  revision: 2,
  itemCount: 12,
  totalBytes: 4096,
  createdAt: "2026-10-25T02:00:00Z",
};

const localProject = {
  id: "local-project-1",
  name: "本地道路项目",
  description: "本地缓存",
  annotationTypes: ["bbox"],
  imageCount: 12,
  annotatedPercent: 50,
  reviewCount: 0,
  issueCount: 0,
  classCount: 2,
  tagGroupCount: 0,
  status: "active",
  tags: [],
};

beforeEach(() => {
  vi.clearAllMocks();
  vi.mocked(authStatus).mockResolvedValue({
    authenticated: true,
    serverUrl: "https://example.test",
    userId: "user-1",
  });
  vi.mocked(listHostedDatasets).mockResolvedValue([dataset]);
  vi.mocked(getHostedDataset).mockResolvedValue(dataset);
  vi.mocked(listHostedMembers).mockResolvedValue([]);
  vi.mocked(listHostedVersions).mockResolvedValue([version]);
  vi.mocked(listHostingJobs).mockResolvedValue([]);
  vi.mocked(getHostingQuota).mockResolvedValue({ limitBytes: 10_000, usedBytes: 4096, reservedBytes: 0, maxAssets: 100, usedAssets: 12, canManage: true });
  vi.mocked(listRemoteProjects).mockResolvedValue([{ id: "remote-1", name: "已发布项目", role: "owner" }]);
  vi.mocked(remoteProjectOpen).mockResolvedValue({ projectId: "local-project-1", localProjectId: "local-project-1", remoteProjectId: "remote-target-1" });
  vi.mocked(createHostedDataset).mockResolvedValue({ ...dataset, id: "dataset-2", name: "新数据集" });
  vi.mocked(createHostedVersion).mockResolvedValue(version);
  vi.mocked(updateHostedDataset).mockResolvedValue(dataset);
  vi.mocked(addHostedMember).mockResolvedValue({ id: "member-1", userId: "user-2", role: "viewer" });
  vi.mocked(captureHostedProject).mockResolvedValue(dataset);
  vi.mocked(reuseHostedVersion).mockResolvedValue({ projectId: "remote-target-1", reusedAssets: 12 });
});

afterEach(() => {
  delete (window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__;
});

describe("DatasetHostingPanel", () => {
  it("shows hosted datasets and creates a dataset from the desktop entry", async () => {
    render(<DatasetHostingPanel />);
    expect((await screen.findAllByText("道路数据集")).length).toBeGreaterThan(0);
    expect(screen.getByText("1 个")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "新建数据集" }));
    fireEvent.change(screen.getAllByLabelText("名称")[0], { target: { value: "新数据集" } });
    fireEvent.click(screen.getByRole("button", { name: "创建" }));
    await waitFor(() => expect(createHostedDataset).toHaveBeenCalledWith({ name: "新数据集", description: "" }));
    expect(await screen.findByText("数据集已创建")).toBeInTheDocument();
  });

  it("supports members, capturing a published project, and version reuse", async () => {
    const onOpenProject = vi.fn();
    render(<DatasetHostingPanel onOpenProject={onOpenProject} />);
    await screen.findAllByText("道路数据集");

    fireEvent.change(screen.getByLabelText("成员邮箱或用户 ID"), { target: { value: "member@example.com" } });
    fireEvent.click(screen.getByRole("button", { name: "添加" }));
    await waitFor(() => expect(addHostedMember).toHaveBeenCalledWith("dataset-1", "member@example.com", "viewer"));

    fireEvent.change(screen.getByLabelText("已发布远端项目"), { target: { value: "remote-1" } });
    fireEvent.click(screen.getByRole("button", { name: "收录项目" }));
    await waitFor(() => expect(captureHostedProject).toHaveBeenCalledWith("dataset-1", {
      projectId: "remote-1",
      operationId: "operation-test",
    }));

    fireEvent.change(screen.getByLabelText("目标项目"), { target: { value: "" } });
    fireEvent.change(screen.getByLabelText("新项目名称"), { target: { value: "复用项目" } });
    fireEvent.click(screen.getByRole("button", { name: "复用并打开" }));
    await waitFor(() => expect(reuseHostedVersion).toHaveBeenCalledWith("dataset-1", {
      versionId: "version-1",
      targetProjectId: undefined,
      projectName: "复用项目",
      operationId: "operation-test",
    }));
    expect(onOpenProject).toHaveBeenCalledWith("local-project-1", "remote-target-1");
  });

  it("offers immutable version creation and a native-safe export entry", async () => {
    render(<DatasetHostingPanel />);
    await screen.findAllByText("道路数据集");
    fireEvent.change(screen.getByLabelText("新版本名称"), { target: { value: "审阅版" } });
    fireEvent.click(screen.getByRole("button", { name: "创建版本" }));
    await waitFor(() => expect(createHostedVersion).toHaveBeenCalledWith("dataset-1", {
      name: "审阅版",
      operationId: "operation-test",
      baseRevision: 2,
    }));
    expect(screen.getByLabelText("导出版本")).toHaveValue("version-1");
    expect(screen.getByRole("button", { name: "生成导出" })).toBeInTheDocument();
  });

  it("refreshes dataset revision before creating a second version", async () => {
    const revisionThree = { ...dataset, revision: 3, updatedAt: "2026-10-25T03:00:00Z" };
    const firstVersion = { ...version, name: "第一版本", createdAt: "2026-10-25T03:00:00Z" };
    const secondVersion = { ...version, id: "version-2", name: "第二版本", versionNumber: 2, revision: 3, createdAt: "2026-10-25T04:00:00Z" };
    let currentDetail = dataset;
    let currentVersions = [version];
    vi.mocked(getHostedDataset).mockImplementation(async () => currentDetail);
    vi.mocked(listHostedVersions).mockImplementation(async () => currentVersions);
    vi.mocked(createHostedVersion)
      .mockImplementationOnce(async () => {
        currentDetail = revisionThree;
        currentVersions = [firstVersion];
        return firstVersion;
      })
      .mockImplementationOnce(async () => {
        currentVersions = [secondVersion, firstVersion];
        return secondVersion;
      });

    render(<DatasetHostingPanel />);
    await screen.findAllByText("道路数据集");
    fireEvent.change(screen.getByLabelText("新版本名称"), { target: { value: "第一版本" } });
    fireEvent.click(screen.getByRole("button", { name: "创建版本" }));
    await waitFor(() => expect(createHostedVersion).toHaveBeenNthCalledWith(1, "dataset-1", {
      name: "第一版本",
      operationId: "operation-test",
      baseRevision: 2,
    }));
    await waitFor(() => expect(screen.getByText("#3")).toBeInTheDocument());

    fireEvent.change(screen.getByLabelText("新版本名称"), { target: { value: "第二版本" } });
    fireEvent.click(screen.getByRole("button", { name: "创建版本" }));
    await waitFor(() => expect(createHostedVersion).toHaveBeenNthCalledWith(2, "dataset-1", {
      name: "第二版本",
      operationId: "operation-test",
      baseRevision: 3,
    }));
  });

  it("opens a completed export through the native desktop save/reveal action", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: { metadata: { platform: "macos" } },
    });
    vi.mocked(startHostedExport).mockResolvedValue({ id: "job-export", kind: "export", status: "completed", progress: 100 });
    vi.mocked(downloadHostedExport).mockResolvedValue({
      path: "/app_data/exports/roads.zip",
      fileName: "roads.zip",
      byteSize: 42,
      sha256: "abc123",
    });

    render(<DatasetHostingPanel />);
    await screen.findAllByText("道路数据集");
    fireEvent.click(screen.getByRole("button", { name: "生成导出" }));

    const openButton = await screen.findByRole("button", { name: "查看/保存导出文件" });
    fireEvent.click(openButton);
    await waitFor(() => expect(openHostedExport).toHaveBeenCalledWith("job-export"));
  });

  it("shows the iOS Files destination instead of a WebView download link", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: { metadata: { platform: "ios" } },
    });
    vi.mocked(startHostedExport).mockResolvedValue({ id: "job-ios-export", kind: "export", status: "completed", progress: 100 });
    vi.mocked(downloadHostedExport).mockResolvedValue({
      path: "/Documents/Dataset Exports/roads.zip",
      fileName: "roads.zip",
      byteSize: 42,
      sha256: "abc123",
    });

    render(<DatasetHostingPanel />);
    await screen.findAllByText("道路数据集");
    fireEvent.click(screen.getByRole("button", { name: "生成导出" }));

    expect(await screen.findByText("已保存到 文件App→我的iPhone/iPad→Image Annotation→Dataset Exports")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "查看/保存导出文件" })).not.toBeInTheDocument();
  });

  it("hides the Android sandbox path behind the native save action", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: { metadata: { platform: "android" } },
    });
    vi.mocked(startHostedExport).mockResolvedValue({ id: "job-android-export", kind: "export", status: "completed", progress: 100 });
    vi.mocked(downloadHostedExport).mockResolvedValue({
      path: "/data/user/0/com.imageannotation.desktop/files/exports/roads.zip",
      fileName: "roads.zip",
      byteSize: 42,
      sha256: "abc123",
    });

    render(<DatasetHostingPanel />);
    await screen.findAllByText("道路数据集");
    fireEvent.click(screen.getByRole("button", { name: "生成导出" }));

    expect(await screen.findByText(/已保存到设备导出目录/)).toBeInTheDocument();
    expect(screen.queryByText(/\/data\/user\/0\/com\.imageannotation\.desktop/)).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "查看/保存导出文件" })).toBeInTheDocument();
  });

  it("sends the manageable remote UUID for an existing project and hydrates its local ID", async () => {
    const onOpenProject = vi.fn();
    render(
      <DatasetHostingPanel
        localProjects={[localProject]}
        remoteToLocalProjectIds={{ "remote-1": "local-project-1" }}
        onOpenProject={onOpenProject}
      />,
    );
    await screen.findAllByText("道路数据集");
    fireEvent.change(screen.getByLabelText("复用版本"), { target: { value: "version-1" } });
    fireEvent.change(screen.getByLabelText("目标项目"), { target: { value: "remote-1" } });
    fireEvent.click(screen.getByRole("button", { name: "复用并打开" }));

    await waitFor(() => expect(reuseHostedVersion).toHaveBeenCalledWith("dataset-1", expect.objectContaining({
      targetProjectId: "remote-1",
      projectName: undefined,
    })));
    expect(remoteProjectOpen).toHaveBeenCalledWith("remote-target-1");
    expect(onOpenProject).toHaveBeenCalledWith("local-project-1", "remote-target-1");
  });

  it("rejects a detail response that belongs to a previous dataset selection", async () => {
    const secondDataset = { ...dataset, id: "dataset-2", name: "第二数据集" };
    let resolveFirst!: (value: typeof dataset) => void;
    let resolveSecond!: (value: typeof secondDataset) => void;
    const firstDetail = new Promise<typeof dataset>((resolve) => { resolveFirst = resolve; });
    const secondDetail = new Promise<typeof secondDataset>((resolve) => { resolveSecond = resolve; });
    vi.mocked(listHostedDatasets).mockResolvedValue([dataset, secondDataset]);
    vi.mocked(getHostedDataset).mockImplementation((id) => id === "dataset-1" ? firstDetail : secondDetail);

    render(<DatasetHostingPanel />);
    expect(await screen.findByText("第二数据集")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /第二数据集/ }));
    resolveSecond(secondDataset);
    await waitFor(() => expect(screen.getByDisplayValue("第二数据集")).toBeInTheDocument());

    resolveFirst(dataset);
    await new Promise((resolve) => window.setTimeout(resolve, 0));
    expect(screen.getByDisplayValue("第二数据集")).toBeInTheDocument();
  });
});

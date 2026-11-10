import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { DataProjectHome } from "./DataProjectHome";
import type { DatasetProject } from "../../types/domain";
import { SESSION_CHANGE_EVENT } from "../../shared/sessionEvents";

const api = vi.hoisted(() => ({
  assignDatasetToDataProject: vi.fn(),
  listDataProjectDatasets: vi.fn(),
  getDataProjectDatasetGroups: vi.fn(),
  setDataProjectDatasetGroups: vi.fn(),
  commitDataProject: vi.fn(),
  createDataProject: vi.fn(),
  getDataProjectWorkingTree: vi.fn(),
  listDataProjectCommits: vi.fn(),
  listDataProjects: vi.fn(),
  listDataProjectBranches: vi.fn(),
  createDataProjectBranch: vi.fn(),
  switchDataProjectBranch: vi.fn(),
  deleteDataProjectBranch: vi.fn(),
  previewDataProjectMerge: vi.fn(),
  mergeDataProjectBranches: vi.fn(),
  resolveDataProjectMerge: vi.fn(),
  revertDataProject: vi.fn(),
  getDataProjectSyncStatus: vi.fn(),
  publishDataProject: vi.fn(),
  discoverDataProjectRemotes: vi.fn(),
  createRemoteDataProject: vi.fn(),
  connectRemoteDataProject: vi.fn(),
  cloneDataProject: vi.fn(),
  syncDataProject: vi.fn(),
  listDataProjectCloudMembers: vi.fn(),
  upsertDataProjectCloudMember: vi.fn(),
  removeDataProjectCloudMember: vi.fn(),
}));
const datasetApi = vi.hoisted(() => ({ listDatasetProjects: vi.fn() }));

vi.mock("../../api/dataProjects", () => api);
vi.mock("../../api/tauri", () => datasetApi);
vi.mock("../shared/useImageAssetUrls", () => ({ useImageAssetUrls: () => ({}) }));

const datasets: DatasetProject[] = [
  {
    id: "dataset-a",
    name: "道路训练集",
    description: "摄像头图片",
    annotationTypes: ["BBox"],
    imageCount: 2,
    annotatedPercent: 50,
    reviewCount: 1,
    issueCount: 0,
    classCount: 2,
    tagGroupCount: 1,
    status: "已导入",
    tags: ["split: train"],
  },
  {
    id: "dataset-b",
    name: "夜间测试集",
    description: "夜间图片",
    annotationTypes: ["BBox"],
    imageCount: 1,
    annotatedPercent: 0,
    reviewCount: 0,
    issueCount: 0,
    classCount: 1,
    tagGroupCount: 1,
    status: "已导入",
    tags: ["split: val"],
  },
];

const project = (datasetIds: string[] = ["dataset-a"]) => ({
  id: "project-a",
  name: "道路检测项目",
  description: "用于道路目标检测",
  datasetIds,
  createdAt: "2026-10-25T09:00:00Z",
  updatedAt: "2026-10-25T09:00:00Z",
  accountScope: null,
});

const cleanTree = (dataProjectId = "project-a") => ({
  dataProjectId,
  counts: { raw: 1, inProgress: 0, pendingReview: 1, approved: 0, rejected: 0, total: 2 },
  changes: [],
  headCommitId: null,
});

const changedTree = (dataProjectId = "project-a") => ({
  dataProjectId,
  counts: { raw: 0, inProgress: 1, pendingReview: 0, approved: 1, rejected: 0, total: 2 },
  changes: [{ kind: "annotation-updated", datasetId: "dataset-a", imageId: "image-a", fileName: "road.png", detail: "image, annotation, or review metadata changed" }],
  headCommitId: null,
});

function renderHome(overrides: Partial<React.ComponentProps<typeof DataProjectHome>> = {}) {
  return render(
    <DataProjectHome
      projects={datasets}
      projectImages={{}}
      runtimeState="ready"
      runtimeMessage={null}
      testDatasetDownloaded={false}
      testDatasetKey="coco128"
      onDownload={vi.fn()}
      onInfo={vi.fn()}
      onAnnotate={vi.fn()}
      onOpenWindow={vi.fn()}
      onOpen={vi.fn()}
      {...overrides}
    />,
  );
}

beforeEach(() => {
  vi.clearAllMocks();
  api.listDataProjects.mockResolvedValue([project()]);
  api.getDataProjectWorkingTree.mockResolvedValue(cleanTree());
  api.listDataProjectCommits.mockResolvedValue([]);
  api.assignDatasetToDataProject.mockResolvedValue(undefined);
  datasetApi.listDatasetProjects.mockResolvedValue(datasets);
  api.listDataProjectDatasets.mockResolvedValue([datasets[0]]);
  api.getDataProjectDatasetGroups.mockResolvedValue({
    dataProjectId: "project-a",
    branch: "main",
    revision: 1,
    projectMetadata: { schemaVersion: 1, datasetGroups: [] },
    canEdit: true,
  });
  api.setDataProjectDatasetGroups.mockResolvedValue({
    dataProjectId: "project-a",
    branch: "main",
    revision: 2,
    projectMetadata: { schemaVersion: 1, datasetGroups: [] },
    canEdit: true,
  });
  api.createDataProject.mockResolvedValue({ ...project([]), id: "project-new", name: "新的检测项目" });
  api.commitDataProject.mockResolvedValue({ id: "commit-1", dataProjectId: "project-a", parentId: null, secondParentId: null, branch: "main", message: "完成标注", createdAt: "2026-10-25T10:00:00Z", datasetIds: ["dataset-a"], datasetMetadata: {}, entries: [] });
  api.listDataProjectBranches.mockResolvedValue([
    { dataProjectId: "project-a", name: "main", headCommitId: null, current: true, createdAt: "2026-10-25T09:00:00Z", updatedAt: "2026-10-25T09:00:00Z" },
    { dataProjectId: "project-a", name: "review", headCommitId: "commit-r", current: false, createdAt: "2026-10-25T09:00:00Z", updatedAt: "2026-10-25T09:00:00Z" },
  ]);
  api.createDataProjectBranch.mockResolvedValue({ dataProjectId: "project-a", name: "feature", headCommitId: null, current: false, createdAt: "2026-10-25T09:00:00Z", updatedAt: "2026-10-25T09:00:00Z" });
  api.switchDataProjectBranch.mockResolvedValue({ dataProjectId: "project-a", branch: "review", commitId: "commit-r", path: "/tmp/project-a", dirty: false });
  api.deleteDataProjectBranch.mockResolvedValue(undefined);
  api.previewDataProjectMerge.mockResolvedValue({ sessionId: null, commit: null, conflicts: [{ key: "entry:\u0000image-a\u0000annotation", kind: "entry", base: { hash: "0123456789abcdef0123456789abcdef", status: "raw" }, ours: { hash: "abcdef0123456789abcdef0123456789", status: "inProgress" }, theirs: { hash: "fedcba9876543210fedcba9876543210", status: "approved" } }] });
  api.mergeDataProjectBranches.mockResolvedValue({ sessionId: null, commit: { id: "merge-1", dataProjectId: "project-a", parentId: "commit-a", secondParentId: "commit-r", branch: "main", message: "合并 review", createdAt: "2026-10-25T11:00:00Z", datasetIds: ["dataset-a"], datasetMetadata: {}, entries: [] }, conflicts: [] });
  api.resolveDataProjectMerge.mockResolvedValue({ sessionId: null, commit: null, conflicts: [] });
  api.revertDataProject.mockResolvedValue({ id: "revert-1", dataProjectId: "project-a", parentId: "commit-1", secondParentId: null, branch: "main", message: "回退", createdAt: "2026-10-25T11:00:00Z", datasetIds: ["dataset-a"], datasetMetadata: {}, entries: [] });
  api.getDataProjectSyncStatus.mockResolvedValue({ localProjectId: "project-a", remoteProjectId: null, remoteRevision: null, localHead: null, remoteHead: null, dirty: false, state: "unbound", trackingRef: null });
  api.publishDataProject.mockResolvedValue({ localProjectId: "project-a", remoteProjectId: "remote-a", remoteRevision: 1, localHead: null, remoteHead: null, outcome: "pushed", trackingRef: null });
  api.discoverDataProjectRemotes.mockResolvedValue([{ id: "remote-a", organizationId: null, ownerId: "owner-a", name: "云端道路项目", description: "", status: "ready", revision: 1, role: "owner", datasetCount: 1, refCount: 1, commitCount: 1, updatedAt: "2026-10-25T11:00:00Z" }]);
  api.createRemoteDataProject.mockResolvedValue({ id: "remote-a", organizationId: null, ownerId: "owner-a", name: "云端道路项目", description: "", status: "ready", revision: 1, role: "owner", datasetCount: 1, refCount: 1, commitCount: 1, updatedAt: "2026-10-25T11:00:00Z" });
  api.connectRemoteDataProject.mockResolvedValue({ project: { id: "remote-a", organizationId: null, ownerId: "owner-a", name: "云端道路项目", description: "", status: "ready", revision: 1, role: "owner", datasetCount: 1, refCount: 1, commitCount: 1, updatedAt: "2026-10-25T11:00:00Z" }, members: [], datasets: [], commits: [], refs: [], objects: [], remoteRevision: 1 });
  api.cloneDataProject.mockResolvedValue({ localProjectId: "project-cloned", remoteProjectId: "remote-a", remoteRevision: 1, localHead: null, remoteHead: null, outcome: "cloned", trackingRef: null });
  api.syncDataProject.mockResolvedValue({ localProjectId: "project-a", remoteProjectId: "remote-a", remoteRevision: 2, localHead: null, remoteHead: null, outcome: "pushed", trackingRef: null });
  api.listDataProjectCloudMembers.mockResolvedValue([]);
  api.upsertDataProjectCloudMember.mockResolvedValue({ userId: "user-b", role: "viewer" });
  api.removeDataProjectCloudMember.mockResolvedValue({ userId: "user-b", deleted: true });
});

describe("DataProjectHome", () => {
  it("clears account-scoped project metadata immediately on session change", async () => {
    renderHome();
    await screen.findByRole("heading", { name: "道路检测项目" });
    window.dispatchEvent(new Event(SESSION_CHANGE_EVENT));
    await waitFor(() => expect(screen.queryByRole("heading", { name: "道路检测项目" })).not.toBeInTheDocument());
    expect(screen.queryByText("尚未提交")).not.toBeInTheDocument();
    expect(screen.getByText("选择一个数据项目")).toBeInTheDocument();
  });

  it("clears old account history and cloud members on session change", async () => {
    const user = userEvent.setup();
    api.listDataProjectCommits.mockResolvedValue([{ id: "old-commit", dataProjectId: "project-a", parentId: null, secondParentId: null, branch: "main", message: "旧账号提交", createdAt: "2026-10-25T10:00:00Z", datasetIds: ["dataset-a"], datasetMetadata: {}, entries: [] }]);
    api.getDataProjectSyncStatus.mockResolvedValue({ localProjectId: "project-a", remoteProjectId: "remote-a", remoteRevision: 1, localHead: "old-commit", remoteHead: "old-commit", dirty: false, state: "upToDate", trackingRef: "remote-a" });
    api.listDataProjectCloudMembers.mockResolvedValue([{ userId: "old-user", role: "owner", joinedAt: null, email: "old@example.com", displayName: "旧账号成员" }]);
    renderHome();
    await user.click(await screen.findByText("提交历史"));
    await screen.findByText("旧账号提交");
    await user.click(screen.getByRole("button", { name: "查看云端" }));
    await screen.findByText("旧账号成员");
    window.dispatchEvent(new Event(SESSION_CHANGE_EVENT));
    await waitFor(() => expect(screen.queryByText("旧账号提交")).not.toBeInTheDocument());
    expect(screen.queryByText("旧账号成员")).not.toBeInTheDocument();
    expect(screen.getByText("选择一个数据项目")).toBeInTheDocument();
  });

  it("clears selection and details when project listing fails", async () => {
    api.listDataProjects.mockResolvedValueOnce([project()]).mockRejectedValueOnce(new Error("账户不可用"));
    const view = renderHome();
    await screen.findByRole("heading", { name: "道路检测项目" });
    view.rerender(
      <DataProjectHome
        projects={[{ ...datasets[0], imageCount: 3 }, datasets[1]]}
        projectImages={{}}
        runtimeState="ready"
        runtimeMessage={null}
        testDatasetDownloaded={false}
        testDatasetKey="coco128"
        onDownload={vi.fn()}
        onInfo={vi.fn()}
        onAnnotate={vi.fn()}
        onOpenWindow={vi.fn()}
        onOpen={vi.fn()}
      />,
    );
    await waitFor(() => expect(screen.getByRole("alert")).toHaveTextContent("账户不可用"));
    expect(screen.queryByRole("heading", { name: "道路检测项目" })).not.toBeInTheDocument();
    expect(screen.getByText("选择一个数据项目")).toBeInTheDocument();
  });

  it("reports a malformed project-list response without passing it to a React updater", async () => {
    api.listDataProjects.mockResolvedValueOnce({ invalid: true } as never);
    renderHome();
    await waitFor(() => expect(screen.getByRole("alert")).toHaveTextContent("数据项目响应格式无效"));
    expect(screen.getByText("选择一个数据项目")).toBeInTheDocument();
  });

  it("creates a project and refreshes the project list when dataset props change", async () => {
    const user = userEvent.setup();
    const view = renderHome();
    await screen.findByRole("heading", { name: "道路检测项目" });

    await user.click(screen.getByRole("button", { name: "新建数据项目" }));
    await user.type(screen.getByLabelText("项目名称"), "新的检测项目");
    await user.click(screen.getByRole("button", { name: "创建项目" }));
    expect(api.createDataProject).toHaveBeenCalledWith("新的检测项目", "");

    const callsBeforeRefresh = api.listDataProjects.mock.calls.length;
    view.rerender(
      <DataProjectHome
        projects={[{ ...datasets[0], imageCount: 3 }, datasets[1]]}
        projectImages={{}}
        runtimeState="ready"
        runtimeMessage={null}
        testDatasetDownloaded={false}
        testDatasetKey="coco128"
        onDownload={vi.fn()}
        onInfo={vi.fn()}
        onAnnotate={vi.fn()}
        onOpenWindow={vi.fn()}
        onOpen={vi.fn()}
      />,
    );
    await waitFor(() => expect(api.listDataProjects.mock.calls.length).toBeGreaterThan(callsBeforeRefresh));
  });

  it("starts on a project with datasets when no project is already selected", async () => {
    api.listDataProjects.mockResolvedValue([
      { ...project([]), id: "empty-project", name: "空项目" },
      project(),
    ]);
    renderHome();
    expect(await screen.findByRole("heading", { name: "道路检测项目" })).toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: "空项目" })).not.toBeInTheDocument();
  });

  it("uses the controlled overview route without loading project working trees", async () => {
    const onNavigateProject = vi.fn();
    renderHome({ onNavigateProject });

    expect(await screen.findByRole("heading", { name: /^项目/ })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "进入项目" })).toBeInTheDocument();
    expect(api.listDataProjects).toHaveBeenCalledTimes(1);
    expect(api.getDataProjectWorkingTree).not.toHaveBeenCalled();
    expect(api.listDataProjectCommits).not.toHaveBeenCalled();

    await userEvent.setup().click(screen.getByRole("button", { name: "进入项目" }));
    expect(onNavigateProject).toHaveBeenCalledWith("project-a", "data");
  });

  it("loads the pending-dataset drawer lazily instead of treating an empty prop catalog as complete", async () => {
    const onNavigateProject = vi.fn();
    renderHome({ onNavigateProject, projects: [] });

    expect(await screen.findByRole("heading", { name: /^项目/ })).toBeInTheDocument();
    expect(datasetApi.listDatasetProjects).not.toHaveBeenCalled();
    await userEvent.setup().click(screen.getByRole("button", { name: "待归属数据" }));
    expect(await screen.findByRole("dialog", { name: "待归属数据集" })).toBeInTheDocument();
    expect(datasetApi.listDatasetProjects).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("article", { name: "夜间测试集 数据集卡片" })).toBeInTheDocument();
  });

  it("loads only the selected project's datasets and groups in the controlled workspace", async () => {
    const onNavigateProject = vi.fn();
    const onDatasetsChanged = vi.fn();
    renderHome({
      dataProjectId: "project-a",
      dataProject: project(),
      section: "data",
      onNavigateProject,
      onNavigateHome: vi.fn(),
      onDatasetsChanged,
    });

    expect(await screen.findByRole("heading", { name: "数据集" })).toBeInTheDocument();
    expect(api.listDataProjectDatasets).toHaveBeenCalledWith("project-a");
    expect(api.getDataProjectDatasetGroups).toHaveBeenCalledWith("project-a");
    expect(api.getDataProjectWorkingTree).toHaveBeenCalledWith("project-a");
    expect(api.listDataProjects).not.toHaveBeenCalled();

    await userEvent.setup().click(screen.getByRole("button", { name: "版本" }));
    expect(onNavigateProject).toHaveBeenCalledWith("project-a", "versions");
  });

  it("reloads the latest group revision after a concurrent CAS conflict", async () => {
    const user = userEvent.setup();
    const initialGroups = {
      dataProjectId: "project-a",
      branch: "main",
      revision: 1,
      projectMetadata: { schemaVersion: 1 as const, datasetGroups: [{ id: "group-a", name: "原分组", datasetKeys: ["dataset-a"], position: 0 }] },
      canEdit: true,
    };
    const latestGroups = {
      ...initialGroups,
      revision: 2,
      projectMetadata: { schemaVersion: 1 as const, datasetGroups: [{ id: "group-a", name: "服务端分组", datasetKeys: ["dataset-a"], position: 0 }] },
    };
    api.getDataProjectDatasetGroups.mockResolvedValueOnce(initialGroups).mockResolvedValueOnce(latestGroups);
    api.setDataProjectDatasetGroups.mockRejectedValueOnce(new Error("409 WORKTREE_REVISION_MISMATCH: working tree changed"));

    renderHome({
      dataProjectId: "project-a",
      dataProject: project(),
      section: "data",
      onNavigateProject: vi.fn(),
      onNavigateHome: vi.fn(),
    });

    await screen.findByRole("heading", { name: "数据集" });
    await user.click(screen.getByRole("button", { name: "分组菜单 原分组" }));
    await user.click(screen.getByRole("button", { name: "重命名" }));
    const input = screen.getByRole("textbox", { name: "重命名分组 原分组" });
    await user.clear(input);
    await user.type(input, "本地分组");
    fireEvent.keyDown(input, { key: "Enter", code: "Enter" });

    await waitFor(() => expect(api.getDataProjectDatasetGroups).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(screen.getByRole("button", { name: "分组菜单 服务端分组" })).toBeInTheDocument());
    await waitFor(() => expect(screen.getAllByRole("alert").some((node) => node.textContent?.includes("已重新读取最新状态"))).toBe(true));
    expect(api.setDataProjectDatasetGroups).toHaveBeenCalledWith("project-a", "main", 1, expect.any(Array));
  });

  it("keeps viewer versions read-only while allowing branch browsing and export", async () => {
    const onExportDataProject = vi.fn();
    api.listDataProjectCommits.mockResolvedValueOnce([{ id: "commit-viewer", dataProjectId: "project-a", parentId: null, secondParentId: null, branch: "main", message: "只读版本", createdAt: "2026-10-25T10:00:00Z", datasetIds: ["dataset-a"], datasetMetadata: {}, entries: [] }]);
    renderHome({
      dataProjectId: "project-a",
      dataProject: { ...project(), role: "viewer" },
      section: "versions",
      onNavigateProject: vi.fn(),
      onNavigateHome: vi.fn(),
      onExportDataProject,
    });

    await screen.findByRole("heading", { name: "版本" });
    expect(screen.getByRole("combobox", { name: "当前分支" })).toBeEnabled();
    expect(screen.queryByRole("textbox", { name: "新分支名称" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "创建" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "提交版本" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "预览合并" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "导出清单" })).toBeInTheDocument();
    expect(await screen.findByRole("button", { name: "回退" })).toBeDisabled();
  });

  it("hides viewer dataset mutation shortcuts in settings but keeps export", async () => {
    renderHome({
      dataProjectId: "project-a",
      dataProject: { ...project(), role: "viewer" },
      section: "settings",
      onNavigateProject: vi.fn(),
      onNavigateHome: vi.fn(),
      onCreateDataset: vi.fn(),
      onUploadImages: vi.fn(),
      onExportDataProject: vi.fn(),
    });

    await screen.findByRole("heading", { name: "设置" });
    expect(screen.queryByRole("button", { name: "新建数据集" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "上传图像" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "导出图像与标签清单" })).toBeInTheDocument();
  });

  it("assigns an existing unowned dataset from the scoped project drawer", async () => {
    const user = userEvent.setup();
    renderHome({
      dataProjectId: "project-a",
      dataProject: project(),
      section: "data",
      onNavigateProject: vi.fn(),
      onNavigateHome: vi.fn(),
    });

    await screen.findByRole("heading", { name: "数据集" });
    await user.click(screen.getByRole("button", { name: "加入已有" }));
    await screen.findByRole("dialog", { name: "待归属数据集" });
    await user.click(screen.getByRole("button", { name: "加入当前项目" }));
    await waitFor(() => expect(api.assignDatasetToDataProject).toHaveBeenCalledWith("project-a", "dataset-b"));
  });

  it("invalidates an in-flight initial listing before creating and reloading a project", async () => {
    const user = userEvent.setup();
    let resolveInitial!: (value: ReturnType<typeof project>[]) => void;
    const created = { ...project([]), id: "project-created", name: "新检测项目" };
    api.listDataProjects
      .mockImplementationOnce(() => new Promise((resolve) => { resolveInitial = resolve; }))
      .mockResolvedValueOnce([created]);
    api.createDataProject.mockResolvedValue(created);
    renderHome();
    await user.click(screen.getByRole("button", { name: "新建数据项目" }));
    await user.type(screen.getByLabelText("项目名称"), "新检测项目");
    await user.click(screen.getByRole("button", { name: "创建项目" }));
    await waitFor(() => expect(screen.getByRole("heading", { name: "新检测项目" })).toBeInTheDocument());
    expect(screen.queryByRole("heading", { name: "道路检测项目" })).not.toBeInTheDocument();
    resolveInitial([project()]);
  });

  it("keeps unassigned datasets openable when there are no data projects", async () => {
    api.listDataProjects.mockResolvedValue([]);
    renderHome();
    const card = await screen.findByRole("article", { name: "道路训练集 数据集卡片" });
    expect(card).toBeInTheDocument();
    expect(within(card).getByRole("button", { name: "打开" })).toBeInTheDocument();
    expect(within(card).getByRole("button", { name: "开始标注" })).toBeInTheDocument();
    expect(screen.getAllByText("先创建或选择项目后加入").length).toBe(2);
    expect(screen.queryByRole("button", { name: "加入当前项目" })).not.toBeInTheDocument();
  });

  it("joins and removes an unassigned dataset", async () => {
    const user = userEvent.setup();
    api.listDataProjects
      .mockResolvedValueOnce([project()])
      .mockResolvedValueOnce([project(["dataset-a", "dataset-b"])])
      .mockResolvedValueOnce([project(["dataset-a"])])
      .mockResolvedValue([project()]);
    renderHome();
    await screen.findByText("夜间测试集");

    await user.click(screen.getByRole("button", { name: "加入当前项目" }));
    await waitFor(() => expect(api.assignDatasetToDataProject).toHaveBeenCalledWith("project-a", "dataset-b"));
    await waitFor(() => expect(screen.getAllByRole("button", { name: /移出项目/ }).length).toBe(2));

    const nightDatasetCard = screen.getByRole("article", { name: "夜间测试集 数据集卡片" }).parentElement;
    expect(nightDatasetCard).not.toBeNull();
    await user.click(within(nightDatasetCard as HTMLElement).getByRole("button", { name: /移出项目/ }));
    await waitFor(() => expect(api.assignDatasetToDataProject).toHaveBeenCalledWith(null, "dataset-b"));
  });

  it("keeps an empty commit disabled", async () => {
    const user = userEvent.setup();
    renderHome();
    await screen.findByRole("heading", { name: "道路检测项目" });
    await user.click(screen.getByText("工作区变更"));
    await waitFor(() => expect(screen.getByRole("button", { name: "提交版本" })).toBeDisabled());
    expect(screen.getByPlaceholderText("工作区干净时不能创建空提交")).toBeDisabled();
    expect(api.commitDataProject).not.toHaveBeenCalled();
  });

  it("commits changes and refreshes the working tree and history", async () => {
    const user = userEvent.setup();
    api.getDataProjectWorkingTree
      .mockResolvedValueOnce(changedTree())
      .mockResolvedValueOnce(cleanTree());
    api.listDataProjectCommits
      .mockResolvedValueOnce([])
      .mockResolvedValueOnce([{ id: "commit-1", dataProjectId: "project-a", parentId: null, secondParentId: null, branch: "main", message: "完成标注", createdAt: "2026-10-25T10:00:00Z", datasetIds: ["dataset-a"], datasetMetadata: {}, entries: [] }]);
    renderHome();
    await screen.findByRole("heading", { name: "道路检测项目" });
    await user.click(screen.getByText("工作区变更"));
    await screen.findByDisplayValue("").catch(() => undefined);
    const input = await screen.findByLabelText("提交说明");
    await user.type(input, "完成标注");
    await user.click(screen.getByRole("button", { name: "提交版本" }));
    await waitFor(() => expect(api.commitDataProject).toHaveBeenCalledWith("project-a", "main", "完成标注"));
    await screen.findByText("完成标注");
    expect(screen.getByText("工作区干净，没有可提交的图像或标签变化。")).toBeInTheDocument();
  });

  it("does not let an older project response overwrite a newer selection", async () => {
    const user = userEvent.setup();
    let resolveProjectA!: (value: ReturnType<typeof changedTree>) => void;
    let resolveProjectB!: (value: ReturnType<typeof cleanTree>) => void;
    api.listDataProjects.mockResolvedValue([
      project(["dataset-a"]),
      { ...project(["dataset-b"]), id: "project-b", name: "夜间检测项目" },
    ]);
    api.getDataProjectWorkingTree.mockImplementation((id: string) => new Promise((resolve) => {
      if (id === "project-a") resolveProjectA = resolve;
      else resolveProjectB = resolve;
    }));
    api.listDataProjectCommits.mockResolvedValue([]);
    renderHome();
    await screen.findByRole("button", { name: /夜间检测项目/ });
    await user.click(screen.getByRole("button", { name: /夜间检测项目/ }));
    await waitFor(() => expect(resolveProjectA).toBeDefined());
    resolveProjectB(cleanTree("project-b"));
    await user.click(screen.getByText("工作区变更"));
    await waitFor(() => expect(screen.getByText("工作区干净，没有可提交的图像或标签变化。")).toBeInTheDocument());
    resolveProjectA(changedTree("project-a"));
    await waitFor(() => expect(screen.getByRole("heading", { name: "夜间检测项目" })).toBeInTheDocument());
    expect(screen.getByText("工作区干净，没有可提交的图像或标签变化。")).toBeInTheDocument();
  });

  it("loads branches, creates a branch, and switches only after a clean checkout", async () => {
    const user = userEvent.setup();
    renderHome();
    await screen.findByRole("heading", { name: "道路检测项目" });
    await user.click(screen.getByRole("button", { name: "读取分支" }));
    expect(await screen.findByRole("combobox", { name: "当前分支" })).toBeInTheDocument();
    await user.type(screen.getByRole("textbox", { name: "新分支名称" }), "feature");
    await user.click(screen.getByRole("button", { name: "创建分支" }));
    expect(api.createDataProjectBranch).toHaveBeenCalledWith("project-a", "feature");
    await user.selectOptions(screen.getByRole("combobox", { name: "当前分支" }), "review");
    await waitFor(() => expect(api.switchDataProjectBranch).toHaveBeenCalledWith("project-a", "review"));
  });

  it("hides the protected main branch delete action when another branch is active", async () => {
    const user = userEvent.setup();
    api.listDataProjectBranches.mockResolvedValueOnce([
      { dataProjectId: "project-a", name: "feature", headCommitId: "commit-f", current: true, createdAt: "2026-10-25T09:00:00Z", updatedAt: "2026-10-25T09:00:00Z" },
      { dataProjectId: "project-a", name: "main", headCommitId: "commit-m", current: false, createdAt: "2026-10-25T09:00:00Z", updatedAt: "2026-10-25T09:00:00Z" },
    ]);
    renderHome();
    await screen.findByRole("heading", { name: "道路检测项目" });
    await user.click(screen.getByRole("button", { name: "读取分支" }));
    expect(screen.getByLabelText("分支列表")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "删除" })).not.toBeInTheDocument();
  });

  it("shows three-way conflicts and submits the selected source resolution", async () => {
    const user = userEvent.setup();
    renderHome();
    await screen.findByRole("heading", { name: "道路检测项目" });
    await user.click(screen.getByRole("button", { name: "读取分支" }));
    await user.selectOptions(screen.getByRole("combobox", { name: "合并源分支" }), "review");
    await user.click(screen.getByRole("button", { name: "预览合并" }));
    expect(await screen.findByText("基线版本")).toBeInTheDocument();
    expect(screen.getByText(/条目：.*标签/)).toBeInTheDocument();
    expect(screen.getAllByText(/版本 0123456789/).length).toBeGreaterThan(0);
    expect(screen.getAllByText("当前分支").length).toBeGreaterThan(1);
    expect(screen.getByText("源分支", { selector: "b" })).toBeInTheDocument();
    await user.click(screen.getByRole("radio", { name: /源分支/ }));
    await user.type(screen.getByRole("textbox", { name: "合并提交说明" }), "合并 review");
    await user.click(screen.getByRole("button", { name: "确认合并" }));
    await waitFor(() => expect(api.mergeDataProjectBranches).toHaveBeenCalledWith("project-a", "review", "main", "合并 review"));
  });

  it("prefers readable conflict labels and descriptions from the merge planner", async () => {
    const user = userEvent.setup();
    api.previewDataProjectMerge.mockResolvedValueOnce({
      sessionId: null,
      commit: null,
      conflicts: [{
        key: "entry:\u0000image-a\u0000annotation",
        kind: "annotation",
        displayName: "道路训练集 / road.png",
        baseDescription: "基线标签版本",
        oursDescription: "当前分支标签版本",
        theirsDescription: "源分支标签版本",
        base: { hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef", status: "raw" },
        ours: { hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789", status: "inProgress" },
        theirs: { hash: "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210", status: "approved" },
      }],
    });
    renderHome();
    await screen.findByRole("heading", { name: "道路检测项目" });
    await user.click(screen.getByRole("button", { name: "读取分支" }));
    await user.selectOptions(screen.getByRole("combobox", { name: "合并源分支" }), "review");
    await user.click(screen.getByRole("button", { name: "预览合并" }));
    expect(await screen.findByText("道路训练集 / road.png")).toBeInTheDocument();
    expect(screen.getByText(/基线标签版本/)).toBeInTheDocument();
    expect(screen.getByText(/当前分支标签版本/)).toBeInTheDocument();
    expect(screen.getByText(/源分支标签版本/)).toBeInTheDocument();
    expect(screen.getAllByText(/版本 0123456789/).length).toBe(1);
    expect(screen.queryByText(/entry:/)).not.toBeInTheDocument();
  });

  it("disables branch checkout and history recovery while the working tree is dirty", async () => {
    const user = userEvent.setup();
    api.getDataProjectWorkingTree.mockResolvedValueOnce(changedTree());
    api.listDataProjectCommits.mockResolvedValueOnce([{ id: "commit-1", dataProjectId: "project-a", parentId: null, secondParentId: null, branch: "main", message: "完成标注", createdAt: "2026-10-25T10:00:00Z", datasetIds: ["dataset-a"], datasetMetadata: {}, entries: [] }]);
    renderHome();
    await screen.findByRole("heading", { name: "道路检测项目" });
    await user.click(screen.getByRole("button", { name: "读取分支" }));
    expect(screen.getByRole("combobox", { name: "当前分支" })).toBeDisabled();
    await user.click(screen.getByText("提交历史"));
    await user.click(screen.getByText("查看版本内容（0 张图像）"));
    expect(screen.getByRole("button", { name: /回退到此版本/ })).toBeDisabled();
  });

  it("surfaces cloud permission or connectivity errors without claiming sync success", async () => {
    const user = userEvent.setup();
    api.getDataProjectSyncStatus.mockRejectedValueOnce(new Error("403 forbidden"));
    renderHome();
    await screen.findByRole("heading", { name: "道路检测项目" });
    await user.click(screen.getByRole("button", { name: "查看云端" }));
    await waitFor(() => expect(screen.getByRole("alert")).toHaveTextContent("权限不足：403 forbidden"));
    expect(screen.queryByText("同步请求已完成，状态已刷新。")).not.toBeInTheDocument();
  });

  it("recovers the real cloud status after a failed sync", async () => {
    const user = userEvent.setup();
    const connected = { localProjectId: "project-a", remoteProjectId: "remote-a", remoteRevision: 1, localHead: null, remoteHead: null, dirty: false, state: "upToDate", trackingRef: "remote-a" } as const;
    api.getDataProjectSyncStatus.mockResolvedValue(connected);
    api.syncDataProject.mockRejectedValueOnce(new Error("网络中断"));
    renderHome();
    await screen.findByRole("heading", { name: "道路检测项目" });
    await user.click(screen.getByRole("button", { name: "查看云端" }));
    await user.click(await screen.findByRole("button", { name: "手动同步" }));
    await waitFor(() => expect(screen.getByRole("alert")).toHaveTextContent("网络中断"));
    expect(screen.getAllByText("已同步").length).toBeGreaterThan(0);
    expect(screen.queryByText("同步返回：pushed")).not.toBeInTheDocument();
  });

  it("discovers and clones a remote project, then selects the refreshed local project", async () => {
    const user = userEvent.setup();
    api.listDataProjects.mockResolvedValueOnce([project()]).mockResolvedValueOnce([
      project(),
      { ...project([]), id: "project-cloned", name: "云端道路项目" },
    ]);
    renderHome();
    await screen.findByRole("heading", { name: "道路检测项目" });
    await user.click(screen.getByRole("button", { name: "查看云端" }));
    await user.click(screen.getByRole("button", { name: "发现远端项目" }));
    await screen.findByText("云端道路项目");
    await user.click(screen.getByRole("button", { name: "导入本地" }));
    await waitFor(() => expect(api.cloneDataProject).toHaveBeenCalledWith("remote-a"));
    await waitFor(() => expect(screen.getByRole("heading", { name: "云端道路项目" })).toBeInTheDocument());
  });

  it("shows owner member management and refreshes members after saving", async () => {
    const user = userEvent.setup();
    api.getDataProjectSyncStatus.mockResolvedValue({ localProjectId: "project-a", remoteProjectId: "remote-a", remoteRevision: 1, localHead: null, remoteHead: null, dirty: false, state: "upToDate", trackingRef: "remote-a" });
    api.listDataProjectCloudMembers
      .mockResolvedValueOnce([{ userId: "user-a", role: "owner", joinedAt: null, email: "owner@example.com", displayName: "项目所有者" }])
      .mockResolvedValue([{ userId: "user-a", role: "owner", joinedAt: null, email: "owner@example.com", displayName: "项目所有者" }, { userId: "user-b", role: "viewer", joinedAt: null, email: "member@example.com", displayName: null }]);
    renderHome();
    await screen.findByRole("heading", { name: "道路检测项目" });
    await user.click(screen.getByRole("button", { name: "查看云端" }));
    expect(await screen.findByText("项目所有者")).toBeInTheDocument();
    await user.type(screen.getByRole("textbox", { name: "成员用户 ID" }), "user-b");
    await user.click(screen.getByRole("button", { name: "保存成员" }));
    await waitFor(() => expect(api.upsertDataProjectCloudMember).toHaveBeenCalledWith("project-a", "user-b", "viewer"));
    await waitFor(() => expect(screen.getByText("member@example.com")).toBeInTheDocument());
  });

  it("keeps connected viewer projects read-only", async () => {
    const user = userEvent.setup();
    api.getDataProjectSyncStatus.mockResolvedValue({ localProjectId: "project-a", remoteProjectId: "remote-a", remoteRevision: 1, localHead: null, remoteHead: null, dirty: false, state: "upToDate", trackingRef: "remote-a" });
    api.discoverDataProjectRemotes.mockResolvedValue([{ id: "remote-a", organizationId: null, ownerId: "owner-a", name: "云端道路项目", description: "", status: "ready", revision: 1, role: "viewer", datasetCount: 1, refCount: 1, commitCount: 1, updatedAt: "2026-10-25T11:00:00Z" }]);
    api.listDataProjectCloudMembers.mockResolvedValue([{ userId: "user-a", role: "owner", joinedAt: null, email: "owner@example.com", displayName: "项目所有者" }]);
    renderHome();
    await screen.findByRole("heading", { name: "道路检测项目" });
    await user.click(screen.getByRole("button", { name: "查看云端" }));
    expect(await screen.findByText(/当前角色为 viewer/)).toBeInTheDocument();
    const pullButton = screen.getByRole("button", { name: "读取云端更新" });
    expect(pullButton).toBeEnabled();
    await user.click(pullButton);
    await waitFor(() => expect(api.syncDataProject).toHaveBeenCalledWith("project-a"));
    expect(screen.queryByRole("button", { name: "保存成员" })).not.toBeInTheDocument();
    expect(api.upsertDataProjectCloudMember).not.toHaveBeenCalled();
  });

  it("keeps a viewer local-ahead state after a rejected pull", async () => {
    const user = userEvent.setup();
    const ahead = { localProjectId: "project-a", remoteProjectId: "remote-a", remoteRevision: 3, localHead: "local-3", remoteHead: "remote-2", dirty: false, state: "readOnlyAhead", trackingRef: "origin::main" } as const;
    api.getDataProjectSyncStatus.mockResolvedValue(ahead);
    api.discoverDataProjectRemotes.mockResolvedValue([{ id: "remote-a", organizationId: null, ownerId: "owner-a", name: "云端道路项目", description: "", status: "ready", revision: 3, role: "viewer", datasetCount: 1, refCount: 1, commitCount: 3, updatedAt: "2026-10-25T11:00:00Z" }]);
    api.syncDataProject.mockRejectedValueOnce(new Error("云端数据项目当前角色为只读，本地存在领先提交，不能推送"));
    renderHome();
    await screen.findByRole("heading", { name: "道路检测项目" });
    await user.click(screen.getByRole("button", { name: "查看云端" }));
    const pullButton = await screen.findByRole("button", { name: "读取云端更新" });
    await user.click(pullButton);
    await waitFor(() => expect(screen.getByRole("alert")).toHaveTextContent("不能推送"));
    expect(screen.getAllByText("本地分支有新提交，当前云端只读").length).toBeGreaterThan(0);
    expect(screen.queryByText(/同步返回：/)).not.toBeInTheDocument();
  });

  it("disables viewer pull while the local working tree is dirty", async () => {
    const user = userEvent.setup();
    api.getDataProjectWorkingTree.mockResolvedValue(changedTree());
    api.getDataProjectSyncStatus.mockResolvedValue({ localProjectId: "project-a", remoteProjectId: "remote-a", remoteRevision: 3, localHead: "local-3", remoteHead: "remote-2", dirty: true, state: "dirty", trackingRef: "origin::main" });
    api.discoverDataProjectRemotes.mockResolvedValue([{ id: "remote-a", organizationId: null, ownerId: "owner-a", name: "云端道路项目", description: "", status: "ready", revision: 3, role: "viewer", datasetCount: 1, refCount: 1, commitCount: 3, updatedAt: "2026-10-25T11:00:00Z" }]);
    renderHome();
    await screen.findByRole("heading", { name: "道路检测项目" });
    await user.click(screen.getByRole("button", { name: "查看云端" }));
    const pullButton = await screen.findByRole("button", { name: "读取云端更新" });
    expect(pullButton).toBeDisabled();
    expect(screen.getAllByText("有未提交变更").length).toBeGreaterThan(0);
    expect(api.syncDataProject).not.toHaveBeenCalled();
  });
});

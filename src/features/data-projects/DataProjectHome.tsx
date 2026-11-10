import { useEffect, useMemo, useRef, useState } from "react";
import { Check, ChevronDown, Cloud, CloudOff, GitBranch, GitCommitHorizontal, GitMerge, History, Plus, RefreshCw, RotateCcw, Server, UploadCloud, X } from "lucide-react";
import { DatasetCard } from "../datasets/DatasetCard";
import { RuntimeStatePanel } from "../shared/RuntimeStatePanel";
import type { DatasetImage, DatasetProject } from "../../types/domain";
import {
  assignDatasetToDataProject,
  cloneDataProject,
  commitDataProject,
  connectRemoteDataProject,
  createRemoteDataProject,
  createDataProject,
  createDataProjectBranch,
  deleteDataProjectBranch,
  discoverDataProjectRemotes,
  getDataProjectSyncStatus,
  getDataProjectWorkingTree,
  listDataProjectCloudMembers,
  listDataProjectBranches,
  listDataProjectCommits,
  listDataProjects,
  mergeDataProjectBranches,
  previewDataProjectMerge,
  publishDataProject,
  removeDataProjectCloudMember,
  resolveDataProjectMerge,
  revertDataProject,
  switchDataProjectBranch,
  syncDataProject,
  upsertDataProjectCloudMember,
} from "../../api/dataProjects";
import type {
  DataProjectBranch,
  DataProject,
  DataProjectChange,
  DataProjectCommit,
  DataProjectMergeConflict,
  DataProjectCloudMember,
  DataProjectCounts,
  DataProjectSyncStatus,
  RemoteDataProject,
  DataProjectWorkingTree,
  DataProjectSection,
} from "../../types/dataProjects";
import { SESSION_CHANGE_EVENT } from "../../shared/sessionEvents";
import { DataProjectOverview, DataProjectWorkspace } from "./DataProjectWorkspace";
import "./data-projects.css";

export type DataProjectRuntimeState = "loading" | "ready" | "downloading" | "backend-unavailable" | "download-error";

export type DataProjectHomeProps = {
  projects?: DatasetProject[];
  projectImages?: Record<string, DatasetImage[]>;
  runtimeState?: DataProjectRuntimeState;
  runtimeMessage?: string | null;
  testDatasetDownloaded?: boolean;
  testDatasetKey?: string;
  onDownload?: (datasetKey: string) => void;
  onInfo?: () => void;
  onAnnotate?: (project: DatasetProject) => void;
  onOpenWindow?: (project: DatasetProject) => void;
  onOpen?: (projectId: string) => void;
  onCreateDataset?: (dataProjectId?: string) => void;
  onDatasetsChanged?: () => void;
  onExportDataProject?: (dataProjectId: string) => void | Promise<void>;
  exportingDataProjectId?: string | null;
  dataProjectId?: string;
  dataProject?: DataProject | null;
  section?: DataProjectSection;
  onNavigateProject?: (id: string, section?: DataProjectSection) => void;
  onNavigateHome?: () => void;
  onUploadImages?: (dataProjectId: string, datasetId?: string) => void;
  onOpenUnassigned?: () => void;
};

function formatDate(value: string) {
  const parsed = new Date(value);
  return Number.isNaN(parsed.valueOf()) ? value : parsed.toLocaleString("zh-CN", { dateStyle: "short", timeStyle: "short" });
}

function changeLabel(change: DataProjectChange) {
  const kind = change.kind.toLowerCase();
  if (kind.includes("dataset") && kind.includes("add")) return "加入数据集";
  if (kind.includes("dataset") && (kind.includes("remove") || kind.includes("delete"))) return "移除数据集";
  if (kind === "dataset-changed") return "数据集信息变更";
  if (kind.includes("image") && (kind.includes("add") || kind.includes("create"))) return "新增图像";
  if (kind.includes("image") && (kind.includes("remove") || kind.includes("delete"))) return "移除图像";
  if (kind === "image-changed") return "图像或标签变更";
  if (kind.includes("annotation") || kind.includes("label")) return "标签变更";
  if (kind.includes("review") || kind.includes("qa")) return "审核状态变更";
  return change.kind || "数据变更";
}

function normalizeCounts(tree: DataProjectWorkingTree | null): DataProjectCounts {
  const counts = tree?.counts;
  return {
    raw: counts?.raw ?? 0,
    inProgress: counts?.inProgress ?? 0,
    pendingReview: counts?.pendingReview ?? 0,
    approved: counts?.approved ?? 0,
    rejected: counts?.rejected ?? 0,
    total: counts?.total ?? 0,
  };
}

function countLabel(count: number) {
  return count.toLocaleString("zh-CN");
}

function featureError(cause: unknown): string {
  const message = cause instanceof Error ? cause.message : String(cause);
  if (/403|forbidden|permission|权限|unauthori[sz]ed|401/i.test(message)) {
    return `权限不足：${message}`;
  }
  return message;
}

function cloudStatusLabel(status: string | undefined): string {
  switch (status) {
    case "syncing": return "同步中";
    case "needsSync": return "待同步";
    case "readOnlyAhead": return "本地分支有新提交，当前云端只读";
    case "dirty": return "有未提交变更";
    case "diverged": return "分叉";
    case "upToDate": return "已同步";
    case "offline": return "离线";
    case "error": return "同步失败";
    default: return "未连接";
  }
}

const conflictFieldLabels: Record<string, string> = {
  entry: "条目",
  metadata: "元数据",
  membership: "数据集归属",
  image: "图像",
  annotation: "标签",
  review: "审核",
  split: "数据集划分",
  status: "状态",
  qaStatus: "质检状态",
};

function readableConflictText(value: string): string {
  return value
    .replace(/[\u0000-\u001f\u007f-\u009f]+/g, " / ")
    .replace(/(^|[:/\s])(entry|metadata|membership|image|annotation|review|split|status|qaStatus)(?=$|[:/\s])/g, (_match, prefix: string, field: string) => `${prefix}${conflictFieldLabels[field] ?? field}`)
    .replace(/^(条目|元数据|数据集归属):/, "$1：")
    .replace(/\b[a-f0-9]{16,}\b/gi, (hash) => `版本 ${hash.slice(0, 10)}`)
    .replace(/\s{2,}/g, " ")
    .trim();
}

function conflictDisplayName(conflict: DataProjectMergeConflict): string {
  const displayName = typeof conflict.displayName === "string" && conflict.displayName.trim()
    ? conflict.displayName.trim()
    : conflict.key;
  return readableConflictText(displayName);
}

function conflictKindLabel(kind: string): string {
  return readableConflictText(kind);
}

function conflictValueLabel(value: DataProjectMergeConflict["base"], description?: string | null): string {
  const readableDescription = typeof description === "string" && description.trim()
    ? readableConflictText(description)
    : null;
  if (!value) return readableDescription ?? "删除/不存在";
  if (typeof value === "object") {
    const record = value as Record<string, unknown>;
    const hash = typeof record.hash === "string" ? readableConflictText(record.hash) : "";
    const status = typeof record.status === "string"
      ? record.status
      : typeof record.qaStatus === "string" ? record.qaStatus : "已记录";
    return [readableDescription ?? readableConflictText(status), hash].filter(Boolean).join(" · ");
  }
  const fallback = readableConflictText(String(value));
  return readableDescription && fallback !== String(value) ? `${readableDescription} · ${fallback}` : readableDescription ?? fallback;
}

function conflictKey(conflict: DataProjectMergeConflict): string {
  return conflict.key;
}

function StatusSummary({ counts }: { counts: DataProjectCounts | null }) {
  const items = [
    ["未处理", counts?.raw, "raw"],
    ["处理中", counts?.inProgress, "in-progress"],
    ["待审核", counts?.pendingReview, "pending"],
    ["已通过", counts?.approved, "approved"],
    ["已驳回", counts?.rejected, "rejected"],
  ] as const;
  return (
    <div className="data-projects-status-inline" aria-label="数据处理状态统计">
      {items.map(([label, count, tone]) => (
        <span className={`data-projects-status-item ${tone}`} key={tone}><strong>{count === undefined ? "—" : countLabel(count)}</strong><span>{label}</span></span>
      ))}
      <span className="data-projects-status-item total"><strong>{counts ? countLabel(counts.total) : "—"}</strong><span>总图像</span></span>
    </div>
  );
}

function LegacyDataProjectHome({
  projects = [],
  projectImages = {},
  runtimeState = "ready",
  runtimeMessage = null,
  testDatasetDownloaded = false,
  testDatasetKey = "",
  onDownload = () => undefined,
  onInfo = () => undefined,
  onAnnotate = () => undefined,
  onOpenWindow = () => undefined,
  onOpen = () => undefined,
  onDatasetsChanged,
  onExportDataProject,
  exportingDataProjectId,
}: DataProjectHomeProps) {
  const [dataProjects, setDataProjects] = useState<DataProject[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [workingTree, setWorkingTree] = useState<DataProjectWorkingTree | null>(null);
  const [commits, setCommits] = useState<DataProjectCommit[]>([]);
  const [loadingProjects, setLoadingProjects] = useState(true);
  const [loadingDetails, setLoadingDetails] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [formOpen, setFormOpen] = useState(false);
  const [newName, setNewName] = useState("");
  const [newDescription, setNewDescription] = useState("");
  const [commitMessage, setCommitMessage] = useState("");
  const [busy, setBusy] = useState(false);
  const [branches, setBranches] = useState<DataProjectBranch[]>([]);
  const [branchesOpen, setBranchesOpen] = useState(false);
  const [branchBusy, setBranchBusy] = useState(false);
  const [branchError, setBranchError] = useState<string | null>(null);
  const [newBranchName, setNewBranchName] = useState("");
  const [mergeSourceBranch, setMergeSourceBranch] = useState("");
  const [mergePreview, setMergePreview] = useState<Awaited<ReturnType<typeof previewDataProjectMerge>> | null>(null);
  const [mergeResolutions, setMergeResolutions] = useState<Record<string, "base" | "ours" | "theirs"> >({});
  const [mergeSessionId, setMergeSessionId] = useState<string | null>(null);
  const [mergePreviewSourceBranch, setMergePreviewSourceBranch] = useState<string | null>(null);
  const [mergePreviewTargetBranch, setMergePreviewTargetBranch] = useState<string | null>(null);
  const [mergeMessage, setMergeMessage] = useState("");
  const [revertingCommitId, setRevertingCommitId] = useState<string | null>(null);
  const [revertMessage, setRevertMessage] = useState("");
  const [cloudState, setCloudState] = useState<DataProjectSyncStatus | null>(null);
  const [cloudOpen, setCloudOpen] = useState(false);
  const [cloudBusy, setCloudBusy] = useState(false);
  const [cloudError, setCloudError] = useState<string | null>(null);
  const [cloudMessage, setCloudMessage] = useState<string | null>(null);
  const [cloudRole, setCloudRole] = useState<string | null>(null);
  const [cloudMembers, setCloudMembers] = useState<DataProjectCloudMember[]>([]);
  const [memberBusy, setMemberBusy] = useState(false);
  const [memberError, setMemberError] = useState<string | null>(null);
  const [memberUserId, setMemberUserId] = useState("");
  const [memberRole, setMemberRole] = useState("viewer");
  const [remoteProjects, setRemoteProjects] = useState<RemoteDataProject[]>([]);
  const [remoteBusy, setRemoteBusy] = useState(false);
  const projectsRequest = useRef(0);
  const detailRequest = useRef(0);
  const commitRequest = useRef(0);
  const branchRequest = useRef(0);
  const cloudRequest = useRef(0);
  const selectedIdRef = useRef<string | null>(null);
  selectedIdRef.current = selectedId;
  const clearAdvancedState = () => {
    branchRequest.current += 1;
    cloudRequest.current += 1;
    setBranches([]);
    setBranchesOpen(false);
    setBranchBusy(false);
    setMergePreview(null);
    setMergeResolutions({});
    setMergeSessionId(null);
    setMergePreviewSourceBranch(null);
    setMergePreviewTargetBranch(null);
    setMergeSourceBranch("");
    setMergeMessage("");
    setNewBranchName("");
    setBranchError(null);
    setCloudState(null);
    setCloudOpen(false);
    setCloudBusy(false);
    setCloudError(null);
    setCloudMessage(null);
    setCloudRole(null);
    setCloudMembers([]);
    setMemberBusy(false);
    setMemberError(null);
    setMemberUserId("");
    setMemberRole("viewer");
    setRemoteProjects([]);
    setRemoteBusy(false);
    setRevertingCommitId(null);
    setRevertMessage("");
  };

  const reloadProjects = async () => {
    const request = ++projectsRequest.current;
    detailRequest.current += 1;
    commitRequest.current += 1;
    clearAdvancedState();
    setWorkingTree(null);
    setCommits([]);
    setLoadingDetails(Boolean(selectedIdRef.current));
    setLoadingProjects(true);
    setError(null);
    try {
      const loaded = await listDataProjects();
      if (!Array.isArray(loaded) || loaded.some((item) => !item || typeof item.id !== "string" || typeof item.name !== "string" || !Array.isArray(item.datasetIds))) {
        throw new Error("数据项目响应格式无效");
      }
      if (projectsRequest.current !== request) return;
      setDataProjects(loaded);
      setSelectedId((current) => {
        if (current && loaded.some((item) => item.id === current)) return current;
        return loaded.find((item) => item.datasetIds.length > 0)?.id ?? loaded[0]?.id ?? null;
      });
      return loaded;
    } catch (cause) {
      if (projectsRequest.current !== request) return;
      selectedIdRef.current = null;
      setDataProjects([]);
      setSelectedId(null);
      setWorkingTree(null);
      setCommits([]);
      setLoadingDetails(false);
      setCommitMessage("");
      setError(featureError(cause instanceof Error ? cause : "读取数据项目失败"));
    } finally {
      if (projectsRequest.current === request) setLoadingProjects(false);
    }
  };

  useEffect(() => {
    void reloadProjects();
    return () => {
      projectsRequest.current += 1;
      detailRequest.current += 1;
    };
  }, [projects]);

  useEffect(() => {
    const handleSessionChange = () => {
      projectsRequest.current += 1;
      detailRequest.current += 1;
      commitRequest.current += 1;
      selectedIdRef.current = null;
      setDataProjects([]);
      setSelectedId(null);
      setWorkingTree(null);
      setCommits([]);
      clearAdvancedState();
      setLoadingProjects(true);
      setLoadingDetails(false);
      setCommitMessage("");
      setBusy(false);
      setError(null);
    };
    window.addEventListener(SESSION_CHANGE_EVENT, handleSessionChange);
    return () => window.removeEventListener(SESSION_CHANGE_EVENT, handleSessionChange);
  }, []);

  const selectedProject = useMemo(
    () => dataProjects.find((item) => item.id === selectedId) ?? null,
    [dataProjects, selectedId],
  );

  const selectedDatasetIdentity = useMemo(
    () => selectedProject?.datasetIds.slice().sort().join("\u0001") ?? "",
    [selectedProject],
  );

  useEffect(() => {
    if (!selectedId) {
      detailRequest.current += 1;
      commitRequest.current += 1;
      setWorkingTree(null);
      setCommits([]);
      clearAdvancedState();
      setLoadingDetails(false);
      setBusy(false);
      return;
    }
    const request = ++detailRequest.current;
    commitRequest.current += 1;
    clearAdvancedState();
    setWorkingTree(null);
    setCommits([]);
    setBusy(false);
    setLoadingDetails(true);
    setError(null);
    Promise.all([getDataProjectWorkingTree(selectedId), listDataProjectCommits(selectedId)])
      .then(([tree, history]) => {
        if (detailRequest.current !== request) return;
        if (!tree || !Array.isArray(tree.changes)) throw new Error("项目工作区响应格式无效");
        if (!Array.isArray(history) || history.some((item) => !item || typeof item.id !== "string" || !Array.isArray(item.entries) || !Array.isArray(item.datasetIds))) throw new Error("提交历史响应格式无效");
        setWorkingTree(tree);
        setCommits(history);
      })
      .catch((cause) => {
        if (detailRequest.current !== request) return;
        setError(featureError(cause instanceof Error ? cause : "读取项目工作区失败"));
        setWorkingTree(null);
        setCommits([]);
      })
      .finally(() => {
        if (detailRequest.current === request) setLoadingDetails(false);
      });
  }, [selectedId, selectedDatasetIdentity, dataProjects]);

  const assignedIds = useMemo(() => new Set(dataProjects.flatMap((item) => item.datasetIds)), [dataProjects]);
  const datasetNames = useMemo(() => new Map(projects.map((dataset) => [dataset.id, dataset.name])), [projects]);
  const selectedDatasetIds = new Set<string>(selectedProject?.datasetIds ?? []);
  const selectedDatasets = projects.filter((dataset) => selectedDatasetIds.has(dataset.id));
  const unavailableDatasetIds = [...selectedDatasetIds].filter((id) => !projects.some((dataset) => dataset.id === id));
  const unassignedDatasets = projects.filter((dataset) => !assignedIds.has(dataset.id));
  const counts = normalizeCounts(workingTree);

  async function assign(datasetId: string, dataProjectId: string | null) {
    if (!selectedProject && dataProjectId) return;
    setBusy(true);
    setError(null);
    try {
      await assignDatasetToDataProject(dataProjectId, datasetId);
      await reloadProjects();
    } catch (cause) {
      setError(featureError(cause instanceof Error ? cause : "更新数据集归属失败"));
    } finally {
      setBusy(false);
    }
  }

  async function createProject() {
    const name = newName.trim();
    if (!name) {
      setError("请输入数据项目名称");
      return;
    }
    const request = ++projectsRequest.current;
    detailRequest.current += 1;
    commitRequest.current += 1;
    selectedIdRef.current = null;
    setDataProjects([]);
    setSelectedId(null);
    setWorkingTree(null);
    setCommits([]);
    setLoadingDetails(false);
    setLoadingProjects(true);
    setBusy(true);
    setError(null);
    try {
      const created = await createDataProject(name, newDescription.trim());
      if (projectsRequest.current !== request) return;
      const refreshed = await reloadProjects();
      if (projectsRequest.current !== request + 1) return;
      if (refreshed?.some((item) => item.id === created.id)) {
        selectedIdRef.current = created.id;
        setSelectedId(created.id);
      }
      setNewName("");
      setNewDescription("");
      setFormOpen(false);
    } catch (cause) {
      if (projectsRequest.current === request) setError(featureError(cause instanceof Error ? cause : "创建数据项目失败"));
    } finally {
      setBusy(false);
    }
  }

  async function commit() {
    const message = commitMessage.trim();
    if (!selectedId || !message || !workingTree || workingTree.changes.length === 0) return;
    const projectId = selectedId;
    const request = ++commitRequest.current;
    setBusy(true);
    setError(null);
    try {
      let branchName = branches.find((branch) => branch.current)?.name;
      if (!branchName) {
        const loaded = await listDataProjectBranches(projectId);
        if (!Array.isArray(loaded)) throw new Error("分支响应格式无效");
        branchName = loaded.find((branch) => branch.current)?.name ?? loaded[0]?.name;
      }
      if (!branchName) throw new Error("当前项目没有可提交的分支");
      if (selectedIdRef.current !== projectId || commitRequest.current !== request) return;
      await commitDataProject(projectId, branchName, message);
      const [tree, history] = await Promise.all([getDataProjectWorkingTree(projectId), listDataProjectCommits(projectId)]);
      if (selectedIdRef.current === projectId && commitRequest.current === request) {
        setCommitMessage("");
        setWorkingTree(tree);
        setCommits(history);
        onDatasetsChanged?.();
      }
    } catch (cause) {
      if (selectedIdRef.current === projectId && commitRequest.current === request) {
        setError(featureError(cause instanceof Error ? cause : "提交项目版本失败"));
      }
    } finally {
      if (selectedIdRef.current === projectId && commitRequest.current === request) setBusy(false);
    }
  }

  const currentBranch = branches.find((branch) => branch.current) ?? branches[0] ?? null;
  const hasUncommittedChanges = Boolean(workingTree?.changes.length);
  const cloudDirty = hasUncommittedChanges || Boolean(cloudState?.dirty);
  const canManageCloud = cloudRole === "owner" || cloudRole === "admin";
  const canWriteConnectedCloud = cloudRole === "owner" || cloudRole === "admin" || cloudRole === "editor";
  const canWriteCloud = !cloudState?.remoteProjectId || canWriteConnectedCloud;

  async function loadBranches(open = true) {
    const projectId = selectedIdRef.current;
    if (!projectId) return;
    const request = ++branchRequest.current;
    setBranchBusy(true);
    setBranchError(null);
    try {
      const loaded = await listDataProjectBranches(projectId);
      if (!Array.isArray(loaded)) throw new Error("分支响应格式无效");
      if (selectedIdRef.current !== projectId || branchRequest.current !== request) return;
      setBranches(loaded);
      setMergeSourceBranch((current) => {
        if (current && loaded.some((branch) => branch.name === current && !branch.current)) return current;
        return loaded.find((branch) => !branch.current)?.name ?? "";
      });
      setBranchesOpen(open);
    } catch (cause) {
      if (selectedIdRef.current === projectId && branchRequest.current === request) setBranchError(featureError(cause));
    } finally {
      if (selectedIdRef.current === projectId && branchRequest.current === request) setBranchBusy(false);
    }
  }

  async function createBranch() {
    const projectId = selectedIdRef.current;
    const name = newBranchName.trim();
    if (!projectId || !name) return;
    const request = ++branchRequest.current;
    setBranchBusy(true);
    setBranchError(null);
    try {
      await createDataProjectBranch(projectId, name);
      if (selectedIdRef.current !== projectId || branchRequest.current !== request) return;
      setNewBranchName("");
      await loadBranches(true);
    } catch (cause) {
      if (selectedIdRef.current === projectId && branchRequest.current === request) setBranchError(featureError(cause));
      setBranchBusy(false);
    }
  }

  async function checkoutBranch(branchName: string) {
    const projectId = selectedIdRef.current;
    if (!projectId || !branchName || branchName === currentBranch?.name) return;
    if (hasUncommittedChanges) {
      setBranchError("工作区有未提交变更，请先提交后再切换分支。");
      return;
    }
    const request = ++branchRequest.current;
    setBranchBusy(true);
    setBranchError(null);
    try {
      await switchDataProjectBranch(projectId, branchName);
      if (selectedIdRef.current !== projectId || branchRequest.current !== request) return;
      setMergePreview(null);
      setMergeResolutions({});
      setMergeSessionId(null);
      await reloadProjects();
      if (selectedIdRef.current === projectId) {
        onDatasetsChanged?.();
        await loadBranches(true);
      }
    } catch (cause) {
      if (selectedIdRef.current === projectId && branchRequest.current === request) setBranchError(featureError(cause));
    } finally {
      if (selectedIdRef.current === projectId && branchRequest.current === request) setBranchBusy(false);
    }
  }

  async function deleteBranch(branch: DataProjectBranch) {
    const projectId = selectedIdRef.current;
    if (!projectId || branch.current) return;
    const request = ++branchRequest.current;
    setBranchBusy(true);
    setBranchError(null);
    try {
      await deleteDataProjectBranch(projectId, branch.name);
      if (selectedIdRef.current !== projectId || branchRequest.current !== request) return;
      await loadBranches(true);
    } catch (cause) {
      if (selectedIdRef.current === projectId && branchRequest.current === request) setBranchError(featureError(cause));
    } finally {
      if (selectedIdRef.current === projectId && branchRequest.current === request) setBranchBusy(false);
    }
  }

  async function previewMerge() {
    const projectId = selectedIdRef.current;
    const targetBranch = currentBranch?.name;
    const sourceBranch = mergeSourceBranch.trim();
    if (!projectId || !targetBranch || !sourceBranch) {
      setBranchError("请选择要合并的源分支。");
      return;
    }
    if (sourceBranch === targetBranch) {
      setBranchError("源分支和当前分支不能相同。");
      return;
    }
    const request = ++branchRequest.current;
    setBranchBusy(true);
    setBranchError(null);
    try {
      const preview = await previewDataProjectMerge(projectId, sourceBranch, targetBranch);
      if (!Array.isArray(preview.conflicts)) throw new Error("合并预览响应格式无效");
      if (selectedIdRef.current !== projectId || branchRequest.current !== request) return;
      setMergePreview(preview);
      setMergeSessionId(null);
      setMergePreviewSourceBranch(sourceBranch);
      setMergePreviewTargetBranch(targetBranch);
      setMergeResolutions({});
      if (!preview.conflicts.length) setMergeMessage((current) => current || `合并 ${sourceBranch}`);
    } catch (cause) {
      if (selectedIdRef.current === projectId && branchRequest.current === request) setBranchError(featureError(cause));
    } finally {
      if (selectedIdRef.current === projectId && branchRequest.current === request) setBranchBusy(false);
    }
  }

  async function confirmMerge() {
    const projectId = selectedIdRef.current;
    const preview = mergePreview;
    if (!projectId || !preview) return;
    if (hasUncommittedChanges) {
      setBranchError("工作区有未提交变更，请先提交后再合并。");
      return;
    }
    const missing = preview.conflicts.find((conflict) => !mergeResolutions[conflictKey(conflict)]);
    if (missing) {
      setBranchError("请为每一项冲突选择基线、当前分支或源分支版本。");
      return;
    }
    if (preview.conflicts.some((conflict) => mergeResolutions[conflictKey(conflict)] === "base")) {
      // The native resolver currently accepts only ours/theirs. Reject this
      // choice before creating a server-side merge session so a UI-only base
      // selection cannot leave an orphaned unresolved session behind.
      setBranchError("当前后端合并解析仅支持当前分支或源分支；基线版本可查看但不能作为写入选择。");
      return;
    }
    const message = mergeMessage.trim();
    if (!message) {
      setBranchError("请输入合并提交说明。");
      return;
    }
    const request = ++branchRequest.current;
    setBranchBusy(true);
    setBranchError(null);
    try {
      let sessionId = mergeSessionId;
      let conflicts = preview.conflicts;
      if (!sessionId) {
        const result = await mergeDataProjectBranches(
          projectId,
          mergePreviewSourceBranch ?? mergeSourceBranch,
          mergePreviewTargetBranch ?? currentBranch?.name ?? "main",
          message,
        );
        if (selectedIdRef.current !== projectId || branchRequest.current !== request) return;
        conflicts = Array.isArray(result.conflicts) ? result.conflicts : [];
        sessionId = result.sessionId;
        if (conflicts.length && !sessionId) {
          throw new Error("合并仍有未解决冲突，请检查每一项选择。");
        }
        if (sessionId) setMergeSessionId(sessionId);
      }
      if (sessionId) {
        for (const conflict of conflicts) {
          const resolution = mergeResolutions[conflictKey(conflict)];
          if (!resolution || resolution === "base") {
            throw new Error("当前后端合并会话仅支持选择当前分支或源分支版本；请重新选择后确认。");
          }
          const resolved = await resolveDataProjectMerge(projectId, sessionId, conflictKey(conflict), resolution);
          if (selectedIdRef.current !== projectId || branchRequest.current !== request) return;
          sessionId = resolved.sessionId ?? sessionId;
          if (Array.isArray(resolved.conflicts) && resolved.conflicts.length > 0) {
            setMergeSessionId(sessionId);
          }
        }
      }
      setMergePreview(null);
      setMergeResolutions({});
      setMergeSessionId(null);
      setMergePreviewSourceBranch(null);
      setMergePreviewTargetBranch(null);
      setMergeMessage("");
      await reloadProjects();
      if (selectedIdRef.current === projectId) {
        onDatasetsChanged?.();
        await loadBranches(true);
      }
    } catch (cause) {
      if (selectedIdRef.current === projectId && branchRequest.current === request) setBranchError(featureError(cause));
    } finally {
      if (selectedIdRef.current === projectId && branchRequest.current === request) setBranchBusy(false);
    }
  }

  async function revertCommit(commitId: string) {
    const projectId = selectedIdRef.current;
    const message = revertMessage.trim();
    if (!projectId || !message) return;
    if (hasUncommittedChanges) {
      setBranchError("工作区有未提交变更，请先提交后再回退。");
      return;
    }
    const request = ++branchRequest.current;
    setBranchBusy(true);
    setBranchError(null);
    try {
      let branchName = currentBranch?.name;
      if (!branchName) {
        const loaded = await listDataProjectBranches(projectId);
        if (!Array.isArray(loaded)) throw new Error("分支响应格式无效");
        branchName = loaded.find((branch) => branch.current)?.name ?? loaded[0]?.name;
      }
      if (!branchName) throw new Error("当前项目没有可回退的分支");
      if (selectedIdRef.current !== projectId || branchRequest.current !== request) return;
      await revertDataProject(projectId, branchName, commitId, message);
      if (selectedIdRef.current !== projectId || branchRequest.current !== request) return;
      setRevertingCommitId(null);
      setRevertMessage("");
      await reloadProjects();
      if (selectedIdRef.current === projectId) {
        onDatasetsChanged?.();
        await loadBranches(true);
      }
    } catch (cause) {
      if (selectedIdRef.current === projectId && branchRequest.current === request) setBranchError(featureError(cause));
    } finally {
      if (selectedIdRef.current === projectId && branchRequest.current === request) setBranchBusy(false);
    }
  }

  async function loadCloud() {
    const projectId = selectedIdRef.current;
    if (!projectId) return;
    const request = ++cloudRequest.current;
    setCloudBusy(true);
    setCloudError(null);
    try {
      const state = await getDataProjectSyncStatus(projectId);
      if (selectedIdRef.current !== projectId || cloudRequest.current !== request) return;
      if (!state || typeof state !== "object" || typeof state.state !== "string" || typeof state.localProjectId !== "string") {
        throw new Error("云端同步状态响应格式无效");
      }
      setCloudState(state);
      setMemberError(null);
      setCloudOpen(true);
      if (state.remoteProjectId) {
        // Sync status intentionally does not expose ACL role. Resolve it from
        // the account-scoped remote listing before enabling writes or member
        // management; an unknown role remains read-only in the UI.
        let resolvedRole = cloudRole;
        const cachedRemote = remoteProjects.find((remote) => remote.id === state.remoteProjectId);
        if (cachedRemote) {
          resolvedRole = cachedRemote.role;
        } else {
          try {
            const remotes = await discoverDataProjectRemotes();
            if (Array.isArray(remotes)) {
              setRemoteProjects(remotes);
              resolvedRole = remotes.find((remote) => remote.id === state.remoteProjectId)?.role ?? null;
            }
          } catch {
            // The connected status remains useful even when discovery is
            // unavailable. Keep writes disabled until a role is known.
          }
        }
        if (selectedIdRef.current !== projectId || cloudRequest.current !== request) return;
        setCloudRole(resolvedRole ?? null);
        setMemberBusy(true);
        try {
          const members = await listDataProjectCloudMembers(projectId);
          if (selectedIdRef.current === projectId && cloudRequest.current === request) {
            if (!Array.isArray(members)) throw new Error("云端成员响应格式无效");
            setCloudMembers(members);
          }
        } catch (cause) {
          if (selectedIdRef.current === projectId && cloudRequest.current === request) setMemberError(featureError(cause));
        } finally {
          if (selectedIdRef.current === projectId && cloudRequest.current === request) setMemberBusy(false);
        }
      } else {
        setCloudMembers([]);
        setMemberBusy(false);
      }
    } catch (cause) {
      if (selectedIdRef.current === projectId && cloudRequest.current === request) setCloudError(featureError(cause));
    } finally {
      if (selectedIdRef.current === projectId && cloudRequest.current === request) setCloudBusy(false);
    }
  }

  async function refreshCloudMembers() {
    const projectId = selectedIdRef.current;
    if (!projectId || !cloudState?.remoteProjectId) return;
    const request = ++cloudRequest.current;
    setMemberBusy(true);
    setMemberError(null);
    try {
      const members = await listDataProjectCloudMembers(projectId);
      if (selectedIdRef.current !== projectId || cloudRequest.current !== request) return;
      if (!Array.isArray(members)) throw new Error("云端成员响应格式无效");
      setCloudMembers(members);
    } catch (cause) {
      if (selectedIdRef.current === projectId && cloudRequest.current === request) setMemberError(featureError(cause));
    } finally {
      if (selectedIdRef.current === projectId && cloudRequest.current === request) setMemberBusy(false);
    }
  }

  async function saveCloudMember() {
    const projectId = selectedIdRef.current;
    const userId = memberUserId.trim();
    if (!projectId || !userId || !canManageCloud) return;
    const request = ++cloudRequest.current;
    setMemberBusy(true);
    setMemberError(null);
    try {
      await upsertDataProjectCloudMember(projectId, userId, memberRole);
      if (selectedIdRef.current !== projectId || cloudRequest.current !== request) return;
      setMemberUserId("");
      await refreshCloudMembers();
    } catch (cause) {
      if (selectedIdRef.current === projectId && cloudRequest.current === request) setMemberError(featureError(cause));
    } finally {
      if (selectedIdRef.current === projectId && cloudRequest.current === request) setMemberBusy(false);
    }
  }

  async function removeCloudMember(member: DataProjectCloudMember) {
    const projectId = selectedIdRef.current;
    if (!projectId || !canManageCloud) return;
    const request = ++cloudRequest.current;
    setMemberBusy(true);
    setMemberError(null);
    try {
      await removeDataProjectCloudMember(projectId, member.userId);
      if (selectedIdRef.current !== projectId || cloudRequest.current !== request) return;
      await refreshCloudMembers();
    } catch (cause) {
      if (selectedIdRef.current === projectId && cloudRequest.current === request) setMemberError(featureError(cause));
    } finally {
      if (selectedIdRef.current === projectId && cloudRequest.current === request) setMemberBusy(false);
    }
  }

  async function publishCloud(remoteProjectId?: string, remoteRole?: string) {
    const projectId = selectedIdRef.current;
    const project = selectedProject;
    if (!projectId || !project) return;
    if (cloudDirty) {
      setCloudError("工作区有未提交变更，请先提交后再发布到云端。");
      return;
    }
    const requestedRole = remoteRole ?? cloudRole;
    if ((cloudState?.remoteProjectId || remoteProjectId) && !(
      requestedRole === "owner" || requestedRole === "admin" || requestedRole === "editor"
    )) {
      setCloudError("当前云端角色为只读或尚未确认权限，不能发布写入。");
      return;
    }
    const request = ++cloudRequest.current;
    setCloudBusy(true);
    setCloudError(null);
    setCloudMessage(null);
    try {
      let remoteId = remoteProjectId ?? cloudState?.remoteProjectId ?? null;
      let role = remoteRole ?? cloudRole;
      if (!remoteId) {
        const created = await createRemoteDataProject(project.name, project.description);
        remoteId = created.id;
        role = created.role;
      }
      const bundle = await connectRemoteDataProject(projectId, remoteId);
      role = bundle.project.role || role;
      const report = await publishDataProject(projectId);
      const state = await getDataProjectSyncStatus(projectId);
      if (selectedIdRef.current !== projectId || cloudRequest.current !== request) return;
      setCloudRole(role);
      setCloudState(state);
      setCloudMessage(`云端返回：${report.outcome}，远端修订 ${report.remoteRevision}。`);
    } catch (cause) {
      if (selectedIdRef.current === projectId && cloudRequest.current === request) setCloudError(featureError(cause));
    } finally {
      if (selectedIdRef.current === projectId && cloudRequest.current === request) setCloudBusy(false);
    }
  }

  async function syncCloud() {
    const projectId = selectedIdRef.current;
    if (!projectId) return;
    if (cloudDirty) {
      setCloudError("工作区有未提交变更，请先提交后再同步，避免覆盖本地工作。");
      return;
    }
    const request = ++cloudRequest.current;
    setCloudBusy(true);
    setCloudError(null);
    setCloudMessage(null);
    setCloudState((current) => current ? { ...current, state: "syncing" } : {
      localProjectId: projectId,
      remoteProjectId: null,
      remoteRevision: null,
      localHead: null,
      remoteHead: null,
      dirty: false,
      state: "syncing",
      trackingRef: null,
    });
    try {
      const result = await syncDataProject(projectId);
      const state = await getDataProjectSyncStatus(projectId);
      if (selectedIdRef.current !== projectId || cloudRequest.current !== request) return;
      setCloudState(state);
      setCloudMessage(`同步返回：${result.outcome}，远端修订 ${result.remoteRevision}。`);
      onDatasetsChanged?.();
    } catch (cause) {
      if (selectedIdRef.current === projectId && cloudRequest.current === request) {
        setCloudError(featureError(cause));
        try {
          const recovered = await getDataProjectSyncStatus(projectId);
          if (selectedIdRef.current === projectId && cloudRequest.current === request) {
            setCloudState(recovered);
          }
        } catch {
          if (selectedIdRef.current === projectId && cloudRequest.current === request) {
            setCloudState((current) => current ? { ...current, state: "error" } : current);
          }
        }
      }
    } finally {
      if (selectedIdRef.current === projectId && cloudRequest.current === request) setCloudBusy(false);
    }
  }

  async function discoverRemotes() {
    const projectId = selectedIdRef.current;
    if (!projectId) return;
    const request = ++cloudRequest.current;
    setRemoteBusy(true);
    setCloudError(null);
    try {
      const remotes = await discoverDataProjectRemotes();
      if (!Array.isArray(remotes)) throw new Error("远端项目响应格式无效");
      if (selectedIdRef.current !== projectId || cloudRequest.current !== request) return;
      setRemoteProjects(remotes);
      const connected = remotes.find((remote) => remote.id === cloudState?.remoteProjectId);
      if (connected) setCloudRole(connected.role);
    } catch (cause) {
      if (selectedIdRef.current === projectId && cloudRequest.current === request) setCloudError(featureError(cause));
    } finally {
      if (selectedIdRef.current === projectId && cloudRequest.current === request) setRemoteBusy(false);
    }
  }

  async function cloneRemote(remote: RemoteDataProject) {
    const remoteId = remote.id;
    const request = ++cloudRequest.current;
    setRemoteBusy(true);
    setCloudError(null);
    try {
      const report = await cloneDataProject(remoteId);
      if (cloudRequest.current !== request) return;
      setCloudMessage(`已导入远端项目“${remote.name}”，云端返回：${report.outcome}。`);
      onDatasetsChanged?.();
      const refreshed = await reloadProjects();
      if (refreshed?.some((item) => item.id === report.localProjectId)) {
        selectedIdRef.current = report.localProjectId;
        setSelectedId(report.localProjectId);
      }
    } catch (cause) {
      if (cloudRequest.current === request) setCloudError(featureError(cause));
    } finally {
      if (cloudRequest.current === request) setRemoteBusy(false);
    }
  }

  if (runtimeState === "backend-unavailable") {
    return <main className="content"><section className="dataset-main"><RuntimeStatePanel title="后端未连接" message="请在 Tauri 桌面环境启动应用。" /></section></main>;
  }

  return (
    <main className="content"><section className="dataset-main data-projects-page">
      <div className="page-heading data-projects-heading">
        <div><h1>数据标注</h1></div>
        <div className="filter-row data-projects-actions">
          <button type="button" onClick={() => setFormOpen((open) => !open)}><Plus size={16} />新建数据项目</button>
          <button type="button" onClick={onInfo}>工程信息</button>
          <button type="button" onClick={() => onDownload(testDatasetKey)} disabled={runtimeState === "loading" || runtimeState === "downloading" || testDatasetDownloaded}><RefreshCw size={15} />{runtimeState === "downloading" ? "正在下载测试数据" : testDatasetDownloaded ? "测试数据已下载" : "下载测试数据集"}</button>
        </div>
      </div>

      {formOpen ? <div className="data-projects-create" aria-label="新建数据项目">
        <label><span>项目名称</span><input autoFocus value={newName} onChange={(event) => setNewName(event.target.value)} placeholder="例如：道路目标检测" /></label>
        <label><span>项目说明</span><input value={newDescription} onChange={(event) => setNewDescription(event.target.value)} placeholder="记录数据来源、版本目标或团队约定" /></label>
        <div><button type="button" className="primary" disabled={busy || !newName.trim()} onClick={() => void createProject()}>创建项目</button><button type="button" onClick={() => setFormOpen(false)}>取消</button></div>
      </div> : null}

      {error ? <div className="data-projects-error" role="alert">{error}<button type="button" aria-label="关闭错误" onClick={() => setError(null)}><X size={15} /></button></div> : null}
      {runtimeState === "downloading" ? <RuntimeStatePanel title="正在下载测试数据" message="下载完成后会显示数据集和图像预览。" /> : null}
      {runtimeState === "download-error" ? <RuntimeStatePanel title="测试数据下载失败" message={runtimeMessage ?? "请检查网络后重试。"} actionLabel="重试下载" onAction={() => onDownload(testDatasetKey)} /> : null}
      {runtimeState === "ready" && projects.length === 0 ? <RuntimeStatePanel title="暂无数据集" message="可以新建或导入本地数据集，也可以点击“下载测试数据集”获取 COCO128 示例。" /> : null}

      <div className="data-projects-layout">
        <aside className="data-projects-list" aria-label="数据项目列表">
          <div className="data-projects-list-heading"><strong>项目</strong><span>{loadingProjects ? "读取中" : `${dataProjects.length} 个`}</span></div>
          {dataProjects.map((item) => <button className={`data-project-row ${item.id === selectedId ? "active" : ""}`} key={item.id} type="button" onClick={() => setSelectedId(item.id)}><span><strong>{item.name}</strong></span><em>{item.datasetIds.length} 个数据集</em></button>)}
          {!loadingProjects && dataProjects.length === 0 ? <div className="data-projects-list-empty">还没有数据项目，先创建一个项目再归属数据集。</div> : null}
        </aside>

        <div className="data-projects-detail">
          {!selectedProject ? <div className="data-projects-empty"><GitCommitHorizontal size={24} /><h2>选择一个数据项目</h2></div> : <>
            <div className="data-projects-detail-title"><div><h2>{selectedProject.name}</h2></div></div>
            <StatusSummary counts={workingTree ? counts : null} />

            <section className="data-projects-section"><div className="data-projects-section-title"><div><h3>项目数据集</h3></div><strong>{selectedDatasetIds.size} 个</strong></div>
              {selectedDatasets.length > 0 ? <div className="dataset-grid data-projects-dataset-grid">{selectedDatasets.map((dataset) => <div className="data-projects-dataset-item" key={dataset.id}><DatasetCard dataset={dataset} previewImages={projectImages[dataset.id] ?? []} onOpen={() => onOpen(dataset.id)} onAnnotate={() => onAnnotate(dataset)} onOpenWindow={() => onOpenWindow(dataset)} /><button className="data-projects-remove" type="button" disabled={busy} onClick={() => void assign(dataset.id, null)}><X size={14} />移出项目</button></div>)}</div> : <div className="data-projects-section-empty">这个项目还没有数据集，请从下方未归属数据集中加入。</div>}
              {unavailableDatasetIds.map((id) => <div className="data-projects-unassigned-row" key={id}><span><strong>历史数据集</strong><small>当前数据集列表中暂不可用，历史提交仍保留</small></span><button type="button" disabled={busy} onClick={() => void assign(id, null)}>移出项目</button></div>)}
            </section>

            <details className="data-projects-project-settings">
              <summary><span>项目设置</span></summary>
              <div className="data-projects-project-settings-body">{selectedProject.description ? <p>{selectedProject.description}</p> : null}<span className="data-projects-head"><GitCommitHorizontal size={15} />{workingTree?.headCommitId ? `HEAD ${workingTree.headCommitId.slice(0, 8)}` : "尚未提交"}</span>{onExportDataProject ? <button type="button" disabled={busy || exportingDataProjectId === selectedProject.id} onClick={() => void onExportDataProject(selectedProject.id)}>{exportingDataProjectId === selectedProject.id ? "导出中..." : "导出图像与标签清单"}</button> : null}</div>
            </details>

            <section className="data-projects-section data-projects-branches-section">
              <div className="data-projects-section-title">
                <div><h3><GitBranch size={16} />分支与合并</h3></div>
                <button type="button" className="data-projects-section-action" aria-expanded={branchesOpen} disabled={branchBusy} onClick={() => branchesOpen ? setBranchesOpen(false) : void loadBranches(true)}><ChevronDown size={14} />{branchBusy ? "处理中..." : branchesOpen ? "收起" : "读取分支"}</button>
              </div>
              {branchError ? <div className="data-projects-feature-error" role="alert">{branchError}</div> : null}
              {branchesOpen ? <div className="data-projects-branches-body">
                {branches.length ? <>
                  <div className="data-projects-branch-toolbar">
                    <label><span>当前分支</span><select aria-label="当前分支" value={currentBranch?.name ?? ""} disabled={branchBusy || hasUncommittedChanges} onChange={(event) => void checkoutBranch(event.target.value)}>{branches.map((branch) => <option value={branch.name} key={branch.name}>{branch.name}{branch.current ? "（当前）" : ""}</option>)}</select></label>
                    <div className="data-projects-branch-list" aria-label="分支列表">{branches.map((branch) => <div className={`data-projects-branch-row ${branch.current ? "active" : ""}`} key={branch.name}><span><GitBranch size={14} /><strong>{branch.name}</strong>{branch.current ? <em>当前</em> : null}<small>{branch.headCommitId ? `HEAD ${branch.headCommitId.slice(0, 8)}` : "尚无提交"}</small></span>{!branch.current && branch.name !== "main" ? <button type="button" disabled={branchBusy} onClick={() => void deleteBranch(branch)} title="删除分支">删除</button> : null}</div>)}</div>
                  </div>
                  <div className="data-projects-create-branch"><input aria-label="新分支名称" value={newBranchName} onChange={(event) => setNewBranchName(event.target.value)} placeholder="新分支名称" /><button type="button" disabled={branchBusy || !newBranchName.trim()} onClick={() => void createBranch()}><Plus size={14} />创建分支</button></div>
                  <div className="data-projects-merge-box">
                    <div className="data-projects-merge-heading"><GitMerge size={15} /><strong>合并到当前分支</strong></div>
                    <div className="data-projects-merge-controls"><label><span>源分支</span><select aria-label="合并源分支" value={mergeSourceBranch} disabled={branchBusy} onChange={(event) => setMergeSourceBranch(event.target.value)}><option value="">选择源分支</option>{branches.filter((branch) => !branch.current).map((branch) => <option value={branch.name} key={branch.name}>{branch.name}</option>)}</select></label><button type="button" disabled={branchBusy || !mergeSourceBranch || !currentBranch} onClick={() => void previewMerge()}><GitMerge size={14} />预览合并</button></div>
                    {mergePreview ? <div className="data-projects-merge-preview">
                      <div className="data-projects-merge-summary"><span>当前：{mergePreviewTargetBranch ?? "未知"}</span><span>来源：{mergePreviewSourceBranch ?? "未知"}</span><span>{mergePreview.conflicts.length ? `${mergePreview.conflicts.length} 项冲突` : "没有冲突"}</span></div>
                      {mergePreview.conflicts.length ? <div className="data-projects-conflicts">{mergePreview.conflicts.map((conflict) => { const key = conflictKey(conflict); return <div className="data-projects-conflict" key={key}><div className="data-projects-conflict-title"><strong>{conflictDisplayName(conflict)}</strong><small>{conflictKindLabel(conflict.kind)}</small></div><div className="data-projects-conflict-values"><label><input type="radio" name={`conflict-${key}`} checked={mergeResolutions[key] === "base"} onChange={() => setMergeResolutions((current) => ({ ...current, [key]: "base" }))} /><span><b>基线版本</b>{conflictValueLabel(conflict.base, conflict.baseDescription)}</span></label><label><input type="radio" name={`conflict-${key}`} checked={mergeResolutions[key] === "ours"} onChange={() => setMergeResolutions((current) => ({ ...current, [key]: "ours" }))} /><span><b>当前分支</b>{conflictValueLabel(conflict.ours, conflict.oursDescription)}</span></label><label><input type="radio" name={`conflict-${key}`} checked={mergeResolutions[key] === "theirs"} onChange={() => setMergeResolutions((current) => ({ ...current, [key]: "theirs" }))} /><span><b>源分支</b>{conflictValueLabel(conflict.theirs, conflict.theirsDescription)}</span></label></div></div>; })}</div> : <p className="data-projects-merge-clean">此合并预览没有冲突，可以创建双父提交。</p>}
                      {mergeSessionId ? <small className="data-projects-merge-session">冲突会话 {mergeSessionId}</small> : null}
                      <div className="data-projects-merge-confirm"><input aria-label="合并提交说明" value={mergeMessage} onChange={(event) => setMergeMessage(event.target.value)} placeholder="合并提交说明" /><button className="primary" type="button" disabled={branchBusy || hasUncommittedChanges || !mergeMessage.trim() || mergePreview.conflicts.some((conflict) => !mergeResolutions[conflictKey(conflict)])} onClick={() => void confirmMerge()}><GitMerge size={14} />确认合并</button>{hasUncommittedChanges ? <small>请先提交工作区</small> : null}</div>
                    </div> : null}
                  </div>
                </> : <div className="data-projects-section-empty">暂无可用分支信息，请重试；后端拒绝或权限不足时会在此显示原因。</div>}
              </div> : null}
            </section>

            <details className="data-projects-section data-projects-version-section data-projects-collapsible-section"><summary className="data-projects-section-title data-projects-disclosure-summary"><div><h3>工作区变更</h3></div><span className="data-projects-summary-meta"><strong>{workingTree?.changes.length ?? (loadingDetails ? "读取中" : "—")} 项</strong><ChevronDown size={14} /></span></summary>
              <div className="data-projects-collapsible-content"><div className="data-projects-version-actions"><button type="button" disabled={busy || loadingProjects || loadingDetails} onClick={() => void reloadProjects()}><RefreshCw size={14} />刷新变更</button></div>
                {loadingDetails ? <div className="data-projects-section-empty">正在读取工作区...</div> : <>
                  <div className="data-projects-commit-box"><input aria-label="提交说明" disabled={!workingTree?.changes.length} value={commitMessage} onChange={(event) => setCommitMessage(event.target.value)} placeholder={workingTree?.changes.length ? "为这次数据变化写一句说明" : "工作区干净时不能创建空提交"} /><button className="primary" type="button" disabled={busy || !commitMessage.trim() || !workingTree?.changes.length} onClick={() => void commit()}><Check size={15} />提交版本</button></div>
                  {workingTree?.changes.length ? <div className="data-projects-changes">{workingTree.changes.map((change, index) => <div className="data-projects-change" key={`${change.datasetId}-${change.imageId ?? change.fileName ?? index}`}><span className="data-projects-change-kind">{changeLabel(change)}</span><span>{change.fileName || change.imageId || datasetNames.get(change.datasetId) || "数据集"}</span>{change.detail ? <small>{change.detail}</small> : null}</div>)}</div> : <div className="data-projects-section-empty">{workingTree ? "工作区干净，没有可提交的图像或标签变化。" : "工作区暂不可用，修复错误后刷新变更。"}</div>}
                </>}
              </div>
            </details>

            <details className="data-projects-section data-projects-history-section data-projects-collapsible-section"><summary className="data-projects-section-title data-projects-disclosure-summary"><div><h3><History size={16} />提交历史</h3></div><span className="data-projects-summary-meta"><strong>{commits.length} 次</strong><ChevronDown size={14} /></span></summary>
              <div className="data-projects-history-body">{commits.length ? <div className="data-projects-history">{commits.map((commitItem) => <div className="data-projects-history-row" key={commitItem.id}><GitCommitHorizontal size={17} /><div><strong>{commitItem.message}</strong><small>{formatDate(commitItem.createdAt)} · {commitItem.datasetIds.length} 个数据集 · {commitItem.entries.length} 个条目{commitItem.secondParentId ? " · 合并提交" : ""}</small></div><code title={commitItem.id}>{commitItem.id.slice(0, 8)}</code><details className="data-projects-version-detail"><summary>查看版本内容（{commitItem.entries.length} 张图像）</summary><p>父版本：{commitItem.parentId?.slice(0, 8) ?? "初始版本"}{commitItem.secondParentId ? " · 第二父版本：" + commitItem.secondParentId.slice(0, 8) : ""}</p>{commitItem.entries.length ? <div className="data-projects-version-files">{commitItem.entries.map((entry) => <div key={`${entry.datasetId}-${entry.imageId}`}><strong>{entry.fileName}</strong><span>{entry.status} · {entry.qaStatus || "未审核"}</span><small title={entry.imageHash}>图像 {entry.imageHash.slice(0, 12)}</small><small title={entry.annotationHash}>标签 {entry.annotationHash.slice(0, 12)}</small></div>)}</div> : <p>此版本未包含图像。</p>}</details>{revertingCommitId === commitItem.id ? <div className="data-projects-revert-box"><p>回退会创建新提交并保留现有历史。</p><input aria-label="回退提交说明" value={revertMessage} onChange={(event) => setRevertMessage(event.target.value)} placeholder="回退提交说明" /><button className="primary" type="button" disabled={branchBusy || hasUncommittedChanges || !revertMessage.trim()} onClick={() => void revertCommit(commitItem.id)}><RotateCcw size={14} />确认回退</button><button type="button" disabled={branchBusy} onClick={() => setRevertingCommitId(null)}>取消</button></div> : <button type="button" className="data-projects-revert-button" disabled={branchBusy || hasUncommittedChanges} title={hasUncommittedChanges ? "工作区有未提交变更，先提交后才能回退" : "回退到此版本，并创建新提交"} onClick={() => { setRevertingCommitId(commitItem.id); setRevertMessage("回退到 " + commitItem.id.slice(0, 8)); }}>回退到此版本（创建新提交）</button>}</div>)}</div> : <div className="data-projects-section-empty">还没有提交记录。完成一次有意义的数据变化后再提交。</div>}</div>
            </details>

            <section className="data-projects-section data-projects-cloud-section">
              <div className="data-projects-section-title">
                <div><h3>{cloudState?.state === "offline" || cloudState?.state === "error" ? <CloudOff size={16} /> : <Cloud size={16} />}云端项目</h3><span>按需查看同步状态。</span></div>
                <div className="data-projects-cloud-title-actions"><strong>{cloudStatusLabel(cloudState?.state)}</strong><button type="button" className="data-projects-section-action" aria-expanded={cloudOpen} disabled={cloudBusy} onClick={() => cloudOpen ? setCloudOpen(false) : void loadCloud()}><ChevronDown size={14} />{cloudBusy ? "处理中..." : cloudOpen ? "收起" : "查看云端"}</button></div>
              </div>
              {cloudError ? <div className="data-projects-feature-error" role="alert">{cloudError}</div> : null}
              {cloudOpen ? <div className="data-projects-cloud-body">
                <div className="data-projects-cloud-status"><Server size={15} /><div><strong>{cloudStatusLabel(cloudState?.state)}</strong><span>{cloudState?.remoteProjectId ? "远端 " + cloudState.remoteProjectId : "尚未连接云端项目"} · 修订 {cloudState?.remoteRevision ?? "—"}</span></div><dl><div><dt>本地脏</dt><dd>{cloudState?.dirty ? "是" : "否"}</dd></div><div><dt>本地 HEAD</dt><dd>{cloudState?.localHead?.slice(0, 8) ?? "—"}</dd></div><div><dt>远端 HEAD</dt><dd>{cloudState?.remoteHead?.slice(0, 8) ?? "—"}</dd></div></dl></div>
                <div className="data-projects-cloud-actions">{!cloudState?.remoteProjectId ? <button type="button" disabled={cloudBusy || cloudDirty} onClick={() => void publishCloud()}><UploadCloud size={14} />首次发布到云端</button> : null}<button type="button" disabled={cloudBusy || cloudDirty || !cloudState?.remoteProjectId} title={cloudState?.remoteProjectId && !canWriteCloud ? "只读角色仅拉取云端更新" : undefined} onClick={() => void syncCloud()}><RefreshCw size={14} />{cloudState?.remoteProjectId && !canWriteCloud ? "读取云端更新" : "手动同步"}</button><button type="button" disabled={remoteBusy || cloudBusy} onClick={() => void discoverRemotes()}><Server size={14} />发现远端项目</button>{cloudState?.remoteProjectId && !canWriteCloud ? <small>当前角色只读，仅拉取云端更新；本地领先变更需在可写成员端合并。</small> : cloudDirty ? <small>请先提交工作区后再进行云端操作</small> : null}</div>
                {cloudMessage ? <p className="data-projects-cloud-message" role="status">{cloudMessage}</p> : null}
                {cloudState?.remoteProjectId ? <section className="data-projects-members" aria-label="云端成员">
                  <div className="data-projects-members-heading"><div><strong>云端成员</strong><span>仅项目 owner/admin 可管理成员；不会发送邀请邮件。</span></div><button type="button" disabled={memberBusy} onClick={() => void refreshCloudMembers()}><RefreshCw size={13} />刷新成员</button></div>
                  {memberError ? <div className="data-projects-feature-error" role="alert">{memberError}</div> : null}
                  {memberBusy && cloudMembers.length === 0 ? <p className="data-projects-members-state">正在读取成员...</p> : cloudMembers.length ? <div className="data-projects-member-list">{cloudMembers.map((member) => <div className="data-projects-member-row" key={member.userId}><span><strong>{member.displayName || member.email || member.userId}</strong>{member.displayName || member.email ? <small>{member.userId}</small> : null}</span><em>{member.role}</em>{canManageCloud && member.role !== "owner" ? <button type="button" disabled={memberBusy} onClick={() => void removeCloudMember(member)}>移除</button> : null}</div>)}</div> : <p className="data-projects-members-state">暂无成员信息，或当前账户无权读取成员。</p>}
                  {canManageCloud ? <div className="data-projects-member-form"><input aria-label="成员用户 ID" value={memberUserId} onChange={(event) => setMemberUserId(event.target.value)} placeholder="输入现有账号的用户 ID" /><select aria-label="成员角色" value={memberRole} onChange={(event) => setMemberRole(event.target.value)}><option value="viewer">viewer（只读）</option><option value="editor">editor（可写）</option><option value="admin">admin（管理）</option></select><button type="button" disabled={memberBusy || !memberUserId.trim()} onClick={() => void saveCloudMember()}>保存成员</button></div> : <p className="data-projects-members-readonly">当前角色为 {cloudRole || "只读/未确认"}，成员管理不可用。</p>}
                </section> : null}
                <div className="data-projects-remote-list"><div className="data-projects-remote-heading"><strong>可发现的远端项目</strong>{remoteBusy ? <span>读取中...</span> : null}</div>{remoteProjects.length ? remoteProjects.map((remote) => <div className="data-projects-remote-row" key={remote.id}><span><strong>{remote.name}</strong><small>{remote.description || "未填写说明"} · 修订 {remote.revision} · {remote.role}</small></span><div className="data-projects-remote-actions"><button type="button" disabled={remoteBusy || cloudBusy || cloudDirty || !["owner", "admin", "editor"].includes(remote.role)} title={!['owner', 'admin', 'editor'].includes(remote.role) ? "当前角色只读，不能发布" : undefined} onClick={() => void publishCloud(remote.id, remote.role)}>连接并发布</button><button type="button" disabled={remoteBusy || cloudBusy} onClick={() => void cloneRemote(remote)}>导入本地</button></div></div>) : <p>点击“发现远端项目”后显示可导入的项目。</p>}</div>
              </div> : <div className="data-projects-section-empty">点击“查看云端”管理同步和远端项目。</div>}
            </section>
          </>}

          <section className="data-projects-section data-projects-unassigned-section"><div className="data-projects-section-title"><div><h3>未归属数据集</h3><span>直接打开或开始标注；也可加入项目。</span></div><strong>{unassignedDatasets.length} 个</strong></div>
            {unassignedDatasets.length > 0 ? <div className="dataset-grid data-projects-dataset-grid">{unassignedDatasets.map((dataset) => <div className="data-projects-dataset-item data-projects-unassigned-card" key={dataset.id}><DatasetCard dataset={dataset} previewImages={projectImages[dataset.id] ?? []} onOpen={() => onOpen(dataset.id)} onAnnotate={() => onAnnotate(dataset)} onOpenWindow={() => onOpenWindow(dataset)} />{selectedProject ? <button className="data-projects-assign" type="button" disabled={busy} onClick={() => void assign(dataset.id, selectedProject.id)}><Plus size={14} />加入当前项目</button> : <span className="data-projects-assign-hint">先创建或选择项目后加入</span>}</div>)}</div> : <div className="data-projects-section-empty">所有现有数据集都已经归属到某个数据项目，仍可在项目内移出。</div>}
          </section>
        </div>
      </div>
    </section></main>
  );
}

/**
 * The legacy prop shape remains supported for the desktop shell while the
 * controlled route shape is used by the Web and the new project navigation.
 * Keeping this adapter here lets both shells share the same project/data
 * presentation without making the data home fetch every project detail.
 */
export function DataProjectHome(props: DataProjectHomeProps) {
  const controlled = props.dataProjectId !== undefined || props.section !== undefined || props.onNavigateProject !== undefined;
  if (!controlled) return <LegacyDataProjectHome {...props} />;

  const navigateProject = props.onNavigateProject ?? (() => undefined);
  const navigateHome = props.onNavigateHome ?? (() => undefined);
  const onOpen = props.onOpen ?? (() => undefined);
  const onAnnotate = props.onAnnotate ?? (() => undefined);

  if (props.dataProjectId) {
    return <DataProjectWorkspace
      dataProjectId={props.dataProjectId}
      project={props.dataProject}
      section={props.section ?? "data"}
      projects={props.projects}
      projectImages={props.projectImages}
      onNavigateProject={navigateProject}
      onNavigateHome={navigateHome}
      onCreateDataset={props.onCreateDataset}
      onUploadImages={props.onUploadImages}
      onOpen={onOpen}
      onAnnotate={onAnnotate}
      onOpenWindow={props.onOpenWindow}
      onOpenUnassigned={props.onOpenUnassigned}
      onDatasetsChanged={props.onDatasetsChanged}
      onExportDataProject={props.onExportDataProject}
      exportingDataProjectId={props.exportingDataProjectId}
    />;
  }

  return <DataProjectOverview
    projects={props.projects}
    projectImages={props.projectImages}
    onNavigateProject={navigateProject}
    onCreateDataset={props.onCreateDataset}
    onUploadImages={props.onUploadImages}
    onOpen={onOpen}
    onAnnotate={onAnnotate}
    onOpenWindow={props.onOpenWindow}
    onOpenUnassigned={props.onOpenUnassigned}
  />;
}

import {
  Archive,
  Check,
  ChevronRight,
  CircleAlert,
  Database,
  Download,
  FileArchive,
  HardDrive,
  LoaderCircle,
  Plus,
  RefreshCw,
  Save,
  Upload,
  UserPlus,
  Users,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  addHostedMember,
  archiveHostedDataset,
  beginStableHostingOperation,
  captureHostedProject,
  completeStableHostingOperation,
  createHostedDataset,
  createHostedVersion,
  downloadHostedExport,
  getHostedDataset,
  getHostedVersion,
  getHostingJob,
  getHostingQuota,
  isHostingConflict,
  listHostingJobs,
  listHostedMembers,
  listHostedDatasets,
  listHostedVersions,
  openHostedExport,
  readHostingArchive,
  removeHostedMember,
  reuseHostedVersion,
  startHostedExport,
  updateHostedDataset,
  updateHostingQuota,
  uploadHostedArchive,
} from "../../api/hosting";
import { authStatus, listRemoteProjects, remoteProjectOpen } from "../../api/mobile";
import type { DatasetProject } from "../../types/domain";
import type { RemoteProjectSummary } from "../../types/mobile";
import { SESSION_CHANGE_EVENT } from "../../shared/sessionEvents";
import { isIosRuntime, isMobileRuntime, isTauriRuntime } from "../../platform/runtime";
import type {
  HostedDataset,
  HostedDatasetMember,
  HostedDatasetVersion,
  HostedDatasetVersionDetail,
  HostedDownloadResult,
  HostingFormat,
  HostingJob,
  HostingQuota,
} from "../../types/hosting";

export type DatasetHostingPanelProps = {
  onClose?: () => void;
  onOpenProject?: (localProjectId: string, remoteProjectId?: string) => void;
  localProjects?: DatasetProject[];
  /** Maps a server project UUID to its local cache ID when already hydrated. */
  remoteToLocalProjectIds?: Record<string, string>;
  compact?: boolean;
};

const HOSTING_FORMATS: Array<{ value: HostingFormat; label: string }> = [
  { value: "native", label: "原生标注" },
  { value: "coco", label: "COCO" },
  { value: "yolo", label: "YOLO" },
  { value: "labelme", label: "LabelMe" },
];

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function formatBytes(value: number): string {
  if (!Number.isFinite(value) || value <= 0) return "0 B";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  const index = Math.min(Math.floor(Math.log(value) / Math.log(1024)), units.length - 1);
  return `${(value / (1024 ** index)).toFixed(index === 0 ? 0 : 1)} ${units[index]}`;
}

function formatDate(value?: string | null): string {
  if (!value) return "未记录";
  const parsed = new Date(value);
  return Number.isNaN(parsed.valueOf()) ? value : parsed.toLocaleString("zh-CN", { dateStyle: "short", timeStyle: "short" });
}

function isTerminalJob(job: HostingJob | null): boolean {
  return Boolean(job && ["completed", "failed", "cancelled"].includes(job.status));
}

function roleLabel(role: string): string {
  if (role === "owner") return "所有者";
  if (role === "manager") return "管理员";
  if (role === "viewer") return "查看者";
  return role || "成员";
}

function jobLabel(job: HostingJob): string {
  if (job.status === "completed") return "已完成";
  if (job.status === "failed") return job.error || "失败";
  if (job.status === "cancelled") return "已取消";
  return job.message || (job.status === "running" ? "处理中" : "排队中");
}

type SessionScope = {
  authenticated?: boolean;
  serverUrl?: string | null;
  userId?: string | null;
  accountId?: string | null;
};

function accountScopeOf(session: SessionScope | null | undefined): string {
  if (!session?.authenticated) return "anonymous";
  const account = session.userId || session.accountId || "unknown-account";
  return `${session.serverUrl || "unknown-server"}:${account}`;
}

function abortError(): Error {
  return new Error("托管任务已取消");
}

function sleepWithAbort(delay: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    const timer = window.setTimeout(() => {
      signal.removeEventListener("abort", onAbort);
      resolve();
    }, delay);
    const onAbort = () => {
      window.clearTimeout(timer);
      signal.removeEventListener("abort", onAbort);
      reject(abortError());
    };
    signal.addEventListener("abort", onAbort, { once: true });
    if (signal.aborted) onAbort();
  });
}

export function DatasetHostingPanel({
  onClose,
  onOpenProject,
  localProjects = [],
  remoteToLocalProjectIds = {},
  compact = false,
}: DatasetHostingPanelProps) {
  const [datasets, setDatasets] = useState<HostedDataset[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [members, setMembers] = useState<HostedDatasetMember[]>([]);
  const [versions, setVersions] = useState<HostedDatasetVersion[]>([]);
  const [versionDetail, setVersionDetail] = useState<HostedDatasetVersionDetail | null>(null);
  const [remoteProjects, setRemoteProjects] = useState<RemoteProjectSummary[]>([]);
  const [jobs, setJobs] = useState<HostingJob[]>([]);
  const [quota, setQuota] = useState<HostingQuota | null>(null);
  const [download, setDownload] = useState<HostedDownloadResult | null>(null);
  const [downloadJobId, setDownloadJobId] = useState<string | null>(null);
  const [openingExport, setOpeningExport] = useState(false);
  const [busy, setBusy] = useState(false);
  const [detailBusy, setDetailBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [newDatasetOpen, setNewDatasetOpen] = useState(false);
  const [newName, setNewName] = useState("");
  const [newDescription, setNewDescription] = useState("");
  const [editName, setEditName] = useState("");
  const [editDescription, setEditDescription] = useState("");
  const [memberUserId, setMemberUserId] = useState("");
  const [memberRole, setMemberRole] = useState<"manager" | "viewer">("viewer");
  const [captureProjectId, setCaptureProjectId] = useState("");
  const [versionName, setVersionName] = useState("");
  const [reuseVersionId, setReuseVersionId] = useState("");
  const [reuseTargetProjectId, setReuseTargetProjectId] = useState("");
  const [reuseProjectName, setReuseProjectName] = useState("");
  const [importFormat, setImportFormat] = useState<HostingFormat>("native");
  const [exportFormat, setExportFormat] = useState<HostingFormat>("native");
  const [exportVersionId, setExportVersionId] = useState("");
  const [quotaLimit, setQuotaLimit] = useState("");
  const [quotaAssets, setQuotaAssets] = useState("");
  const fileInputRef = useRef<HTMLInputElement>(null);
  const mountedRef = useRef(false);
  const lifecycleRef = useRef(0);
  const selectedIdRef = useRef<string | null>(null);
  const accountScopeRef = useRef("anonymous");
  const remoteProjectsRef = useRef<RemoteProjectSummary[]>([]);
  const listRequestRef = useRef(0);
  const detailRequestRef = useRef(0);
  const versionRequestRef = useRef(0);
  const jobsRequestRef = useRef(0);
  const pollControllersRef = useRef(new Set<AbortController>());

  const selected = useMemo(
    () => datasets.find((dataset) => dataset.id === selectedId) ?? null,
    [datasets, selectedId],
  );
  const canManage = selected ? selected.role === "owner" || selected.role === "manager" : true;
  const canArchive = selected?.role === "owner";
  const reusableRemoteProjects = useMemo(
    () => remoteProjects.filter((project) => project.role === "owner" || project.role === "manager"),
    [remoteProjects],
  );
  const quotaPercent = quota && quota.limitBytes > 0
    ? Math.min(100, Math.round(((quota.usedBytes + quota.reservedBytes) / quota.limitBytes) * 100))
    : 0;

  const canUpdate = useCallback((lifecycle: number, datasetId?: string | null) => (
    mountedRef.current
    && lifecycleRef.current === lifecycle
    && (datasetId === undefined || selectedIdRef.current === datasetId)
  ), []);

  const cancelPolls = useCallback(() => {
    pollControllersRef.current.forEach((controller) => controller.abort());
    pollControllersRef.current.clear();
  }, []);

  const clearSessionState = useCallback(() => {
    selectedIdRef.current = null;
    remoteProjectsRef.current = [];
    setDatasets([]);
    setSelectedId(null);
    setMembers([]);
    setVersions([]);
    setJobs([]);
    setVersionDetail(null);
    setDownload(null);
    setDownloadJobId(null);
    setOpeningExport(false);
    setQuota(null);
    setCaptureProjectId("");
    setReuseVersionId("");
    setExportVersionId("");
    setError(null);
  }, []);

  const loadDatasets = useCallback(async () => {
    const lifecycle = lifecycleRef.current;
    const request = ++listRequestRef.current;
    let session: SessionScope | null = null;
    try {
      session = await authStatus();
    } catch {
      // The session-change event still supplies a scope when the native
      // status command is temporarily unavailable. Keep the current scope and
      // let the authenticated reads report their own error.
    }
    if (!canUpdate(lifecycle) || request !== listRequestRef.current) return;
    if (session) {
      accountScopeRef.current = accountScopeOf(session);
      if (!session.authenticated) {
        clearSessionState();
        return;
      }
    }
    const [datasetResult, quotaResult, remoteResult] = await Promise.allSettled([
      listHostedDatasets(),
      getHostingQuota(),
      listRemoteProjects(),
    ]);
    if (!canUpdate(lifecycle) || request !== listRequestRef.current) return;
    if (datasetResult.status === "fulfilled") {
      setDatasets(datasetResult.value);
      setSelectedId((current) => {
        const next = current && datasetResult.value.some((item) => item.id === current)
          ? current
          : datasetResult.value[0]?.id ?? null;
        selectedIdRef.current = next;
        return next;
      });
    } else {
      setError(errorMessage(datasetResult.reason));
    }
    if (quotaResult.status === "fulfilled") {
      setQuota(quotaResult.value);
      setQuotaLimit(String(quotaResult.value.limitBytes));
      setQuotaAssets(String(quotaResult.value.maxAssets));
    }
    if (remoteResult.status === "fulfilled") {
      remoteProjectsRef.current = remoteResult.value;
      setRemoteProjects(remoteResult.value);
      setReuseTargetProjectId((current) => remoteResult.value.some((item) => (
        item.id === current && (item.role === "owner" || item.role === "manager")
      )) ? current : "");
    }
  }, [canUpdate, clearSessionState]);

  const loadSelected = useCallback(async (datasetId: string | null) => {
    const lifecycle = lifecycleRef.current;
    const request = ++detailRequestRef.current;
    const jobsRequest = ++jobsRequestRef.current;
    const versionRequest = versionRequestRef.current;
    if (!datasetId) {
      if (!canUpdate(lifecycle)) return;
      setMembers([]);
      setVersions([]);
      setJobs([]);
      if (versionRequest === versionRequestRef.current) setVersionDetail(null);
      setDetailBusy(false);
      return;
    }
    setDetailBusy(true);
    setError(null);
    try {
      const [datasetResult, membersResult, versionsResult, jobsResult] = await Promise.all([
        getHostedDataset(datasetId),
        listHostedMembers(datasetId),
        listHostedVersions(datasetId),
        listHostingJobs(datasetId),
      ]);
      if (!canUpdate(lifecycle, datasetId)
        || request !== detailRequestRef.current
        || jobsRequest !== jobsRequestRef.current) return;
      setDatasets((current) => [datasetResult, ...current.filter((item) => item.id !== datasetResult.id)]);
      setEditName(datasetResult.name);
      setEditDescription(datasetResult.description);
      setMembers(membersResult);
      setVersions(versionsResult);
      setJobs(jobsResult);
      if (versionRequest === versionRequestRef.current) setVersionDetail(null);
      setExportVersionId((current) => versionsResult.some((item) => item.id === current)
        ? current
        : versionsResult[0]?.id || "");
      setReuseVersionId((current) => versionsResult.some((item) => item.id === current)
        ? current
        : versionsResult[0]?.id || "");
      setCaptureProjectId((current) => remoteProjectsRef.current.some((item) => item.id === current)
        ? current
        : remoteProjectsRef.current[0]?.id || "");
    } catch (cause) {
      if (canUpdate(lifecycle, datasetId)
        && request === detailRequestRef.current
        && jobsRequest === jobsRequestRef.current) setError(errorMessage(cause));
    } finally {
      if (canUpdate(lifecycle, datasetId) && request === detailRequestRef.current) setDetailBusy(false);
    }
  }, [canUpdate]);

  /**
   * Mutation responses can lag behind the list/detail projections (and quota
   * is maintained by a separate server transaction). Refresh both projections
   * under the same lifecycle guard before allowing another write from this
   * panel. This keeps the next baseRevision and version metadata current.
   */
  const refreshSelectedState = useCallback(async (lifecycle: number, datasetId: string) => {
    await Promise.all([
      loadSelected(datasetId),
      (async () => {
        try {
          const nextQuota = await getHostingQuota();
          if (!canUpdate(lifecycle, datasetId)) return;
          setQuota(nextQuota);
          setQuotaLimit(String(nextQuota.limitBytes));
          setQuotaAssets(String(nextQuota.maxAssets));
        } catch (cause) {
          if (canUpdate(lifecycle, datasetId)) setError(errorMessage(cause));
        }
      })(),
    ]);
  }, [canUpdate, loadSelected]);

  useEffect(() => {
    mountedRef.current = true;
    void loadDatasets();
    const handleSessionChange = (event: Event) => {
      const detail = (event as CustomEvent<SessionScope>).detail;
      lifecycleRef.current += 1;
      listRequestRef.current += 1;
      detailRequestRef.current += 1;
      versionRequestRef.current += 1;
      jobsRequestRef.current += 1;
      cancelPolls();
      accountScopeRef.current = accountScopeOf(detail);
      clearSessionState();
      setBusy(false);
      setDetailBusy(false);
      if (detail?.authenticated) void loadDatasets();
    };
    window.addEventListener(SESSION_CHANGE_EVENT, handleSessionChange);
    return () => {
      mountedRef.current = false;
      lifecycleRef.current += 1;
      listRequestRef.current += 1;
      detailRequestRef.current += 1;
      versionRequestRef.current += 1;
      jobsRequestRef.current += 1;
      cancelPolls();
      window.removeEventListener(SESSION_CHANGE_EVENT, handleSessionChange);
    };
  }, [cancelPolls, clearSessionState, loadDatasets]);

  useEffect(() => {
    selectedIdRef.current = selectedId;
    cancelPolls();
    setBusy(false);
    setDetailBusy(false);
    versionRequestRef.current += 1;
    jobsRequestRef.current += 1;
    setVersionDetail(null);
    setExportVersionId("");
    setReuseVersionId("");
    setDownload(null);
    setDownloadJobId(null);
    setOpeningExport(false);
    void loadSelected(selectedId);
  }, [cancelPolls, loadSelected, selectedId]);

  function beginOperation(datasetId: string, action: string, payload: unknown) {
    return beginStableHostingOperation({
      accountScope: accountScopeRef.current,
      datasetId,
      action,
      payload,
    });
  }

  function finishKnownOperation(handle: ReturnType<typeof beginOperation>, cause?: unknown) {
    if (!cause || isHostingConflict(cause)) completeStableHostingOperation(handle);
  }

  function showError(cause: unknown) {
    setMessage(null);
    setError(errorMessage(cause));
  }

  async function createDataset() {
    if (!newName.trim()) {
      setError("请输入数据集名称");
      return;
    }
    const lifecycle = lifecycleRef.current;
    setBusy(true);
    setError(null);
    try {
      const created = await createHostedDataset({ name: newName, description: newDescription });
      if (!canUpdate(lifecycle)) return;
      setDatasets((current) => [created, ...current.filter((item) => item.id !== created.id)]);
      setSelectedId(created.id);
      setNewName("");
      setNewDescription("");
      setNewDatasetOpen(false);
      setMessage("数据集已创建");
    } catch (cause) {
      if (canUpdate(lifecycle)) showError(cause);
    } finally {
      if (canUpdate(lifecycle)) setBusy(false);
    }
  }

  async function saveDataset() {
    if (!selected) return;
    const lifecycle = lifecycleRef.current;
    const datasetId = selected.id;
    setBusy(true);
    setError(null);
    try {
      const updated = await updateHostedDataset(selected.id, {
        name: editName,
        description: editDescription,
        baseRevision: selected.revision,
      });
      if (!canUpdate(lifecycle, datasetId)) return;
      setDatasets((current) => current.map((item) => item.id === updated.id ? updated : item));
      setMessage("数据集信息已保存");
    } catch (cause) {
      if (canUpdate(lifecycle, datasetId)) showError(cause);
    } finally {
      if (canUpdate(lifecycle, datasetId)) setBusy(false);
    }
  }

  async function archiveDataset() {
    if (!selected) return;
    const lifecycle = lifecycleRef.current;
    const datasetId = selected.id;
    setBusy(true);
    setError(null);
    try {
      await archiveHostedDataset(selected.id);
      if (!canUpdate(lifecycle, datasetId)) return;
      const remaining = datasets.filter((item) => item.id !== selected.id);
      setDatasets(remaining);
      setSelectedId(remaining[0]?.id ?? null);
      setMessage("数据集已归档");
    } catch (cause) {
      if (canUpdate(lifecycle, datasetId)) showError(cause);
    } finally {
      if (canUpdate(lifecycle, datasetId)) setBusy(false);
    }
  }

  async function addMember() {
    if (!selected || !memberUserId.trim()) {
      setError("请输入成员邮箱或用户 ID");
      return;
    }
    const lifecycle = lifecycleRef.current;
    const datasetId = selected.id;
    setBusy(true);
    setError(null);
    try {
      const member = await addHostedMember(selected.id, memberUserId, memberRole);
      if (!canUpdate(lifecycle, datasetId)) return;
      setMembers((current) => [...current.filter((item) => item.id !== member.id), member]);
      setMemberUserId("");
      setMessage("成员权限已更新");
    } catch (cause) {
      if (canUpdate(lifecycle, datasetId)) showError(cause);
    } finally {
      if (canUpdate(lifecycle, datasetId)) setBusy(false);
    }
  }

  async function removeMember(member: HostedDatasetMember) {
    if (!selected) return;
    const lifecycle = lifecycleRef.current;
    const datasetId = selected.id;
    setBusy(true);
    try {
      await removeHostedMember(selected.id, member.id);
      if (!canUpdate(lifecycle, datasetId)) return;
      setMembers((current) => current.filter((item) => item.id !== member.id));
      setMessage("成员已移除");
    } catch (cause) {
      if (canUpdate(lifecycle, datasetId)) showError(cause);
    } finally {
      if (canUpdate(lifecycle, datasetId)) setBusy(false);
    }
  }

  async function captureProject() {
    if (!selected || !captureProjectId) {
      setError("请选择已发布的远端项目");
      return;
    }
    const lifecycle = lifecycleRef.current;
    const datasetId = selected.id;
    const projectId = captureProjectId;
    const operation = beginOperation(datasetId, "capture", { projectId });
    setBusy(true);
    setError(null);
    try {
      const updated = await captureHostedProject(selected.id, {
        projectId: captureProjectId,
        operationId: operation.operationId,
      });
      finishKnownOperation(operation);
      if (!canUpdate(lifecycle, datasetId)) return;
      setDatasets((current) => current.map((item) => item.id === updated.id ? updated : item));
      await refreshSelectedState(lifecycle, datasetId);
      if (!canUpdate(lifecycle, datasetId)) return;
      setMessage("远端项目已收录");
    } catch (cause) {
      finishKnownOperation(operation, cause);
      if (canUpdate(lifecycle, datasetId)) showError(cause);
    } finally {
      if (canUpdate(lifecycle, datasetId)) setBusy(false);
    }
  }

  async function createVersion() {
    if (!selected || !versionName.trim()) {
      setError("请输入版本名称");
      return;
    }
    const lifecycle = lifecycleRef.current;
    const datasetId = selected.id;
    const name = versionName.trim();
    const operation = beginOperation(datasetId, "version", {
      name,
      baseRevision: selected.revision,
    });
    setBusy(true);
    setError(null);
    try {
      const created = await createHostedVersion(selected.id, {
        name,
        operationId: operation.operationId,
        baseRevision: selected.revision,
      });
      finishKnownOperation(operation);
      if (!canUpdate(lifecycle, datasetId)) return;
      setVersions((current) => [created, ...current.filter((item) => item.id !== created.id)]);
      await refreshSelectedState(lifecycle, datasetId);
      if (!canUpdate(lifecycle, datasetId)) return;
      setVersionName("");
      setExportVersionId(created.id);
      setReuseVersionId(created.id);
      setMessage("不可变版本已创建");
    } catch (cause) {
      finishKnownOperation(operation, cause);
      if (canUpdate(lifecycle, datasetId)) showError(cause);
    } finally {
      if (canUpdate(lifecycle, datasetId)) setBusy(false);
    }
  }

  async function viewVersion(version: HostedDatasetVersion) {
    if (!selected) return;
    const lifecycle = lifecycleRef.current;
    const datasetId = selected.id;
    const request = ++versionRequestRef.current;
    setDetailBusy(true);
    try {
      const detail = await getHostedVersion(datasetId, version.id);
      if (!canUpdate(lifecycle, datasetId) || request !== versionRequestRef.current) return;
      setVersionDetail(detail);
    } catch (cause) {
      if (canUpdate(lifecycle, datasetId) && request === versionRequestRef.current) showError(cause);
    } finally {
      if (canUpdate(lifecycle, datasetId) && request === versionRequestRef.current) setDetailBusy(false);
    }
  }

  async function reuseVersion() {
    if (!selected || !reuseVersionId) {
      setError("请选择要复用的版本");
      return;
    }
    const lifecycle = lifecycleRef.current;
    const datasetId = selected.id;
    const versionId = reuseVersionId;
    const targetProjectId = reuseTargetProjectId.trim() || undefined;
    const projectName = reuseProjectName.trim() || undefined;
    const operation = beginOperation(datasetId, "reuse", {
      versionId,
      targetProjectId,
      projectName,
    });
    setBusy(true);
    setError(null);
    try {
      const result = await reuseHostedVersion(selected.id, {
        versionId,
        targetProjectId,
        projectName,
        operationId: operation.operationId,
      });
      if (!canUpdate(lifecycle, datasetId)) return;
      let localProjectId = result.projectId;
      if (result.projectId) {
        const opened = await remoteProjectOpen(result.projectId);
        if (!canUpdate(lifecycle, datasetId)) return;
        localProjectId = opened.localProjectId ?? opened.projectId;
      }
      finishKnownOperation(operation);
      await refreshSelectedState(lifecycle, datasetId);
      if (!canUpdate(lifecycle, datasetId)) return;
      setMessage(`版本已复用，重用 ${result.reusedAssets} 个资源`);
      if (localProjectId && onOpenProject) onOpenProject(localProjectId, result.projectId || undefined);
    } catch (cause) {
      finishKnownOperation(operation, cause);
      if (canUpdate(lifecycle, datasetId)) showError(cause);
    } finally {
      if (canUpdate(lifecycle, datasetId)) setBusy(false);
    }
  }

  async function pollJob(job: HostingJob, datasetId: string, lifecycle: number): Promise<HostingJob> {
    const controller = new AbortController();
    const jobsRequest = ++jobsRequestRef.current;
    pollControllersRef.current.add(controller);
    let current = job;
    try {
      for (let attempt = 0; attempt < 60 && !isTerminalJob(current); attempt += 1) {
        await sleepWithAbort(500, controller.signal);
        if (!canUpdate(lifecycle, datasetId) || jobsRequest !== jobsRequestRef.current) throw abortError();
        current = await getHostingJob(job.id);
        if (!canUpdate(lifecycle, datasetId) || jobsRequest !== jobsRequestRef.current) throw abortError();
        setJobs((items) => [current, ...items.filter((item) => item.id !== current.id)]);
      }
      return current;
    } finally {
      pollControllersRef.current.delete(controller);
    }
  }

  function isJobCancelled(cause: unknown): boolean {
    return cause instanceof Error && cause.message === "托管任务已取消";
  }

  async function importArchive(file: File) {
    if (!selected) return;
    const lifecycle = lifecycleRef.current;
    const datasetId = selected.id;
    let operation: ReturnType<typeof beginOperation> | null = null;
    setBusy(true);
    setError(null);
    try {
      const base64 = await readHostingArchive(file);
      if (!canUpdate(lifecycle, datasetId)) return;
      operation = beginOperation(datasetId, "import", {
        format: importFormat,
        fileName: file.name,
        byteSize: file.size,
        lastModified: file.lastModified,
      });
      const job = await uploadHostedArchive({
        datasetId,
        format: importFormat,
        fileName: file.name,
        base64,
        operationId: operation.operationId,
      });
      if (!canUpdate(lifecycle, datasetId)) return;
      setJobs((current) => [job, ...current.filter((item) => item.id !== job.id)]);
      const finished = await pollJob(job, datasetId, lifecycle);
      if (finished.status === "failed") {
        // A worker failure is not confirmation that the write was applied.
        // Keep the stable operation ID so an explicit retry cannot create a
        // second import intent; only success or a known conflict clears it.
        const failure = new Error(finished.error || "导入任务失败");
        finishKnownOperation(operation, failure);
        throw failure;
      }
      finishKnownOperation(operation);
      if (!canUpdate(lifecycle, datasetId)) return;
      await loadDatasets();
      await refreshSelectedState(lifecycle, datasetId);
      if (!canUpdate(lifecycle, datasetId)) return;
      setMessage(`导入任务${jobLabel(finished)}`);
    } catch (cause) {
      if (operation) finishKnownOperation(operation, cause);
      if (canUpdate(lifecycle, datasetId) && !isJobCancelled(cause)) showError(cause);
    } finally {
      if (canUpdate(lifecycle, datasetId)) {
        setBusy(false);
        if (fileInputRef.current) fileInputRef.current.value = "";
      }
    }
  }

  async function exportDataset() {
    if (!selected || !exportVersionId) {
      setError("请选择要导出的版本");
      return;
    }
    const lifecycle = lifecycleRef.current;
    const datasetId = selected.id;
    const versionId = exportVersionId;
    const format = exportFormat;
    const operation = beginOperation(datasetId, "export", { versionId, format });
    setBusy(true);
    setError(null);
    setDownload(null);
    setDownloadJobId(null);
    try {
      const job = await startHostedExport(selected.id, {
        versionId,
        format,
        operationId: operation.operationId,
      });
      if (!canUpdate(lifecycle, datasetId)) return;
      setJobs((current) => [job, ...current.filter((item) => item.id !== job.id)]);
      const finished = await pollJob(job, datasetId, lifecycle);
      if (finished.status === "failed") {
        const failure = new Error(finished.error || "导出任务失败");
        finishKnownOperation(operation, failure);
        throw failure;
      }
      const result = await downloadHostedExport(finished.id);
      finishKnownOperation(operation);
      if (!canUpdate(lifecycle, datasetId)) return;
      setDownload(result);
      setDownloadJobId(finished.id);
      setMessage(`导出完成：${result.fileName}`);
    } catch (cause) {
      finishKnownOperation(operation, cause);
      if (canUpdate(lifecycle, datasetId) && !isJobCancelled(cause)) showError(cause);
    } finally {
      if (canUpdate(lifecycle, datasetId)) setBusy(false);
    }
  }

  async function openExport() {
    if (!selected || !download || !downloadJobId) return;
    const lifecycle = lifecycleRef.current;
    const datasetId = selected.id;
    setOpeningExport(true);
    setError(null);
    try {
      await openHostedExport(downloadJobId);
      if (!canUpdate(lifecycle, datasetId)) return;
      setMessage(`已打开导出文件：${download.fileName}`);
    } catch (cause) {
      if (canUpdate(lifecycle, datasetId)) showError(cause);
    } finally {
      if (canUpdate(lifecycle, datasetId)) setOpeningExport(false);
    }
  }

  async function refreshJobs() {
    if (!selected) return;
    const lifecycle = lifecycleRef.current;
    const datasetId = selected.id;
    const request = ++jobsRequestRef.current;
    try {
      const nextJobs = await listHostingJobs(datasetId);
      if (!canUpdate(lifecycle, datasetId) || request !== jobsRequestRef.current) return;
      setJobs(nextJobs);
    } catch (cause) {
      if (canUpdate(lifecycle, datasetId) && request === jobsRequestRef.current) showError(cause);
    }
  }

  async function saveQuota() {
    const limitBytes = Number(quotaLimit);
    const maxAssets = Number(quotaAssets);
    if (!Number.isFinite(limitBytes) || limitBytes < 0 || !Number.isFinite(maxAssets) || maxAssets < 0) {
      setError("请输入有效的配额数值");
      return;
    }
    const lifecycle = lifecycleRef.current;
    setBusy(true);
    try {
      const nextQuota = await updateHostingQuota({ limitBytes, maxAssets });
      if (!canUpdate(lifecycle)) return;
      setQuota(nextQuota);
      setMessage("存储配额已保存");
    } catch (cause) {
      if (canUpdate(lifecycle)) showError(cause);
    } finally {
      if (canUpdate(lifecycle)) setBusy(false);
    }
  }

  return (
    <section className={`hosting-panel${compact ? " hosting-panel--compact" : ""}`} aria-label="数据集托管">
      <div className="hosting-panel__header">
        <div>
          <span className="hosting-eyebrow"><Database size={14} />云端数据集</span>
          <h2>数据集托管</h2>
          <p>集中管理可复用数据资产、版本和成员权限。</p>
        </div>
        <div className="hosting-header-actions">
          <button type="button" aria-label="刷新数据集托管" onClick={() => void loadDatasets()} disabled={busy || detailBusy}><RefreshCw size={15} />刷新</button>
          {onClose ? <button type="button" aria-label="关闭数据集托管" onClick={onClose}><X size={17} /></button> : null}
        </div>
      </div>

      {error ? <div className="hosting-alert hosting-alert--error" role="alert"><CircleAlert size={15} />{error}<button type="button" aria-label="关闭托管错误" onClick={() => setError(null)}><X size={14} /></button></div> : null}
      {message ? <div className="hosting-alert hosting-alert--success" role="status"><Check size={15} />{message}</div> : null}

      <div className="hosting-layout">
        <aside className="hosting-dataset-list">
          <div className="hosting-subheading"><strong>我的数据集</strong><span>{datasets.length} 个</span></div>
          {datasets.length === 0 ? <div className="hosting-empty">还没有云端数据集</div> : datasets.map((dataset) => (
            <button
              className={`hosting-dataset-item${dataset.id === selectedId ? " is-active" : ""}`}
              key={dataset.id}
              type="button"
              onClick={() => setSelectedId(dataset.id)}
            >
              <span><strong>{dataset.name}</strong><small>{dataset.itemCount} 项 · {formatBytes(dataset.totalBytes)}</small></span>
              <ChevronRight size={15} />
            </button>
          ))}
          <button className="hosting-create-toggle" type="button" onClick={() => setNewDatasetOpen((value) => !value)}><Plus size={15} />新建数据集</button>
          {newDatasetOpen ? (
            <div className="hosting-form hosting-form--nested">
              <label>名称<input value={newName} onChange={(event) => setNewName(event.target.value)} placeholder="例如：道路目标检测" /></label>
              <label>描述<textarea value={newDescription} onChange={(event) => setNewDescription(event.target.value)} rows={2} placeholder="用途和标注规范" /></label>
              <button className="hosting-primary" type="button" disabled={busy} onClick={() => void createDataset()}><Plus size={15} />创建</button>
            </div>
          ) : null}
        </aside>

        <div className="hosting-detail">
          {!selected ? <div className="hosting-empty hosting-empty--large"><Database size={22} /><span>选择一个数据集，或先新建一个。</span></div> : (
            <>
              <div className="hosting-detail-header">
                <div><span className="hosting-eyebrow">{roleLabel(selected.role)} · {selected.status === "archived" ? "已归档" : "启用"}</span><h3>{selected.name}</h3><p>{selected.description || "暂无描述"}</p></div>
                <div className="hosting-detail-actions">{canArchive ? <button type="button" disabled={busy} onClick={() => void archiveDataset()}><Archive size={15} />归档</button> : null}</div>
              </div>

              <div className="hosting-edit-grid">
                <label>名称<input value={editName} onChange={(event) => setEditName(event.target.value)} disabled={!canManage || busy} /></label>
                <label>描述<input value={editDescription} onChange={(event) => setEditDescription(event.target.value)} disabled={!canManage || busy} /></label>
                <button type="button" disabled={!canManage || busy} onClick={() => void saveDataset()}><Save size={15} />保存信息</button>
              </div>

              <div className="hosting-metric-grid">
                <div><span>条目</span><strong>{selected.itemCount.toLocaleString("zh-CN")}</strong></div>
                <div><span>占用空间</span><strong>{formatBytes(selected.totalBytes)}</strong></div>
                <div><span>版本</span><strong>{versions.length}</strong></div>
                <div><span>修订</span><strong>#{selected.revision}</strong></div>
              </div>

              <div className="hosting-section">
                <div className="hosting-section-heading"><div><Users size={16} /><strong>成员权限</strong><span>{members.length} 人</span></div></div>
                {canManage ? <div className="hosting-member-form"><input aria-label="成员邮箱或用户 ID" value={memberUserId} onChange={(event) => setMemberUserId(event.target.value)} placeholder="成员邮箱或用户 ID" /><select aria-label="成员角色" value={memberRole} onChange={(event) => setMemberRole(event.target.value as "manager" | "viewer")}><option value="manager">管理员</option><option value="viewer">查看者</option></select><button type="button" disabled={busy || !memberUserId.trim()} onClick={() => void addMember()}><UserPlus size={15} />添加</button></div> : null}
                <div className="hosting-member-list">{members.length === 0 ? <small className="hosting-muted">还没有额外成员</small> : members.map((member) => <div className="hosting-member-row" key={member.id}><div><strong>{member.displayName || member.email || member.userId}</strong><small>{member.email || member.userId} · {roleLabel(member.role)}</small></div>{canManage && member.role !== "owner" ? <button type="button" disabled={busy} onClick={() => void removeMember(member)}>移除</button> : null}</div>)}</div>
              </div>

              <div className="hosting-section">
                <div className="hosting-section-heading"><div><Upload size={16} /><strong>收录远端项目</strong></div></div>
                <div className="hosting-capture-form"><select aria-label="已发布远端项目" value={captureProjectId} onChange={(event) => setCaptureProjectId(event.target.value)}><option value="">选择已发布项目</option>{remoteProjects.map((project) => <option key={project.id} value={project.id}>{project.name}</option>)}</select><button type="button" disabled={!canManage || busy || !captureProjectId} onClick={() => void captureProject()}>收录项目</button></div>
                <small className="hosting-muted">仅收录服务器上已发布的项目，重复操作使用 operationId 幂等。</small>
              </div>

              <div className="hosting-section">
                <div className="hosting-section-heading"><div><FileArchive size={16} /><strong>导入压缩包</strong></div></div>
                <div className="hosting-capture-form hosting-import-row"><select aria-label="导入格式" value={importFormat} onChange={(event) => setImportFormat(event.target.value as HostingFormat)}>{HOSTING_FORMATS.map((format) => <option key={format.value} value={format.value}>{format.label}</option>)}</select><input ref={fileInputRef} type="file" accept=".zip,application/zip" aria-label="选择数据集压缩包" onChange={(event) => { const file = event.target.files?.[0]; if (file) void importArchive(file); }} disabled={!canManage || busy} /><small>最大 64 MiB</small></div>
              </div>

              <div className="hosting-section">
                <div className="hosting-section-heading"><div><Check size={16} /><strong>不可变版本</strong><span>{versions.length} 个</span></div></div>
                {canManage ? <div className="hosting-version-form"><input aria-label="新版本名称" value={versionName} onChange={(event) => setVersionName(event.target.value)} placeholder="版本名称" /><button type="button" disabled={busy || !versionName.trim()} onClick={() => void createVersion()}><Plus size={15} />创建版本</button></div> : null}
                {versions.length === 0 ? <small className="hosting-muted">收录数据后创建第一个版本。</small> : <div className="hosting-version-list">{versions.map((version) => <button className="hosting-version-row" key={version.id} type="button" onClick={() => void viewVersion(version)}><span><strong>{version.name}</strong><small>{version.itemCount} 项 · {formatDate(version.createdAt)}</small></span><ChevronRight size={15} /></button>)}</div>}
                {versionDetail ? <details className="hosting-version-detail" open><summary>版本内容：{versionDetail.name}</summary><pre>{JSON.stringify(versionDetail.manifest ?? {}, null, 2)}</pre></details> : null}
              </div>

              <div className="hosting-section">
                <div className="hosting-section-heading"><div><Database size={16} /><strong>复用版本到项目</strong></div></div>
                <div className="hosting-reuse-grid"><select aria-label="复用版本" value={reuseVersionId} onChange={(event) => setReuseVersionId(event.target.value)}><option value="">选择版本</option>{versions.map((version) => <option key={version.id} value={version.id}>{version.name}</option>)}</select><select aria-label="目标项目" value={reuseTargetProjectId} onChange={(event) => setReuseTargetProjectId(event.target.value)}><option value="">新建项目</option>{reusableRemoteProjects.map((project) => { const localId = remoteToLocalProjectIds[project.id]; const localProject = localId ? localProjects.find((item) => item.id === localId) : undefined; return <option key={project.id} value={project.id}>{localProject?.name || project.name}{localId ? " · 已缓存" : " · 远端"}</option>; })}</select>{!reuseTargetProjectId ? <input aria-label="新项目名称" value={reuseProjectName} onChange={(event) => setReuseProjectName(event.target.value)} placeholder="新项目名称" /> : null}<button type="button" disabled={busy || !reuseVersionId || (!reuseTargetProjectId && !reuseProjectName.trim())} onClick={() => void reuseVersion()}><ChevronRight size={15} />复用并打开</button></div>
              </div>

              <div className="hosting-section">
                <div className="hosting-section-heading"><div><Download size={16} /><strong>导出下载</strong></div></div>
                <div className="hosting-capture-form"><select aria-label="导出版本" value={exportVersionId} onChange={(event) => setExportVersionId(event.target.value)}><option value="">选择版本</option>{versions.map((version) => <option key={version.id} value={version.id}>{version.name}</option>)}</select><select aria-label="导出格式" value={exportFormat} onChange={(event) => setExportFormat(event.target.value as HostingFormat)}>{HOSTING_FORMATS.map((format) => <option key={format.value} value={format.value}>{format.label}</option>)}</select><button type="button" disabled={busy || !exportVersionId} onClick={() => void exportDataset()}><Download size={15} />生成导出</button></div>
                {download ? <div className="hosting-download-result">
                  <div className="hosting-download-result__meta"><strong><Download size={15} />{download.fileName}</strong><small>{isMobileRuntime() ? "已保存到设备导出目录" : `保存到 ${download.path}`} · {formatBytes(download.byteSize)} · SHA-256 {download.sha256.slice(0, 12)}…</small></div>
                  {isTauriRuntime() && !isIosRuntime() ? <button type="button" onClick={() => void openExport()} disabled={openingExport || !downloadJobId}>{openingExport ? "正在打开…" : "查看/保存导出文件"}</button> : isIosRuntime() ? <small className="hosting-download-result__ios-hint">已保存到 文件App→我的iPhone/iPad→Image Annotation→Dataset Exports</small> : <small className="hosting-download-result__ios-hint">文件已保存到本机路径，请在客户端打开。</small>}
                </div> : null}
              </div>

              {jobs.length ? <div className="hosting-section"><div className="hosting-section-heading"><div><LoaderCircle size={16} /><strong>任务进度</strong></div><button type="button" onClick={() => void refreshJobs()} disabled={busy}><RefreshCw size={14} />刷新</button></div><div className="hosting-job-list">{jobs.slice(0, 6).map((job) => <div className="hosting-job-row" key={job.id}><div><strong>{job.kind === "export" ? "导出" : "导入"} · {job.format || "数据集"}</strong><small>{jobLabel(job)}</small></div><progress max={100} value={job.progress} aria-label={`${job.id} 进度`} /><span>{job.progress}%</span></div>)}</div></div> : null}
            </>
          )}
        </div>
      </div>

      {quota ? <div className="hosting-quota"><div className="hosting-section-heading"><div><HardDrive size={16} /><strong>组织存储配额</strong><span>{quota.canManage ? "可管理" : "只读"}</span></div>{quota.canManage ? <button type="button" disabled={busy} onClick={() => void saveQuota()}><Save size={14} />保存配额</button> : null}</div><div className="hosting-quota-bar"><span style={{ width: `${quotaPercent}%` }} /></div><div className="hosting-quota-meta"><span>已用 {formatBytes(quota.usedBytes + quota.reservedBytes)} / {formatBytes(quota.limitBytes)}</span><span>{quota.usedAssets.toLocaleString("zh-CN")} / {quota.maxAssets.toLocaleString("zh-CN")} 项</span></div>{quota.canManage ? <div className="hosting-quota-form"><label>容量（字节）<input value={quotaLimit} onChange={(event) => setQuotaLimit(event.target.value)} inputMode="numeric" /></label><label>最大资源数<input value={quotaAssets} onChange={(event) => setQuotaAssets(event.target.value)} inputMode="numeric" /></label></div> : null}</div> : null}
    </section>
  );
}

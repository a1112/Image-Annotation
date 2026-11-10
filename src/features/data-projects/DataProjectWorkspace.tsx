import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ArrowLeft, ArrowUpRight, FolderKanban, Archive, Check, Cloud, CloudOff, GitBranch, GitCommitHorizontal, GitMerge, History, Plus, RefreshCw, RotateCcw, Search, Server, Settings, UploadCloud, Users, X } from "lucide-react";
import { assignDatasetToDataProject, connectRemoteDataProject, createDataProject, createRemoteDataProject, deleteDataProjectBranch, discoverDataProjectRemotes, getDataProjectDatasetGroups, getDataProjectSyncStatus, getDataProjectWorkingTree, listDataProjectBranches, listDataProjectCloudMembers, listDataProjectCommits, listDataProjectDatasets, listDataProjects, mergeDataProjectBranches, previewDataProjectMerge, publishDataProject, removeDataProjectCloudMember, resolveDataProjectMerge, revertDataProject, setDataProjectDatasetGroups, switchDataProjectBranch, syncDataProject, upsertDataProjectCloudMember, cloneDataProject, commitDataProject, createDataProjectBranch } from "../../api/dataProjects";
import { listDatasetProjects, listProjectImages } from "../../api/tauri";
import type { DatasetImage, DatasetProject } from "../../types/domain";
import type { DataProject, DataProjectBranch, DataProjectChange, DataProjectCloudMember, DataProjectCommit, DataProjectCounts, DataProjectGroupState, DataProjectMergeConflict, DataProjectSection, DataProjectSyncStatus, DataProjectWorkingTree, DatasetGroup, RemoteDataProject } from "../../types/dataProjects";
import { DatasetGroupingBoard, canonicalDatasetKey } from "./DatasetGroupingBoard";
import { DatasetCard } from "../datasets/DatasetCard";

export type DataProjectWorkspaceProps = {
  dataProjectId: string;
  section: DataProjectSection;
  project?: DataProject | null;
  projects?: DatasetProject[];
  projectImages?: Record<string, DatasetImage[]>;
  onNavigateProject: (id: string, section?: DataProjectSection) => void;
  onNavigateHome: () => void;
  onCreateDataset?: (dataProjectId?: string) => void;
  onUploadImages?: (dataProjectId: string, datasetId?: string) => void;
  onOpen: (datasetId: string) => void;
  onAnnotate: (dataset: DatasetProject) => void;
  onOpenWindow?: (dataset: DatasetProject) => void;
  onOpenUnassigned?: () => void;
  onDatasetsChanged?: () => void | Promise<void>;
  onExportDataProject?: (dataProjectId: string) => void | Promise<void>;
  exportingDataProjectId?: string | null;
};

export type DataProjectOverviewProps = {
  projects?: DatasetProject[];
  projectImages?: Record<string, DatasetImage[]>;
  onNavigateProject: (id: string, section?: DataProjectSection) => void;
  onCreateDataset?: (dataProjectId?: string) => void;
  onUploadImages?: (dataProjectId: string, datasetId?: string) => void;
  onOpen: (datasetId: string) => void;
  onAnnotate: (dataset: DatasetProject) => void;
  onOpenWindow?: (dataset: DatasetProject) => void;
  onOpenUnassigned?: () => void;
};

function errorMessage(cause: unknown): string {
  const message = cause instanceof Error ? cause.message : String(cause);
  return /401|403|permission|forbidden|权限/i.test(message) ? `权限不足：${message}` : message;
}

function isRevisionConflict(cause: unknown): boolean {
  const message = cause instanceof Error ? cause.message : String(cause);
  return /409|conflict|revision|worktree[_ -]?revision|changed while|已被其他操作更新/i.test(message);
}

function formatDate(value: string | undefined): string {
  if (!value) return "—";
  const date = new Date(value);
  return Number.isNaN(date.valueOf()) ? value : date.toLocaleString("zh-CN", { dateStyle: "short", timeStyle: "short" });
}

function countLabel(value: number | undefined): string {
  return value === undefined ? "—" : value.toLocaleString("zh-CN");
}

function loadDatasetPreview(datasetId: string): Promise<DatasetImage[]> {
  return listProjectImages(datasetId, undefined, { limit: 1 });
}

function normalizeCounts(tree: DataProjectWorkingTree | null): DataProjectCounts | null {
  if (!tree?.counts) return null;
  return tree.counts;
}

function changeLabel(change: DataProjectChange): string {
  const kind = change.kind.toLowerCase();
  if (kind.includes("dataset") && kind.includes("add")) return "加入数据集";
  if (kind.includes("dataset") && (kind.includes("remove") || kind.includes("delete"))) return "移出数据集";
  if (kind.includes("annotation") || kind.includes("label")) return "标签变更";
  if (kind.includes("review") || kind.includes("qa")) return "审核状态变更";
  if (kind.includes("image")) return "图像变更";
  return change.kind || "数据变更";
}

function readableConflict(value: string): string {
  return value.replace(/[\u0000-\u001f\u007f-\u009f]+/g, " / ").replace(/\b[a-f0-9]{16,}\b/gi, (hash) => `版本 ${hash.slice(0, 10)}`).trim();
}

function conflictKey(conflict: DataProjectMergeConflict): string {
  return conflict.key;
}

function conflictValue(value: unknown, description?: string | null): string {
  if (description?.trim()) return readableConflict(description);
  if (!value) return "删除/不存在";
  if (typeof value === "object") {
    const record = value as Record<string, unknown>;
    const status = typeof record.status === "string" ? record.status : typeof record.qaStatus === "string" ? record.qaStatus : "已记录";
    return readableConflict(status);
  }
  return readableConflict(String(value));
}

type DatasetAssignmentDrawerProps = {
  datasets: DatasetProject[];
  projectImages: Record<string, DatasetImage[]>;
  loading: boolean;
  error: string | null;
  canAssign?: boolean;
  assigningDatasetId?: string | null;
  onClose: () => void;
  onRetry: () => void;
  onOpen: (datasetId: string) => void;
  onAnnotate: (dataset: DatasetProject) => void;
  onOpenWindow?: (dataset: DatasetProject) => void;
  loadPreviewImages?: (datasetId: string) => Promise<DatasetImage[]>;
  onAssign?: (dataset: DatasetProject) => void;
};

function DatasetAssignmentDrawer({
  datasets,
  projectImages,
  loading,
  error,
  canAssign = false,
  assigningDatasetId,
  onClose,
  onRetry,
  onOpen,
  onAnnotate,
  onOpenWindow,
  loadPreviewImages,
  onAssign,
}: DatasetAssignmentDrawerProps) {
  return <aside className="data-projects-dataset-drawer" role="dialog" aria-modal="true" aria-label="待归属数据集">
    <div className="data-projects-dataset-drawer-header"><div><strong>待归属数据集</strong><span>{loading ? "读取中…" : `${datasets.length} 个`}</span></div><button type="button" aria-label="关闭待归属数据集" onClick={onClose}><X size={16} /></button></div>
    {error ? <div className="data-projects-feature-error" role="alert">{error}<button type="button" onClick={onRetry}>重试</button></div> : null}
    {loading ? <div className="data-projects-drawer-empty">正在读取已有数据集…</div> : datasets.length ? <div className="data-projects-drawer-list">{datasets.map((dataset) => <div className="data-projects-drawer-item" key={dataset.id}><DatasetCard compact dataset={dataset} previewImages={projectImages[dataset.id] ?? []} loadPreviewImages={loadPreviewImages} onOpen={() => onOpen(dataset.id)} onAnnotate={() => onAnnotate(dataset)} onOpenWindow={() => onOpenWindow?.(dataset)} />{canAssign && onAssign ? <button type="button" disabled={assigningDatasetId === dataset.id} onClick={() => onAssign(dataset)}>{assigningDatasetId === dataset.id ? "加入中…" : "加入当前项目"}</button> : null}</div>)}</div> : <div className="data-projects-drawer-empty">没有待归属数据集。</div>}
  </aside>;
}

export function DataProjectOverview({
  projectImages = {},
  onNavigateProject,
  onOpen,
  onAnnotate,
  onOpenWindow,
}: DataProjectOverviewProps) {
  const [dataProjects, setDataProjects] = useState<DataProject[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [formOpen, setFormOpen] = useState(false);
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [busy, setBusy] = useState(false);
  const [projectQuery, setProjectQuery] = useState("");
  const [projectSort, setProjectSort] = useState<"updated" | "name">("updated");
  const [datasetDrawerOpen, setDatasetDrawerOpen] = useState(false);
  const [catalog, setCatalog] = useState<DatasetProject[]>([]);
  const [catalogLoading, setCatalogLoading] = useState(false);
  const [catalogError, setCatalogError] = useState<string | null>(null);
  const requestRef = useRef(0);
  const catalogRequestRef = useRef(0);

  const reload = useCallback(async () => {
    const request = ++requestRef.current;
    setLoading(true);
    setError(null);
    try {
      const loaded = await listDataProjects();
      if (request !== requestRef.current) return;
      if (!Array.isArray(loaded)) throw new Error("数据项目响应格式无效");
      setDataProjects(loaded);
    } catch (cause) {
      if (request === requestRef.current) {
        setDataProjects([]);
        setError(errorMessage(cause));
      }
    } finally {
      if (request === requestRef.current) setLoading(false);
    }
  }, []);

  useEffect(() => {
    void reload();
    return () => { requestRef.current += 1; };
  }, [reload]);

  const assignedIds = useMemo(
    () => new Set(dataProjects.flatMap((project) => project.datasetIds.map(canonicalDatasetKey))),
    [dataProjects],
  );
  const availableDatasets = useMemo(
    () => catalog.filter((dataset) => !assignedIds.has(canonicalDatasetKey(dataset.id))),
    [assignedIds, catalog],
  );
  const visibleProjects = useMemo(() => {
    const query = projectQuery.trim().toLocaleLowerCase();
    return dataProjects
      .filter((project) => !query || project.name.toLocaleLowerCase().includes(query))
      .sort((left, right) => projectSort === "name"
        ? left.name.localeCompare(right.name, "zh-CN")
        : (Date.parse(right.updatedAt) || 0) - (Date.parse(left.updatedAt) || 0));
  }, [dataProjects, projectQuery, projectSort]);

  async function openDatasetDrawer() {
    setDatasetDrawerOpen(true);
    setCatalogLoading(true);
    setCatalogError(null);
    const request = ++catalogRequestRef.current;
    try {
      const loaded = await listDatasetProjects();
      if (request !== catalogRequestRef.current) return;
      if (!Array.isArray(loaded)) throw new Error("数据集列表响应格式无效");
      setCatalog(loaded);
    } catch (cause) {
      if (request === catalogRequestRef.current) {
        setCatalog([]);
        setCatalogError(errorMessage(cause));
      }
    } finally {
      if (request === catalogRequestRef.current) setCatalogLoading(false);
    }
  }

  function closeDatasetDrawer() {
    catalogRequestRef.current += 1;
    setDatasetDrawerOpen(false);
    setCatalogLoading(false);
  }

  function openProject(project: DataProject) {
    onNavigateProject(project.id, "data");
  }

  async function createProject() {
    if (!name.trim() || busy) return;
    setBusy(true);
    setError(null);
    try {
      const created = await createDataProject(name.trim(), description.trim());
      setName("");
      setDescription("");
      setFormOpen(false);
      await reload();
      onNavigateProject(created.id, "data");
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  }

  return (
    <main className="content data-projects-page data-projects-overview-page">
      <header className="data-projects-overview-header">
        <h1>项目 <span className="workspace-count">{loading ? "…" : visibleProjects.length === dataProjects.length ? dataProjects.length : `${visibleProjects.length}/${dataProjects.length}`}</span></h1>
        <div className="data-projects-project-filters"><label><Search size={13} /><input aria-label="搜索项目" value={projectQuery} onChange={(event) => setProjectQuery(event.target.value)} placeholder="搜索项目" /></label><select aria-label="项目排序" value={projectSort} onChange={(event) => setProjectSort(event.target.value as "updated" | "name")}><option value="updated">最近更新</option><option value="name">名称</option></select></div>
        <div className="data-projects-overview-actions"><button type="button" onClick={() => void openDatasetDrawer()}>待归属数据</button><button className="primary" type="button" onClick={() => setFormOpen((open) => !open)}><Plus size={14} />新建项目</button></div>
      </header>
      {formOpen ? <section className="data-projects-create" aria-label="新建数据项目"><label><span>项目名称</span><input autoFocus value={name} onChange={(event) => setName(event.target.value)} /></label><label><span>项目说明</span><input value={description} onChange={(event) => setDescription(event.target.value)} /></label><div><button className="primary" type="button" disabled={busy || !name.trim()} onClick={() => void createProject()}>创建项目</button><button type="button" disabled={busy} onClick={() => setFormOpen(false)}>取消</button></div></section> : null}
      {error ? <div className="data-projects-error" role="alert">{error}<button type="button" onClick={() => void reload()}>重试</button></div> : null}
      <section className="data-projects-overview-section" aria-label="数据项目">
        {!loading && dataProjects.length === 0 ? <div className="data-projects-overview-empty">还没有项目。</div> : !loading && visibleProjects.length === 0 ? <div className="data-projects-overview-empty">没有匹配的项目。</div> : <div className="data-projects-project-card-grid">{visibleProjects.map((project) => <article className="data-projects-project-card" key={project.id} role="button" tabIndex={0} onClick={() => openProject(project)} onKeyDown={(event) => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); openProject(project); } }}><div className="data-projects-project-card-heading"><FolderKanban size={17} /><h3>{project.name}</h3><button type="button" aria-label="进入项目" title="进入项目" onClick={(event) => { event.stopPropagation(); openProject(project); }}><ArrowUpRight size={15} /></button></div><div className="data-projects-project-card-meta"><span className="workspace-tag">{project.datasetIds.length} 个数据集</span><time dateTime={project.updatedAt} title={`更新于 ${formatDate(project.updatedAt)}`}>{formatDate(project.updatedAt)}</time></div></article>)}</div>}
      </section>
      {datasetDrawerOpen ? <DatasetAssignmentDrawer datasets={availableDatasets} projectImages={projectImages} loading={catalogLoading} error={catalogError} onClose={closeDatasetDrawer} onRetry={() => void openDatasetDrawer()} onOpen={onOpen} onAnnotate={onAnnotate} onOpenWindow={onOpenWindow} loadPreviewImages={loadDatasetPreview} /> : null}
    </main>
  );
}

type DataSectionProps = {
  projectId: string;
  project: DataProject | null;
  projects?: DatasetProject[];
  projectImages?: Record<string, DatasetImage[]>;
  onCreateDataset?: (dataProjectId?: string) => void;
  onUploadImages?: (dataProjectId: string, datasetId?: string) => void;
  onOpen: (datasetId: string) => void;
  onAnnotate: (dataset: DatasetProject) => void;
  onOpenWindow?: (dataset: DatasetProject) => void;
  onOpenUnassigned?: () => void;
  onDatasetsChanged?: () => void | Promise<void>;
};

export function DataProjectDataSection({ projectId, projectImages = {}, onCreateDataset, onUploadImages, onOpen, onAnnotate, onOpenWindow, onDatasetsChanged }: DataSectionProps) {
  const [datasets, setDatasets] = useState<DatasetProject[]>([]);
  const [groupState, setGroupState] = useState<DataProjectGroupState | null>(null);
  const [tree, setTree] = useState<DataProjectWorkingTree | null>(null);
  const [loading, setLoading] = useState(true);
  const [savingGroups, setSavingGroups] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [datasetDrawerOpen, setDatasetDrawerOpen] = useState(false);
  const [availableDatasets, setAvailableDatasets] = useState<DatasetProject[]>([]);
  const [datasetDrawerLoading, setDatasetDrawerLoading] = useState(false);
  const [datasetDrawerError, setDatasetDrawerError] = useState<string | null>(null);
  const [assigningDatasetId, setAssigningDatasetId] = useState<string | null>(null);
  const requestRef = useRef(0);
  const datasetDrawerRequestRef = useRef(0);

  const load = useCallback(async (): Promise<boolean> => {
    const request = ++requestRef.current;
    setLoading(true);
    setError(null);
    try {
      const [loadedDatasets, loadedGroups, loadedTree] = await Promise.all([
        listDataProjectDatasets(projectId),
        getDataProjectDatasetGroups(projectId),
        getDataProjectWorkingTree(projectId),
      ]);
      if (request !== requestRef.current) return false;
      if (!Array.isArray(loadedDatasets)) throw new Error("项目数据集响应格式无效");
      if (!loadedGroups || !Array.isArray(loadedGroups.projectMetadata?.datasetGroups)) throw new Error("数据集分组响应格式无效");
      setDatasets(loadedDatasets);
      setGroupState(loadedGroups);
      setTree(loadedTree);
      return true;
    } catch (cause) {
      if (request === requestRef.current) {
        setDatasets([]);
        setGroupState(null);
        setTree(null);
        setError(errorMessage(cause));
      }
      return false;
    } finally {
      if (request === requestRef.current) setLoading(false);
    }
  }, [projectId]);

  useEffect(() => {
    void load();
    return () => { requestRef.current += 1; };
  }, [load]);

  const counts = normalizeCounts(tree);
  const canEditProject = groupState?.canEdit === true;

  const refreshScopedData = useCallback(async () => {
    const request = ++requestRef.current;
    try {
      const [loadedDatasets, loadedTree] = await Promise.all([
        listDataProjectDatasets(projectId),
        getDataProjectWorkingTree(projectId),
      ]);
      if (request !== requestRef.current) return;
      if (!Array.isArray(loadedDatasets)) throw new Error("项目数据集响应格式无效");
      setDatasets(loadedDatasets);
      setTree(loadedTree);
    } catch (cause) {
      if (request === requestRef.current) setError(errorMessage(cause));
    }
  }, [projectId]);

  async function loadAvailableDatasets() {
    setDatasetDrawerLoading(true);
    setDatasetDrawerError(null);
    const request = ++datasetDrawerRequestRef.current;
    try {
      const [catalog, dataProjects] = await Promise.all([listDatasetProjects(), listDataProjects()]);
      if (request !== datasetDrawerRequestRef.current) return;
      if (!Array.isArray(catalog) || !Array.isArray(dataProjects)) throw new Error("待归属数据集响应格式无效");
      const assigned = new Set(dataProjects.flatMap((item) => item.datasetIds.map(canonicalDatasetKey)));
      setAvailableDatasets(catalog.filter((dataset) => !assigned.has(canonicalDatasetKey(dataset.id))));
    } catch (cause) {
      if (request === datasetDrawerRequestRef.current) {
        setAvailableDatasets([]);
        setDatasetDrawerError(errorMessage(cause));
      }
    } finally {
      if (request === datasetDrawerRequestRef.current) setDatasetDrawerLoading(false);
    }
  }

  async function openDatasetDrawer() {
    setDatasetDrawerOpen(true);
    await loadAvailableDatasets();
  }

  function closeDatasetDrawer() {
    datasetDrawerRequestRef.current += 1;
    setDatasetDrawerOpen(false);
    setDatasetDrawerLoading(false);
  }

  async function assignAvailableDataset(dataset: DatasetProject) {
    if (assigningDatasetId) return;
    setAssigningDatasetId(dataset.id);
    setDatasetDrawerError(null);
    try {
      await assignDatasetToDataProject(projectId, dataset.id);
      await load();
      await loadAvailableDatasets();
      await onDatasetsChanged?.();
    } catch (cause) {
      setDatasetDrawerError(errorMessage(cause));
    } finally {
      setAssigningDatasetId(null);
    }
  }

  async function persistGroups(groups: DatasetGroup[]) {
    if (!groupState?.canEdit) return;
    setSavingGroups(true);
    setError(null);
    try {
      const next = await setDataProjectDatasetGroups(projectId, groupState.branch, groupState.revision, groups);
      setGroupState(next);
      await refreshScopedData();
      await onDatasetsChanged?.();
    } catch (cause) {
      const message = errorMessage(cause);
      // The board already rolls back its optimistic layout when this promise
      // rejects. Reload while it is unmounted so the next render receives the
      // server's current groups and revision instead of retrying a stale CAS.
      const refreshed = await load();
      setError(isRevisionConflict(cause)
        ? refreshed
          ? "项目分组已被其他操作更新，已重新读取最新状态，请检查后重试。"
          : "项目分组已被其他操作更新，但最新状态读取失败，请稍后重试。"
        : refreshed ? message : `${message}；最新项目数据读取失败，请稍后重试。`);
      throw cause;
    } finally {
      setSavingGroups(false);
    }
  }

  return <div className="data-projects-workspace-section data-projects-data-section">
    {error ? <div className="data-projects-feature-error" role="alert">{error}<button type="button" onClick={() => void load()}>重试</button></div> : null}
    {loading ? <div className="data-projects-section-empty">正在读取项目数据…</div> : <>
      {groupState ? <DatasetGroupingBoard datasets={datasets} projectImages={projectImages} loadPreviewImages={loadDatasetPreview} groups={groupState.projectMetadata.datasetGroups} stateVersion={`${groupState.branch}:${groupState.revision}`} canEdit={groupState.canEdit} saving={savingGroups} onPersist={persistGroups} onOpen={onOpen} onAnnotate={onAnnotate} onOpenWindow={onOpenWindow}
        toolbarContent={counts ? <div className="data-projects-status-inline" aria-label="数据处理状态统计">{[["图像", counts.total], ["待标注", counts.raw], ["处理中", counts.inProgress], ["待审核", counts.pendingReview], ["已通过", counts.approved], ["已驳回", counts.rejected]].filter(([, count], index) => index === 0 || Number(count) > 0).map(([label, count]) => <span className="workspace-tag" key={label as string}><strong>{countLabel(count as number)}</strong>{label}</span>)}</div> : null}
        toolbarActions={canEditProject ? <>{onCreateDataset ? <button type="button" onClick={() => onCreateDataset(projectId)}><Plus size={14} />新建数据集</button> : null}{onUploadImages ? <button type="button" onClick={() => onUploadImages(projectId)}><UploadCloud size={14} />上传图像</button> : null}<button type="button" onClick={() => void openDatasetDrawer()}>加入已有</button></> : null}
      /> : null}
      {datasetDrawerOpen ? <DatasetAssignmentDrawer datasets={availableDatasets} projectImages={projectImages} loading={datasetDrawerLoading} error={datasetDrawerError} canAssign assigningDatasetId={assigningDatasetId} onClose={closeDatasetDrawer} onRetry={() => void openDatasetDrawer()} onOpen={onOpen} onAnnotate={onAnnotate} onOpenWindow={onOpenWindow} loadPreviewImages={loadDatasetPreview} onAssign={(dataset) => void assignAvailableDataset(dataset)} /> : null}
    </>}
  </div>;
}

type VersionsSectionProps = {
  projectId: string;
  readOnly?: boolean;
  onDatasetsChanged?: () => void | Promise<void>;
  onExportDataProject?: (dataProjectId: string) => void | Promise<void>;
  exportingDataProjectId?: string | null;
};

export function DataProjectVersionsSection({ projectId, readOnly = false, onDatasetsChanged, onExportDataProject, exportingDataProjectId }: VersionsSectionProps) {
  const [tree, setTree] = useState<DataProjectWorkingTree | null>(null);
  const [commits, setCommits] = useState<DataProjectCommit[]>([]);
  const [branches, setBranches] = useState<DataProjectBranch[]>([]);
  const [message, setMessage] = useState("");
  const [newBranch, setNewBranch] = useState("");
  const [mergeSource, setMergeSource] = useState("");
  const [mergeMessage, setMergeMessage] = useState("");
  const [mergePreview, setMergePreview] = useState<{ sessionId: string | null; conflicts: DataProjectMergeConflict[] } | null>(null);
  const [resolutions, setResolutions] = useState<Record<string, "ours" | "theirs">>({});
  const [revertId, setRevertId] = useState<string | null>(null);
  const [revertMessage, setRevertMessage] = useState("");
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const requestRef = useRef(0);
  const currentBranch = branches.find((branch) => branch.current) ?? branches[0] ?? null;
  const dirty = Boolean(tree?.changes.length);

  const load = useCallback(async () => {
    const request = ++requestRef.current;
    setLoading(true);
    setError(null);
    try {
      const [nextTree, nextCommits, nextBranches] = await Promise.all([
        getDataProjectWorkingTree(projectId),
        listDataProjectCommits(projectId),
        listDataProjectBranches(projectId),
      ]);
      if (request !== requestRef.current) return;
      if (!Array.isArray(nextTree.changes) || !Array.isArray(nextCommits) || !Array.isArray(nextBranches)) throw new Error("项目版本响应格式无效");
      setTree(nextTree);
      setCommits(nextCommits);
      setBranches(nextBranches);
      setMergeSource((current) => current && nextBranches.some((branch) => branch.name === current && !branch.current) ? current : nextBranches.find((branch) => !branch.current)?.name ?? "");
    } catch (cause) {
      if (request === requestRef.current) {
        setTree(null);
        setCommits([]);
        setBranches([]);
        setError(errorMessage(cause));
      }
    } finally {
      if (request === requestRef.current) setLoading(false);
    }
  }, [projectId]);

  useEffect(() => {
    void load();
    return () => { requestRef.current += 1; };
  }, [load]);

  async function commit() {
    const text = message.trim();
    if (readOnly || !text || !currentBranch || !tree?.changes.length || busy) return;
    setBusy(true);
    setError(null);
    try {
      await commitDataProject(projectId, currentBranch.name, text);
      setMessage("");
      await load();
      await onDatasetsChanged?.();
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  }

  async function createBranch() {
    const name = newBranch.trim();
    if (readOnly || !name || busy) return;
    setBusy(true);
    try {
      await createDataProjectBranch(projectId, name);
      setNewBranch("");
      await load();
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  }

  async function switchBranch(name: string) {
    if (!name || name === currentBranch?.name || dirty || busy) return;
    setBusy(true);
    setError(null);
    try {
      await switchDataProjectBranch(projectId, name);
      await load();
      await onDatasetsChanged?.();
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  }

  async function deleteBranch(name: string) {
    if (readOnly || !name || name === "main" || busy) return;
    setBusy(true);
    try {
      await deleteDataProjectBranch(projectId, name);
      await load();
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  }

  async function previewMerge() {
    if (readOnly || !mergeSource || !currentBranch || busy || mergeSource === currentBranch.name) return;
    setBusy(true);
    setError(null);
    try {
      const preview = await previewDataProjectMerge(projectId, mergeSource, currentBranch.name);
      setMergePreview({ sessionId: preview.sessionId, conflicts: preview.conflicts });
      setResolutions({});
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  }

  async function confirmMerge() {
    const text = mergeMessage.trim();
    if (readOnly || !mergeSource || !currentBranch || !text || busy || dirty || !mergePreview) return;
    const missing = mergePreview.conflicts.find((conflict) => !resolutions[conflictKey(conflict)]);
    if (missing) {
      setError("请为每项冲突选择当前分支或源分支版本。");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const merged = await mergeDataProjectBranches(projectId, mergeSource, currentBranch.name, text);
      let sessionId = merged.sessionId;
      let remaining = merged.conflicts;
      if (sessionId) {
        for (const conflict of remaining) {
          const resolution = resolutions[conflictKey(conflict)];
          if (!resolution) throw new Error("请为每项冲突选择版本");
          const resolved = await resolveDataProjectMerge(projectId, sessionId, conflictKey(conflict), resolution);
          sessionId = resolved.sessionId ?? sessionId;
          remaining = resolved.conflicts;
        }
      }
      if (remaining.length > 0) throw new Error("合并仍有未解决冲突");
      setMergePreview(null);
      setResolutions({});
      setMergeMessage("");
      await load();
      await onDatasetsChanged?.();
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  }

  async function revert(commitItem: DataProjectCommit) {
    const text = revertMessage.trim();
    if (readOnly || !text || !currentBranch || dirty || busy) return;
    setBusy(true);
    setError(null);
    try {
      await revertDataProject(projectId, currentBranch.name, commitItem.id, text);
      setRevertId(null);
      setRevertMessage("");
      await load();
      await onDatasetsChanged?.();
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  }

  return <div className="data-projects-workspace-section data-projects-versions-section">
    <header className="data-projects-workspace-heading"><div><h2><GitCommitHorizontal size={18} />版本</h2></div><div className="data-projects-workspace-actions"><button type="button" disabled={loading || busy} onClick={() => void load()}><RefreshCw size={14} />刷新</button>{onExportDataProject ? <button type="button" disabled={busy || exportingDataProjectId === projectId} onClick={() => void onExportDataProject(projectId)}><Archive size={14} />{exportingDataProjectId === projectId ? "导出中…" : "导出清单"}</button> : null}</div></header>
    {error ? <div className="data-projects-feature-error" role="alert">{error}</div> : null}
    {loading ? <div className="data-projects-section-empty">正在读取版本…</div> : <>
      <section className="data-projects-version-panel"><div className="data-projects-section-title"><div><h3><GitBranch size={15} />分支</h3></div><span>{currentBranch?.name ?? "—"}</span></div><div className="data-projects-branch-toolbar"><label><span>当前分支</span><select aria-label="当前分支" value={currentBranch?.name ?? ""} disabled={busy || dirty} onChange={(event) => void switchBranch(event.target.value)}>{branches.map((branch) => <option key={branch.name} value={branch.name}>{branch.name}{branch.current ? "（当前）" : ""}</option>)}</select></label>{!readOnly ? <><input aria-label="新分支名称" value={newBranch} onChange={(event) => setNewBranch(event.target.value)} placeholder="新分支名称" /><button type="button" disabled={busy || !newBranch.trim()} onClick={() => void createBranch()}><Plus size={14} />创建</button></> : null}</div><div className="data-projects-branch-list">{branches.map((branch) => <div className="data-projects-branch-row" key={branch.name}><span><strong>{branch.name}</strong>{branch.current ? <em>当前</em> : null}<small>{branch.headCommitId ? `HEAD ${branch.headCommitId.slice(0, 8)}` : "尚无提交"}</small></span>{!readOnly && !branch.current && branch.name !== "main" ? <button type="button" disabled={busy} onClick={() => void deleteBranch(branch.name)}>删除</button> : null}</div>)}</div></section>
      <section className="data-projects-version-panel"><div className="data-projects-section-title"><div><h3><GitCommitHorizontal size={15} />工作区变更</h3></div><strong>{tree?.changes.length ?? 0} 项</strong></div><div className="data-projects-commit-box">{readOnly ? <span className="data-projects-readonly-note">当前项目只读，可浏览变更并导出清单。</span> : <><input aria-label="提交说明" disabled={!tree?.changes.length || busy} value={message} onChange={(event) => setMessage(event.target.value)} placeholder={tree?.changes.length ? "为这次数据变化写一句说明" : "工作区干净时不能创建空提交"} /><button className="primary" type="button" disabled={busy || !message.trim() || !tree?.changes.length} onClick={() => void commit()}><Check size={14} />提交版本</button></>}</div>{tree?.changes.length ? <div className="data-projects-changes">{tree.changes.map((change, index) => <div className="data-projects-change" key={`${change.datasetId}-${change.imageId ?? index}`}><span className="data-projects-change-kind">{changeLabel(change)}</span><span>{change.fileName || change.imageId || change.datasetId}</span><small>{change.detail}</small></div>)}</div> : <div className="data-projects-section-empty">工作区干净，没有可提交的变化。</div>}</section>
      {!readOnly ? <section className="data-projects-version-panel"><div className="data-projects-section-title"><div><h3><GitMerge size={15} />合并</h3></div></div><div className="data-projects-merge-controls"><label><span>源分支</span><select aria-label="合并源分支" value={mergeSource} disabled={busy} onChange={(event) => setMergeSource(event.target.value)}><option value="">选择源分支</option>{branches.filter((branch) => !branch.current).map((branch) => <option key={branch.name} value={branch.name}>{branch.name}</option>)}</select></label><button type="button" disabled={busy || !mergeSource || !currentBranch} onClick={() => void previewMerge()}><GitMerge size={14} />预览合并</button></div>{mergePreview ? <div className="data-projects-merge-preview"><p>{mergePreview.conflicts.length ? `${mergePreview.conflicts.length} 项冲突` : "没有冲突"}</p>{mergePreview.conflicts.map((conflict) => <div className="data-projects-conflict" key={conflictKey(conflict)}><strong>{readableConflict(conflict.displayName || conflict.key)}</strong><span className="data-projects-conflict-base">基线：{conflictValue(conflict.base, conflict.baseDescription)}</span><div className="data-projects-conflict-values"><label><input type="radio" name={`version-conflict-${conflictKey(conflict)}`} checked={resolutions[conflictKey(conflict)] === "ours"} onChange={() => setResolutions((current) => ({ ...current, [conflictKey(conflict)]: "ours" }))} /><span>当前分支：{conflictValue(conflict.ours, conflict.oursDescription)}</span></label><label><input type="radio" name={`version-conflict-${conflictKey(conflict)}`} checked={resolutions[conflictKey(conflict)] === "theirs"} onChange={() => setResolutions((current) => ({ ...current, [conflictKey(conflict)]: "theirs" }))} /><span>源分支：{conflictValue(conflict.theirs, conflict.theirsDescription)}</span></label></div></div>)}<input aria-label="合并提交说明" value={mergeMessage} onChange={(event) => setMergeMessage(event.target.value)} placeholder="合并提交说明" /><button className="primary" type="button" disabled={busy || dirty || !mergeMessage.trim() || mergePreview.conflicts.some((conflict) => !resolutions[conflictKey(conflict)])} onClick={() => void confirmMerge()}>确认合并</button></div> : null}</section> : null}
      <section className="data-projects-version-panel"><div className="data-projects-section-title"><div><h3><History size={15} />提交历史</h3></div><strong>{commits.length} 次</strong></div>{commits.length ? <div className="data-projects-history">{commits.map((commitItem) => <article className="data-projects-history-row" key={commitItem.id}><GitCommitHorizontal size={15} /><div><strong>{commitItem.message}</strong><small>{formatDate(commitItem.createdAt)} · {commitItem.datasetIds.length} 个数据集{commitItem.secondParentId ? " · 合并提交" : ""}</small><small>父提交：{commitItem.parentId?.slice(0, 8) ?? "无"}{commitItem.secondParentId ? ` · 第二父提交：${commitItem.secondParentId.slice(0, 8)}` : ""}</small></div><code title={commitItem.id}>{commitItem.id.slice(0, 8)}</code>{revertId === commitItem.id ? <div className="data-projects-revert-box"><input aria-label="回退提交说明" value={revertMessage} onChange={(event) => setRevertMessage(event.target.value)} placeholder="回退提交说明" /><button className="primary" type="button" disabled={busy || dirty || readOnly || !revertMessage.trim()} onClick={() => void revert(commitItem)}>确认回退</button><button type="button" disabled={busy || readOnly} onClick={() => setRevertId(null)}>取消</button></div> : <button type="button" disabled={busy || dirty || readOnly} onClick={() => { setRevertId(commitItem.id); setRevertMessage(`回退到 ${commitItem.id.slice(0, 8)}`); }} title={readOnly ? "当前项目只读" : dirty ? "请先提交未保存变更" : "回退会创建新提交并保留历史"}><RotateCcw size={13} />回退</button>}<details className="data-projects-version-detail"><summary>查看版本内容（{commitItem.entries.length} 条）</summary><div className="data-projects-version-files">{commitItem.entries.length ? commitItem.entries.map((entry) => <div key={`${entry.datasetId}-${entry.imageId}`}><strong>{entry.fileName || entry.imageId}</strong><span>{entry.status || "未处理"}{entry.qaStatus ? ` · ${entry.qaStatus}` : ""}</span><small>图像 {entry.imageHash.slice(0, 10)} · 标签 {entry.annotationHash.slice(0, 10)}</small></div>) : <span>此提交没有图像条目。</span>}</div></details></article>)}</div> : <div className="data-projects-section-empty">还没有提交记录。</div>}</section>
    </>}
  </div>;
}

type CollaborationSectionProps = {
  projectId: string;
  project?: DataProject | null;
  onDatasetsChanged?: () => void | Promise<void>;
};

export function DataProjectCollaborationSection({ projectId, project, onDatasetsChanged }: CollaborationSectionProps) {
  const [status, setStatus] = useState<DataProjectSyncStatus | null>(null);
  const [remotes, setRemotes] = useState<RemoteDataProject[]>([]);
  const [members, setMembers] = useState<DataProjectCloudMember[]>([]);
  const [role, setRole] = useState<string>("");
  const [memberId, setMemberId] = useState("");
  const [memberRole, setMemberRole] = useState("viewer");
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const requestRef = useRef(0);
  const canManage = role === "owner" || role === "admin";

  const load = useCallback(async () => {
    const request = ++requestRef.current;
    setLoading(true);
    setError(null);
    setMessage(null);
    try {
      const nextStatus = await getDataProjectSyncStatus(projectId);
      if (request !== requestRef.current) return;
      setStatus(nextStatus);
      const discovered = await discoverDataProjectRemotes();
      if (request !== requestRef.current) return;
      setRemotes(Array.isArray(discovered) ? discovered : []);
      const remote = discovered.find((candidate) => candidate.id === nextStatus.remoteProjectId);
      setRole(remote?.role ?? "");
      if (nextStatus.remoteProjectId) {
        const loadedMembers = await listDataProjectCloudMembers(projectId);
        if (request !== requestRef.current) return;
        setMembers(Array.isArray(loadedMembers) ? loadedMembers : []);
      } else {
        setMembers([]);
      }
    } catch (cause) {
      if (request === requestRef.current) setError(errorMessage(cause));
    } finally {
      if (request === requestRef.current) setLoading(false);
    }
  }, [projectId]);

  useEffect(() => {
    void load();
    return () => { requestRef.current += 1; };
  }, [load]);

  async function refreshStatus() {
    try {
      const next = await getDataProjectSyncStatus(projectId);
      setStatus(next);
    } catch (cause) {
      setError(errorMessage(cause));
    }
  }

  async function publish(remoteProjectId?: string, remoteRole?: string) {
    const targetRole = remoteRole ?? role;
    if (busy || status?.dirty || (remoteProjectId || status?.remoteProjectId) && targetRole === "viewer") return;
    setBusy(true);
    setError(null);
    setMessage(null);
    try {
      let remoteId = remoteProjectId ?? status?.remoteProjectId ?? null;
      if (!remoteId) {
        if (!project) throw new Error("项目资料尚未加载，暂不能创建云端项目");
        const created = await createRemoteDataProject(project.name, project.description);
        remoteId = created.id;
      }
      await connectRemoteDataProject(projectId, remoteId);
      const report = await publishDataProject(projectId);
      if (!report || !report.remoteProjectId) throw new Error("云端发布未返回远端项目");
      setMessage(`云端项目已更新，修订 ${report.remoteRevision}。`);
      await load();
      await onDatasetsChanged?.();
    } catch (cause) {
      setError(errorMessage(cause));
      await refreshStatus();
    } finally {
      setBusy(false);
    }
  }

  async function sync() {
    if (busy || status?.dirty || !status?.remoteProjectId) return;
    setBusy(true);
    setError(null);
    setMessage(null);
    try {
      const report = await syncDataProject(projectId);
      if (report.outcome === "readOnlyAhead") {
        setMessage("本地分支有新提交，当前云端只读。");
      } else {
        setMessage(`同步状态：${report.outcome}`);
      }
      await load();
      await onDatasetsChanged?.();
    } catch (cause) {
      setError(errorMessage(cause));
      await refreshStatus();
    } finally {
      setBusy(false);
    }
  }

  async function clone(remote: RemoteDataProject) {
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      const report = await cloneDataProject(remote.id);
      if (!report || !report.localProjectId) throw new Error("导入远端项目未返回本地项目");
      setMessage(`已导入远端项目“${remote.name}”。`);
      await onDatasetsChanged?.();
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  }

  async function saveMember() {
    if (!canManage || !memberId.trim() || busy || !status?.remoteProjectId) return;
    setBusy(true);
    setError(null);
    try {
      await upsertDataProjectCloudMember(projectId, memberId.trim(), memberRole);
      setMemberId("");
      setMembers(await listDataProjectCloudMembers(projectId));
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  }

  async function removeMember(member: DataProjectCloudMember) {
    if (!canManage || busy || member.role === "owner") return;
    setBusy(true);
    try {
      await removeDataProjectCloudMember(projectId, member.userId);
      setMembers(await listDataProjectCloudMembers(projectId));
    } catch (cause) {
      setError(errorMessage(cause));
    } finally {
      setBusy(false);
    }
  }

  return <div className="data-projects-workspace-section data-projects-collaboration-section">
    <header className="data-projects-workspace-heading"><div><h2><Cloud size={18} />协作</h2></div><div className="data-projects-workspace-actions"><button type="button" disabled={busy || loading} onClick={() => void load()}><RefreshCw size={14} />刷新状态</button></div></header>
    {error ? <div className="data-projects-feature-error" role="alert">{error}</div> : null}
    {loading ? <div className="data-projects-section-empty">正在读取云端状态…</div> : <>
      <section className="data-projects-cloud-status-panel"><div className="data-projects-cloud-status-heading"><div>{status?.state === "offline" || status?.state === "error" ? <CloudOff size={16} /> : <Cloud size={16} />}<strong>{status?.state === "readOnlyAhead" ? "本地分支有新提交，当前云端只读" : status?.state === "upToDate" ? "已同步" : status?.state === "dirty" ? "有未提交变更" : status?.state === "diverged" ? "分叉" : status?.state === "syncing" ? "同步中" : "未连接"}</strong></div><span>{status?.remoteProjectId ? `云端修订 ${status.remoteRevision ?? "—"}` : "尚未连接云端项目"}</span></div><dl><div><dt>本地 HEAD</dt><dd>{status?.localHead?.slice(0, 8) ?? "—"}</dd></div><div><dt>远端 HEAD</dt><dd>{status?.remoteHead?.slice(0, 8) ?? "—"}</dd></div><div><dt>角色</dt><dd>{role || "未确认"}</dd></div></dl><div className="data-projects-cloud-actions">{!status?.remoteProjectId ? <button type="button" disabled={busy || status?.dirty || role === "viewer"} onClick={() => void publish()}><UploadCloud size={14} />首次发布到云端</button> : null}<button type="button" disabled={busy || Boolean(status?.dirty) || !status?.remoteProjectId} onClick={() => void sync()}>{role === "viewer" ? "读取云端更新" : "手动同步"}</button></div>{message ? <p className="data-projects-cloud-message" role="status">{message}</p> : null}</section>
      <section className="data-projects-cloud-status-panel"><div className="data-projects-section-title"><div><h3><Server size={15} />远端项目</h3></div><span>{remotes.length} 个可发现项目</span></div>{remotes.length ? <div className="data-projects-remote-list">{remotes.map((remote) => <div className="data-projects-remote-row" key={remote.id}><span><strong>{remote.name}</strong><small>{remote.description || "未填写说明"} · {remote.role}</small></span><div className="data-projects-remote-actions"><button type="button" disabled={busy || Boolean(status?.dirty) || !["owner", "admin", "editor"].includes(remote.role)} title={!['owner', 'admin', 'editor'].includes(remote.role) ? "当前角色只读，不能发布" : undefined} onClick={() => void publish(remote.id, remote.role)}>连接并发布</button><button type="button" disabled={busy} onClick={() => void clone(remote)}>导入本地</button></div></div>)}</div> : <div className="data-projects-section-empty">当前没有可导入的远端项目。</div>}</section>
      {status?.remoteProjectId ? <section className="data-projects-cloud-status-panel" aria-label="云端成员"><div className="data-projects-section-title"><div><h3><Users size={15} />成员</h3></div><span>{canManage ? "可管理" : `当前角色：${role || "只读"}`}</span></div>{members.length ? <div className="data-projects-member-list">{members.map((member) => <div className="data-projects-member-row" key={member.userId}><span><strong>{member.displayName || member.email || member.userId}</strong>{member.email ? <small>{member.email}</small> : null}</span><em>{member.role}</em>{canManage && member.role !== "owner" ? <button type="button" disabled={busy} onClick={() => void removeMember(member)}>移除</button> : null}</div>)}</div> : <div className="data-projects-section-empty">暂无成员信息。</div>}{canManage ? <div className="data-projects-member-form"><input aria-label="成员用户 ID" value={memberId} onChange={(event) => setMemberId(event.target.value)} placeholder="输入现有账号的用户 ID" /><select aria-label="成员角色" value={memberRole} onChange={(event) => setMemberRole(event.target.value)}><option value="viewer">viewer（只读）</option><option value="editor">editor（可写）</option><option value="admin">admin（管理）</option></select><button type="button" disabled={busy || !memberId.trim()} onClick={() => void saveMember()}>保存成员</button></div> : null}</section> : null}
    </>}
  </div>;
}

type SettingsSectionProps = {
  projectId: string;
  project: DataProject | null;
  readOnly?: boolean;
  onCreateDataset?: (dataProjectId?: string) => void;
  onUploadImages?: (dataProjectId: string, datasetId?: string) => void;
  onExportDataProject?: (dataProjectId: string) => void | Promise<void>;
  exportingDataProjectId?: string | null;
};

export function DataProjectSettingsSection({ projectId, project, readOnly = false, onCreateDataset, onUploadImages, onExportDataProject, exportingDataProjectId }: SettingsSectionProps) {
  return <div className="data-projects-workspace-section data-projects-settings-section">
    <header className="data-projects-workspace-heading"><div><h2><Settings size={18} />设置</h2></div></header>
    <section className="data-projects-settings-card"><dl><div><dt>项目名称</dt><dd>{project?.name ?? projectId}</dd></div><div><dt>项目说明</dt><dd>{project?.description || "未填写说明"}</dd></div><div><dt>数据集</dt><dd>{project?.datasetIds.length ?? "—"} 个</dd></div><div><dt>创建时间</dt><dd>{formatDate(project?.createdAt)}</dd></div><div><dt>更新时间</dt><dd>{formatDate(project?.updatedAt)}</dd></div></dl><div className="data-projects-settings-actions">{!readOnly && onCreateDataset ? <button type="button" onClick={() => onCreateDataset(projectId)}><Plus size={14} />新建数据集</button> : null}{!readOnly && onUploadImages ? <button type="button" onClick={() => onUploadImages(projectId)}><UploadCloud size={14} />上传图像</button> : null}{onExportDataProject ? <button type="button" disabled={exportingDataProjectId === projectId} onClick={() => void onExportDataProject(projectId)}><Archive size={14} />{exportingDataProjectId === projectId ? "导出中…" : "导出图像与标签清单"}</button> : null}</div></section>
  </div>;
}

export function DataProjectWorkspace(props: DataProjectWorkspaceProps) {
  const { dataProjectId, section, project: projectProp, projects, projectImages, onNavigateProject, onNavigateHome, onCreateDataset, onUploadImages, onOpen, onAnnotate, onOpenWindow, onOpenUnassigned, onDatasetsChanged, onExportDataProject, exportingDataProjectId } = props;
  const [project, setProject] = useState<DataProject | null>(projectProp ?? null);
  const [projectError, setProjectError] = useState<string | null>(null);
  const requestRef = useRef(0);
  const readOnlyProject = project?.role === "viewer";

  useEffect(() => {
    setProjectError(null);
    if (projectProp) {
      setProject(projectProp);
      return;
    }
    const request = ++requestRef.current;
    setProject(null);
    void listDataProjects()
      .then((loaded) => {
        if (request !== requestRef.current) return;
        if (!Array.isArray(loaded)) throw new Error("数据项目响应格式无效");
        const found = loaded.find((item) => item.id === dataProjectId) ?? null;
        setProject(found);
        setProjectError(found ? null : "找不到这个数据项目。");
      })
      .catch((cause) => { if (request === requestRef.current) setProjectError(errorMessage(cause)); });
    return () => { requestRef.current += 1; };
  }, [dataProjectId, projectProp]);

  const sectionItems: Array<[DataProjectSection, string]> = [["data", "数据"], ["versions", "版本"], ["collaboration", "协作"], ["settings", "设置"]];

  return <main className="content data-projects-page data-projects-workspace-page">
    <header className="data-projects-workspace-topbar"><button type="button" aria-label="返回项目" title="全部项目" onClick={onNavigateHome}><ArrowLeft size={16} /></button><div className="data-projects-project-title"><h1>{project?.name ?? "项目"}</h1><span className="workspace-tag">{project?.datasetIds.length ?? "—"} 数据集</span></div><nav aria-label="项目工作区">{sectionItems.map(([key, label]) => <button key={key} type="button" className={section === key ? "active" : ""} aria-current={section === key ? "page" : undefined} onClick={() => onNavigateProject(dataProjectId, key)}><span>{key === "data" ? <GitCommitHorizontal size={14} /> : key === "versions" ? <GitBranch size={14} /> : key === "collaboration" ? <Cloud size={14} /> : <Settings size={14} />}</span>{label}</button>)}</nav></header>
    {projectError ? <div className="data-projects-error" role="alert">{projectError}</div> : null}
    {section === "data" ? <DataProjectDataSection projectId={dataProjectId} project={project} projects={projects} projectImages={projectImages} onCreateDataset={onCreateDataset} onUploadImages={onUploadImages} onOpen={onOpen} onAnnotate={onAnnotate} onOpenWindow={onOpenWindow} onOpenUnassigned={onOpenUnassigned} onDatasetsChanged={onDatasetsChanged} /> : null}
    {section === "versions" ? <DataProjectVersionsSection projectId={dataProjectId} readOnly={readOnlyProject} onDatasetsChanged={onDatasetsChanged} onExportDataProject={onExportDataProject} exportingDataProjectId={exportingDataProjectId} /> : null}
    {section === "collaboration" ? <DataProjectCollaborationSection projectId={dataProjectId} project={project} onDatasetsChanged={onDatasetsChanged} /> : null}
    {section === "settings" ? <DataProjectSettingsSection projectId={dataProjectId} project={project} readOnly={readOnlyProject} onCreateDataset={onCreateDataset} onUploadImages={onUploadImages} onExportDataProject={onExportDataProject} exportingDataProjectId={exportingDataProjectId} /> : null}
  </main>;
}

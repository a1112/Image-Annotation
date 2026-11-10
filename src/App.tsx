import { useForegroundSync } from "./features/shared/useForegroundSync";
import {
  ArrowLeft,
  CheckCircle2,
  CircleAlert,
  ClipboardCheck,
  Download,
  FileJson,
  FolderKanban,
  Home,
  ImageIcon,
  Layers3,
  Maximize2,
  Minus,
  Play,
  Plus,
  Save,
  ShieldCheck,
  Tags,
  X,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  analyzeDataSource,
  clearCompletedBackendTasks,
  createDatasetProject,
  detectBackendConnection,
  downloadTestDataset,
  importFiles,
  importImages,
  importYoloDataset,
  invalidateImageAssets,
  isBackendUnavailableError,
  listBackendTasks,
  listBuiltinDatasets,
  listDatasetProjects,
  listProjectImages,
  openAnnotationWindow,
  openBackendTaskTray,
  openLocalDataset,
  pickDataSource,
} from "./api/tauri";
import type { BackendConnection } from "./api/tauri";
import type {
  BackendTask,
  BuiltinDataset,
  DatasetProject,
} from "./types/domain";
import { CollaborationPanel } from "./features/collaboration/CollaborationPanel";
import { DataProjectHome, AnnotationWorkspace, ProjectWorkspace } from "./shared/LazyWorkspaces";
import { DataSubmitDialog } from "./features/datasets/DataSubmitDialog";
import type { ProjectTopbarContext } from "./features/projects/types";
import { MobileWorkspaceShell } from "./features/mobile/MobileWorkspaceShell";
import type { ProjectTab } from "./shared/routes";
import type { DataProjectSection } from "./types/dataProjects";
import { assignDatasetToDataProject, listDataProjectDatasets, listDataProjects } from "./api/dataProjects";
import { annotationReturnRoute, clearAnnotationReturnRoutes, parseWorkspaceRoute, rememberAnnotationReturn, workspaceRouteHash } from "./shared/workspaceRoutes";
import { clearDatasetViewStates } from "./features/projects/viewState";
import { SESSION_CHANGE_EVENT } from "./shared/sessionEvents";
import { isMobileRuntime } from "./platform/runtime";
import { beginDesktopWindowDrag, runDesktopCommand } from "./platform/desktop";
import { navigate } from "./shared/navigation";

type DataRuntimeState = "loading" | "ready" | "downloading" | "backend-unavailable" | "download-error";
type DatasetCreationForm = {
  name: string;
  datasetType: "yolo-detect" | "yolo-seg" | "image-classification";
  demoTemplate: "demo-bbox" | "demo-polygon" | "demo-classification" | "empty";
};
type Route =
  | { name: "datasets" }
  | { name: "dataProject"; dataProjectId: string; section: DataProjectSection }
  | { name: "project"; projectId: string; dataProjectId?: string; tab?: ProjectTab }
  | { name: "annotate"; projectId: string; imageId?: string }
  | { name: "backendTasks" };

const defaultTestDatasetKey = "coco128";
const defaultDatasetCreationForm: DatasetCreationForm = {
  name: "Demo BBox 数据集",
  datasetType: "yolo-detect",
  demoTemplate: "demo-bbox",
};
function parseRoute(): Route {
  const route = parseWorkspaceRoute(window.location.hash);
  if (route.kind === "annotate") return { name: "annotate", projectId: route.datasetId, imageId: route.imageId };
  if (route.kind === "dataProject") return { name: "dataProject", dataProjectId: route.dataProjectId, section: route.section };
  if (route.kind === "dataset") return { name: "project", projectId: route.datasetId, dataProjectId: route.dataProjectId, tab: route.tab };
  if (route.kind === "backendTasks") return { name: "backendTasks" };
  return { name: "datasets" };
}





function TopBar({
  activeProjectId,
  activeProjectContext,
  backendConnection,
  onBackendTasks,
  onDataSubmit,
  onDatasets,
  onProjectAnnotate,
  onProjectOpenWindow,
  onProjectTab,
  onCollaboration,
  showCollaboration,
}: {
  activeProjectId?: string;
  activeProjectContext: ProjectTopbarContext | null;
  backendConnection: BackendConnection;
  onBackendTasks: () => void;
  onDataSubmit: () => void;
  onDatasets: () => void;
  onProjectAnnotate: (projectId: string, imageId?: string) => void;
  onProjectOpenWindow: (project: DatasetProject) => void;
  onProjectTab: (projectId: string, tab: ProjectTab) => void;
  onCollaboration: () => void;
  showCollaboration: boolean;
}) {
  const showWindowControls = backendConnection.mode === "tauri";
  const isProjectScope = Boolean(activeProjectId);
  const activeProject = activeProjectContext?.project ?? null;

  return (
    <header className="topbar" data-tauri-drag-region onMouseDown={beginDesktopWindowDrag}>
      <button
        aria-label={isProjectScope ? "返回项目数据" : "项目首页"}
        className="brand"
        data-no-drag
        onClick={onDatasets}
        type="button"
      >
        {isProjectScope ? <ArrowLeft size={19} /> : <ImageIcon size={19} />}
      </button>
      <div className="topbar-context">
        {activeProject ? (
          <>
            <strong>{activeProject.name}</strong>

          </>
        ) : isProjectScope ? (
          <span>加载数据集...</span>
        ) : null}
      </div>
      <div className="topbar-actions" data-no-drag>
        {isProjectScope && activeProjectId ? (
          <>
            {activeProject ? (
              <>
                <button type="button" onClick={() => onProjectAnnotate(activeProject.id, activeProjectContext?.firstImageId)}>
                  <Play size={16} />
                  开始标注
                </button>
                <button type="button" onClick={() => onProjectOpenWindow(activeProject)}>
                  <Layers3 size={16} />
                  独立窗口标注
                </button>
              </>
            ) : null}
            <button type="button" onClick={onDataSubmit}>
              <Plus size={16} />
              添加数据
            </button>
            <button type="button" onClick={() => onProjectTab(activeProjectId, "快照")}>
              <Save size={16} />
              快照管理
            </button>
            <button className="primary" type="button" onClick={() => onProjectTab(activeProjectId, "导出")}>
              <Download size={16} />
              导出数据集
            </button>
          </>
        ) : (
          <>
            <button type="button" onClick={onBackendTasks}>
              <ClipboardCheck size={16} />
              后端任务
            </button>
          </>
        )}
        {showCollaboration ? (
          <button type="button" onClick={onCollaboration}>
            <FolderKanban size={16} />
            协同登录
          </button>
        ) : null}
        {backendConnection.mode === "unavailable" ? <span className="sync-state unavailable"><CircleAlert size={15} />后端未连接</span> : null}
        {showWindowControls ? (
          <>
            <button aria-label="最小化" type="button" onClick={() => runDesktopCommand("minimize_window")}>
              <Minus size={16} />
            </button>
            <button aria-label="最大化" type="button" onClick={() => runDesktopCommand("toggle_maximize_window")}>
              <Maximize2 size={16} />
            </button>
            <button aria-label="关闭到托盘" type="button" onClick={() => runDesktopCommand("close_window")}>
              <X size={16} />
            </button>
          </>
        ) : null}
      </div>
    </header>
  );
}

function IconRail({ onHome, onTasks, inProjects }: { onHome: () => void; onTasks: () => void; inProjects: boolean }) {
  return <nav className="icon-rail" aria-label="主导航">
    <button aria-label="项目" title="项目" aria-pressed={inProjects} className={inProjects ? "active" : ""} type="button" onClick={onHome}><Home size={20} /></button>
    <button aria-label="任务中心" title="任务中心" aria-pressed={!inProjects} type="button" onClick={onTasks}><ClipboardCheck size={20} /></button>
  </nav>;
}

function ProjectInfoDialog({ onClose }: { onClose: () => void }) {
  return (
    <div className="modal-backdrop">
      <section aria-labelledby="project-info-title" className="dataset-dialog info-dialog" role="dialog">
        <div className="detail-header">
          <div>
            <span className="eyebrow">工程目录</span>
            <h2 id="project-info-title">工程信息</h2>
          </div>
          <button aria-label="关闭工程信息" type="button" onClick={onClose}>
            <X size={16} />
          </button>
        </div>
        <div className="info-path">data/workspaces/default</div>
        <section>
          <h3>
            <Tags size={16} />
            数据工程结构
          </h3>
          {["registry.json", "projects/{projectId}/project.json", "assets/original", "assets/thumbnails", "annotations/native", "snapshots", "exports"].map((item) => (
            <div className="detail-row" key={item}>
              <span>{item}</span>
            </div>
          ))}
        </section>
        <section>
          <h3>
            <ShieldCheck size={16} />
            当前能力
          </h3>
          <div className="check-row ok">
            <CheckCircle2 size={15} />
            COCO/YOLO 小数据集导入
          </div>
          <div className="check-row ok">
            <CheckCircle2 size={15} />
            BBox 与 Polygon 读取
          </div>
          <div className="check-row warning">
            <CircleAlert size={15} />
            Mask/Keypoints 编辑器后续扩展
          </div>
        </section>
        <section>
          <h3>
            <FileJson size={16} />
            导出格式
          </h3>
          <div className="format-row">
            <span>COCO JSON</span>
            <strong>标准导出</strong>
          </div>
          <div className="format-row">
            <span>YOLO TXT</span>
            <strong>原始格式</strong>
          </div>
        </section>
      </section>
    </div>
  );
}



function CreateDatasetDialog({
  form,
  onCancel,
  onChange,
  onCreate,
}: {
  form: DatasetCreationForm;
  onCancel: () => void;
  onChange: (form: DatasetCreationForm) => void;
  onCreate: () => void;
}) {
  return (
    <div className="modal-backdrop">
      <section aria-labelledby="create-dataset-title" className="dataset-dialog" role="dialog">
        <div className="dialog-header">
          <h2 id="create-dataset-title">新建数据集</h2>
          <button aria-label="关闭弹窗" type="button" onClick={onCancel}>
            <X size={16} />
          </button>
        </div>
        <label>
          <span>数据集名称</span>
          <input
            aria-label="数据集名称"
            onChange={(event) => onChange({ ...form, name: event.target.value })}
            value={form.name}
          />
        </label>
        <label>
          <span>数据集类型</span>
          <select
            aria-label="数据集类型"
            onChange={(event) => {
              const datasetType = event.target.value as DatasetCreationForm["datasetType"];
              const demoTemplate =
                datasetType === "yolo-seg"
                  ? "demo-polygon"
                  : datasetType === "image-classification"
                    ? "demo-classification"
                    : "demo-bbox";
              onChange({
                ...form,
                datasetType,
                demoTemplate,
              });
            }}
            value={form.datasetType}
          >
            <option value="yolo-detect">目标检测 / YOLO BBox</option>
            <option value="yolo-seg">实例分割 / YOLO Polygon</option>
            <option value="image-classification">图像分类 / Classification</option>
          </select>
        </label>
        <label>
          <span>初始化模板</span>
          <select
            aria-label="初始化模板"
            onChange={(event) => onChange({ ...form, demoTemplate: event.target.value as DatasetCreationForm["demoTemplate"] })}
            value={form.demoTemplate}
          >
            <option value="demo-bbox">Demo BBox：生成 3 张样例图和检测标签</option>
            <option value="demo-polygon">Demo Polygon：生成 3 张样例图和分割标签</option>
            <option value="demo-classification">Demo Classification：生成 3 张样例图和分类目录</option>
            <option value="empty">空数据集：仅创建工程结构</option>
          </select>
        </label>
        <div className="dialog-actions">
          <button type="button" onClick={onCancel}>
            取消
          </button>
          <button className="primary" type="button" onClick={onCreate} disabled={!form.name.trim()}>
            创建数据集
          </button>
        </div>
      </section>
    </div>
  );
}

function BackendTaskTray({
  tasks,
  state,
  message,
  onClearCompleted,
  onClose,
  onRefresh,
  variant = "overlay",
}: {
  tasks: BackendTask[];
  state: "idle" | "loading" | "error";
  message: string | null;
  onClearCompleted: () => void;
  onClose: () => void;
  onRefresh: () => void;
  variant?: "overlay" | "window";
}) {
  const content = (
    <aside aria-label="后端任务托盘" className={`backend-task-tray ${variant}`} role="complementary">
      <div className="task-tray-header">
        <div>
          <span className="eyebrow">Rust 后端</span>
          <h2>后端任务</h2>
        </div>
        <button aria-label="关闭后端任务托盘" type="button" onClick={onClose}>
          <X size={16} />
        </button>
      </div>
      <div className="task-tray-actions">
        <button type="button" onClick={onRefresh}>
          刷新
        </button>
        <button type="button" onClick={onClearCompleted}>
          清理已完成
        </button>
      </div>
      {state === "loading" ? (
        <div className="task-empty">正在读取后端任务...</div>
      ) : state === "error" ? (
        <div className="task-empty error">{message ?? "后端未连接"}</div>
      ) : tasks.length === 0 ? (
        <div className="task-empty">暂无后端任务</div>
      ) : (
        <div className="backend-task-list">
          {tasks.map((task) => (
            <article className={`backend-task-card ${task.status}`} key={task.id}>
              <div className="task-card-title">
                <div>
                  <h3>{task.title}</h3>
                  <span>{task.kind}</span>
                </div>
                <strong>{task.status}</strong>
              </div>
              <p>{task.message}</p>
              <div className="progress-bar" aria-label={`${task.progress}%`}>
                <span style={{ width: `${task.progress}%` }} />
              </div>
              <div className="task-card-meta">
                <span>ID: {task.id}</span>
                <span>{task.finishedAt ? "已结束" : "进行中"}</span>
              </div>
            </article>
          ))}
        </div>
      )}
    </aside>
  );

  if (variant === "window") {
    return <div className="task-window-shell">{content}</div>;
  }

  return (
    <div className="task-tray-backdrop">
      {content}
    </div>
  );
}



export default function App() {
  const mobileRuntime = isMobileRuntime();
  const [route, setRoute] = useState<Route>(() => parseRoute());
  useForegroundSync("projectId" in route ? route.projectId : undefined);
  const routeRef = useRef(route);
  routeRef.current = route;
  const acceptedHashRef = useRef(window.location.hash);
  const leaveGuardRef = useRef<(() => Promise<boolean>) | null>(null);
  const datasetsRequestRef = useRef(0);
  const registerLeaveGuard = useCallback((guard: (() => Promise<boolean>) | null) => {
    leaveGuardRef.current = guard;
  }, []);
  const [projects, setProjects] = useState<DatasetProject[]>([]);
  const [builtinDatasets, setBuiltinDatasets] = useState<BuiltinDataset[]>([]);
  const [runtimeState, setRuntimeState] = useState<DataRuntimeState>("ready");
  const [runtimeMessage, setRuntimeMessage] = useState<string | null>(null);
  const [createDialogOpen, setCreateDialogOpen] = useState(false);
  const [creationProjectId, setCreationProjectId] = useState<string | undefined>();
  const [dataSubmitScopeId, setDataSubmitScopeId] = useState<string | undefined>();
  const [dataSubmitDatasetId, setDataSubmitDatasetId] = useState<string | undefined>();
  const [workspaceRevision, setWorkspaceRevision] = useState(0);
  const [dataSubmitOpen, setDataSubmitOpen] = useState(false);
  const [projectInfoOpen, setProjectInfoOpen] = useState(false);
  const [collaborationOpen, setCollaborationOpen] = useState(false);
  const [createForm, setCreateForm] = useState<DatasetCreationForm>(defaultDatasetCreationForm);
  const [taskTrayOpen, setTaskTrayOpen] = useState(false);
  const [backendTasks, setBackendTasks] = useState<BackendTask[]>([]);
  const [taskTrayState, setTaskTrayState] = useState<"idle" | "loading" | "error">("idle");
  const [taskTrayMessage, setTaskTrayMessage] = useState<string | null>(null);
  const [projectTopbarContext, setProjectTopbarContext] = useState<ProjectTopbarContext | null>(null);
  const [backendConnection, setBackendConnection] = useState<BackendConnection>({
    mode: "checking",
    label: "连接中",
    health: null,
  });

  useEffect(() => {
    let sequence = 0;
    const onHashChange = () => {
      const requestedHash = window.location.hash;
      const next = parseRoute();
      const current = routeRef.current;
      const leavingWorkspace = current.name === "annotate"
        && (next.name !== "annotate" || next.projectId !== current.projectId);
      const guard = leaveGuardRef.current;
      if (leavingWorkspace && guard) {
        const request = ++sequence;
        void guard().then((allowed) => {
          if (request !== sequence || window.location.hash !== requestedHash) return;
          if (!allowed) {
            window.history.replaceState(null, "", acceptedHashRef.current || "#/datasets");
            return;
          }
          acceptedHashRef.current = requestedHash;
          setRoute(next);
        }).catch(() => {
          if (request === sequence) window.history.replaceState(null, "", acceptedHashRef.current || "#/datasets");
        });
        return;
      }
      sequence += 1;
      acceptedHashRef.current = requestedHash;
      setRoute(next);
    };
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);

  useEffect(() => {
    setProjectTopbarContext(null);
  }, [route.name === "project" ? route.projectId : route.name]);

  useEffect(() => {
    const handleSessionChange = () => {
      // A native logout/login can finish while the dataset view is still
      // mounted. Clear account-scoped data synchronously so the previous
      // account's remote cache and offline metadata cannot flash in the new
      // session while Rust reloads the visible local projects.
      setProjects([]);
      setRuntimeMessage(null);
      setRuntimeState("ready");
      clearAnnotationReturnRoutes();
      clearDatasetViewStates();

      const currentRoute = routeRef.current;
      if (currentRoute.name !== "datasets") {
        // Authentication changes must not be blocked by an annotation leave
        // guard: the old account/project route is no longer valid.
        leaveGuardRef.current = null;
        const nextHash = "#/projects";
        window.history.replaceState(null, "", nextHash);
        acceptedHashRef.current = nextHash;
        setRoute({ name: "datasets" });
        return;
      }

      setWorkspaceRevision((value) => value + 1);
    };
    window.addEventListener(SESSION_CHANGE_EVENT, handleSessionChange);
    return () => window.removeEventListener(SESSION_CHANGE_EVENT, handleSessionChange);
  }, []);

  const updateProjectTopbarContext = useCallback((context: ProjectTopbarContext | null) => {
    setProjectTopbarContext(context);
  }, []);

  useEffect(() => {
    if (dataSubmitOpen || collaborationOpen) void refreshDatasets();
  }, [dataSubmitOpen, collaborationOpen, dataSubmitScopeId]);

  useEffect(() => {
    let cancelled = false;
    detectBackendConnection().then((connection) => {
      if (!cancelled) {
        setBackendConnection(connection);
        if (connection.mode === "unavailable") setRuntimeState("backend-unavailable");
      }
    });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    if (route.name === "backendTasks") {
      loadBackendTasks();
    }
  }, [route.name]);

  async function refreshDatasets() {
    const requestId = ++datasetsRequestRef.current;
    setRuntimeMessage(null);
    try {
      const [builtin, currentProjects] = await Promise.all([
        listBuiltinDatasets(),
        dataSubmitScopeId ? listDataProjectDatasets(dataSubmitScopeId) : listDatasetProjects(),
      ]);
      if (requestId !== datasetsRequestRef.current) return;
      setBuiltinDatasets(builtin);
      setProjects(currentProjects);
      setRuntimeState("ready");
    } catch (error) {
      if (requestId !== datasetsRequestRef.current) return;
      setBuiltinDatasets([]);
      setProjects([]);
      if (isBackendUnavailableError(error)) {
        setRuntimeState("backend-unavailable");
        return;
      }
      setRuntimeState("download-error");
      setRuntimeMessage(error instanceof Error ? error.message : String(error));
    }
  }

  async function handleDownload(datasetKey: string) {
    setRuntimeState("downloading");
    setRuntimeMessage(null);
    try {
      await downloadTestDataset(datasetKey);
      await refreshDatasets();
    } catch (error) {
      if (isBackendUnavailableError(error)) {
        setRuntimeState("backend-unavailable");
        return;
      }
      setRuntimeState("download-error");
      setRuntimeMessage(error instanceof Error ? error.message : String(error));
    }
  }

  async function handleSubmitDownload(datasetKey: string) {
    setDataSubmitOpen(false);
    await handleDownload(datasetKey);
  }

  async function handleImportImages(projectId: string, sourcePath: string) {
    await importImages(projectId, sourcePath);
    setDataSubmitOpen(false);
    await refreshDatasets();
  }

  async function handleImportYolo(projectId: string, sourcePath: string) {
    await importYoloDataset(projectId, sourcePath);
    setDataSubmitOpen(false);
    await refreshDatasets();
  }

  async function handleImportFiles(projectId: string, sourcePaths: string[]) {
    await importFiles(projectId, sourcePaths);
    setDataSubmitOpen(false);
    await refreshDatasets();
  }

  async function handleOpenLocalDataset(sourcePath: string, datasetType: string) {
    const project = await openLocalDataset(sourcePath, datasetType);
    setDataSubmitOpen(false);
    if (dataSubmitScopeId) {
      const owner = (await listDataProjects()).find((item) => item.datasetIds.includes(project.id));
      if (!owner || owner.id === dataSubmitScopeId) await assignDatasetToDataProject(dataSubmitScopeId, project.id);
    }
    setWorkspaceRevision((value) => value + 1);
    navigate(workspaceRouteHash({ kind: "dataset", dataProjectId: dataSubmitScopeId, datasetId: project.id, tab: "图片" }));
  }

  async function handlePickDataSource(selectionType: "folder" | "files") {
    return pickDataSource(selectionType);
  }

  async function handleAnalyzeDataSource(sourcePaths: string[]) {
    return analyzeDataSource(sourcePaths);
  }

  async function annotateProject(project: DatasetProject) {
    const images = await listProjectImages(project.id, undefined, { offset: 0, limit: 1 });
    openProjectAnnotationRoute(project.id, images[0]?.id);
  }

  async function openProjectWindow(project: DatasetProject) {
    const images = await listProjectImages(project.id, undefined, { offset: 0, limit: 1 });
    if (mobileRuntime) {
      openProjectAnnotationRoute(project.id, images[0]?.id);
      return;
    }
    await openAnnotationWindow(project.id, images[0]?.id);
  }

  async function handleCreateDataset() {
    const created = await createDatasetProject(createForm.name, createForm.datasetType, createForm.demoTemplate);
    if (creationProjectId) await assignDatasetToDataProject(creationProjectId, created.id);
    setCreateDialogOpen(false);
    setCreateForm(defaultDatasetCreationForm);
    invalidateImageAssets();
    setWorkspaceRevision((value) => value + 1);
    if (creationProjectId) navigate(workspaceRouteHash({ kind: "dataProject", dataProjectId: creationProjectId, section: "data" }));
  }

  async function loadBackendTasks() {
    setTaskTrayState("loading");
    setTaskTrayMessage(null);
    try {
      setBackendTasks(await listBackendTasks());
      setTaskTrayState("idle");
    } catch (error) {
      setBackendTasks([]);
      setTaskTrayState("error");
      setTaskTrayMessage(
        isBackendUnavailableError(error)
          ? "后端未连接，请在 Tauri 桌面环境启动应用。"
          : error instanceof Error
            ? error.message
            : String(error),
      );
    }
  }

  async function openBackendTasks() {
    try {
      await openBackendTaskTray();
    } catch (error) {
      setTaskTrayOpen(true);
      setTaskTrayState(isBackendUnavailableError(error) ? "error" : "loading");
      setTaskTrayMessage(
        isBackendUnavailableError(error)
          ? "后端未连接，请在 Tauri 桌面环境启动应用。"
          : null,
      );
      if (!isBackendUnavailableError(error)) {
        await loadBackendTasks();
      }
    }
  }

  function openProjectTab(projectId: string, tab: ProjectTab) {
    const current = routeRef.current;
    navigate(workspaceRouteHash({ kind: "dataset", dataProjectId: current.name === "project" ? current.dataProjectId : current.name === "dataProject" ? current.dataProjectId : undefined, datasetId: projectId, tab }));
  }

  function openProjectAnnotationRoute(projectId: string, imageId?: string) {
    rememberAnnotationReturn(projectId, window.location.hash);
    navigate(workspaceRouteHash({ kind: "annotate", datasetId: projectId, imageId }));
  }

  async function returnFromAnnotation(datasetId: string) {
    const target = annotationReturnRoute(datasetId);
    if (target !== "#/projects") { navigate(target); return; }
    try {
      const owner = (await listDataProjects()).find((project) => project.datasetIds.includes(datasetId));
      navigate(owner ? workspaceRouteHash({ kind: "dataProject", dataProjectId: owner.id, section: "data" }) : target);
    } catch { navigate(target); }
  }

  function openDataProject(dataProjectId: string, section: DataProjectSection = "data") {
    navigate(workspaceRouteHash({ kind: "dataProject", dataProjectId, section }));
  }

  function openDataSubmit(dataProjectId?: string, datasetId?: string) {
    setDataSubmitScopeId(dataProjectId);
    setDataSubmitDatasetId(datasetId);
    setDataSubmitOpen(true);
  }

  function openDatasetManagement(datasetId: string) {
    const current = routeRef.current;
    navigate(workspaceRouteHash({ kind: "dataset", dataProjectId: current.name === "dataProject" ? current.dataProjectId : undefined, datasetId, tab: "图片" }));
  }

  async function clearCompletedTasks() {
    try {
      await clearCompletedBackendTasks();
      await loadBackendTasks();
    } catch (error) {
      setTaskTrayState("error");
      setTaskTrayMessage(
        isBackendUnavailableError(error)
          ? "后端未连接，请在 Tauri 桌面环境启动应用。"
          : error instanceof Error
            ? error.message
            : String(error),
      );
    }
  }

  if (route.name === "backendTasks") {
    const taskTray = (
      <BackendTaskTray
        message={taskTrayMessage}
        onClearCompleted={clearCompletedTasks}
        onClose={() => runDesktopCommand("close_window")}
        onRefresh={loadBackendTasks}
        state={taskTrayState}
        tasks={backendTasks}
        variant="window"
      />
    );
    // The desktop backend-task monitor is a process tray, not the mobile
    // collaboration task queue. Do not mount it inside the mobile shell: it
    // creates a fixed overlay above the mobile bottom navigation.
    return mobileRuntime ? <MobileWorkspaceShell route={window.location.hash} onNavigate={navigate}>{null}</MobileWorkspaceShell> : taskTray;
  }

  if (route.name === "annotate") {
    const annotationWorkspace = <AnnotationWorkspace imageId={route.imageId} projectId={route.projectId} showWindowControls={!mobileRuntime && backendConnection.mode === "tauri"} mobileRuntime={mobileRuntime} registerLeaveGuard={registerLeaveGuard} onBack={() => void returnFromAnnotation(route.projectId)} onNavigateImage={(imageId) => navigate(workspaceRouteHash({ kind: "annotate", datasetId: route.projectId, imageId }))} />;
    return mobileRuntime ? <MobileWorkspaceShell route={window.location.hash} projectId={route.projectId} onNavigate={navigate}>{annotationWorkspace}</MobileWorkspaceShell> : annotationWorkspace;
  }

  const desktopWorkspace = (
    <div className="app-shell">
      <TopBar
        activeProjectId={route.name === "project" ? route.projectId : undefined}
        activeProjectContext={route.name === "project" ? projectTopbarContext : null}
        backendConnection={backendConnection}
        onBackendTasks={openBackendTasks}
        onDataSubmit={() => openDataSubmit(route.name === "project" ? route.dataProjectId : undefined, route.name === "project" ? route.projectId : undefined)}
        onDatasets={() => route.name === "project" && route.dataProjectId ? openDataProject(route.dataProjectId) : navigate("#/projects")}
        onProjectAnnotate={openProjectAnnotationRoute}
        onProjectOpenWindow={openProjectWindow}
        onProjectTab={openProjectTab}
        onCollaboration={() => setCollaborationOpen(true)}
        showCollaboration={!mobileRuntime}
      />
      <div className="app-body">
        <IconRail onHome={() => navigate("#/projects")} onTasks={openBackendTasks} inProjects />
        {(route.name === "datasets" || route.name === "dataProject") && (
          <DataProjectHome
            key={`${route.name === "dataProject" ? route.dataProjectId : "home"}:${workspaceRevision}`}
            dataProjectId={route.name === "dataProject" ? route.dataProjectId : undefined}
            section={route.name === "dataProject" ? route.section : "data"}
            onNavigateProject={openDataProject} onNavigateHome={() => navigate("#/projects")}
            onDatasetsChanged={() => invalidateImageAssets()}
            onCreateDataset={(id) => { setCreationProjectId(id); setCreateDialogOpen(true); }}
            onUploadImages={openDataSubmit}
            onAnnotate={annotateProject}
            onDownload={handleDownload}
            onInfo={() => setProjectInfoOpen(true)}
            onOpen={openDatasetManagement}
            onOpenWindow={openProjectWindow}
            projectImages={{}}
            projects={[]}
            runtimeMessage={runtimeMessage}
            runtimeState={runtimeState}
            testDatasetDownloaded={builtinDatasets.some((dataset) => dataset.key === defaultTestDatasetKey && dataset.downloaded)}
            testDatasetKey={defaultTestDatasetKey}
          />
        )}
        {route.name === "project" && (
          <ProjectWorkspace
            key={route.projectId}
            onProjectContextChange={updateProjectTopbarContext}
            onAnnotate={openProjectAnnotationRoute}
            onBack={() => route.dataProjectId ? openDataProject(route.dataProjectId) : navigate("#/projects")}
            onTabChange={(tab) => openProjectTab(route.projectId, tab)}
            mobileRuntime={mobileRuntime}
            projectId={route.projectId}
            routeTab={route.tab}
          />
        )}
      </div>
      {createDialogOpen ? (
        <CreateDatasetDialog
          form={createForm}
          onCancel={() => setCreateDialogOpen(false)}
          onChange={setCreateForm}
          onCreate={handleCreateDataset}
        />
      ) : null}
      {dataSubmitOpen ? (
        <DataSubmitDialog
          datasets={builtinDatasets}
          projects={projects}
          initialDatasetId={dataSubmitDatasetId}
          onAnalyzeSource={handleAnalyzeDataSource}
          onCancel={() => setDataSubmitOpen(false)}
          onDownload={handleSubmitDownload}
          onImportFiles={handleImportFiles}
          onImportImages={handleImportImages}
          onImportYolo={handleImportYolo}
          onOpenLocal={handleOpenLocalDataset}
          onPickSource={handlePickDataSource}
        />
      ) : null}
      {projectInfoOpen ? <ProjectInfoDialog onClose={() => setProjectInfoOpen(false)} /> : null}
      {collaborationOpen ? <CollaborationPanel localProjects={projects} onClose={() => setCollaborationOpen(false)} onOpenProject={(localProjectId) => { setCollaborationOpen(false); openDatasetManagement(localProjectId); }} /> : null}
      {taskTrayOpen ? (
        <BackendTaskTray
          message={taskTrayMessage}
          onClearCompleted={clearCompletedTasks}
          onClose={() => setTaskTrayOpen(false)}
          onRefresh={loadBackendTasks}
          state={taskTrayState}
          tasks={backendTasks}
        />
      ) : null}
    </div>
  );
  return mobileRuntime ? <MobileWorkspaceShell route={window.location.hash} projectId={route.name === "project" ? route.projectId : undefined} onNavigate={navigate}>{desktopWorkspace}</MobileWorkspaceShell> : desktopWorkspace;
}

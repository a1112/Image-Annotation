import { ArrowLeft, CheckCircle2, LogIn, LogOut, RefreshCw, Upload } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import type { FormEvent } from "react";
import { DataProjectHome, AnnotationWorkspace, ProjectWorkspace } from "../../shared/LazyWorkspaces";
import type { ProjectTopbarContext } from "../projects/types";
import { clearDatasetViewStates } from "../projects/viewState";
import { annotationReturnRoute, clearAnnotationReturnRoutes, parseWorkspaceRoute, rememberAnnotationReturn, workspaceRouteHash, type WorkspaceRoute } from "../../shared/workspaceRoutes";
import type { DataProjectSection } from "../../types/dataProjects";
import { ImageTransformsPanel } from "./ImageTransformsPanel";
import { listDataProjects, listDataProjectDatasets, getDataProjectDatasetGroups } from "../../api/dataProjects";
import {
  createWebDataset,
  exportWebDataProject,
  getWebSession,
  loginWeb,
  logoutWeb,
  uploadWebDatasetImages,
  WebApiError,
  type WebSession,
} from "../../api/web";
import {
  configureBackendTransport,
  invalidateImageAssets,
} from "../../api/tauri";
import type { DatasetProject } from "../../types/domain";
import type { DataProject } from "../../types/dataProjects";
import { dispatchSessionChange, SESSION_CHANGE_EVENT } from "../../shared/sessionEvents";
import "./web.css";

type WebRoute = Exclude<WorkspaceRoute, { kind: "backendTasks" }>;

const DEVICE_STORAGE_KEY = "image-annotation:web-device-id";

function routeFromHash(hash: string): WebRoute {
  const route = parseWorkspaceRoute(hash);
  return route.kind === "backendTasks" ? { kind: "home" } : route;
}

function routeToHash(route: WebRoute): string {
  return workspaceRouteHash(route, true);
}

function webErrorMessage(error: unknown, fallback: string): string {
  if (error instanceof WebApiError) {
    if (error.status === 401) return "登录已过期，请重新登录。";
    if (error.status === 403) return `权限不足：${error.message}`;
    if (error.status === 0) return `远端服务不可用：${error.message}`;
    return error.message;
  }
  return error instanceof Error ? error.message : fallback;
}

function deviceId(): string {
  try {
    const existing = window.localStorage.getItem(DEVICE_STORAGE_KEY);
    if (existing) return existing;
    const generated = typeof window.crypto?.randomUUID === "function"
      ? window.crypto.randomUUID()
      : `web-${Date.now()}-${Math.random().toString(36).slice(2)}`;
    window.localStorage.setItem(DEVICE_STORAGE_KEY, generated);
    return generated;
  } catch {
    return `web-${Date.now()}-${Math.random().toString(36).slice(2)}`;
  }
}

function extractCreatedDatasetId(value: unknown): string | null {
  if (!value || typeof value !== "object") return null;
  const record = value as Record<string, unknown>;
  if (typeof record.id === "string" && record.id) return record.id;
  if (typeof record.projectId === "string" && record.projectId) return record.projectId;
  if (record.dataset && typeof record.dataset === "object") {
    const nested = record.dataset as Record<string, unknown>;
    if (typeof nested.id === "string" && nested.id) return nested.id;
  }
  return null;
}

function navigateWeb(route: WebRoute, setRoute?: (route: WebRoute) => void) {
  const hash = routeToHash(route);
  if (window.location.hash === hash) {
    setRoute?.(route);
  } else {
    window.location.hash = hash;
  }
}

function downloadJsonFile(fileName: string, value: unknown): void {
  const payload = JSON.stringify(value, null, 2);
  const blob = new Blob([payload], { type: "application/json;charset=utf-8" });
  const objectUrl = typeof URL.createObjectURL === "function"
    ? URL.createObjectURL(blob)
    : `data:application/json;charset=utf-8,${encodeURIComponent(payload)}`;
  const link = document.createElement("a");
  link.href = objectUrl;
  link.download = fileName;
  link.rel = "noopener";
  document.body.appendChild(link);
  link.click();
  link.remove();
  if (objectUrl.startsWith("blob:") && typeof URL.revokeObjectURL === "function") URL.revokeObjectURL(objectUrl);
}

function WebLoginScreen({ onAuthenticated }: { onAuthenticated: (session: WebSession) => void }) {
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit(event: FormEvent) {
    event.preventDefault();
    if (!email.trim() || !password) {
      setError("请输入邮箱和密码。");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const session = await loginWeb({
        email: email.trim(),
        password,
        deviceId: deviceId(),
        deviceName: "Image Annotation Web",
      });
      if (!session.authenticated || !session.user) {
        throw new Error("登录响应未建立有效会话");
      }
      onAuthenticated(session);
    } catch (cause) {
      setError(webErrorMessage(cause, "登录失败，请检查账号或服务地址。"));
    } finally {
      setBusy(false);
    }
  }

  return (
    <main className="web-auth-page">
      <section className="web-auth-card" aria-labelledby="web-login-title">
        <div className="web-auth-mark"><LogIn size={21} /></div>
        <span className="web-eyebrow">IMAGE ANNOTATION / WEB</span>
        <h1 id="web-login-title">登录图像标注</h1>
        <form className="web-login-form" onSubmit={(event) => void submit(event)}>
          <label><span>邮箱</span><input autoComplete="username" autoFocus value={email} onChange={(event) => setEmail(event.target.value)} type="email" /></label>
          <label><span>密码</span><input autoComplete="current-password" value={password} onChange={(event) => setPassword(event.target.value)} type="password" /></label>
          {error ? <div className="web-alert error" role="alert">{error}</div> : null}
          <button className="primary web-login-button" disabled={busy || !email.trim() || !password} type="submit">
            <LogIn size={16} />{busy ? "正在登录…" : "登录"}
          </button>
        </form>
      </section>
    </main>
  );
}

function WebDatasetDialog({
  mode,
  projects,
  onClose,
  onChanged,
  activeDataProjectId,
  initialDatasetId,
}: {
  activeDataProjectId?: string;
  initialDatasetId?: string;
  mode: "create" | "upload" | null;
  projects: DatasetProject[];
  onClose: () => void;
  onChanged: () => Promise<void>;
}) {
  const [name, setName] = useState("");
  const [datasetType, setDatasetType] = useState("yolo-detect");
  const [demoTemplate, setDemoTemplate] = useState("empty");
  const [dataProjectId, setDataProjectId] = useState("");
  const [datasetId, setDatasetId] = useState("");
  const [files, setFiles] = useState<File[]>([]);
  const [dataProjects, setDataProjects] = useState<DataProject[]>([]);
  const [loadingProjects, setLoadingProjects] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const requestRef = useRef(0);



  useEffect(() => {
    if (!mode) return;
    setName("");
    setDatasetType("yolo-detect");
    setDemoTemplate("empty");
    setDataProjectId(activeDataProjectId ?? "");
    setDatasetId(initialDatasetId ?? projects[0]?.id ?? "");
    setFiles([]);
    setError(null);
    setMessage(null);
    const request = ++requestRef.current;
    setLoadingProjects(true);
    void listDataProjects()
      .then((loaded) => {
        if (requestRef.current !== request) return;
        if (!Array.isArray(loaded)) throw new Error("数据项目响应格式无效");
        setDataProjects(loaded);
      })
      .catch((cause) => {
        if (requestRef.current === request) setError(webErrorMessage(cause, "读取数据项目失败。"));
      })
      .finally(() => {
        if (requestRef.current === request) setLoadingProjects(false);
      });
    return () => { requestRef.current += 1; };
  }, [mode, activeDataProjectId, initialDatasetId]);

  if (!mode) return null;

  async function submit(event: FormEvent) {
    event.preventDefault();
    setBusy(true);
    setError(null);
    setMessage(null);
    try {
      if (mode === "create") {
        if (!name.trim()) {
          setError("请输入数据集名称。");
          return;
        }
        const created = await createWebDataset({
          name: name.trim(),
          datasetType,
          demoTemplate,
          dataProjectId: dataProjectId || null,
        });
        const createdId = extractCreatedDatasetId(created);
        if (files.length > 0) {
          if (!createdId) throw new Error("创建响应缺少数据集 ID，无法上传文件。请刷新后重试。");
          await uploadWebDatasetImages(createdId, files);
        }
        setMessage(files.length ? `数据集已创建并上传 ${files.length} 张图像。` : "数据集已创建。");
      } else {
        if (!datasetId) {
          setError("请选择要上传的数据集。");
          return;
        }
        if (files.length === 0) {
          setError("请选择至少一个图像文件。");
          return;
        }
        await uploadWebDatasetImages(datasetId, files);
        setMessage(`已上传 ${files.length} 张图像。`);
      }
      await onChanged();
      setFiles([]);
      if (mode === "create") setName("");
    } catch (cause) {
      setError(webErrorMessage(cause, mode === "create" ? "创建数据集失败。" : "上传图像失败。"));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="web-modal-backdrop" role="presentation" onMouseDown={(event) => { if (event.currentTarget === event.target && !busy) onClose(); }}>
      <section className="web-modal" aria-labelledby="web-dataset-dialog-title" role="dialog" aria-modal="true">
        <div className="web-modal-header"><div><h2 id="web-dataset-dialog-title">{mode === "create" ? "新建数据集" : "上传图像"}</h2></div><button type="button" disabled={busy} onClick={onClose}>关闭</button></div>
        <form className="web-dataset-form" onSubmit={(event) => void submit(event)}>
          {mode === "create" ? <>
            <label><span>数据集名称</span><input autoFocus value={name} onChange={(event) => setName(event.target.value)} placeholder="例如：道路检测训练集" /></label>
            <label><span>数据类型</span><select value={datasetType} onChange={(event) => setDatasetType(event.target.value)}><option value="yolo-detect">目标检测 / YOLO BBox</option><option value="yolo-seg">实例分割 / YOLO Polygon</option><option value="image-classification">图像分类</option></select></label>
            <label><span>初始化模板</span><select value={demoTemplate} onChange={(event) => setDemoTemplate(event.target.value)}><option value="empty">空数据集</option><option value="demo-bbox">Demo BBox</option><option value="demo-polygon">Demo Polygon</option><option value="demo-classification">Demo Classification</option></select></label>
            {!activeDataProjectId ? <label><span>数据项目</span><select disabled={loadingProjects} value={dataProjectId} onChange={(event) => setDataProjectId(event.target.value)}><option value="">选择项目</option>{dataProjects.map((project) => <option key={project.id} value={project.id}>{project.name}</option>)}</select></label> : null}
          </> : <label><span>目标数据集</span><select value={datasetId} onChange={(event) => setDatasetId(event.target.value)}><option value="">选择数据集</option>{projects.map((project) => <option key={project.id} value={project.id}>{project.name}</option>)}</select></label>}
          <label><span>图像文件</span><input accept="image/*" multiple type="file" onChange={(event) => setFiles(Array.from(event.target.files ?? []))} /></label>
          {files.length ? <small className="web-file-hint">已选择 {files.length} 张图像</small> : null}
          {error ? <div className="web-alert error" role="alert">{error}</div> : null}
          {message ? <div className="web-alert success" role="status"><CheckCircle2 size={16} />{message}</div> : null}
          <div className="web-modal-actions"><button type="button" disabled={busy} onClick={onClose}>取消</button><button className="primary" disabled={busy || (mode === "create" ? !name.trim() : !datasetId || !files.length)} type="submit">{busy ? "处理中…" : mode === "create" ? "创建数据集" : "上传图像"}</button></div>
        </form>
      </section>
    </div>
  );
}

function WebAuthenticatedWorkspace({ session, onSignedOut }: { session: WebSession; onSignedOut: () => void }) {
  const [route, setRoute] = useState<WebRoute>(() => routeFromHash(window.location.hash));
  const routeRef = useRef(route);
  routeRef.current = route;
  const acceptedHashRef = useRef(routeToHash(route));
  const leaveGuardRef = useRef<(() => Promise<boolean>) | null>(null);
  const navigationRef = useRef(0);
  const [workspaceRevision, setWorkspaceRevision] = useState(0);
  const [notice, setNotice] = useState<string | null>(null);
  const [exportingDataProjectId, setExportingDataProjectId] = useState<string | null>(null);
  const [dialog, setDialog] = useState<"create" | "upload" | null>(null);
  const [dialogProjectId, setDialogProjectId] = useState<string | undefined>();
  const [dialogDatasetId, setDialogDatasetId] = useState<string | undefined>();
  const [uploadDatasets, setUploadDatasets] = useState<DatasetProject[]>([]);
  const [refreshing, setRefreshing] = useState(false);
  const [datasetContext, setDatasetContext] = useState<ProjectTopbarContext | null>(null);
  const requestRef = useRef(0);

  const permissionProjectId = route.kind === "dataset" || route.kind === "annotate"
    ? (route.kind === "dataset" ? route.dataProjectId : undefined) ?? route.datasetId.split("::")[0]
    : undefined;
  const [projectPermission, setProjectPermission] = useState<{ projectId: string; canEdit: boolean } | null>(null);
  const canEditDataset = projectPermission && projectPermission.projectId === permissionProjectId ? projectPermission.canEdit : undefined;
  useEffect(() => {
    if (!permissionProjectId) return;
    let active = true;
    void getDataProjectDatasetGroups(permissionProjectId).then((state) => {
      if (active) setProjectPermission({ projectId: permissionProjectId, canEdit: state.canEdit });
    }).catch(() => {
      if (active) setProjectPermission({ projectId: permissionProjectId, canEdit: false });
    });
    return () => { active = false; };
  }, [permissionProjectId, workspaceRevision]);

  const registerLeaveGuard = useCallback((guard: (() => Promise<boolean>) | null) => {
    leaveGuardRef.current = guard;
  }, []);

  useEffect(() => {
    const canonical = routeToHash(routeRef.current);
    if (window.location.hash !== canonical) window.history.replaceState(null, "", canonical);
    acceptedHashRef.current = canonical;
    const onHashChange = () => {
      const requestedHash = window.location.hash;
      const next = routeFromHash(requestedHash);
      const canonicalHash = routeToHash(next);
      const current = routeRef.current;
      const leavingAnnotation = current.kind === "annotate"
        && (next.kind !== "annotate" || next.datasetId !== current.datasetId);
      const guard = leaveGuardRef.current;
      const accept = () => {
        acceptedHashRef.current = canonicalHash;
        if (window.location.hash !== canonicalHash) window.history.replaceState(null, "", canonicalHash);
        setRoute(next);
      };
      if (leavingAnnotation && guard) {
        const request = ++navigationRef.current;
        void guard().then((allowed) => {
          if (request !== navigationRef.current || window.location.hash !== requestedHash) return;
          if (allowed) accept();
          else window.history.replaceState(null, "", acceptedHashRef.current);
        }).catch(() => { if (request === navigationRef.current) window.history.replaceState(null, "", acceptedHashRef.current); });
        return;
      }
      navigationRef.current += 1;
      accept();
    };
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);

  useEffect(() => {
    setDatasetContext(null);
  }, [route.kind === "dataset" ? route.datasetId : route.kind]);

  useEffect(() => {
    const handleSessionChange = (event: Event) => {
      if ((event as CustomEvent<{ authenticated?: boolean }>).detail?.authenticated === false) {
        requestRef.current += 1;
        clearAnnotationReturnRoutes();
        clearDatasetViewStates();
        onSignedOut();
      }
    };
    window.addEventListener(SESSION_CHANGE_EVENT, handleSessionChange);
    return () => { requestRef.current += 1; window.removeEventListener(SESSION_CHANGE_EVENT, handleSessionChange); };
  }, [onSignedOut]);

  const goHome = useCallback(() => navigateWeb({ kind: "home" }, setRoute), []);
  const openDataProject = useCallback((dataProjectId: string, section: DataProjectSection = "data") => {
    navigateWeb({ kind: "dataProject", dataProjectId, section }, setRoute);
  }, []);
  const openAnnotation = useCallback((datasetId: string, imageId?: string) => {
    rememberAnnotationReturn(datasetId, window.location.hash);
    navigateWeb({ kind: "annotate", datasetId, imageId }, setRoute);
  }, []);
  const openDatasetManagement = useCallback((datasetId: string) => {
    const current = routeRef.current;
    const dataProjectId = current.kind === "dataProject" ? current.dataProjectId
      : datasetId.includes("::") ? datasetId.slice(0, datasetId.indexOf("::")) : undefined;
    navigateWeb({ kind: "dataset", dataProjectId, datasetId, tab: "图片" }, setRoute);
  }, []);
  const onDatasetsChanged = useCallback(async () => {
    invalidateImageAssets();
    setWorkspaceRevision((value) => value + 1);
    if (dialogProjectId) setUploadDatasets(await listDataProjectDatasets(dialogProjectId));
  }, [dialogProjectId]);

  const openCreateDataset = useCallback((dataProjectId?: string) => {
    setDialogProjectId(dataProjectId);
    setDialogDatasetId(undefined);
    setDialog("create");
  }, []);

  const openUpload = useCallback(async (dataProjectId: string, datasetId?: string) => {
    const request = ++requestRef.current;
    setRefreshing(true);
    setNotice(null);
    try {
      const datasets = await listDataProjectDatasets(dataProjectId);
      if (request !== requestRef.current) return;
      setUploadDatasets(datasets);
      setDialogProjectId(dataProjectId);
      setDialogDatasetId(datasetId);
      setDialog("upload");
    } catch (cause) { if (request === requestRef.current) setNotice(webErrorMessage(cause, "读取项目数据集失败。")); }
    finally { if (request === requestRef.current) setRefreshing(false); }
  }, []);

  const exportDataProject = useCallback(async (dataProjectId: string) => {
    setExportingDataProjectId(dataProjectId);
    setNotice(null);
    try {
      const result = await exportWebDataProject(dataProjectId);
      if (result.status !== "succeeded") throw new Error(`导出未完成（${result.status}）`);
      downloadJsonFile("data-project-manifest.json", result.manifest);
      setNotice("已下载图像与标签清单。");
    } catch (cause) { setNotice(webErrorMessage(cause, "导出图像与标签清单失败。")); }
    finally { setExportingDataProjectId(null); }
  }, []);

  async function signOut() {
    if (route.kind === "annotate" && leaveGuardRef.current && !(await leaveGuardRef.current())) return;
    try { await logoutWeb(); } catch { /* Clear the local session even when the server is unavailable. */ }
    clearAnnotationReturnRoutes();
    clearDatasetViewStates();
    onSignedOut();
    dispatchSessionChange({ authenticated: false });
  }

  const userLabel = session.user?.displayName || session.user?.email || "已登录用户";
  const datasetParentId = route.kind === "dataset" ? route.dataProjectId : undefined;
  const goBack = () => {
    if (route.kind === "annotate") window.location.hash = annotationReturnRoute(route.datasetId, true);
    else if (route.kind === "dataset" && route.dataProjectId) openDataProject(route.dataProjectId);
    else goHome();
  };

  return (
    <div className={`web-app-shell workspace-${route.kind}`}>
      <header className="web-shell-header">
        <button className="web-brand" type="button" onClick={goHome}><span className="web-brand-mark">IA</span><span><strong>{route.kind === "annotate" ? "数据标注" : "数据项目"}</strong></span></button>
        {route.kind === "dataset" && datasetContext ? <span className="web-dataset-title">{datasetContext.project.name}</span> : null}
        <div className="web-shell-actions">
          <span className="web-session-chip" title={session.user?.email ?? undefined}>{userLabel}</span>
          {route.kind !== "home" ? <button type="button" onClick={goBack}><ArrowLeft size={15} />{route.kind === "annotate" ? "返回数据管理" : route.kind === "dataset" ? "返回项目" : "全部项目"}</button> : null}
          {route.kind === "dataset" && datasetParentId && canEditDataset ? <button type="button" onClick={() => void openUpload(datasetParentId, route.datasetId)}><Upload size={15} />上传图像</button> : null}
          {route.kind === "dataset" ? <button className="primary" disabled={!canEditDataset} title={canEditDataset === false ? "只读项目" : undefined} type="button" onClick={() => openAnnotation(route.datasetId, datasetContext?.firstImageId)}>开始标注</button> : null}
          {route.kind !== "annotate" ? <button aria-label="刷新工作区" disabled={refreshing} type="button" onClick={() => { invalidateImageAssets(); setWorkspaceRevision((value) => value + 1); }}><RefreshCw size={15} />刷新</button> : null}
          <button type="button" onClick={() => void signOut()}><LogOut size={15} />退出</button>
        </div>
      </header>
      {notice ? <div className="web-alert warning" role="status">{notice}</div> : null}
      {route.kind === "annotate" ? canEditDataset === undefined ? <main className="workspace-loading" role="status">正在确认项目权限…</main> : !canEditDataset ? <main className="workspace-loading" role="status">此项目为只读，请返回数据管理浏览图像与标注。</main> : <main className="web-annotation-view"><div className="web-annotation-content"><AnnotationWorkspace projectId={route.datasetId} imageId={route.imageId} showWindowControls={false} mobileRuntime registerLeaveGuard={registerLeaveGuard} onNavigateImage={(imageId) => navigateWeb({ kind: "annotate", datasetId: route.datasetId, imageId }, setRoute)} /></div></main>
        : route.kind === "dataset" ? <div className="web-dataset-management-view"><ProjectWorkspace key={`${route.datasetId}:${workspaceRevision}`} projectId={route.datasetId} routeTab={route.tab ?? "图片"} mobileRuntime browserRuntime viewScopeKey={`${session.user?.id}:${session.user?.organizationId ?? ""}`} onProjectContextChange={setDatasetContext} onAnnotate={openAnnotation} onBack={goBack} onTabChange={(tab) => navigateWeb({ ...route, tab }, setRoute)} />{canEditDataset ? <ImageTransformsPanel datasetId={route.datasetId} accountId={session.user?.id ?? undefined} /> : null}</div>
        : <main className="web-datasets-page"><div className="web-datasets-content"><DataProjectHome
            key={`${route.kind === "dataProject" ? route.dataProjectId : "home"}:${workspaceRevision}`}
            dataProjectId={route.kind === "dataProject" ? route.dataProjectId : undefined}
            section={route.kind === "dataProject" ? route.section : "data"}
            onNavigateProject={openDataProject} onNavigateHome={goHome}
            projects={[]} projectImages={{}} runtimeState="ready" runtimeMessage={null}
            testDatasetDownloaded testDatasetKey="" onDownload={() => undefined} onInfo={() => undefined}
            onCreateDataset={openCreateDataset} onUploadImages={(projectId, datasetId) => void openUpload(projectId, datasetId)}
            onAnnotate={(dataset) => openAnnotation(dataset.id)} onOpenWindow={(dataset) => openAnnotation(dataset.id)}
            onOpen={openDatasetManagement} onDatasetsChanged={() => invalidateImageAssets()}
            onExportDataProject={exportDataProject} exportingDataProjectId={exportingDataProjectId}
          /></div></main>}
      <WebDatasetDialog mode={dialog} projects={uploadDatasets} activeDataProjectId={dialogProjectId} initialDatasetId={dialogDatasetId} onClose={() => setDialog(null)} onChanged={onDatasetsChanged} />
    </div>
  );
}

export default function WebWorkspaceApp() {
  const [session, setSession] = useState<WebSession | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const sessionRequestRef = useRef(0);

  useEffect(() => {
    configureBackendTransport("web");
    document.body.classList.add("web-body");
    let active = true;
    const request = ++sessionRequestRef.current;
    void getWebSession()
      .then((next) => {
        if (!active || sessionRequestRef.current !== request) return;
        setSession(next.authenticated ? next : null);
        setError(null);
      })
      .catch((cause) => {
        if (!active || sessionRequestRef.current !== request) return;
        setSession(null);
        setError(webErrorMessage(cause, "无法连接远端服务。"));
      })
      .finally(() => { if (active && sessionRequestRef.current === request) setLoading(false); });
    return () => {
      active = false;
      sessionRequestRef.current += 1;
      configureBackendTransport("local");
      document.body.classList.remove("web-body");
    };
  }, []);

  useEffect(() => {
    const handleSessionChange = (event: Event) => {
      const detail = (event as CustomEvent<{ authenticated?: boolean }>).detail;
      if (detail?.authenticated === false) {
        sessionRequestRef.current += 1;
        setSession(null);
        setLoading(false);
        return;
      }
      if (detail?.authenticated !== true) return;
      const request = ++sessionRequestRef.current;
      setSession(null);
      setLoading(true);
      void getWebSession()
        .then((next) => {
          if (sessionRequestRef.current !== request) return;
          setSession(next.authenticated ? next : null);
          setError(null);
        })
        .catch((cause) => {
          if (sessionRequestRef.current !== request) return;
          setSession(null);
          setError(webErrorMessage(cause, "无法读取新的远端会话。"));
        })
        .finally(() => { if (sessionRequestRef.current === request) setLoading(false); });
    };
    window.addEventListener(SESSION_CHANGE_EVENT, handleSessionChange);
    return () => window.removeEventListener(SESSION_CHANGE_EVENT, handleSessionChange);
  }, []);

  const onSignedOut = useCallback(() => {
    setSession(null);
    setLoading(false);
  }, []);

  if (loading) {
    return <main className="web-auth-page"><section className="web-auth-card web-loading-card"><span className="web-spinner" /><h1>正在加载…</h1></section></main>;
  }
  if (!session) {
    return <><WebLoginScreen onAuthenticated={(next) => { setError(null); setSession(next); }} />{error ? <div className="web-auth-global-error" role="alert">{error}</div> : null}</>;
  }
  return <WebAuthenticatedWorkspace session={session} onSignedOut={onSignedOut} />;
}

export { routeFromHash, routeToHash, deviceId };

import {
  Check,
  CircleAlert,
  Cloud,
  CloudOff,
  Database,
  FolderKanban,
  ListTodo,
  LogIn,
  RefreshCw,
  Server,
  Settings2,
  ShieldAlert,
  Smartphone,
  UserRound,
  WifiOff,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { FormEvent, ReactNode } from "react";
import {
  authAcceptInvitation,
  authLogin,
  authLogout,
  authRefresh,
  authStatus,
  defaultRemoteServerUrl,
  deviceInfo,
  flushOfflineQueue,
  getAccountProfile,
  listRemoteProjects,
  offlineStatus,
  remoteProjectOpen,
  revokeRemoteDevice,
} from "../../api/mobile";
import {
  getProjectSyncSummary,
  listDatasetProjects,
  listIssues,
  listSyncConflicts,
  listTasks,
  resolveSyncConflict,
} from "../../api/tauri";
import type { AccountProfile, AuthSession, DeviceInfo, OfflineStatus, RemoteProjectSummary } from "../../types/mobile";
import type { AnnotationTask, DatasetProject, IssueRecord, SyncConflict, SyncSummary } from "../../types/domain";
import { remoteProjectRoleLabel, remoteProjectStatusLabel } from "../../shared/collaborationLabels";
import { dispatchSessionChange } from "../../shared/sessionEvents";
import { MobileCollaborationPanel } from "./MobileCollaborationPanel";
import { DatasetHostingPanel } from "../hosting/DatasetHostingPanel";

type MobilePanel = "settings" | "conflicts" | "hosting" | null;

export type MobileWorkspaceShellProps = {
  children: ReactNode;
  route: string;
  projectId?: string;
  onNavigate: (route: string) => void;
};

const emptyOfflineStatus: OfflineStatus = {
  online: false,
  queuedOperations: 0,
  conflictCount: 0,
  cachedAssets: 0,
  lastSyncAt: null,
  lastError: null,
};

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function decodeRoute(route: string): string {
  try {
    return decodeURIComponent(route);
  } catch {
    // A malformed hash should not break the mobile shell; leave it available
    // for the ASCII route checks below and fall back to the projects section.
    return route;
  }
}

function routeSection(route: string): "projects" | "tasks" | "review" {
  const decodedRoute = decodeRoute(route);
  if (decodedRoute.includes("/质检") || decodedRoute.includes("review")) return "review";
  if (decodedRoute.includes("/任务") || decodedRoute.includes("tasks") || decodedRoute.includes("backend-tasks")) return "tasks";
  return "projects";
}

function isAnnotationRoute(route: string): boolean {
  const normalizedRoute = decodeRoute(route).replace(/^#\/?/, "");
  return normalizedRoute === "annotate" || normalizedRoute.startsWith("annotate/");
}

function conflictDescription(conflict: SyncConflict): string {
  const local = conflict.local ? JSON.stringify(conflict.local) : "无本地版本";
  const remote = conflict.remote ? JSON.stringify(conflict.remote) : "无远端版本";
  return `本地：${local.slice(0, 140)}；远端：${remote.slice(0, 140)}`;
}

export function MobileWorkspaceShell({
  children,
  route,
  projectId,
  onNavigate,
}: MobileWorkspaceShellProps) {
  const [session, setSession] = useState<AuthSession | null>(null);
  const [device, setDevice] = useState<DeviceInfo | null>(null);
  const [offline, setOffline] = useState<OfflineStatus>(emptyOfflineStatus);
  const [projects, setProjects] = useState<DatasetProject[]>([]);
  const [remoteProjects, setRemoteProjects] = useState<RemoteProjectSummary[]>([]);
  const [remoteProjectIds, setRemoteProjectIds] = useState<Record<string, string>>({});
  const [accountProfile, setAccountProfile] = useState<AccountProfile | null>(null);
  const [tasks, setTasks] = useState<AnnotationTask[]>([]);
  const [issues, setIssues] = useState<IssueRecord[]>([]);
  const [conflicts, setConflicts] = useState<SyncConflict[]>([]);
  const [summary, setSummary] = useState<SyncSummary | null>(null);
  const [panel, setPanel] = useState<MobilePanel>(null);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [serverUrl, setServerUrl] = useState(defaultRemoteServerUrl());
  const [authMode, setAuthMode] = useState<"login" | "accept">("login");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [invitationToken, setInvitationToken] = useState("");
  const [invitationDisplayName, setInvitationDisplayName] = useState("");
  const sessionMutationRef = useRef(0);

  const resetSessionScopedState = useCallback(() => {
    setProjects([]);
    setRemoteProjects([]);
    setRemoteProjectIds({});
    setAccountProfile(null);
    setTasks([]);
    setIssues([]);
    setConflicts([]);
    setSummary(null);
    setOffline(emptyOfflineStatus);
    setMessage(null);
  }, []);

  useEffect(() => {
    const receive = (event: Event) => {
      const detail = (event as CustomEvent<{ projectId?: string; status: OfflineStatus }>).detail;
      if (detail && (detail.projectId === projectId || detail.projectId === undefined)) setOffline(detail.status);
    };
    window.addEventListener("image-annotation:sync-status", receive);
    return () => window.removeEventListener("image-annotation:sync-status", receive);
  }, [projectId]);

  const refreshRemoteState = useCallback(async (knownSession?: AuthSession) => {
    const requestVersion = sessionMutationRef.current;
    const currentSession = knownSession ?? await authStatus();
    if (requestVersion !== sessionMutationRef.current) return;
    setSession(currentSession);
    if (!currentSession.authenticated) {
      resetSessionScopedState();
      return;
    }
    const [projectResult, profileResult] = await Promise.allSettled([
      listRemoteProjects(),
      getAccountProfile(),
    ]);
    if (requestVersion !== sessionMutationRef.current) return;
    if (projectResult.status === "fulfilled") setRemoteProjects(projectResult.value);
    if (profileResult.status === "fulfilled") {
      setAccountProfile(profileResult.value);
      setSession((current) => current
        ? { ...current, userId: current.userId ?? profileResult.value.id, displayName: profileResult.value.displayName }
        : current);
    }
    const firstError = [projectResult, profileResult]
      .find((result): result is PromiseRejectedResult => result.status === "rejected");
    if (firstError) setMessage(errorMessage(firstError.reason));
  }, [resetSessionScopedState]);

  const reloadLocalState = useCallback(async () => {
    const requestVersion = sessionMutationRef.current;
    const [sessionResult, deviceResult, offlineResult, projectResult] = await Promise.allSettled([
      authStatus(),
      deviceInfo(),
      offlineStatus(projectId),
      listDatasetProjects(),
    ]);
    if (sessionResult.status === "fulfilled" && requestVersion === sessionMutationRef.current) {
      setSession(sessionResult.value);
      if (sessionResult.value.serverUrl) setServerUrl(sessionResult.value.serverUrl);
      if (sessionResult.value.authenticated) {
        void refreshRemoteState();
      } else {
        resetSessionScopedState();
      }
    }
    if (requestVersion === sessionMutationRef.current && deviceResult.status === "fulfilled") setDevice(deviceResult.value);
    if (requestVersion === sessionMutationRef.current && offlineResult.status === "fulfilled") setOffline(offlineResult.value);
    if (requestVersion === sessionMutationRef.current && projectResult.status === "fulfilled") setProjects(projectResult.value ?? []);
    const firstError = [sessionResult, deviceResult, offlineResult, projectResult]
      .find((result): result is PromiseRejectedResult => result.status === "rejected");
    if (firstError && requestVersion === sessionMutationRef.current) setMessage(errorMessage(firstError.reason));
  }, [projectId, refreshRemoteState, resetSessionScopedState]);

  useEffect(() => {
    void reloadLocalState();
  }, [reloadLocalState]);

  useEffect(() => {
    if (!projectId) {
      setTasks([]);
      setIssues([]);
      setConflicts([]);
      setSummary(null);
      return;
    }

    let active = true;
    const requestVersion = sessionMutationRef.current;
    void Promise.allSettled([
      listTasks(projectId),
      listIssues(projectId, true),
      listSyncConflicts(projectId),
      getProjectSyncSummary(projectId),
      offlineStatus(projectId),
    ]).then((results) => {
      if (!active || requestVersion !== sessionMutationRef.current) return;
      const [taskResult, issueResult, conflictResult, summaryResult, offlineResult] = results;
      if (taskResult.status === "fulfilled") setTasks(taskResult.value ?? []);
      if (issueResult.status === "fulfilled") setIssues(issueResult.value ?? []);
      if (conflictResult.status === "fulfilled") setConflicts(conflictResult.value ?? []);
      if (summaryResult.status === "fulfilled") setSummary(summaryResult.value);
      if (offlineResult.status === "fulfilled") setOffline(offlineResult.value);
    });
    return () => {
      active = false;
    };
  }, [projectId]);

  const activeSection = routeSection(route);
  const annotationRoute = isAnnotationRoute(route);
  const activeProject = useMemo(
    () => projects.find((project) => project.id === projectId),
    [projectId, projects],
  );
  const activeRemoteProjectId = projectId ? remoteProjectIds[projectId] : undefined;
  const openConflictCount = conflicts.filter((conflict) => conflict.status === "open").length;
  const online = offline.online && !offline.lastError;
  const notify = useCallback((nextMessage: string) => setMessage(nextMessage), []);

  async function openRemoteProject(remoteProject: RemoteProjectSummary) {
    setBusy(true);
    setMessage(null);
    try {
      // A project already present in the native cache is opened by its local
      // id. This prevents calling bootstrap again with a local id after a
      // session reload or when the server and local ids happen to match.
      const existingLocal = projects.find((project) => project.id === remoteProject.id);
      if (existingLocal) {
        setRemoteProjectIds((current) => ({ ...current, [existingLocal.id]: remoteProject.id }));
        onNavigate(`#/datasets/${existingLocal.id}`);
        return;
      }
      const opened = await remoteProjectOpen(remoteProject.id);
      const localProjectId = opened.localProjectId ?? opened.projectId;
      if (!localProjectId) throw new Error("远端项目没有返回本地缓存 ID");
      setRemoteProjectIds((current) => ({ ...current, [localProjectId]: remoteProject.id }));
      setProjects((current) => current.some((project) => project.id === localProjectId)
        ? current
        : opened.project ? [...current, opened.project] : current);
      setMessage(`已打开远端项目：${remoteProject.name}`);
      onNavigate(`#/datasets/${localProjectId}`);
    } catch (error) {
      setMessage(`打开远端项目失败：${errorMessage(error)}`);
    } finally {
      setBusy(false);
    }
  }

  async function revokeDevice(deviceId: string) {
    if (!accountProfile || deviceId === session?.deviceId) return;
    setBusy(true);
    try {
      await revokeRemoteDevice(deviceId);
      const nextProfile = await getAccountProfile();
      setAccountProfile(nextProfile);
      setMessage("已撤销该设备会话");
    } catch (error) {
      setMessage(`撤销设备失败：${errorMessage(error)}`);
    } finally {
      setBusy(false);
    }
  }

  function navigateTo(section: "projects" | "tasks" | "review") {
    if (section === "projects") {
      onNavigate(projectId ? `#/datasets/${projectId}` : "#/datasets");
    } else if (section === "tasks") {
      // The desktop backend-task tray is a process monitor, not the mobile
      // annotation queue. Ask the user to choose a project first on mobile.
      onNavigate(projectId ? `#/datasets/${projectId}/任务` : "#/datasets");
    } else {
      onNavigate(projectId ? `#/datasets/${projectId}/质检` : "#/datasets");
    }
  }

  async function submitLogin(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const requestVersion = ++sessionMutationRef.current;
    setBusy(true);
    setMessage(null);
    try {
      const nextSession = authMode === "accept"
        ? await authAcceptInvitation({
          serverUrl: serverUrl.trim(),
          token: invitationToken.trim(),
          displayName: invitationDisplayName.trim(),
          password,
          deviceName: "Image Annotation Mobile",
        })
        : await authLogin({ serverUrl: serverUrl.trim(), username: username.trim(), password });
      if (requestVersion !== sessionMutationRef.current) return;
      resetSessionScopedState();
      setSession(nextSession);
      setPassword("");
      setInvitationToken("");
      setInvitationDisplayName("");
      dispatchSessionChange({
        authenticated: true,
        userId: nextSession.userId ?? nextSession.accountId ?? null,
        serverUrl: nextSession.serverUrl,
      });
      if (projectId) onNavigate("#/datasets");
      await refreshRemoteState(nextSession);
      const localProjects = await listDatasetProjects();
      if (requestVersion === sessionMutationRef.current) setProjects(localProjects);
      setMessage(authMode === "accept" ? "已接受邀请，远端项目已加载" : "已登录，远端协同已启用");
    } catch (error) {
      setMessage(errorMessage(error));
    } finally {
      setBusy(false);
    }
  }

  async function logout() {
    setBusy(true);
    const requestVersion = ++sessionMutationRef.current;
    try {
      await authLogout();
      if (requestVersion !== sessionMutationRef.current) return;
      resetSessionScopedState();
      setSession(null);
      dispatchSessionChange({ authenticated: false, userId: null, serverUrl: null });
      onNavigate("#/datasets");
      try {
        const localProjects = await listDatasetProjects();
        if (requestVersion === sessionMutationRef.current) setProjects(localProjects);
      } catch (error) {
        setMessage(`已退出登录，但刷新本地项目失败：${errorMessage(error)}`);
        return;
      }
      setMessage("已退出登录，仍可使用本地缓存");
    } catch (error) {
      setMessage(errorMessage(error));
    } finally {
      setBusy(false);
    }
  }

  async function refreshSession() {
    setBusy(true);
    sessionMutationRef.current += 1;
    try {
      setSession(await authRefresh());
      await refreshRemoteState();
      setMessage("登录会话已刷新");
    } catch (error) {
      setMessage(errorMessage(error));
    } finally {
      setBusy(false);
    }
  }

  async function refreshOfflineState() {
    setBusy(true);
    try {
      setOffline(await offlineStatus(projectId));
      setMessage("离线状态已刷新");
    } catch (error) {
      setMessage(errorMessage(error));
    } finally {
      setBusy(false);
    }
  }

  async function uploadOfflineChanges() {
    setBusy(true);
    try {
      setOffline(await flushOfflineQueue(projectId));
      setMessage("离线操作已提交，冲突请在审阅中处理");
    } catch (error) {
      setMessage(errorMessage(error));
    } finally {
      setBusy(false);
    }
  }

  async function resolveConflict(conflict: SyncConflict, resolution: "local" | "remote") {
    if (!projectId) return;
    setBusy(true);
    try {
      await resolveSyncConflict(projectId, conflict.id, resolution);
      setConflicts((current) => current.filter((item) => item.id !== conflict.id));
      setOffline((current) => ({ ...current, conflictCount: Math.max(0, current.conflictCount - 1) }));
      setMessage(resolution === "local" ? "已保留本地版本" : "已采用远端版本");
    } catch (error) {
      setMessage(errorMessage(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className={`mobile-shell${annotationRoute ? " is-annotation-route" : ""}`}>
      <header className="mobile-shell-header" data-testid="mobile-shell-header">
        <div className="mobile-brand">
          <div className="mobile-brand-mark" aria-hidden="true"><FolderKanban size={18} /></div>
          <div>
            <strong>Image Annotation</strong>
            <span>{activeProject?.name ?? (session?.authenticated ? session.displayName || "已登录" : "本地工作区")}</span>
          </div>
        </div>
        <button
          className={`mobile-connectivity-chip ${online ? "is-online" : "is-offline"}`}
          onClick={() => setPanel("settings")}
          type="button"
          aria-label="打开服务器与离线设置"
        >
          {online ? <Cloud size={15} /> : <CloudOff size={15} />}
          <span>{online ? "在线" : "离线"}</span>
        </button>
      </header>

      {projectId && summary && annotationRoute ? (
        <button className="mobile-sync-strip mobile-sync-strip--annotation" onClick={() => setPanel("conflicts")} type="button">
          <Server size={14} /><span>{summary.projectMode === "local_only" ? "本地项目" : "协同项目"}</span>
          <span>任务 {tasks.length}</span><span>缺陷 {issues.length}</span>
          <span>待上传 {summary.pendingOperations}</span><span>冲突 {summary.conflictCount}</span>
          {summary.conflictCount > 0 ? <CircleAlert size={15} /> : null}
        </button>
      ) : null}

      <div className={`mobile-shell-content${annotationRoute ? " mobile-shell-content--annotation" : ""}`}>
        {activeSection === "projects" && !annotationRoute ? (
          <section className="mobile-remote-projects" aria-label="远端项目">
            <div className="mobile-collaboration-heading">
              <div>
                <span className="mobile-eyebrow">多设备协同</span>
                <h2>远端项目</h2>
              </div>
              {session?.authenticated ? <div className="mobile-collaboration-heading-actions">
                <button type="button" aria-label="打开数据集托管" onClick={() => setPanel("hosting")} disabled={busy}>
                  <Database size={16} />数据集托管
                </button>
                <button type="button" aria-label="刷新远端项目" onClick={() => void refreshRemoteState()} disabled={busy}>
                  <RefreshCw size={16} />刷新
                </button>
              </div> : null}
            </div>
            {!session?.authenticated ? (
              <button className="mobile-remote-login-hint" type="button" onClick={() => setPanel("settings")}>
                <LogIn size={16} />登录后发现服务器项目并在多设备间协同
              </button>
            ) : remoteProjects.length === 0 ? (
              <div className="mobile-empty-state"><FolderKanban size={17} />服务器暂无可访问项目</div>
            ) : (
              <div className="mobile-remote-project-list">
                {remoteProjects.map((remoteProject) => {
                  const localProjectId = Object.entries(remoteProjectIds).find(([, remoteId]) => remoteId === remoteProject.id)?.[0]
                    ?? projects.find((project) => project.id === remoteProject.id)?.id;
                  return (
                    <article className="mobile-remote-project-card" key={remoteProject.id}>
                      <div>
                        <strong>{remoteProject.name}</strong>
                        <p>{remoteProject.description || "服务器协同项目"}</p>
                        <small>{remoteProjectStatusLabel(remoteProject.status)}{remoteProject.role ? ` · ${remoteProjectRoleLabel(remoteProject.role)}` : ""}</small>
                      </div>
                      <button className="mobile-primary-button" type="button" disabled={busy} onClick={() => {
                        if (localProjectId) onNavigate(`#/datasets/${localProjectId}`);
                        else void openRemoteProject(remoteProject);
                      }}>
                        {localProjectId ? "打开缓存" : "打开并缓存"}
                      </button>
                    </article>
                  );
                })}
              </div>
            )}
          </section>
        ) : null}
        {activeSection !== "projects" && projectId ? (
          <MobileCollaborationPanel
            authenticated={Boolean(session?.authenticated)}
            localProjectId={projectId}
            onMessage={notify}
            onOpenImage={(imageId) => onNavigate(`#/annotate/${projectId}/${encodeURIComponent(imageId)}`)}
            remoteProjectId={activeRemoteProjectId}
            section={activeSection}
          />
        ) : null}
        {annotationRoute || activeSection === "projects" ? children : null}
      </div>

      {!annotationRoute ? (
        <div className="mobile-capability-note" role="note">
          <Smartphone size={13} />
          <span>移动端支持任务、标注、审阅、缺陷与冲突；批量导入导出和本地 AI 请在桌面端完成。</span>
        </div>
      ) : null}

      <nav className="mobile-bottom-nav" data-testid="mobile-bottom-nav" aria-label="移动端底部导航">
        <button className={activeSection === "projects" ? "is-active" : ""} onClick={() => navigateTo("projects")} type="button">
          <FolderKanban size={19} /><span>项目</span>
        </button>
        <button className={activeSection === "tasks" ? "is-active" : ""} onClick={() => navigateTo("tasks")} type="button">
          <ListTodo size={19} /><span>任务</span>
        </button>
        <button className={activeSection === "review" ? "is-active" : ""} onClick={() => navigateTo("review")} type="button">
          <ShieldAlert size={19} /><span>审阅{openConflictCount > 0 ? ` · ${openConflictCount}` : ""}</span>
        </button>
        <button className={panel === "settings" ? "is-active" : ""} onClick={() => setPanel("settings")} type="button">
          <Settings2 size={19} /><span>设置</span>
        </button>
      </nav>

      {message ? (
        <div className="mobile-status-toast" role="status" aria-live="polite">
          <span>{message}</span>
          <button aria-label="关闭提示" onClick={() => setMessage(null)} type="button"><X size={15} /></button>
        </div>
      ) : null}

      {panel === "settings" ? (
        <div className="mobile-modal-backdrop" role="presentation" onMouseDown={(event) => {
          if (event.target === event.currentTarget) setPanel(null);
        }}>
          <section className="mobile-modal" role="dialog" aria-modal="true" aria-labelledby="mobile-settings-title">
            <div className="mobile-modal-header">
              <div><span className="mobile-eyebrow">设备与协同</span><h2 id="mobile-settings-title">服务器设置</h2></div>
              <button aria-label="关闭服务器设置" onClick={() => setPanel(null)} type="button"><X size={19} /></button>
            </div>
            {session?.authenticated ? (
              <div className="mobile-session-card is-authenticated">
                <UserRound size={18} />
                <div><strong>{session.displayName || session.userId || "已登录用户"}</strong><span>{session.serverUrl || serverUrl}</span></div>
                <Check size={18} />
              </div>
            ) : (
              <form className="mobile-login-form" onSubmit={submitLogin}>
                <div className="mobile-auth-mode" role="group" aria-label="登录方式">
                  <button type="button" className={authMode === "login" ? "is-active" : ""} onClick={() => setAuthMode("login")}>账号登录</button>
                  <button type="button" className={authMode === "accept" ? "is-active" : ""} onClick={() => setAuthMode("accept")}>接受邀请</button>
                </div>
                <label>服务器地址<input value={serverUrl} onChange={(event) => setServerUrl(event.target.value)} inputMode="url" autoComplete="url" /></label>
                {authMode === "accept" ? (
                  <>
                    <label>邀请码<input value={invitationToken} onChange={(event) => setInvitationToken(event.target.value)} autoComplete="one-time-code" /></label>
                    <label>显示名<input value={invitationDisplayName} onChange={(event) => setInvitationDisplayName(event.target.value)} autoComplete="name" /></label>
                    <label>新密码<input value={password} onChange={(event) => setPassword(event.target.value)} type="password" autoComplete="new-password" /></label>
                  </>
                ) : (
                  <>
                    <label>用户名<input type="email" inputMode="email" autoCapitalize="none" autoCorrect="off" spellCheck={false} value={username} onChange={(event) => setUsername(event.target.value)} autoComplete="username" /></label>
                    <label>密码<input value={password} onChange={(event) => setPassword(event.target.value)} type="password" autoComplete="current-password" /></label>
                  </>
                )}
                <button className="mobile-primary-button" disabled={busy || !serverUrl.trim() || (authMode === "accept" ? !invitationToken.trim() || !invitationDisplayName.trim() : !username.trim()) || !password} type="submit">
                  <LogIn size={16} />{busy ? (authMode === "accept" ? "接受中…" : "登录中…") : (authMode === "accept" ? "接受邀请并连接" : "登录并连接")}
                </button>
              </form>
            )}
            {session?.authenticated && accountProfile ? (
              <div className="mobile-device-sessions" aria-label="登录设备会话">
                <div className="mobile-offline-card-title"><UserRound size={17} /><strong>登录设备</strong><span>{accountProfile.devices.length} 台</span></div>
                {accountProfile.devices.map((remoteDevice) => {
                  const isCurrent = remoteDevice.deviceId === session.deviceId;
                  return (
                    <div className={`mobile-device-session ${remoteDevice.revokedAt ? "is-revoked" : ""}`} key={remoteDevice.id}>
                      <div><strong>{remoteDevice.name || remoteDevice.deviceId}</strong><small>{remoteDevice.platform} · {remoteDevice.lastSeenAt || "未记录"}{isCurrent ? " · 当前设备" : ""}</small></div>
                      {!isCurrent && !remoteDevice.revokedAt ? <button type="button" disabled={busy} onClick={() => void revokeDevice(remoteDevice.id)}>撤销</button> : <span>{remoteDevice.revokedAt ? "已撤销" : "当前"}</span>}
                    </div>
                  );
                })}
              </div>
            ) : null}
            <div className="mobile-device-card">
              <div><Smartphone size={17} /><span>当前设备</span></div>
              <strong>{device?.deviceId || session?.deviceId || "等待 Rust 核心返回设备 ID"}</strong>
              <small>{device?.platform || "native"}{device?.model ? ` · ${device.model}` : ""}{device?.appVersion ? ` · v${device.appVersion}` : ""}</small>
            </div>
            <div className="mobile-offline-card">
              <div className="mobile-offline-card-title"><WifiOff size={17} /><strong>离线工作区</strong><span className={online ? "is-online" : "is-offline"}>{online ? "在线" : "可离线"}</span></div>
              <div className="mobile-metric-row"><span>待上传 {offline.queuedOperations}</span><span>缓存 {offline.cachedAssets}</span><span>冲突 {offline.conflictCount}</span></div>
              {offline.lastError ? <small className="mobile-error-text">{offline.lastError}</small> : null}
              <div className="mobile-inline-actions">
                <button disabled={busy} onClick={() => void refreshOfflineState()} type="button"><RefreshCw size={15} />刷新状态</button>
                <button disabled={busy || offline.queuedOperations === 0} onClick={() => void uploadOfflineChanges()} type="button"><Cloud size={15} />上传离线操作</button>
              </div>
            </div>
            {session?.authenticated ? (
              <div className="mobile-inline-actions mobile-account-actions">
                <button disabled={busy} onClick={() => void refreshSession()} type="button"><RefreshCw size={15} />刷新登录</button>
                <button disabled={busy} onClick={() => void logout()} type="button"><LogIn size={15} />退出登录</button>
              </div>
            ) : null}
          </section>
        </div>
      ) : null}

      {panel === "conflicts" ? (
        <div className="mobile-modal-backdrop" role="presentation" onMouseDown={(event) => {
          if (event.target === event.currentTarget) setPanel(null);
        }}>
          <section className="mobile-modal" role="dialog" aria-modal="true" aria-labelledby="mobile-conflicts-title">
            <div className="mobile-modal-header">
              <div><span className="mobile-eyebrow">同步保护</span><h2 id="mobile-conflicts-title">待处理冲突</h2></div>
              <button aria-label="关闭冲突列表" onClick={() => setPanel(null)} type="button"><X size={19} /></button>
            </div>
            {conflicts.length === 0 ? <div className="mobile-empty-state"><Check size={20} /><span>当前没有待处理冲突</span></div> : null}
            <div className="mobile-conflict-list">
              {conflicts.map((conflict) => (
                <article className="mobile-conflict-card" key={conflict.id}>
                  <div className="mobile-conflict-card-title"><CircleAlert size={17} /><strong>{conflict.entityType} · {conflict.entityId}</strong></div>
                  <p>{conflictDescription(conflict)}</p>
                  <div className="mobile-inline-actions">
                    <button disabled={busy} onClick={() => void resolveConflict(conflict, "local")} type="button">保留本地</button>
                    <button className="mobile-primary-button" disabled={busy} onClick={() => void resolveConflict(conflict, "remote")} type="button">采用远端</button>
                  </div>
                </article>
              ))}
            </div>
          </section>
        </div>
      ) : null}

      {panel === "hosting" ? (
        <div className="mobile-modal-backdrop" role="presentation" onMouseDown={(event) => {
          if (event.target === event.currentTarget) setPanel(null);
        }}>
          <section className="mobile-modal mobile-hosting-modal" role="dialog" aria-modal="true" aria-labelledby="mobile-hosting-title">
            <div className="mobile-modal-header">
              <div><span className="mobile-eyebrow">云端资产</span><h2 id="mobile-hosting-title">数据集托管</h2></div>
              <button aria-label="关闭数据集托管" onClick={() => setPanel(null)} type="button"><X size={19} /></button>
            </div>
            <DatasetHostingPanel
              compact
              localProjects={projects}
              remoteToLocalProjectIds={Object.fromEntries(Object.entries(remoteProjectIds).map(([localId, remoteId]) => [remoteId, localId]))}
              onClose={() => setPanel(null)}
              onOpenProject={(localProjectId, remoteProjectId) => {
                if (remoteProjectId) setRemoteProjectIds((current) => ({ ...current, [localProjectId]: remoteProjectId }));
                setPanel(null);
                onNavigate(`#/datasets/${localProjectId}`);
              }}
            />
          </section>
        </div>
      ) : null}

      {projectId && summary && !annotationRoute ? (
        <button className="mobile-sync-strip" onClick={() => setPanel("conflicts")} type="button">
          <Server size={14} /><span>{summary.projectMode === "local_only" ? "本地项目" : "协同项目"}</span>
          <span>任务 {tasks.length}</span><span>缺陷 {issues.length}</span>
          <span>待上传 {summary.pendingOperations}</span><span>冲突 {summary.conflictCount}</span>
          {summary.conflictCount > 0 ? <CircleAlert size={15} /> : null}
        </button>
      ) : null}
    </div>
  );
}

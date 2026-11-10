import { Check, CircleAlert, Cloud, LogIn, RefreshCw, Smartphone, UserRound, X } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import {
  authAcceptInvitation,
  authLogin,
  authLogout,
  authRefresh,
  authStatus,
  createRemoteInvitation,
  defaultRemoteServerUrl,
  getAccountProfile,
  listRemoteProjects,
  remoteProjectOpen,
  revokeRemoteDevice,
} from "../../api/mobile";
import { publishProject } from "../../api/tauri";
import type { DatasetProject } from "../../types/domain";
import type { AccountProfile, AuthSession, RemoteInvitation, RemoteProjectSummary } from "../../types/mobile";
import { remoteProjectRoleLabel, remoteProjectStatusLabel } from "../../shared/collaborationLabels";
import { dispatchSessionChange } from "../../shared/sessionEvents";
import { DatasetHostingPanel } from "../hosting/DatasetHostingPanel";

export type CollaborationPanelProps = {
  localProjects?: DatasetProject[];
  onClose: () => void;
  onOpenProject: (localProjectId: string, remoteProjectId?: string) => void;
};

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

type AuthMode = "login" | "accept";
type MessageTone = "success" | "error";

export function CollaborationPanel({ localProjects = [], onClose, onOpenProject }: CollaborationPanelProps) {
  const [session, setSession] = useState<AuthSession | null>(null);
  const [profile, setProfile] = useState<AccountProfile | null>(null);
  const [remoteProjects, setRemoteProjects] = useState<RemoteProjectSummary[]>([]);
  const [knownLocalIds, setKnownLocalIds] = useState<Record<string, string>>({});
  const [serverUrl, setServerUrl] = useState(defaultRemoteServerUrl());
  const [authMode, setAuthMode] = useState<AuthMode>("login");
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [invitationToken, setInvitationToken] = useState("");
  const [invitationDisplayName, setInvitationDisplayName] = useState("");
  const [invitationEmail, setInvitationEmail] = useState("");
  const [invitationRole, setInvitationRole] = useState("annotator");
  const [generatedInvitation, setGeneratedInvitation] = useState<{ projectId: string; invitation: RemoteInvitation } | null>(null);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [messageTone, setMessageTone] = useState<MessageTone>("error");
  const [hostingOpen, setHostingOpen] = useState(false);

  const showMessage = useCallback((text: string, tone: MessageTone = "error") => {
    setMessageTone(tone);
    setMessage(text);
  }, []);

  const clearMessage = useCallback(() => {
    setMessageTone("error");
    setMessage(null);
  }, []);

  const load = useCallback(async (knownSession?: AuthSession) => {
    const current = knownSession ?? await authStatus();
    setSession(current);
    if (current.serverUrl) setServerUrl(current.serverUrl);
    if (!current.authenticated) {
      setRemoteProjects([]);
      setProfile(null);
      return;
    }
    const [projectsResult, profileResult] = await Promise.allSettled([listRemoteProjects(), getAccountProfile()]);
    if (projectsResult.status === "fulfilled") setRemoteProjects(projectsResult.value);
    if (profileResult.status === "fulfilled") setProfile(profileResult.value);
    const firstError = [projectsResult, profileResult]
      .find((result): result is PromiseRejectedResult => result.status === "rejected");
    if (firstError) showMessage(messageOf(firstError.reason));
  }, [showMessage]);

  useEffect(() => {
    void load().catch((error) => showMessage(messageOf(error)));
  }, [load, showMessage]);

  async function submitLogin(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setBusy(true);
    clearMessage();
    try {
      const next = authMode === "accept"
        ? await authAcceptInvitation({
          serverUrl: serverUrl.trim(),
          token: invitationToken.trim(),
          displayName: invitationDisplayName.trim(),
          password,
          deviceName: "Image Annotation Desktop",
        })
        : await authLogin({ serverUrl: serverUrl.trim(), username: username.trim(), password, deviceName: "Image Annotation Desktop" });
      setPassword("");
      setInvitationToken("");
      dispatchSessionChange({
        authenticated: true,
        userId: next.userId ?? next.accountId ?? null,
        serverUrl: next.serverUrl,
      });
      await load(next);
      showMessage(authMode === "accept" ? "已接受邀请，远端项目已加载" : "已登录，远端项目已加载", "success");
    } catch (error) {
      showMessage(messageOf(error));
    } finally {
      setBusy(false);
    }
  }

  async function createInvitation(project: RemoteProjectSummary) {
    const email = invitationEmail.trim();
    if (!email) {
      showMessage("请输入受邀成员邮箱");
      return;
    }
    setBusy(true);
    clearMessage();
    try {
      const invitation = await createRemoteInvitation(project.id, email, invitationRole);
      setGeneratedInvitation({ projectId: project.id, invitation });
      setInvitationEmail("");
      showMessage("邀请码已生成，请通过安全渠道人工分享", "success");
    } catch (error) {
      showMessage(`生成邀请码失败：${messageOf(error)}`);
    } finally {
      setBusy(false);
    }
  }

  async function copyInvitationToken(token: string) {
    try {
      if (typeof navigator === "undefined" || !navigator.clipboard) throw new Error("剪贴板不可用");
      await navigator.clipboard.writeText(token);
      showMessage("邀请码已复制", "success");
    } catch {
      showMessage("复制失败，请手动复制邀请码");
    }
  }

  const canSubmitAuth = authMode === "accept"
    ? Boolean(serverUrl.trim() && invitationToken.trim() && invitationDisplayName.trim() && password)
    : Boolean(serverUrl.trim() && username.trim() && password);

  async function openRemoteProject(project: RemoteProjectSummary) {
    const localId = knownLocalIds[project.id] ?? (localProjects.some((localProject) => localProject.id === project.id) ? project.id : null);
    if (localId) {
      onOpenProject(localId, project.id);
      return;
    }
    setBusy(true);
    try {
      const opened = await remoteProjectOpen(project.id);
      const nextLocalId = opened.localProjectId ?? opened.projectId;
      setKnownLocalIds((current) => ({ ...current, [project.id]: nextLocalId }));
      onOpenProject(nextLocalId, project.id);
    } catch (error) {
      showMessage(`打开远端项目失败：${messageOf(error)}`);
    } finally {
      setBusy(false);
    }
  }

  async function publishLocalProject(project: DatasetProject) {
    if (!session?.serverUrl || !session.deviceId) {
      showMessage("当前登录会话缺少服务器或设备信息");
      return;
    }
    setBusy(true);
    clearMessage();
    try {
      const published = await publishProject(project.id, {
        serverUrl: session.serverUrl,
        deviceId: session.deviceId,
        mode: "mirrored",
        cachePolicy: "on_demand",
      });
      setKnownLocalIds((current) => ({ ...current, [published.remoteProjectId]: project.id }));
      await load(session);
      showMessage(`已发布本地项目：${project.name}`, "success");
    } catch (error) {
      showMessage(`发布项目失败：${messageOf(error)}`);
    } finally {
      setBusy(false);
    }
  }

  async function revokeDevice(deviceId: string) {
    if (deviceId === session?.deviceId) return;
    setBusy(true);
    try {
      await revokeRemoteDevice(deviceId);
      setProfile(await getAccountProfile());
      showMessage("已撤销设备会话", "success");
    } catch (error) {
      showMessage(`撤销设备失败：${messageOf(error)}`);
    } finally {
      setBusy(false);
    }
  }

  async function logout() {
    setBusy(true);
    clearMessage();
    try {
      await authLogout();
      dispatchSessionChange({ authenticated: false, userId: null, serverUrl: null });
      await load();
      showMessage("已退出登录", "success");
    } catch (error) {
      showMessage(messageOf(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="collaboration-dialog-backdrop" role="presentation" onMouseDown={(event) => {
      if (event.target === event.currentTarget) onClose();
    }}>
      <section className="collaboration-dialog" role="dialog" aria-modal="true" aria-labelledby="collaboration-dialog-title">
        <div className="collaboration-dialog__header">
          <div><span className="eyebrow">多设备协同</span><h2 id="collaboration-dialog-title">账号与远端项目</h2></div>
          <button type="button" aria-label="关闭协同面板" onClick={onClose}><X size={18} /></button>
        </div>
        {session?.authenticated ? (
          <div className="collaboration-session-card">
            <UserRound size={18} /><div><strong>{profile?.displayName || session.displayName || session.userId || "已登录用户"}</strong><small>{profile?.email || session.serverUrl || serverUrl}</small></div><Check size={18} />
          </div>
        ) : (
          <form className="collaboration-login-form" onSubmit={submitLogin}>
            <div className="collaboration-auth-mode" role="group" aria-label="登录方式">
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
                <label>用户名<input value={username} onChange={(event) => setUsername(event.target.value)} autoComplete="username" /></label>
                <label>密码<input value={password} onChange={(event) => setPassword(event.target.value)} type="password" autoComplete="current-password" /></label>
              </>
            )}
            <button className="primary" type="submit" disabled={busy || !canSubmitAuth}><LogIn size={16} />{busy ? (authMode === "accept" ? "接受中…" : "登录中…") : (authMode === "accept" ? "接受邀请并发现项目" : "登录并发现项目")}</button>
          </form>
        )}
        {message ? <div className={`collaboration-message collaboration-message--${messageTone}`} role="status" aria-live="polite">{messageTone === "success" ? <Check size={15} /> : <CircleAlert size={15} />}{message}</div> : null}
        {session?.authenticated ? (
          <>
            <div className="collaboration-hosting-entry">
              <div><strong>云端数据集</strong><small>版本、成员权限、项目收录与导入导出</small></div>
              <button type="button" onClick={() => setHostingOpen((current) => !current)} aria-expanded={hostingOpen} aria-controls="desktop-hosting-panel">
                <Cloud size={15} />{hostingOpen ? "收起托管" : "打开数据集托管"}
              </button>
            </div>
            {hostingOpen ? <div id="desktop-hosting-panel"><DatasetHostingPanel
              compact
              localProjects={localProjects}
              remoteToLocalProjectIds={knownLocalIds}
              onOpenProject={(localProjectId, remoteProjectId) => {
                if (remoteProjectId) setKnownLocalIds((current) => ({ ...current, [remoteProjectId]: localProjectId }));
                onOpenProject(localProjectId, remoteProjectId);
              }}
            /></div> : null}
            <div className="collaboration-section-heading"><strong>本地项目</strong><span>{localProjects.length} 个</span></div>
            {localProjects.length === 0 ? <div className="collaboration-empty">当前没有可发布的本地项目</div> : <div className="collaboration-project-list">
              {localProjects.map((project) => {
                const remoteId = Object.entries(knownLocalIds).find(([, localId]) => localId === project.id)?.[0];
                return (
                  <article key={project.id} className="collaboration-project-card">
                    <div className="collaboration-project-card__main">
                      <div><strong>{project.name}</strong><small>{project.description || "本地项目"}</small></div>
                      <div className="collaboration-project-actions">
                        <button type="button" onClick={() => onOpenProject(project.id, remoteId)}>{remoteId ? "打开已发布" : "打开本地"}</button>
                        <button type="button" className="primary" disabled={busy} onClick={() => void publishLocalProject(project)}>{remoteId ? "更新服务器" : "发布到服务器"}</button>
                      </div>
                    </div>
                  </article>
                );
              })}
            </div>}
            <div className="collaboration-section-heading"><strong>远端项目</strong><button type="button" onClick={() => void load()} disabled={busy}><RefreshCw size={15} />刷新</button></div>
            {remoteProjects.length === 0 ? <div className="collaboration-empty">当前账号没有可访问的项目</div> : <div className="collaboration-project-list">
              {remoteProjects.map((project) => {
                const canInvite = project.role === "owner" || project.role === "manager";
                const invitation = generatedInvitation?.projectId === project.id ? generatedInvitation.invitation : null;
                return (
                  <article key={project.id} className="collaboration-project-card">
                    <div className="collaboration-project-card__main"><div><strong>{project.name}</strong><small>{project.description || "服务器协同项目"}</small><small>{remoteProjectStatusLabel(project.status)}{project.role ? ` · ${remoteProjectRoleLabel(project.role)}` : ""}</small></div><button type="button" className="primary" disabled={busy} onClick={() => void openRemoteProject(project)}>{knownLocalIds[project.id] || localProjects.some((localProject) => localProject.id === project.id) ? "打开缓存" : "打开并缓存"}</button></div>
                    {canInvite ? (
                      <div className="collaboration-invitation-form">
                        <div className="collaboration-invitation-heading"><strong>邀请成员</strong><small>人工分享一次性邀请码</small></div>
                        <div className="collaboration-invitation-fields">
                          <input aria-label={`邀请邮箱-${project.id}`} type="email" placeholder="成员邮箱" value={invitationEmail} onChange={(event) => setInvitationEmail(event.target.value)} autoComplete="email" />
                          <select aria-label={`邀请角色-${project.id}`} value={invitationRole} onChange={(event) => setInvitationRole(event.target.value)}>
                            <option value="annotator">标注员</option>
                            <option value="reviewer">审阅员</option>
                            <option value="viewer">查看者</option>
                            <option value="manager">管理员</option>
                          </select>
                          <button type="button" disabled={busy || !invitationEmail.trim()} onClick={() => void createInvitation(project)}>生成邀请码</button>
                        </div>
                        {invitation ? (
                          <div className="collaboration-invitation-result" role="status">
                            <code className="collaboration-invitation-token">{invitation.token}</code>
                            <button type="button" onClick={() => void copyInvitationToken(invitation.token)}>复制</button>
                            {invitation.expiresAt ? <small>有效期至 {new Date(invitation.expiresAt).toLocaleString()}</small> : null}
                          </div>
                        ) : null}
                      </div>
                    ) : null}
                  </article>
                );
              })}
            </div>}
            <div className="collaboration-section-heading"><strong>登录设备</strong><span>{profile?.devices.length ?? 0} 台</span></div>
            <div className="collaboration-device-list">
              {profile?.devices.map((device) => <div className="collaboration-device-row" key={device.id}><div><strong>{device.name || device.deviceId}</strong><small><Smartphone size={12} />{device.platform}{device.deviceId === session.deviceId ? " · 当前设备" : ""}</small></div>{device.deviceId === session.deviceId ? <span>当前</span> : <button type="button" disabled={busy || Boolean(device.revokedAt)} onClick={() => void revokeDevice(device.id)}>{device.revokedAt ? "已撤销" : "撤销"}</button>}</div>)}
            </div>
            <div className="collaboration-account-actions"><button type="button" onClick={() => void authRefresh().then((next) => load(next)).catch((error) => showMessage(messageOf(error)))} disabled={busy}><RefreshCw size={15} />刷新登录</button><button type="button" onClick={() => void logout()} disabled={busy}>退出登录</button></div>
          </>
        ) : null}
        <div className="collaboration-footnote"><Cloud size={13} />凭据和请求会话由 Rust 核心管理，前端不保存 token。</div>
      </section>
    </div>
  );
}

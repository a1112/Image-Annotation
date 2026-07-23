import {
  AlertTriangle,
  ArrowRight,
  Columns3,
  GitCompareArrows,
  ImageIcon,
  List,
  MessageSquare,
  Plus,
  RefreshCw,
  Server,
  Trash2,
  Users,
  WifiOff,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import {
  addIssueComment,
  configureRemoteProject,
  createIssue,
  getHybridDiagnostics,
  getProjectSyncSummary,
  listIssueComments,
  listIssues,
  listRemoteProjectMembers,
  listSyncConflicts,
  publishProject,
  removeRemoteProjectMember,
  resolveSyncConflict,
  storeProjectCredential,
  syncProject,
  transitionIssue,
  upsertRemoteProjectMember,
} from "./api/tauri";
import type {
  IssueComment,
  IssueRecord,
  IssueSeverity,
  IssueStatus,
  HybridDiagnostics,
  ProjectRole,
  RemoteProjectMember,
  SyncConflict,
  SyncSummary,
} from "./types/domain";

type IssueView = "list" | "board" | "images";

const severityLabels: Record<IssueSeverity, string> = {
  blocker: "阻塞",
  critical: "严重",
  major: "主要",
  minor: "次要",
  suggestion: "建议",
};

const statusLabels: Record<IssueStatus, string> = {
  open: "待处理",
  in_progress: "处理中",
  resolved: "已修复",
  pending_review: "待复查",
  closed: "已关闭",
  reopened: "重新打开",
};

const nextStatus: Partial<Record<IssueStatus, IssueStatus>> = {
  open: "in_progress",
  in_progress: "resolved",
  resolved: "pending_review",
  pending_review: "closed",
  reopened: "in_progress",
};

function getDeviceId() {
  const key = "image-annotation-device-id";
  const current = window.localStorage.getItem(key);
  if (current) return current;
  const value = window.crypto?.randomUUID?.() ?? `device-${Date.now()}`;
  window.localStorage.setItem(key, value);
  return value;
}

export function HybridProjectPanel({ projectId }: { projectId: string }) {
  const [issues, setIssues] = useState<IssueRecord[]>([]);
  const [comments, setComments] = useState<IssueComment[]>([]);
  const [conflicts, setConflicts] = useState<SyncConflict[]>([]);
  const [diagnostics, setDiagnostics] = useState<HybridDiagnostics | null>(null);
  const [members, setMembers] = useState<RemoteProjectMember[]>([]);
  const [summary, setSummary] = useState<SyncSummary | null>(null);
  const [selectedIssueId, setSelectedIssueId] = useState<string | null>(null);
  const [view, setView] = useState<IssueView>("list");
  const [statusFilter, setStatusFilter] = useState<"all" | IssueStatus>("all");
  const [severityFilter, setSeverityFilter] = useState<"all" | IssueSeverity>("all");
  const [query, setQuery] = useState("");
  const [panel, setPanel] = useState<"issues" | "conflicts" | "members" | "diagnostics">("issues");
  const [showCreate, setShowCreate] = useState(false);
  const [showRemote, setShowRemote] = useState(false);
  const [showMemberEditor, setShowMemberEditor] = useState(false);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);

  const reload = useCallback(async () => {
    const [nextIssues, nextSummary, nextConflicts, nextDiagnostics] = await Promise.all([
      listIssues(projectId, true),
      getProjectSyncSummary(projectId),
      listSyncConflicts(projectId),
      getHybridDiagnostics(projectId),
    ]);
    setIssues(nextIssues);
    setSummary(nextSummary);
    setConflicts(nextConflicts);
    setDiagnostics(nextDiagnostics);
  }, [projectId]);

  useEffect(() => {
    reload().catch((error) => setMessage(error instanceof Error ? error.message : String(error)));
    const timer = window.setInterval(() => {
      reload().catch(() => undefined);
    }, 15_000);
    return () => window.clearInterval(timer);
  }, [reload]);

  useEffect(() => {
    if (!selectedIssueId) {
      setComments([]);
      return;
    }
    listIssueComments(projectId, selectedIssueId)
      .then(setComments)
      .catch((error) => setMessage(error instanceof Error ? error.message : String(error)));
  }, [projectId, selectedIssueId]);

  const filteredIssues = useMemo(() => issues.filter((issue) => {
    const matchesStatus = statusFilter === "all" || issue.status === statusFilter;
    const matchesSeverity = severityFilter === "all" || issue.severity === severityFilter;
    const normalizedQuery = query.trim().toLocaleLowerCase();
    const matchesQuery = !normalizedQuery
      || `${issue.title} ${issue.description} ${issue.imageId} ${issue.assigneeId ?? ""}`
        .toLocaleLowerCase()
        .includes(normalizedQuery);
    return matchesStatus && matchesSeverity && matchesQuery;
  }), [issues, query, severityFilter, statusFilter]);

  const selectedIssue = issues.find((issue) => issue.id === selectedIssueId) ?? null;
  const openConflictCount = conflicts.filter((conflict) => conflict.status === "open").length;

  async function runSync() {
    setBusy(true);
    setMessage(null);
    try {
      const result = await syncProject(projectId);
      setMessage(`同步完成：上传 ${result.pushed}，拉取 ${result.pulled}，冲突 ${result.conflicts}`);
      await reload();
    } catch (error) {
      setMessage(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  }

  async function moveIssue(issue: IssueRecord, target: IssueStatus) {
    setBusy(true);
    try {
      const updated = await transitionIssue(projectId, issue.id, target);
      setIssues((current) => current.map((item) => item.id === updated.id ? updated : item));
      setSelectedIssueId(updated.id);
      setSummary(await getProjectSyncSummary(projectId));
    } catch (error) {
      setMessage(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  }

  async function resolveConflict(conflict: SyncConflict, resolution: "local" | "remote") {
    setBusy(true);
    try {
      await resolveSyncConflict(projectId, conflict.id, resolution);
      await reload();
    } catch (error) {
      setMessage(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  }

  async function loadMembers() {
    if (summary?.projectMode === "local_only") {
      setMembers([]);
      return;
    }
    setBusy(true);
    try {
      setMembers(await listRemoteProjectMembers(projectId));
    } catch (error) {
      setMessage(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  }

  async function changeMemberRole(member: RemoteProjectMember, role: ProjectRole) {
    setBusy(true);
    try {
      const updated = await upsertRemoteProjectMember(projectId, member.userId, role);
      setMembers((current) => current.map((item) => item.userId === updated.userId
        ? { ...item, ...updated }
        : item));
    } catch (error) {
      setMessage(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  }

  async function removeMember(member: RemoteProjectMember) {
    setBusy(true);
    try {
      await removeRemoteProjectMember(projectId, member.userId);
      setMembers((current) => current.filter((item) => item.userId !== member.userId));
    } catch (error) {
      setMessage(error instanceof Error ? error.message : String(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="hybrid-workspace">
      <header className="hybrid-header">
        <div>
          <span className="eyebrow">QUALITY & SYNC</span>
          <h2>缺陷与协作</h2>
        </div>
        <div className="hybrid-header-actions">
          <button className={panel === "issues" ? "active" : ""} type="button" onClick={() => setPanel("issues")}>
            <AlertTriangle size={14} />
            缺陷
            <strong>{issues.filter((issue) => issue.status !== "closed").length}</strong>
          </button>
          <button className={panel === "conflicts" ? "active danger" : ""} type="button" onClick={() => setPanel("conflicts")}>
            <GitCompareArrows size={14} />
            冲突
            <strong>{openConflictCount}</strong>
          </button>
          <button className={panel === "diagnostics" ? "active" : ""} type="button" onClick={() => setPanel("diagnostics")}>
            <Server size={14} />
            诊断
          </button>
          <button
            className={panel === "members" ? "active" : ""}
            disabled={summary?.projectMode === "local_only"}
            type="button"
            onClick={() => {
              setPanel("members");
              void loadMembers();
            }}
          >
            <Users size={14} />
            成员
            {members.length > 0 ? <strong>{members.length}</strong> : null}
          </button>
          <button type="button" onClick={() => setShowRemote(true)}>
            <Server size={14} />
            远程配置
          </button>
          <button className="primary" disabled={busy || summary?.projectMode === "local_only"} type="button" onClick={runSync}>
            <RefreshCw className={busy ? "spin" : ""} size={14} />
            立即同步
          </button>
        </div>
      </header>

      <div className="hybrid-sync-strip">
        <span className={`hybrid-mode ${summary?.projectMode ?? "local_only"}`}>
          {summary?.projectMode === "mirrored" ? "完整镜像" : summary?.projectMode === "cloud_linked" ? "云端关联" : "纯本地"}
        </span>
        <span><strong>{summary?.pendingOperations ?? 0}</strong> 待同步</span>
        <span><strong>{summary?.failedOperations ?? 0}</strong> 失败</span>
        <span><strong>{summary?.conflictCount ?? 0}</strong> 冲突</span>
        <span>最后推送 {summary?.lastPushedAt ?? "从未"}</span>
        <span>最后拉取 {summary?.lastPulledAt ?? "从未"}</span>
        {summary?.projectMode === "local_only" ? <span className="offline-note"><WifiOff size={13} />离线可用</span> : null}
      </div>

      {message ? <div className="hybrid-message">{message}<button type="button" onClick={() => setMessage(null)}><X size={13} /></button></div> : null}

      {panel === "issues" ? (
        <>
          <div className="issue-toolbar">
            <div className="issue-view-switch">
              <button className={view === "list" ? "active" : ""} type="button" onClick={() => setView("list")}><List size={14} />列表</button>
              <button className={view === "board" ? "active" : ""} type="button" onClick={() => setView("board")}><Columns3 size={14} />看板</button>
              <button className={view === "images" ? "active" : ""} type="button" onClick={() => setView("images")}><ImageIcon size={14} />图片定位</button>
            </div>
            <input aria-label="搜索缺陷" placeholder="搜索标题、图片、负责人" value={query} onChange={(event) => setQuery(event.target.value)} />
            <select aria-label="按状态筛选" value={statusFilter} onChange={(event) => setStatusFilter(event.target.value as "all" | IssueStatus)}>
              <option value="all">全部状态</option>
              {Object.entries(statusLabels).map(([value, label]) => <option key={value} value={value}>{label}</option>)}
            </select>
            <select aria-label="按严重度筛选" value={severityFilter} onChange={(event) => setSeverityFilter(event.target.value as "all" | IssueSeverity)}>
              <option value="all">全部严重度</option>
              {Object.entries(severityLabels).map(([value, label]) => <option key={value} value={value}>{label}</option>)}
            </select>
            <button className="primary" type="button" onClick={() => setShowCreate(true)}><Plus size={14} />新建缺陷</button>
          </div>

          <div className={`issue-content view-${view}`}>
            <IssueCollection
              issues={filteredIssues}
              selectedIssueId={selectedIssueId}
              view={view}
              onSelect={setSelectedIssueId}
            />
            {selectedIssue ? (
              <IssueDetail
                busy={busy}
                comments={comments}
                issue={selectedIssue}
                onAddComment={async (content) => {
                  const comment = await addIssueComment(projectId, selectedIssue.id, content);
                  setComments((current) => [...current, comment]);
                  setSummary(await getProjectSyncSummary(projectId));
                }}
                onClose={() => setSelectedIssueId(null)}
                onTransition={(target) => moveIssue(selectedIssue, target)}
              />
            ) : null}
          </div>
        </>
      ) : panel === "conflicts" ? (
        <ConflictCenter conflicts={conflicts} busy={busy} onResolve={resolveConflict} />
      ) : panel === "members" ? (
        <MemberCenter
          busy={busy}
          members={members}
          onAdd={() => setShowMemberEditor(true)}
          onRefresh={loadMembers}
          onRemove={removeMember}
          onRoleChange={changeMemberRole}
        />
      ) : (
        <DiagnosticsPanel diagnostics={diagnostics} onRefresh={reload} />
      )}

      {showCreate ? (
        <CreateIssueDialog
          onClose={() => setShowCreate(false)}
          onCreate={async (input) => {
            const issue = await createIssue(projectId, input);
            setIssues((current) => [issue, ...current]);
            setSelectedIssueId(issue.id);
            setShowCreate(false);
            setSummary(await getProjectSyncSummary(projectId));
          }}
        />
      ) : null}
      {showRemote ? (
        <RemoteConfigDialog
          projectId={projectId}
          onClose={() => setShowRemote(false)}
          onConfigured={async (resultMessage) => {
            setShowRemote(false);
            setSummary(await getProjectSyncSummary(projectId));
            if (resultMessage) setMessage(resultMessage);
          }}
        />
      ) : null}
      {showMemberEditor ? (
        <MemberDialog
          onClose={() => setShowMemberEditor(false)}
          onSubmit={async (userId, role) => {
            const member = await upsertRemoteProjectMember(projectId, userId, role);
            setMembers((current) => {
              const exists = current.some((item) => item.userId === member.userId);
              return exists
                ? current.map((item) => item.userId === member.userId ? { ...item, ...member } : item)
                : [...current, member];
            });
            setShowMemberEditor(false);
          }}
        />
      ) : null}
    </div>
  );
}

const roleLabels: Record<ProjectRole, string> = {
  owner: "所有者",
  manager: "经理",
  annotator: "标注员",
  reviewer: "审核员",
  viewer: "查看者",
};

function MemberCenter({
  members,
  busy,
  onAdd,
  onRefresh,
  onRemove,
  onRoleChange,
}: {
  members: RemoteProjectMember[];
  busy: boolean;
  onAdd: () => void;
  onRefresh: () => Promise<void>;
  onRemove: (member: RemoteProjectMember) => Promise<void>;
  onRoleChange: (member: RemoteProjectMember, role: ProjectRole) => Promise<void>;
}) {
  return (
    <section className="member-center">
      <header>
        <div><Users size={17} /><strong>项目成员</strong><span>{members.length}</span></div>
        <div>
          <button disabled={busy} type="button" onClick={() => void onRefresh()}><RefreshCw size={14} />刷新</button>
          <button className="primary" disabled={busy} type="button" onClick={onAdd}><Plus size={14} />添加成员</button>
        </div>
      </header>
      {members.length > 0 ? (
        <div className="member-table">
          <div className="member-table-head"><span>用户 ID</span><span>角色</span><span>加入时间</span><span /></div>
          {members.map((member) => (
            <div className="member-row" key={member.userId}>
              <strong>{member.userId}</strong>
              <select
                aria-label={`修改 ${member.userId} 的角色`}
                disabled={busy}
                value={member.role}
                onChange={(event) => void onRoleChange(member, event.target.value as ProjectRole)}
              >
                {(Object.keys(roleLabels) as ProjectRole[]).map((role) => (
                  <option key={role} value={role}>{roleLabels[role]}</option>
                ))}
              </select>
              <span>{formatDiagnosticTime(member.joinedAt)}</span>
              <button
                aria-label={`移除 ${member.userId}`}
                className="danger-icon"
                disabled={busy}
                type="button"
                onClick={() => void onRemove(member)}
              >
                <Trash2 size={14} />
              </button>
            </div>
          ))}
        </div>
      ) : (
        <div className="hybrid-empty"><Users size={23} /><strong>暂无可显示成员</strong></div>
      )}
    </section>
  );
}

function MemberDialog({
  onClose,
  onSubmit,
}: {
  onClose: () => void;
  onSubmit: (userId: string, role: ProjectRole) => Promise<void>;
}) {
  const [userId, setUserId] = useState("");
  const [role, setRole] = useState<ProjectRole>("annotator");
  const [submitting, setSubmitting] = useState(false);
  return (
    <div className="modal-backdrop hybrid-dialog-backdrop">
      <form className="hybrid-dialog" onSubmit={(event) => {
        event.preventDefault();
        setSubmitting(true);
        void onSubmit(userId.trim(), role).finally(() => setSubmitting(false));
      }}>
        <header><div><span className="eyebrow">PROJECT MEMBER</span><h3>添加项目成员</h3></div><button type="button" onClick={onClose}><X size={16} /></button></header>
        <label>用户 UUID<input required value={userId} onChange={(event) => setUserId(event.target.value)} /></label>
        <label>角色<select value={role} onChange={(event) => setRole(event.target.value as ProjectRole)}>{(Object.keys(roleLabels) as ProjectRole[]).map((value) => <option key={value} value={value}>{roleLabels[value]}</option>)}</select></label>
        <footer><button type="button" onClick={onClose}>取消</button><button className="primary" disabled={submitting} type="submit">{submitting ? "添加中" : "添加成员"}</button></footer>
      </form>
    </div>
  );
}

function DiagnosticsPanel({
  diagnostics,
  onRefresh,
}: {
  diagnostics: HybridDiagnostics | null;
  onRefresh: () => Promise<void>;
}) {
  if (!diagnostics) {
    return <div className="hybrid-empty"><Server size={23} /><strong>正在读取诊断信息</strong></div>;
  }
  const modeLabel = diagnostics.projectMode === "mirrored"
    ? "完整镜像"
    : diagnostics.projectMode === "cloud_linked"
      ? "云端关联"
      : "纯本地";
  return (
    <section className="hybrid-diagnostics">
      <header>
        <div>
          <span className={`hybrid-mode ${diagnostics.projectMode}`}>{modeLabel}</span>
          <strong>{diagnostics.remoteProjectId ?? diagnostics.projectId}</strong>
          <small>{diagnostics.serverUrl ?? "未关联服务器"}</small>
        </div>
        <button type="button" onClick={() => void onRefresh()}><RefreshCw size={14} />刷新</button>
      </header>
      <div className="diagnostic-metrics">
        <article>
          <span>待同步</span>
          <strong>{diagnostics.pendingOperations}</strong>
          <small>{diagnostics.retryingOperations} 正在重试</small>
        </article>
        <article className={diagnostics.failedOperations > 0 ? "warning" : ""}>
          <span>失败</span>
          <strong>{diagnostics.failedOperations}</strong>
          <small>最早 {formatDiagnosticTime(diagnostics.oldestPendingAt)}</small>
        </article>
        <article className={diagnostics.conflictCount > 0 ? "danger" : ""}>
          <span>冲突</span>
          <strong>{diagnostics.conflictCount}</strong>
          <small>需要人工决策</small>
        </article>
        <article>
          <span>本地缓存</span>
          <strong>{formatBytes(diagnostics.cacheBytes)}</strong>
          <small>{diagnostics.cacheEntries} 个对象</small>
        </article>
      </div>
      <div className="diagnostic-table">
        <div><span>设备</span><strong>{diagnostics.deviceId ?? "本地设备"}</strong></div>
        <div><span>游标</span><strong>{diagnostics.cursor ?? "未初始化"}</strong></div>
        <div><span>最后推送</span><strong>{formatDiagnosticTime(diagnostics.lastPushedAt)}</strong></div>
        <div><span>最后拉取</span><strong>{formatDiagnosticTime(diagnostics.lastPulledAt)}</strong></div>
        <div><span>缓存策略</span><strong>{diagnostics.cachePolicy ?? "未配置"}</strong></div>
        <div><span>自动同步</span><strong>{diagnostics.autoSync ? "启用" : "停用"}</strong></div>
      </div>
      {diagnostics.lastError ? (
        <div className="diagnostic-error">
          <AlertTriangle size={15} />
          <div><strong>最后错误</strong><pre>{diagnostics.lastError}</pre></div>
        </div>
      ) : null}
    </section>
  );
}

function formatDiagnosticTime(value: string | null) {
  if (!value) return "从未";
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
}

function formatBytes(value: number) {
  if (value < 1024) return `${value} B`;
  if (value < 1024 ** 2) return `${(value / 1024).toFixed(1)} KB`;
  if (value < 1024 ** 3) return `${(value / 1024 ** 2).toFixed(1)} MB`;
  return `${(value / 1024 ** 3).toFixed(1)} GB`;
}

function IssueCollection({
  issues,
  selectedIssueId,
  view,
  onSelect,
}: {
  issues: IssueRecord[];
  selectedIssueId: string | null;
  view: IssueView;
  onSelect: (id: string) => void;
}) {
  if (issues.length === 0) {
    return <div className="hybrid-empty"><AlertTriangle size={22} /><strong>没有符合条件的缺陷</strong><span>调整筛选条件或创建新缺陷。</span></div>;
  }
  if (view === "board") {
    return (
      <div className="issue-board">
        {(Object.keys(statusLabels) as IssueStatus[]).map((status) => (
          <section key={status}>
            <header><strong>{statusLabels[status]}</strong><span>{issues.filter((issue) => issue.status === status).length}</span></header>
            {issues.filter((issue) => issue.status === status).map((issue) => (
              <IssueCard issue={issue} key={issue.id} selected={selectedIssueId === issue.id} onSelect={onSelect} />
            ))}
          </section>
        ))}
      </div>
    );
  }
  if (view === "images") {
    const grouped = issues.reduce((result, issue) => {
      const current = result.get(issue.imageId) ?? [];
      current.push(issue);
      result.set(issue.imageId, current);
      return result;
    }, new Map<string, IssueRecord[]>());
    return (
      <div className="issue-image-groups">
        {Array.from(grouped.entries()).map(([imageId, imageIssues]) => (
          <section key={imageId}>
            <header><ImageIcon size={15} /><strong>{imageId}</strong><span>{imageIssues.length}</span></header>
            {imageIssues.map((issue) => <IssueCard issue={issue} key={issue.id} selected={selectedIssueId === issue.id} onSelect={onSelect} />)}
          </section>
        ))}
      </div>
    );
  }
  return (
    <div className="issue-list">
      {issues.map((issue) => <IssueCard issue={issue} key={issue.id} selected={selectedIssueId === issue.id} onSelect={onSelect} />)}
    </div>
  );
}

function IssueCard({ issue, selected, onSelect }: { issue: IssueRecord; selected: boolean; onSelect: (id: string) => void }) {
  return (
    <button className={`issue-card ${selected ? "selected" : ""}`} type="button" onClick={() => onSelect(issue.id)}>
      <span className={`severity-dot ${issue.severity}`} />
      <span className="issue-card-main"><strong>{issue.title}</strong><small>{issue.imageId}</small></span>
      <span className={`issue-status ${issue.status}`}>{statusLabels[issue.status]}</span>
      <span className={`issue-severity ${issue.severity}`}>{severityLabels[issue.severity]}</span>
      <span className="issue-assignee">{issue.assigneeId ?? "未分配"}</span>
      <ArrowRight size={14} />
    </button>
  );
}

function IssueDetail({
  issue,
  comments,
  busy,
  onClose,
  onTransition,
  onAddComment,
}: {
  issue: IssueRecord;
  comments: IssueComment[];
  busy: boolean;
  onClose: () => void;
  onTransition: (status: IssueStatus) => void;
  onAddComment: (content: string) => Promise<void>;
}) {
  const [content, setContent] = useState("");
  const target = nextStatus[issue.status];
  return (
    <aside className="issue-detail">
      <header><div><span>{issue.id}</span><h3>{issue.title}</h3></div><button type="button" onClick={onClose}><X size={15} /></button></header>
      <div className="issue-detail-meta">
        <span className={`issue-status ${issue.status}`}>{statusLabels[issue.status]}</span>
        <span className={`issue-severity ${issue.severity}`}>{severityLabels[issue.severity]}</span>
        <span>图片 {issue.imageId}</span>
        <span>负责人 {issue.assigneeId ?? "未分配"}</span>
      </div>
      <p>{issue.description || "没有补充说明。"}</p>
      <div className="issue-comments">
        <strong><MessageSquare size={14} />评论 {comments.length}</strong>
        {comments.map((comment) => <article key={comment.id}><span>{comment.authorId}</span><p>{comment.content}</p><small>{comment.createdAt}</small></article>)}
      </div>
      <form onSubmit={(event) => {
        event.preventDefault();
        if (!content.trim()) return;
        onAddComment(content).then(() => setContent(""));
      }}>
        <textarea placeholder="添加处理记录…" value={content} onChange={(event) => setContent(event.target.value)} />
        <button disabled={!content.trim()} type="submit">添加评论</button>
      </form>
      <div className="issue-transition-actions">
        {target ? <button className="primary" disabled={busy} type="button" onClick={() => onTransition(target)}>流转至 {statusLabels[target]}</button> : null}
        {issue.status === "pending_review" ? <button disabled={busy} type="button" onClick={() => onTransition("reopened")}>复查不通过</button> : null}
        {issue.status === "closed" ? <button disabled={busy} type="button" onClick={() => onTransition("reopened")}>重新打开</button> : null}
      </div>
    </aside>
  );
}

function ConflictCenter({
  conflicts,
  busy,
  onResolve,
}: {
  conflicts: SyncConflict[];
  busy: boolean;
  onResolve: (conflict: SyncConflict, resolution: "local" | "remote") => void;
}) {
  const openConflicts = conflicts.filter((conflict) => conflict.status === "open");
  if (openConflicts.length === 0) {
    return <div className="hybrid-empty"><GitCompareArrows size={23} /><strong>没有待处理冲突</strong><span>本地与服务器版本目前一致。</span></div>;
  }
  return (
    <div className="conflict-list">
      {openConflicts.map((conflict) => (
        <article key={conflict.id}>
          <header><div><span>{conflict.entityType}</span><strong>{conflict.entityId}</strong></div><small>{conflict.createdAt}</small></header>
          <div className="conflict-versions">
            <section><strong>共同基础</strong><pre>{JSON.stringify(conflict.base, null, 2)}</pre></section>
            <section className="local"><strong>本地版本</strong><pre>{JSON.stringify(conflict.local, null, 2)}</pre></section>
            <section className="remote"><strong>服务器版本</strong><pre>{JSON.stringify(conflict.remote, null, 2)}</pre></section>
          </div>
          <footer>
            <button disabled={busy} type="button" onClick={() => onResolve(conflict, "remote")}>保留服务器版本</button>
            <button className="primary" disabled={busy} type="button" onClick={() => onResolve(conflict, "local")}>保留本地并重试</button>
          </footer>
        </article>
      ))}
    </div>
  );
}

function CreateIssueDialog({
  onClose,
  onCreate,
}: {
  onClose: () => void;
  onCreate: (input: { imageId: string; annotationObjectId?: string; title: string; description: string; severity: IssueSeverity }) => Promise<void>;
}) {
  const [imageId, setImageId] = useState("");
  const [objectId, setObjectId] = useState("");
  const [title, setTitle] = useState("");
  const [description, setDescription] = useState("");
  const [severity, setSeverity] = useState<IssueSeverity>("major");
  return (
    <div className="modal-backdrop hybrid-dialog-backdrop">
      <form className="hybrid-dialog" onSubmit={(event) => {
        event.preventDefault();
        void onCreate({ imageId, annotationObjectId: objectId || undefined, title, description, severity });
      }}>
        <header><div><span className="eyebrow">NEW ISSUE</span><h3>创建缺陷</h3></div><button type="button" onClick={onClose}><X size={16} /></button></header>
        <label>图片 ID<input required value={imageId} onChange={(event) => setImageId(event.target.value)} /></label>
        <label>标注对象 ID（可选）<input value={objectId} onChange={(event) => setObjectId(event.target.value)} /></label>
        <label>标题<input required value={title} onChange={(event) => setTitle(event.target.value)} /></label>
        <label>严重度<select value={severity} onChange={(event) => setSeverity(event.target.value as IssueSeverity)}>{Object.entries(severityLabels).map(([value, label]) => <option key={value} value={value}>{label}</option>)}</select></label>
        <label>描述<textarea value={description} onChange={(event) => setDescription(event.target.value)} /></label>
        <footer><button type="button" onClick={onClose}>取消</button><button className="primary" type="submit">创建缺陷</button></footer>
      </form>
    </div>
  );
}

function RemoteConfigDialog({
  projectId,
  onClose,
  onConfigured,
}: {
  projectId: string;
  onClose: () => void;
  onConfigured: (resultMessage?: string) => Promise<void>;
}) {
  const [action, setAction] = useState<"publish" | "link">("publish");
  const [serverUrl, setServerUrl] = useState("http://127.0.0.1:8080");
  const [remoteProjectId, setRemoteProjectId] = useState(projectId);
  const [mode, setMode] = useState<"cloud_linked" | "mirrored">("cloud_linked");
  const [cachePolicy, setCachePolicy] = useState<"thumbnail_only" | "on_demand" | "full_mirror">("on_demand");
  const [accessToken, setAccessToken] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  return (
    <div className="modal-backdrop hybrid-dialog-backdrop">
      <form className="hybrid-dialog" onSubmit={async (event) => {
        event.preventDefault();
        setSubmitting(true);
        setError(null);
        try {
          if (action === "publish") {
            const result = await publishProject(projectId, {
              serverUrl,
              deviceId: getDeviceId(),
              mode,
              cachePolicy,
              accessToken: accessToken.trim() || undefined,
            });
            await onConfigured(
              `发布完成：上传 ${result.uploadedAssets}，复用 ${result.reusedAssets}，初始化标注 ${result.initializedAnnotations}，冲突 ${result.conflicts}`,
            );
          } else {
            await configureRemoteProject(projectId, {
              serverUrl,
              remoteProjectId,
              deviceId: getDeviceId(),
              mode,
              cachePolicy,
              autoSync: true,
            });
            if (accessToken.trim()) {
              await storeProjectCredential(projectId, accessToken.trim());
            }
            await onConfigured();
          }
        } catch (caught) {
          setError(caught instanceof Error ? caught.message : String(caught));
        } finally {
          setSubmitting(false);
        }
      }}>
        <header><div><span className="eyebrow">REMOTE PROJECT</span><h3>远程项目配置</h3></div><button type="button" onClick={onClose}><X size={16} /></button></header>
        <div className="remote-action-switch">
          <button className={action === "publish" ? "active" : ""} type="button" onClick={() => setAction("publish")}>发布当前项目</button>
          <button className={action === "link" ? "active" : ""} type="button" onClick={() => setAction("link")}>关联已有项目</button>
        </div>
        <label>服务器地址<input required value={serverUrl} onChange={(event) => setServerUrl(event.target.value)} /></label>
        {action === "link" ? <label>服务器项目 ID<input required value={remoteProjectId} onChange={(event) => setRemoteProjectId(event.target.value)} /></label> : null}
        <label>项目模式<select value={mode} onChange={(event) => setMode(event.target.value as typeof mode)}><option value="cloud_linked">云端关联</option><option value="mirrored">完整镜像</option></select></label>
        <label>缓存策略<select value={cachePolicy} onChange={(event) => setCachePolicy(event.target.value as typeof cachePolicy)}><option value="thumbnail_only">仅缩略图</option><option value="on_demand">按需原图</option><option value="full_mirror">完整缓存</option></select></label>
        <label>Access Token<input autoComplete="off" placeholder={action === "publish" ? "发布时必填，保存到系统凭据库" : "留空则使用系统凭据库"} type="password" value={accessToken} onChange={(event) => setAccessToken(event.target.value)} /></label>
        {error ? <div className="remote-config-error"><AlertTriangle size={14} />{error}</div> : null}
        <footer><button disabled={submitting} type="button" onClick={onClose}>取消</button><button className="primary" disabled={submitting} type="submit">{submitting ? "处理中…" : action === "publish" ? "发布项目" : "保存并关联"}</button></footer>
      </form>
    </div>
  );
}

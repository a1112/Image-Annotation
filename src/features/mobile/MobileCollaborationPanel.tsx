import { Check, CircleAlert, Cloud, LoaderCircle, LockKeyhole, RotateCcw } from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";
import {
  claimRemoteTaskItem,
  listRemoteReviewQueue,
  listRemoteTaskItems,
  listRemoteTasks,
  releaseRemoteTaskItem,
  flushOfflineQueue,
} from "../../api/mobile";
import {
  cacheRemoteAsset,
  claimTaskItem,
  listReviewQueue,
  listTaskItems,
  listTasks,
  releaseTaskItem,
  reviewTaskItem,
} from "../../api/tauri";
import type { DatasetImage, TaskItem, AnnotationTask } from "../../types/domain";
import type { RemoteReviewItem, RemoteTask, RemoteTaskItem } from "../../types/mobile";

type CollaborationTask = RemoteTask | AnnotationTask;
type CollaborationItem = RemoteTaskItem | TaskItem;
type CollaborationReview = RemoteReviewItem | DatasetImage;

export type MobileCollaborationPanelProps = {
  localProjectId: string;
  remoteProjectId?: string;
  authenticated: boolean;
  section: "tasks" | "review";
  onMessage: (message: string) => void;
  onOpenImage: (imageId: string) => void;
};

function isRemoteReview(item: CollaborationReview): item is RemoteReviewItem {
  return "imageId" in item;
}

function displayStatus(value?: string | null): string {
  const labels: Record<string, string> = {draft:"草稿",active:"进行中",unassigned:"待分配",assigned:"待领取",in_progress:"标注中",paused:"已暂停",submitted:"待审阅",pending_review:"待审阅",approved:"已通过",rejected:"已退回",done:"已完成",archived:"已归档",closed:"已关闭",resolved:"已修复"};
  return value ? labels[value] ?? value : "待处理";
}

function itemImageId(item: CollaborationItem): string {
  return item.imageId;
}

export function MobileCollaborationPanel({
  localProjectId,
  remoteProjectId,
  authenticated,
  section,
  onMessage,
  onOpenImage,
}: MobileCollaborationPanelProps) {
  const [tasks, setTasks] = useState<CollaborationTask[]>([]);
  const [items, setItems] = useState<CollaborationItem[]>([]);
  const [reviewQueue, setReviewQueue] = useState<CollaborationReview[]>([]);
  const [selectedTaskId, setSelectedTaskId] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      if (remoteProjectId && authenticated) {
        const remoteReviewPromise = section === "review"
          ? listRemoteReviewQueue(remoteProjectId)
          : Promise.resolve<RemoteReviewItem[]>([]);
        const [remoteTasksResult, remoteReviewResult] = await Promise.allSettled([
          listRemoteTasks(remoteProjectId),
          remoteReviewPromise,
        ]);
        if (remoteTasksResult.status === "fulfilled") {
          setTasks(remoteTasksResult.value);
          const firstTask = remoteTasksResult.value[0];
          setSelectedTaskId((current) => current ?? firstTask?.id ?? null);
        }
        if (remoteReviewResult.status === "fulfilled") setReviewQueue(remoteReviewResult.value);
        const firstTask = remoteTasksResult.status === "fulfilled" ? remoteTasksResult.value[0] : null;
        if (firstTask) {
          const itemResult = await listRemoteTaskItems(firstTask.id);
          setItems(itemResult);
        } else {
          setItems([]);
        }
        if (remoteTasksResult.status === "rejected") {
          const localTasks = await listTasks(localProjectId);
          setTasks(localTasks);
          setSelectedTaskId((current) => current ?? localTasks[0]?.id ?? null);
          setItems(localTasks[0] ? await listTaskItems(localProjectId, localTasks[0].id) : []);
        }
        if (remoteReviewResult.status === "rejected" && section === "review") setReviewQueue(await listReviewQueue(localProjectId));
        const firstError = [remoteTasksResult, remoteReviewResult]
          .find((result): result is PromiseRejectedResult => result.status === "rejected");
        if (firstError) onMessage(`远端协同读取失败：${firstError.reason instanceof Error ? firstError.reason.message : String(firstError.reason)}`);
      } else {
        const [localTasks, localReview] = await Promise.all([
          listTasks(localProjectId),
          section === "review" ? listReviewQueue(localProjectId) : Promise.resolve<DatasetImage[]>([]),
        ]);
        setTasks(localTasks);
        setSelectedTaskId((current) => current ?? localTasks[0]?.id ?? null);
        setReviewQueue(localReview);
        const firstTask = localTasks[0];
        setItems(firstTask ? await listTaskItems(localProjectId, firstTask.id) : []);
      }
    } catch (error) {
      onMessage(`任务/审阅读取失败：${error instanceof Error ? error.message : String(error)}`);
    } finally {
      setLoading(false);
    }
  }, [authenticated, localProjectId, onMessage, remoteProjectId, section]);

  useEffect(() => {
    void load();
  }, [load]);

  const selectedTask = useMemo(
    () => tasks.find((task) => task.id === selectedTaskId) ?? tasks[0] ?? null,
    [selectedTaskId, tasks],
  );

  async function toggleClaim(item: CollaborationItem) {
    if (!selectedTask) return;
    const id = itemImageId(item);
    setBusyId(id);
    try {
      const locked = Boolean(item.lockedAt) || item.status === "claimed" || item.status === "in_progress";
      if (remoteProjectId && authenticated) {
        // Claims are server-authoritative. A disconnected device may continue
        // an already cached assignment, but cannot claim another person's work.
        if (locked) await releaseRemoteTaskItem(selectedTask.id, id);
        else await claimRemoteTaskItem(selectedTask.id, id);
        await flushOfflineQueue(localProjectId).catch(() => undefined);
      } else {
        if (locked) await releaseTaskItem(localProjectId, selectedTask.id, id);
        else await claimTaskItem(localProjectId, selectedTask.id, id);
      }
      onMessage(locked ? "已释放任务项" : "已领取任务项");
      await load();
    } catch (error) {
      onMessage(`任务项操作失败：${error instanceof Error ? error.message : String(error)}`);
    } finally {
      setBusyId(null);
    }
  }

  async function retainTaskOffline() {
    setBusyId("offline-cache");
    let saved = 0;
    try {
      for (const item of items) {
        const localImageId = "clientImageId" in item && item.clientImageId ? item.clientImageId : item.imageId;
        await cacheRemoteAsset(localProjectId, localImageId, undefined, true);
        saved += 1;
        onMessage(`正在保留任务图片 ${saved}/${items.length}`);
      }
      onMessage(`已离线保留 ${saved} 张原图，可断网继续标注`);
    } catch (error) {
      onMessage(`已保留 ${saved}/${items.length} 张，剩余下载失败：${error instanceof Error ? error.message : String(error)}`);
    } finally {
      setBusyId(null);
    }
  }

  async function decideReview(item: CollaborationReview, decision: "approve" | "reject") {
    const id = "imageId" in item ? item.imageId : item.id;
    setBusyId(id);
    try {
      const localImageId = isRemoteReview(item) ? item.clientImageId ?? item.imageId : item.id;
      // One durable write path. Network/auth/revision failures must never cause
      // a second independent review operation via another transport.
      await reviewTaskItem(localProjectId, localImageId, decision === "approve" ? "approved" : "rejected", "");
      let confirmed = !remoteProjectId;
      if (remoteProjectId && authenticated) {
        try {
          const status = await flushOfflineQueue(localProjectId);
          confirmed = status.queuedOperations === 0 && status.conflictCount === 0 && !status.lastError;
        } catch {
          confirmed = false;
        }
      }
      setReviewQueue((current) => current.filter((candidate) => ("imageId" in candidate ? candidate.imageId : candidate.id) !== id));
      onMessage(confirmed ? (decision === "approve" ? "已通过审阅" : "已退回审阅") : "审阅已保存，等待同步确认");
    } catch (error) {
      onMessage(`审阅操作失败：${error instanceof Error ? error.message : String(error)}`);
    } finally {
      setBusyId(null);
    }
  }

  return (
    <section className="mobile-collaboration-panel" aria-label={section === "tasks" ? "移动任务协同" : "移动审阅协同"}>
      <div className="mobile-collaboration-heading">
        <div>
          <span className="mobile-eyebrow">{remoteProjectId && authenticated ? "远端协同" : "本地队列"}</span>
          <h2>{section === "tasks" ? "任务领取" : "审阅队列"}</h2>
        </div>
        <button type="button" aria-label="刷新任务和审阅" onClick={() => void load()} disabled={loading}>
          {loading ? <LoaderCircle className="is-spinning" size={16} /> : <RotateCcw size={16} />}
        </button>
      </div>
      {!authenticated && remoteProjectId ? (
        <div className="mobile-collaboration-notice"><LockKeyhole size={16} />登录后才能领取远端任务和提交审阅。</div>
      ) : null}
      {section === "tasks" ? (
        <>
          <div className="mobile-collaboration-task-tabs">
            {tasks.length === 0 ? <span className="mobile-empty-state">暂无任务</span> : null}
            {tasks.map((task) => (
              <button
                type="button"
                key={task.id}
                className={selectedTask?.id === task.id ? "is-active" : ""}
                onClick={() => {
                  setSelectedTaskId(task.id);
                  if (remoteProjectId && authenticated) void listRemoteTaskItems(task.id).then(setItems);
                  else void listTaskItems(localProjectId, task.id).then(setItems);
                }}
              >
                <span>{task.name}</span><small>{displayStatus(task.status)}</small>
              </button>
            ))}
          </div>
          {remoteProjectId && items.length > 0 ? <button type="button" disabled={busyId !== null} onClick={() => void retainTaskOffline()}><Cloud size={15} />离线保留此任务图片（{items.length}）</button> : null}
          <div className="mobile-collaboration-item-list">
            {items.length === 0 ? <span className="mobile-empty-state">此任务暂无图片项</span> : null}
            {items.map((item) => {
              const imageId = itemImageId(item);
              const localImageId = "clientImageId" in item && item.clientImageId ? item.clientImageId : imageId;
              const imageLabel = "fileName" in item && typeof item.fileName === "string" && item.fileName
                ? item.fileName
                : localImageId;
              const locked = Boolean(item.lockedAt) || item.status === "claimed" || item.status === "in_progress";
              return (
                <article className="mobile-collaboration-item" key={item.id || imageId}>
                  <div><strong>{imageLabel}</strong><small>{displayStatus(item.status)}</small></div>
                  <div className="mobile-inline-actions">
                    <button type="button" onClick={() => onOpenImage(localImageId)}>打开标注</button>
                    <button type="button" disabled={busyId === imageId} onClick={() => void toggleClaim(item)}>
                      {locked ? "释放" : "领取"}
                    </button>
                  </div>
                </article>
              );
            })}
          </div>
        </>
      ) : (
        <div className="mobile-collaboration-review-list">
          {reviewQueue.length === 0 ? <span className="mobile-empty-state"><Check size={17} />暂无待审阅图片</span> : null}
          {reviewQueue.map((item) => {
            const id = "imageId" in item ? item.imageId : item.id;
            const localImageId = "clientImageId" in item && item.clientImageId ? item.clientImageId : id;
            const label = "fileName" in item && item.fileName ? item.fileName : id;
            return (
              <article className="mobile-collaboration-review-card" key={id}>
                <div><strong>{label}</strong><small>{displayStatus("qaStatus" in item ? item.qaStatus : "annotationStatus" in item ? item.annotationStatus : item.status)}</small></div>
                <div className="mobile-inline-actions">
                  <button type="button" onClick={() => onOpenImage(localImageId)}>查看标注</button>
                  <button type="button" disabled={busyId === id} onClick={() => void decideReview(item, "reject")}><CircleAlert size={14} />退回</button>
                  <button type="button" className="mobile-primary-button" disabled={busyId === id} onClick={() => void decideReview(item, "approve")}><Check size={14} />通过</button>
                </div>
              </article>
            );
          })}
        </div>
      )}
      <div className="mobile-collaboration-footnote"><Cloud size={13} />{remoteProjectId && authenticated ? "任务领取由服务器确认；标注与审阅可离线保存并在联网后同步。" : "操作写入本地 Rust 队列，联网后再同步。"}</div>
    </section>
  );
}

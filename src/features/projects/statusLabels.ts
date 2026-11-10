const statusLabels: Record<string, string> = {
  approved: "已通过",
  cancelled: "已取消",
  completed: "已完成",
  failed: "失败",
  in_progress: "处理中",
  inProgress: "处理中",
  pending: "待处理",
  pending_review: "待审核",
  pendingReview: "待审核",
  raw: "未处理",
  rejected: "已驳回",
  running: "进行中",
};

export function displayStatusLabel(status: string): string {
  return statusLabels[status] ?? status;
}

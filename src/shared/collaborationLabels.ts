const projectStatusLabels: Record<string, string> = {
  active: "启用",
  archived: "已归档",
  draft: "草稿",
  paused: "已暂停",
  disabled: "已停用",
  deleted: "已删除",
};

const projectRoleLabels: Record<string, string> = {
  owner: "所有者",
  manager: "管理员",
  annotator: "标注员",
  reviewer: "审阅员",
  viewer: "查看者",
};

export function remoteProjectStatusLabel(status?: string | null): string {
  if (!status) return projectStatusLabels.active;
  return projectStatusLabels[status.toLowerCase()] ?? status;
}

export function remoteProjectRoleLabel(role?: string | null): string {
  if (!role) return "";
  return projectRoleLabels[role.toLowerCase()] ?? role;
}

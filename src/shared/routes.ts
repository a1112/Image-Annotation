import { Archive, ClipboardCheck, Download, Home, ImageIcon, Layers3, ShieldCheck, Tags } from "lucide-react";
import type { LucideIcon } from "lucide-react";

export type ProjectTab = "概览" | "数据分组" | "图片" | "类别" | "任务" | "质检" | "快照" | "导出";

export const projectTabs: ProjectTab[] = ["概览", "数据分组", "图片", "类别", "任务", "质检", "快照", "导出"];

export const projectTabIcons: Record<ProjectTab, LucideIcon> = {
  "概览": Home,
  "数据分组": Tags,
  "图片": ImageIcon,
  "类别": Layers3,
  "任务": ClipboardCheck,
  "质检": ShieldCheck,
  "快照": Archive,
  "导出": Download,
};

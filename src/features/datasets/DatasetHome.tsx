import { Download } from "lucide-react";
import { DatasetCard } from "./DatasetCard";
import { RuntimeStatePanel } from "../shared/RuntimeStatePanel";
import type { DatasetImage, DatasetProject } from "../../types/domain";

export type DatasetRuntimeState = "loading" | "ready" | "downloading" | "backend-unavailable" | "download-error";

export function DatasetHome({
  projects,
  projectImages,
  runtimeState,
  runtimeMessage,
  testDatasetDownloaded,
  testDatasetKey,
  onDownload,
  onInfo,
  onAnnotate,
  onOpenWindow,
  onOpen,
}: {
  projects: DatasetProject[];
  projectImages: Record<string, DatasetImage[]>;
  runtimeState: DatasetRuntimeState;
  runtimeMessage: string | null;
  testDatasetDownloaded: boolean;
  testDatasetKey: string;
  onDownload: (datasetKey: string) => void;
  onInfo: () => void;
  onAnnotate: (project: DatasetProject) => void;
  onOpenWindow: (project: DatasetProject) => void;
  onOpen: (projectId: string) => void;
}) {
  return (
    <main className="content"><section className="dataset-main">
      <div className="page-heading"><div><h1>数据集</h1><p>管理本地数据工程、进入标注任务、查看标注进度。</p></div><div className="filter-row">
        <button type="button" disabled={runtimeState === "loading" || runtimeState === "downloading" || runtimeState === "backend-unavailable" || testDatasetDownloaded} onClick={() => onDownload(testDatasetKey)}><Download size={16} />{runtimeState === "downloading" ? "正在下载测试数据" : testDatasetDownloaded ? "测试数据已下载" : "下载测试数据集"}</button>
        <button type="button" onClick={onInfo}>工程信息</button>
        {["全部", "已导入", "待下载", "BBox", "Polygon"].map((filter) => <button className={filter === "全部" ? "selected" : ""} key={filter} type="button">{filter}</button>)}
      </div></div>
      {runtimeState === "backend-unavailable" ? <RuntimeStatePanel title="后端未连接" message="请在 Tauri 桌面环境启动应用。" /> : runtimeState === "downloading" ? <RuntimeStatePanel title="正在下载测试数据..." message="下载完成后会显示数据集和图片预览，期间可以继续使用本地项目。" /> : runtimeState === "download-error" ? <RuntimeStatePanel title="测试数据下载失败" message={runtimeMessage ?? "请检查网络后重试。"} actionLabel="重试下载" onAction={() => onDownload(testDatasetKey)} /> : runtimeState === "loading" ? <RuntimeStatePanel title="正在加载数据集" message="正在读取本地数据工程。" /> : projects.length === 0 ? <RuntimeStatePanel title="暂无数据集" message="可以新建或导入本地数据集，也可以点击“下载测试数据集”获取 COCO128 示例。" /> : null}
      {projects.length > 0 && runtimeState !== "backend-unavailable" ? <div className="dataset-grid">{projects.map((dataset) => <DatasetCard dataset={dataset} key={dataset.id} onAnnotate={() => onAnnotate(dataset)} onOpen={() => onOpen(dataset.id)} onOpenWindow={() => onOpenWindow(dataset)} previewImages={projectImages[dataset.id] ?? []} />)}</div> : null}
    </section></main>
  );
}

import {
  Archive,
  BoxSelect,
  CheckCircle2,
  ChevronDown,
  CircleAlert,
  ClipboardCheck,
  Database,
  Download,
  Eye,
  FileJson,
  FolderKanban,
  Home,
  ImageIcon,
  Layers3,
  Maximize2,
  Minimize2,
  MousePointer2,
  Play,
  Plus,
  Save,
  Search,
  Settings,
  ShieldCheck,
  Square,
  Tags,
  Upload,
  X,
  Zap,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import type { KeyboardEvent, MouseEvent } from "react";
import {
  clearCompletedBackendTasks,
  createDatasetProject,
  downloadTestDataset,
  getFileAssetUrl,
  getImageAnnotations,
  getProjectDetail,
  isBackendUnavailableError,
  listBackendTasks,
  listBuiltinDatasets,
  listDatasetProjects,
  listProjectImages,
  openAnnotationWindow,
  saveImageAnnotations,
} from "./api/tauri";
import type {
  AnnotationObject,
  BackendTask,
  BuiltinDataset,
  DatasetImage,
  DatasetProject,
  ProjectDetail,
} from "./types/domain";
import { invoke } from "@tauri-apps/api/core";

type ProjectTab = "概览" | "数据分组" | "图片" | "类别" | "任务" | "质检" | "导出" | "后端";
type ToolMode = "select" | "bbox" | "polygon" | "pan";
type DataRuntimeState = "loading" | "ready" | "downloading" | "backend-unavailable" | "download-error";
type DatasetCreationForm = {
  name: string;
  datasetType: "yolo-detect" | "yolo-seg";
  demoTemplate: "demo-bbox" | "demo-polygon" | "empty";
};
type Route =
  | { name: "datasets" }
  | { name: "project"; projectId: string; tab?: ProjectTab }
  | { name: "annotate"; projectId: string; imageId?: string };

const projectTabs: ProjectTab[] = ["概览", "数据分组", "图片", "类别", "任务", "质检", "导出", "后端"];
const defaultTestDatasetKey = "coco128";
const defaultDatasetCreationForm: DatasetCreationForm = {
  name: "Demo BBox 数据集",
  datasetType: "yolo-detect",
  demoTemplate: "demo-bbox",
};
const navItems = [
  { label: "首页", icon: Home },
  { label: "数据集", icon: Database },
  { label: "任务", icon: FolderKanban },
  { label: "审核", icon: ClipboardCheck },
  { label: "导出中心", icon: Download },
  { label: "报表", icon: Archive },
  { label: "设置", icon: Settings },
];

async function runDesktopCommand(command: string) {
  try {
    await invoke(command);
  } catch {
    // The UI also runs in a normal browser during development and tests.
  }
}

function parseRoute(): Route {
  const hash = window.location.hash.replace(/^#\/?/, "");
  const parts = hash.split("/").filter(Boolean);
  if (parts[0] === "annotate" && parts[1]) {
    return { name: "annotate", projectId: parts[1], imageId: parts[2] };
  }
  if (parts[0] === "datasets" && parts[1]) {
    return { name: "project", projectId: parts[1] };
  }
  return { name: "datasets" };
}

function navigate(route: string) {
  window.location.hash = route;
}

function formatNumber(value: number) {
  return value.toLocaleString("zh-CN");
}

function useImageAssetUrls(projectId: string | undefined, images: DatasetImage[], limit = 12) {
  const [urls, setUrls] = useState<Record<string, string>>({});

  useEffect(() => {
    if (!projectId || images.length === 0) {
      setUrls({});
      return;
    }

    let cancelled = false;
    Promise.all(
      images.slice(0, limit).map(async (image) => {
        try {
          return [image.id, await getFileAssetUrl(projectId, image.id)] as const;
        } catch {
          return null;
        }
      }),
    ).then((entries) => {
      if (!cancelled) {
        setUrls(Object.fromEntries(entries.filter((entry): entry is readonly [string, string] => Boolean(entry))));
      }
    });

    return () => {
      cancelled = true;
    };
  }, [projectId, images, limit]);

  return urls;
}

function normalizeBox(start: { x: number; y: number }, end: { x: number; y: number }) {
  const x = Math.min(start.x, end.x);
  const y = Math.min(start.y, end.y);
  return {
    x: Number(x.toFixed(1)),
    y: Number(y.toFixed(1)),
    width: Number(Math.abs(end.x - start.x).toFixed(1)),
    height: Number(Math.abs(end.y - start.y).toFixed(1)),
  };
}

function clamp(value: number, min: number, max: number) {
  return Math.max(min, Math.min(max, value));
}

function DatasetCard({
  dataset,
  previewImages,
  onOpen,
  onAnnotate,
  onOpenWindow,
}: {
  dataset: DatasetProject;
  previewImages: DatasetImage[];
  onOpen: () => void;
  onAnnotate: () => void;
  onOpenWindow: () => void;
}) {
  const previewUrls = useImageAssetUrls(dataset.id, previewImages, 3);

  return (
    <article
      aria-label={`${dataset.name} 数据集卡片`}
      className="dataset-card"
      onDoubleClick={onOpen}
      onKeyDown={(event: KeyboardEvent<HTMLElement>) => {
        if (event.key === "Enter") onOpen();
      }}
      tabIndex={0}
    >
      <div className="thumbnail-strip" aria-label={`${dataset.name} samples`}>
        {Array.from({ length: 3 }, (_, index) => {
          const image = previewImages[index];
          return (
          <div className={`sample-thumb traffic-${["a", "b", "c"][index]}`} key={image?.id ?? index}>
            {image && previewUrls[image.id] ? (
              <img alt={`${dataset.name} 预览 ${index + 1}`} src={previewUrls[image.id]} />
            ) : null}
            <span>{index + 1}</span>
          </div>
          );
        })}
      </div>
      <div className="dataset-card__header">
        <div>
          <h3>{dataset.name}</h3>
          <p>{dataset.description}</p>
        </div>
        <span className="quality-chip ready">{dataset.status}</span>
      </div>
      <div className="type-row">
        {dataset.annotationTypes.map((type) => (
          <span className="type-chip" key={type}>
            {type}
          </span>
        ))}
      </div>
      <div className="metric-grid">
        <span>
          <strong>{formatNumber(dataset.imageCount)}</strong>
          图片
        </span>
        <span>
          <strong>{dataset.annotatedPercent}%</strong>
          已标注
        </span>
        <span>
          <strong>{formatNumber(dataset.reviewCount)}</strong>
          待审核
        </span>
        <span>
          <strong>{dataset.classCount}</strong>
          类别
        </span>
      </div>
      <div className="progress-bar" aria-label={`${dataset.annotatedPercent}% annotated`}>
        <span style={{ width: `${dataset.annotatedPercent}%` }} />
      </div>
      <div className="card-footer">
        <span>{dataset.tagGroupCount} 个保存分组</span>
        <div>
          <button type="button" onClick={onOpen}>
            打开
          </button>
          <button type="button" onClick={onAnnotate}>
            标注
          </button>
          <button type="button" onClick={onOpenWindow}>
            独立窗口标注
          </button>
        </div>
      </div>
    </article>
  );
}

function BuiltinDatasetPanel({
  datasets,
  onDownload,
}: {
  datasets: BuiltinDataset[];
  onDownload: (datasetKey: string) => void;
}) {
  return (
    <section className="builtin-panel">
      <div className="detail-header">
        <div>
          <span className="eyebrow">测试数据集</span>
          <h2>内置下载</h2>
        </div>
      </div>
      {datasets.map((dataset) => (
        <article className="builtin-card" key={dataset.key}>
          <div>
            <h3>内置 {dataset.name}</h3>
            <p>{dataset.description}</p>
            <span>{dataset.taskType} / {dataset.format}</span>
          </div>
          <button type="button" onClick={() => onDownload(dataset.key)}>
            下载 {dataset.name}
          </button>
        </article>
      ))}
    </section>
  );
}

function TopBar({
  onBackendTasks,
  onCreateDataset,
  onDataSubmit,
  onDatasets,
}: {
  onBackendTasks: () => void;
  onCreateDataset: () => void;
  onDataSubmit: () => void;
  onDatasets: () => void;
}) {
  return (
    <header className="topbar" data-tauri-drag-region>
      <div className="brand" onClick={onDatasets} role="button" tabIndex={0}>
        <ImageIcon size={19} />
        <span>Image Annotation</span>
      </div>
      <button className="workspace" type="button" data-no-drag>
        数据生产工作区 <ChevronDown size={15} />
      </button>
      <label className="search" data-no-drag>
        <Search size={16} />
        <input placeholder="搜索数据集、标签、文件" />
      </label>
      <div className="topbar-actions" data-no-drag>
        <button type="button" onClick={onBackendTasks}>
          <ClipboardCheck size={16} />
          后端任务
        </button>
        <button type="button" onClick={onDataSubmit}>
          <Upload size={16} />
          数据提交
        </button>
        <button type="button" className="primary" onClick={onCreateDataset}>
          <Plus size={16} />
          新建数据集
        </button>
        <span className="sync-state">
          <CheckCircle2 size={15} />
          本地数据
        </span>
        <button aria-label="最小化" type="button" onClick={() => runDesktopCommand("minimize_window")}>
          <Minimize2 size={16} />
        </button>
        <button aria-label="最大化" type="button" onClick={() => runDesktopCommand("toggle_maximize_window")}>
          <Maximize2 size={16} />
        </button>
        <button aria-label="关闭到托盘" type="button" onClick={() => runDesktopCommand("close_window")}>
          <X size={16} />
        </button>
      </div>
    </header>
  );
}

function IconRail() {
  return (
    <nav className="icon-rail" aria-label="主导航">
      {navItems.map((item) => {
        const Icon = item.icon;
        const active = item.label === "数据集";
        return (
          <button aria-label={item.label} aria-pressed={active} className={active ? "active" : ""} key={item.label} title={item.label} type="button">
            <Icon size={20} />
          </button>
        );
      })}
    </nav>
  );
}

function DatasetHome({
  projects,
  projectImages,
  runtimeState,
  runtimeMessage,
  onDownload,
  onInfo,
  onAnnotate,
  onOpenWindow,
}: {
  projects: DatasetProject[];
  projectImages: Record<string, DatasetImage[]>;
  runtimeState: DataRuntimeState;
  runtimeMessage: string | null;
  onDownload: (datasetKey: string) => void;
  onInfo: () => void;
  onAnnotate: (project: DatasetProject) => void;
  onOpenWindow: (project: DatasetProject) => void;
}) {
  const totals = useMemo(
    () => ({
      images: projects.reduce((sum, project) => sum + project.imageCount, 0),
      classes: projects.reduce((sum, project) => sum + project.classCount, 0),
      review: projects.reduce((sum, project) => sum + project.reviewCount, 0),
      issues: projects.reduce((sum, project) => sum + project.issueCount, 0),
    }),
    [projects],
  );

  return (
    <main className="content">
      <section className="dataset-main">
        <div className="page-heading">
          <div>
            <h1>数据集</h1>
            <p>管理本地数据工程、进入标注任务、查看标注进度。</p>
          </div>
          <div className="filter-row">
            <button type="button" onClick={onInfo}>
              工程信息
            </button>
            {["全部", "已导入", "待下载", "BBox", "Polygon"].map((filter) => (
              <button className={filter === "全部" ? "selected" : ""} key={filter} type="button">
                {filter}
              </button>
            ))}
          </div>
        </div>
        <div className="overview-strip">
          <span>
            <strong>{formatNumber(totals.images)}</strong>
            图片总数
          </span>
          <span>
            <strong>{projects.length}</strong>
            项目
          </span>
          <span>
            <strong>{formatNumber(totals.review)}</strong>
            待审核
          </span>
          <span>
            <strong>{totals.classes}</strong>
            类别
          </span>
          <span>
            <strong>{formatNumber(totals.issues)}</strong>
            问题
          </span>
        </div>
        {runtimeState === "backend-unavailable" ? (
          <RuntimeStatePanel
            title="后端未连接"
            message="请在 Tauri 桌面环境启动应用。"
          />
        ) : runtimeState === "downloading" ? (
          <RuntimeStatePanel
            title="正在下载 COCO128 测试数据..."
            message="首次启动会自动准备真实本地数据集，完成后将显示真实图片预览。"
          />
        ) : runtimeState === "download-error" ? (
          <RuntimeStatePanel
            title="测试数据下载失败"
            message={runtimeMessage ?? "请检查网络后重试。"}
            actionLabel="重试下载"
            onAction={() => onDownload(defaultTestDatasetKey)}
          />
        ) : (
          <>
            {projects.length === 0 ? (
              <RuntimeStatePanel
                title="正在准备测试数据"
                message="尚未发现真实项目，应用会自动下载 COCO128。"
              />
            ) : (
              <div className="dataset-grid">
                {projects.map((dataset) => (
                  <DatasetCard
                    dataset={dataset}
                    key={dataset.id}
                    onAnnotate={() => onAnnotate(dataset)}
                    onOpen={() => navigate(`#/datasets/${dataset.id}`)}
                    onOpenWindow={() => onOpenWindow(dataset)}
                    previewImages={projectImages[dataset.id] ?? []}
                  />
                ))}
              </div>
            )}
          </>
        )}
      </section>
    </main>
  );
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
        <div className="info-path">data/test_data</div>
        <section>
          <h3>
            <Tags size={16} />
            数据工程结构
          </h3>
          {["registry.json", "cache/downloads", "projects/{projectId}/project.json", "annotations/native", "exports"].map((item) => (
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
            <strong>规划中</strong>
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

function DataSubmitDialog({
  datasets,
  onCancel,
  onDownload,
}: {
  datasets: BuiltinDataset[];
  onCancel: () => void;
  onDownload: (datasetKey: string) => void;
}) {
  return (
    <div className="modal-backdrop">
      <section aria-labelledby="data-submit-title" className="dataset-dialog data-submit-dialog" role="dialog">
        <div className="dialog-header">
          <div>
            <span className="eyebrow">数据入口</span>
            <h2 id="data-submit-title">数据提交</h2>
          </div>
          <button aria-label="关闭数据提交" type="button" onClick={onCancel}>
            <X size={16} />
          </button>
        </div>
        <div className="submit-options">
          <article className="submit-option">
            <Upload size={18} />
            <div>
              <h3>本地图片或目录</h3>
              <p>用于后续接入目录选择、批量图片导入和格式识别。</p>
            </div>
            <button type="button" disabled>
              待接入
            </button>
          </article>
          <article className="submit-option">
            <Database size={18} />
            <div>
              <h3>新建空数据集</h3>
              <p>需要先定义数据集类型，再导入真实图片。</p>
            </div>
            <span>使用顶部新建</span>
          </article>
        </div>
        <BuiltinDatasetPanel datasets={datasets} onDownload={onDownload} />
      </section>
    </div>
  );
}

function RuntimeStatePanel({
  title,
  message,
  actionLabel,
  onAction,
}: {
  title: string;
  message: string;
  actionLabel?: string;
  onAction?: () => void;
}) {
  return (
    <section className="runtime-state">
      <CircleAlert size={22} />
      <div>
        <h2>{title}</h2>
        <p>{message}</p>
      </div>
      {actionLabel && onAction ? (
        <button type="button" onClick={onAction}>
          {actionLabel}
        </button>
      ) : null}
    </section>
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
              onChange({
                ...form,
                datasetType,
                demoTemplate: datasetType === "yolo-seg" ? "demo-polygon" : "demo-bbox",
              });
            }}
            value={form.datasetType}
          >
            <option value="yolo-detect">目标检测 / YOLO BBox</option>
            <option value="yolo-seg">实例分割 / YOLO Polygon</option>
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
}: {
  tasks: BackendTask[];
  state: "idle" | "loading" | "error";
  message: string | null;
  onClearCompleted: () => void;
  onClose: () => void;
  onRefresh: () => void;
}) {
  return (
    <div className="task-tray-backdrop">
      <aside aria-label="后端任务托盘" className="backend-task-tray" role="complementary">
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
    </div>
  );
}

function ProjectWorkspace({
  projectId,
  onOpenWindow,
}: {
  projectId: string;
  onOpenWindow: (project: DatasetProject) => void;
}) {
  const [detail, setDetail] = useState<ProjectDetail | null>(null);
  const [images, setImages] = useState<DatasetImage[]>([]);
  const [tab, setTab] = useState<ProjectTab>("概览");
  const [loadError, setLoadError] = useState<{ title: string; message: string } | null>(null);
  const imageUrls = useImageAssetUrls(projectId, images, 36);

  useEffect(() => {
    setLoadError(null);
    Promise.all([getProjectDetail(projectId), listProjectImages(projectId)])
      .then(([nextDetail, nextImages]) => {
        setDetail(nextDetail);
        setImages(nextImages);
      })
      .catch((error) => {
        setDetail(null);
        setImages([]);
        setLoadError(
          isBackendUnavailableError(error)
            ? { title: "后端未连接", message: "请在 Tauri 桌面环境启动应用。" }
            : { title: "数据集未初始化", message: error instanceof Error ? error.message : String(error) },
        );
      });
  }, [projectId]);

  if (loadError) {
    return (
      <main className="project-page">
        <section className="project-main">
          <RuntimeStatePanel title={loadError.title} message={loadError.message} actionLabel="返回数据集" onAction={() => navigate("#/datasets")} />
        </section>
      </main>
    );
  }

  if (!detail) {
    return <main className="project-page"><section className="project-main">加载数据集...</section></main>;
  }

  return (
    <main className="project-page">
      <section className="project-main">
        <div className="project-header">
          <div>
            <button className="text-button" type="button" onClick={() => navigate("#/datasets")}>
              数据集
            </button>
            <h1>{detail.project.name}</h1>
            <p>{detail.project.description}</p>
          </div>
          <div className="project-actions">
            <button type="button" onClick={() => navigate(`#/annotate/${detail.project.id}/${images[0]?.id ?? ""}`)}>
              <Play size={16} />
              开始标注
            </button>
            <button type="button" onClick={() => onOpenWindow(detail.project)}>
              <Layers3 size={16} />
              独立窗口标注
            </button>
            <button className="primary" type="button">
              <Download size={16} />
              导出数据集
            </button>
          </div>
        </div>
        <div className="project-tabs" aria-label="项目页面">
          {projectTabs.map((item) => (
            <button aria-pressed={item === tab} className={item === tab ? "active" : ""} key={item} onClick={() => setTab(item)} type="button">
              {item}
            </button>
          ))}
        </div>
        <section className="project-surface">{renderProjectTab(tab, detail, images, imageUrls)}</section>
      </section>
      <aside className="detail-panel project-side" aria-label="Project details">
        <span className="eyebrow">项目运行时</span>
        <h2>Rust 后端</h2>
        <div className="backend-flow">
          {["Command API", "Project FS", "YOLO Importer", "Annotation Store"].map((layer) => (
            <div key={layer}>{layer}</div>
          ))}
        </div>
        <section>
          <h3>
            <Database size={16} />
            存储方案
          </h3>
          <p className="side-copy">
            SQLite 保存项目索引，图片保留在 data/test_data/projects 下，编辑后的标注写入 annotations/native。
          </p>
        </section>
      </aside>
    </main>
  );
}

function renderProjectTab(
  tab: ProjectTab,
  detail: ProjectDetail,
  images: DatasetImage[],
  imageUrls: Record<string, string>,
) {
  switch (tab) {
    case "数据分组":
      return (
        <div className="tab-layout">
          <div>
            <h2>标签维度</h2>
            <div className="dimension-grid">
              {[["Split", "train", "val", "test"], ["Status", "未标注", "已标注", "待审核"], ["Source", "ultralytics"], ["Format", ...detail.project.annotationTypes]].map(([name, ...values]) => (
                <div className="dimension-block" key={name}>
                  <strong>{name}</strong>
                  <div>{values.map((value) => <span key={value}>{value}</span>)}</div>
                </div>
              ))}
            </div>
          </div>
          <div className="saved-group-list">
            <h2>保存视图</h2>
            {detail.tagGroups.map((group) => (
              <article className="group-card" key={group.id}>
                <div>
                  <h3>{group.name}</h3>
                  <p>{group.conditions.join(" / ")}</p>
                </div>
                <div className="group-metrics">
                  <span>{formatNumber(group.imageCount)} 张图片</span>
                  <span>{group.annotatedPercent}% 已标注</span>
                  <span>{group.issueCount} 个问题</span>
                </div>
              </article>
            ))}
          </div>
        </div>
      );
    case "图片":
      return (
        <div>
          <h2>图片浏览</h2>
          <div className="image-grid">
            {images.slice(0, 24).map((image) => (
              <div className="image-tile" key={image.id}>
                <div className="sample-thumb traffic-a">
                  {imageUrls[image.id] ? <img alt={image.fileName} src={imageUrls[image.id]} /> : null}
                </div>
                <span>{image.fileName}</span>
                <em>{image.status}</em>
              </div>
            ))}
          </div>
        </div>
      );
    case "类别":
      return (
        <div>
          <h2>类别体系</h2>
          <div className="data-table">
            {detail.classes.map((row) => (
              <div className="table-row" key={row.label}>
                <strong>{row.label}</strong>
                <span>{row.count} 个对象</span>
                <span>{row.attributes.join(", ") || "默认属性"}</span>
              </div>
            ))}
          </div>
        </div>
      );
    case "任务":
      return (
        <div>
          <h2>生产任务</h2>
          <div className="data-table">
            {detail.tasks.map((task) => (
              <div className="table-row" key={task.name}>
                <strong>{task.name}</strong>
                <span>{task.owner}</span>
                <span>{task.status}</span>
                <span>{task.progress}%</span>
              </div>
            ))}
          </div>
        </div>
      );
    case "质检":
      return (
        <div className="quality-layout">
          <h2>质检队列</h2>
          {detail.qualityChecks.length === 0 ? <p>暂无质检问题</p> : detail.qualityChecks.map((item) => (
            <div className={`quality-card ${item.severity}`} key={item.name}>
              <strong>{item.name}</strong>
              <span>{item.count}</span>
              <button type="button">查看样本</button>
            </div>
          ))}
        </div>
      );
    case "导出":
      return (
        <div>
          <h2>导出预设</h2>
          {detail.exportPresets.length === 0 ? <p>暂无导出预设</p> : (
            <div className="export-grid">
              {detail.exportPresets.map((preset) => (
                <article className="export-card" key={preset.name}>
                  <h3>{preset.name}</h3>
                  <p>{preset.format}</p>
                  <span>{preset.scope}</span>
                  <strong>{preset.status}</strong>
                </article>
              ))}
            </div>
          )}
        </div>
      );
    case "后端":
      return (
        <div>
          <h2>Rust 后端设计</h2>
          <div className="backend-diagram">
            <div>React 界面</div>
            <div>Tauri 命令 API</div>
            <div>Project FS</div>
            <div>YOLO Importer</div>
            <div>SQLite + JSON</div>
          </div>
        </div>
      );
    default:
      return (
        <div className="overview-detail">
          <h2>项目概览</h2>
          <div className="overview-strip">
            <span><strong>{formatNumber(detail.project.imageCount)}</strong>图片</span>
            <span><strong>{detail.project.annotatedPercent}%</strong>已标注</span>
            <span><strong>{detail.project.tagGroupCount}</strong>标签分组</span>
            <span><strong>{detail.project.classCount}</strong>类别</span>
            <span><strong>{detail.project.issueCount}</strong>问题</span>
          </div>
          <div className="tag-list">{detail.project.tags.map((tag) => <span key={tag}>{tag}</span>)}</div>
        </div>
      );
  }
}

function AnnotationWorkspace({ projectId, imageId }: { projectId: string; imageId?: string }) {
  const [images, setImages] = useState<DatasetImage[]>([]);
  const [imagesLoaded, setImagesLoaded] = useState(false);
  const [loadError, setLoadError] = useState<{ title: string; message: string } | null>(null);
  const [activeImageId, setActiveImageId] = useState(imageId ?? "");
  const [assetUrl, setAssetUrl] = useState("");
  const [objects, setObjects] = useState<AnnotationObject[]>([]);
  const [mode, setMode] = useState<ToolMode>("select");
  const [selectedObjectId, setSelectedObjectId] = useState<string | null>(null);
  const [draftBox, setDraftBox] = useState<{ start: { x: number; y: number }; end: { x: number; y: number } } | null>(null);
  const [dragState, setDragState] = useState<{
    objectId: string;
    start: { x: number; y: number };
    original: NonNullable<AnnotationObject["bbox"]>;
  } | null>(null);
  const activeImage = images.find((image) => image.id === activeImageId) ?? images[0];
  const filmstripUrls = useImageAssetUrls(projectId, images, 12);
  const selectedObject = objects.find((object) => object.id === selectedObjectId) ?? null;

  useEffect(() => {
    setImagesLoaded(false);
    setLoadError(null);
    listProjectImages(projectId)
      .then((items) => {
        setImages(items);
        setImagesLoaded(true);
        if (!activeImageId && items[0]) setActiveImageId(items[0].id);
      })
      .catch((error) => {
        setImages([]);
        setImagesLoaded(true);
        setLoadError(
          isBackendUnavailableError(error)
            ? { title: "后端未连接", message: "请在 Tauri 桌面环境启动应用。" }
            : { title: "图片未找到或数据集未初始化", message: error instanceof Error ? error.message : String(error) },
        );
      });
  }, [projectId]);

  useEffect(() => {
    const nextImageId = activeImageId || imageId;
    if (!nextImageId) return;
    setLoadError(null);
    getFileAssetUrl(projectId, nextImageId)
      .then(setAssetUrl)
      .catch((error) => {
        setAssetUrl("");
        setLoadError({
          title: "图片未找到或数据集未初始化",
          message: error instanceof Error ? error.message : String(error),
        });
      });
    getImageAnnotations(projectId, nextImageId)
      .then((items) => {
        setObjects(items);
        setSelectedObjectId(items[0]?.id ?? null);
      })
      .catch(() => {
        setObjects([]);
        setSelectedObjectId(null);
      });
  }, [projectId, activeImageId, imageId]);

  async function save() {
    const targetImageId = activeImageId || imageId;
    if (!targetImageId) return;
    await saveImageAnnotations(projectId, targetImageId, objects);
  }

  if (loadError) {
    return (
      <main className="annotation-page">
        <section className="workspace-area">
          <RuntimeStatePanel title={loadError.title} message={loadError.message} actionLabel="返回数据集" onAction={() => navigate("#/datasets")} />
        </section>
      </main>
    );
  }

  if (imagesLoaded && images.length === 0) {
    return (
      <main className="annotation-page">
        <section className="workspace-area">
          <RuntimeStatePanel title="图片未找到或数据集未初始化" message="请先完成 COCO128 测试数据下载。" actionLabel="返回数据集" onAction={() => navigate("#/datasets")} />
        </section>
      </main>
    );
  }

  function eventClientPosition(event: MouseEvent<Element>) {
    const nativeEvent = event.nativeEvent as globalThis.MouseEvent & {
      x?: number;
      pageX?: number;
      pageY?: number;
    };
    const clientX = Number.isFinite(event.clientX)
      ? event.clientX
      : Number.isFinite(nativeEvent.clientX)
        ? nativeEvent.clientX
        : Number.isFinite(nativeEvent.x)
          ? nativeEvent.x ?? 0
          : Number.isFinite(nativeEvent.pageX)
            ? nativeEvent.pageX ?? 0
            : 0;
    const clientY = Number.isFinite(event.clientY)
      ? event.clientY
      : Number.isFinite(nativeEvent.clientY)
        ? nativeEvent.clientY
        : Number.isFinite(nativeEvent.y)
          ? nativeEvent.y ?? 0
          : Number.isFinite(nativeEvent.pageY)
            ? nativeEvent.pageY ?? 0
            : 0;

    return { clientX, clientY };
  }

  function pointFromEvent(event: MouseEvent<SVGSVGElement>) {
    const width = activeImage?.width || 640;
    const height = activeImage?.height || 480;
    const rect = event.currentTarget.getBoundingClientRect();
    const rectWidth = rect.width || width;
    const rectHeight = rect.height || height;
    const { clientX, clientY } = eventClientPosition(event);
    return {
      x: clamp(((clientX - rect.left) / rectWidth) * width, 0, width),
      y: clamp(((clientY - rect.top) / rectHeight) * height, 0, height),
    };
  }

  function beginCanvasInteraction(event: MouseEvent<SVGSVGElement>) {
    if (mode !== "bbox") {
      setSelectedObjectId(null);
      return;
    }

    const point = pointFromEvent(event);
    setDraftBox({ start: point, end: point });
  }

  function updateCanvasInteraction(event: MouseEvent<SVGSVGElement>) {
    const point = pointFromEvent(event);
    if (draftBox) {
      setDraftBox({ ...draftBox, end: point });
      return;
    }

    if (dragState) {
      const dx = point.x - dragState.start.x;
      const dy = point.y - dragState.start.y;
      setObjects((current) =>
        current.map((object) => {
          if (object.id !== dragState.objectId || !object.bbox || !activeImage) return object;
          return {
            ...object,
            bbox: {
              ...object.bbox,
              x: Number(clamp(dragState.original.x + dx, 0, activeImage.width - object.bbox.width).toFixed(1)),
              y: Number(clamp(dragState.original.y + dy, 0, activeImage.height - object.bbox.height).toFixed(1)),
            },
          };
        }),
      );
    }
  }

  function finishCanvasInteraction(event: MouseEvent<SVGSVGElement>) {
    if (draftBox) {
      const box = normalizeBox(draftBox.start, pointFromEvent(event));
      setDraftBox(null);
      if (box.width >= 3 && box.height >= 3) {
        const object: AnnotationObject = {
          id: `ann-${Date.now()}`,
          classId: 0,
          label: "object",
          type: "bbox",
          bbox: box,
          attributes: { source: "manual" },
        };
        setObjects((current) => [...current, object]);
        setSelectedObjectId(object.id);
      }
    }

    setDragState(null);
  }

  function beginObjectDrag(event: MouseEvent<SVGRectElement>, object: AnnotationObject) {
    if (!object.bbox) return;
    event.stopPropagation();
    setMode("select");
    setSelectedObjectId(object.id);
    const ownerSvg = event.currentTarget.ownerSVGElement;
    const width = activeImage?.width || 640;
    const height = activeImage?.height || 480;
    const rect = ownerSvg?.getBoundingClientRect();
    const rectWidth = rect?.width || width;
    const rectHeight = rect?.height || height;
    const { clientX, clientY } = eventClientPosition(event);
    setDragState({
      objectId: object.id,
      start: {
        x: clamp(((clientX - (rect?.left ?? 0)) / rectWidth) * width, 0, width),
        y: clamp(((clientY - (rect?.top ?? 0)) / rectHeight) * height, 0, height),
      },
      original: object.bbox,
    });
  }

  function updateSelectedLabel(label: string) {
    if (!selectedObjectId) return;
    setObjects((current) =>
      current.map((object) => (object.id === selectedObjectId ? { ...object, label } : object)),
    );
  }

  function updateSelectedBbox(field: "x" | "y" | "width" | "height", rawValue: string) {
    if (!selectedObjectId || !activeImage) return;
    const value = Number(rawValue);
    if (!Number.isFinite(value)) return;

    setObjects((current) =>
      current.map((object) => {
        if (object.id !== selectedObjectId || !object.bbox) return object;
        const next = { ...object.bbox };
        if (field === "x") {
          next.x = clamp(value, 0, activeImage.width - next.width);
        } else if (field === "y") {
          next.y = clamp(value, 0, activeImage.height - next.height);
        } else if (field === "width") {
          next.width = clamp(value, 1, activeImage.width - next.x);
        } else {
          next.height = clamp(value, 1, activeImage.height - next.y);
        }

        return {
          ...object,
          bbox: {
            x: Number(next.x.toFixed(1)),
            y: Number(next.y.toFixed(1)),
            width: Number(next.width.toFixed(1)),
            height: Number(next.height.toFixed(1)),
          },
        };
      }),
    );
  }

  function deleteSelectedObject() {
    if (!selectedObjectId) return;
    setObjects((current) => current.filter((object) => object.id !== selectedObjectId));
    setSelectedObjectId(null);
  }

  return (
    <main className="annotation-page">
      <aside className="tool-rail" aria-label="Annotation tools">
        {[
          { label: "选择", icon: MousePointer2, mode: "select" as ToolMode },
          { label: "BBox", icon: BoxSelect, mode: "bbox" as ToolMode },
          { label: "Polygon", icon: Square, mode: "polygon" as ToolMode },
          { label: "智能工具", icon: Zap, mode: "select" as ToolMode },
          { label: "显示", icon: Eye, mode: "select" as ToolMode },
        ].map((tool) => {
          const Icon = tool.icon;
          return (
            <button
              aria-label={tool.label}
              aria-pressed={mode === tool.mode}
              className={mode === tool.mode ? "active" : ""}
              key={tool.label}
              onClick={() => setMode(tool.mode)}
              type="button"
              title={tool.label}
            >
              <Icon size={20} />
            </button>
          );
        })}
      </aside>
      <section className="workspace-area">
        <div className="annotation-toolbar">
          <div>
            <h1>标注工作台</h1>
            <span>{projectId} / {activeImage?.fileName ?? "加载图片"}</span>
          </div>
          <button type="button" onClick={save}>
            <Save size={16} />
            保存标注
          </button>
        </div>
        <div className="image-stage">
          <div
            className="real-image-stage"
            style={{ aspectRatio: activeImage ? `${activeImage.width} / ${activeImage.height}` : undefined }}
          >
            {assetUrl && activeImage ? <img alt={activeImage.fileName} src={assetUrl} /> : <div className="road-scene" />}
            <svg
              className={`annotation-overlay ${mode === "bbox" ? "drawing" : ""}`}
              data-testid="annotation-canvas"
              onMouseDown={beginCanvasInteraction}
              onMouseMove={updateCanvasInteraction}
              onMouseUp={finishCanvasInteraction}
              viewBox={`0 0 ${activeImage?.width || 640} ${activeImage?.height || 480}`}
              preserveAspectRatio="none"
            >
              {objects.map((object) => object.type === "bbox" && object.bbox ? (
                <g key={object.id}>
                  <rect
                    aria-label={`${object.label} bbox`}
                    className={`annotation-rect ${object.id === selectedObjectId ? "selected" : ""}`}
                    onMouseDown={(event) => beginObjectDrag(event, object)}
                    x={object.bbox.x}
                    y={object.bbox.y}
                    width={object.bbox.width}
                    height={object.bbox.height}
                  />
                  <text x={object.bbox.x + 4} y={Math.max(14, object.bbox.y - 4)}>{object.label}</text>
                </g>
              ) : object.polygon ? (
                <polygon className="annotation-polygon" key={object.id} points={object.polygon.map((point) => `${point.x},${point.y}`).join(" ")} />
              ) : null)}
              {draftBox ? (
                <rect
                  className="annotation-rect draft"
                  {...normalizeBox(draftBox.start, draftBox.end)}
                />
              ) : null}
            </svg>
          </div>
        </div>
        <div className="filmstrip">
          {images.slice(0, 12).map((image) => (
            <button className={image.id === activeImage?.id ? "active" : ""} key={image.id} type="button" onClick={() => setActiveImageId(image.id)}>
              <div className="mini-thumb">
                {filmstripUrls[image.id] ? <img alt={`${image.fileName} 缩略图`} src={filmstripUrls[image.id]} /> : null}
              </div>
              <span>{image.fileName}</span>
            </button>
          ))}
        </div>
      </section>
      <aside className="inspector">
        <h2>对象</h2>
        {objects.map((object) => (
          <button
            aria-pressed={object.id === selectedObjectId}
            className={`object-row ${object.id === selectedObjectId ? "selected" : ""}`}
            key={object.id}
            onClick={() => setSelectedObjectId(object.id)}
            type="button"
          >
            <span className="dot" />
            <span>{object.label}</span>
            <em>{object.type}</em>
            <Eye size={15} />
          </button>
        ))}
        <h2>对象属性</h2>
        <dl>
          <dt>图片</dt>
          <dd>{activeImage?.fileName ?? "-"}</dd>
          <dt>尺寸</dt>
          <dd>{activeImage ? `${activeImage.width} x ${activeImage.height}` : "-"}</dd>
          <dt>对象数</dt>
          <dd>{objects.length}</dd>
          <dt>标签</dt>
          <dd>
            <input
              aria-label="对象标签"
              disabled={!selectedObject}
              onChange={(event) => updateSelectedLabel(event.target.value)}
              value={selectedObject?.label ?? ""}
            />
          </dd>
        </dl>
        {selectedObject?.bbox ? (
          <div className="bbox-editor">
            {[
              { field: "x" as const, label: "X 坐标", value: selectedObject.bbox.x },
              { field: "y" as const, label: "Y 坐标", value: selectedObject.bbox.y },
              { field: "width" as const, label: "宽度", value: selectedObject.bbox.width },
              { field: "height" as const, label: "高度", value: selectedObject.bbox.height },
            ].map((item) => (
              <label key={item.field}>
                <span>{item.label}</span>
                <input
                  aria-label={item.label}
                  min={0}
                  onChange={(event) => updateSelectedBbox(item.field, event.target.value)}
                  step={1}
                  type="number"
                  value={item.value}
                />
              </label>
            ))}
          </div>
        ) : null}
        <div className="inspector-actions">
          <button type="button" onClick={deleteSelectedObject} disabled={!selectedObjectId}>
            删除对象
          </button>
        </div>
        <h2>导出</h2>
        <button className="primary" type="button">
          导出 COCO (JSON)
        </button>
      </aside>
    </main>
  );
}

export default function App() {
  const [route, setRoute] = useState<Route>(() => parseRoute());
  const [projects, setProjects] = useState<DatasetProject[]>([]);
  const [projectImages, setProjectImages] = useState<Record<string, DatasetImage[]>>({});
  const [builtinDatasets, setBuiltinDatasets] = useState<BuiltinDataset[]>([]);
  const [runtimeState, setRuntimeState] = useState<DataRuntimeState>("loading");
  const [runtimeMessage, setRuntimeMessage] = useState<string | null>(null);
  const [createDialogOpen, setCreateDialogOpen] = useState(false);
  const [dataSubmitOpen, setDataSubmitOpen] = useState(false);
  const [projectInfoOpen, setProjectInfoOpen] = useState(false);
  const [createForm, setCreateForm] = useState<DatasetCreationForm>(defaultDatasetCreationForm);
  const [taskTrayOpen, setTaskTrayOpen] = useState(false);
  const [backendTasks, setBackendTasks] = useState<BackendTask[]>([]);
  const [taskTrayState, setTaskTrayState] = useState<"idle" | "loading" | "error">("idle");
  const [taskTrayMessage, setTaskTrayMessage] = useState<string | null>(null);

  useEffect(() => {
    const onHashChange = () => setRoute(parseRoute());
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);

  useEffect(() => {
    refreshDatasets({ autoDownload: true });
  }, []);

  async function refreshDatasets(options: { autoDownload: boolean }) {
    setRuntimeMessage(null);
    try {
      const [builtin, currentProjects] = await Promise.all([
        listBuiltinDatasets(),
        listDatasetProjects(),
      ]);
      const defaultDataset = builtin.find((dataset) => dataset.key === defaultTestDatasetKey);
      if (options.autoDownload && defaultDataset && !defaultDataset.downloaded) {
        setBuiltinDatasets([]);
        setProjects([]);
        setProjectImages({});
        setRuntimeState("downloading");
        await downloadTestDataset(defaultTestDatasetKey);
        await refreshDatasets({ autoDownload: false });
        return;
      }

      const imageEntries = await Promise.all(
        currentProjects.map(async (project) => [
          project.id,
          await listProjectImages(project.id),
        ] as const),
      );
      setBuiltinDatasets(builtin);
      setProjects(currentProjects);
      setProjectImages(Object.fromEntries(imageEntries));
      setRuntimeState("ready");
    } catch (error) {
      setBuiltinDatasets([]);
      setProjects([]);
      setProjectImages({});
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
      await refreshDatasets({ autoDownload: false });
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

  async function annotateProject(project: DatasetProject) {
    const images = await listProjectImages(project.id);
    navigate(`#/annotate/${project.id}/${images[0]?.id ?? ""}`);
  }

  async function openProjectWindow(project: DatasetProject) {
    const images = await listProjectImages(project.id);
    await openAnnotationWindow(project.id, images[0]?.id);
  }

  async function handleCreateDataset() {
    await createDatasetProject(createForm.name, createForm.datasetType, createForm.demoTemplate);
    setCreateDialogOpen(false);
    setCreateForm(defaultDatasetCreationForm);
    await refreshDatasets({ autoDownload: false });
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
    setTaskTrayOpen(true);
    await loadBackendTasks();
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

  return (
    <div className="app-shell">
      <TopBar
        onBackendTasks={openBackendTasks}
        onCreateDataset={() => setCreateDialogOpen(true)}
        onDataSubmit={() => setDataSubmitOpen(true)}
        onDatasets={() => navigate("#/datasets")}
      />
      <div className="app-body">
        <IconRail />
        {route.name === "datasets" && (
          <DatasetHome
            onAnnotate={annotateProject}
            onDownload={handleDownload}
            onInfo={() => setProjectInfoOpen(true)}
            onOpenWindow={openProjectWindow}
            projectImages={projectImages}
            projects={projects}
            runtimeMessage={runtimeMessage}
            runtimeState={runtimeState}
          />
        )}
        {route.name === "project" && <ProjectWorkspace onOpenWindow={openProjectWindow} projectId={route.projectId} />}
        {route.name === "annotate" && <AnnotationWorkspace imageId={route.imageId} projectId={route.projectId} />}
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
          onCancel={() => setDataSubmitOpen(false)}
          onDownload={handleSubmitDownload}
        />
      ) : null}
      {projectInfoOpen ? <ProjectInfoDialog onClose={() => setProjectInfoOpen(false)} /> : null}
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
}

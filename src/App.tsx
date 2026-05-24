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
  createDatasetSnapshot,
  detectBackendConnection,
  downloadTestDataset,
  exportDataset,
  getFileAssetUrl,
  getImageAnnotations,
  getImageAnnotationState,
  importImages,
  importYoloDataset,
  getProjectDetail,
  isBackendUnavailableError,
  listBackendTasks,
  listBuiltinDatasets,
  listDatasetProjects,
  listExports,
  listProjectImages,
  listSnapshots,
  openAnnotationWindow,
  openBackendTaskTray,
  openLocalDataset,
  saveImageAnnotations,
  submitImageAnnotations,
} from "./api/tauri";
import type { BackendConnection } from "./api/tauri";
import type {
  AnnotationObject,
  BackendTask,
  BuiltinDataset,
  DatasetExport,
  DatasetImage,
  DatasetProject,
  DatasetSnapshot,
  ProjectDetail,
} from "./types/domain";
import { invoke } from "@tauri-apps/api/core";

type ProjectTab = "概览" | "数据分组" | "图片" | "类别" | "任务" | "质检" | "快照" | "导出";
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
  | { name: "annotate"; projectId: string; imageId?: string }
  | { name: "backendTasks" };

const projectTabs: ProjectTab[] = ["概览", "数据分组", "图片", "类别", "任务", "质检", "快照", "导出"];
const defaultTestDatasetKey = "coco128";
const datasetPreviewLimit = 3;
const projectImagePageSize = 48;
const annotationImagePageSize = 120;
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
  if (parts[0] === "backend-tasks") {
    return { name: "backendTasks" };
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

function useImageAnnotations(projectId: string | undefined, images: DatasetImage[], limit = 12) {
  const [annotations, setAnnotations] = useState<Record<string, AnnotationObject[]>>({});

  useEffect(() => {
    if (!projectId || images.length === 0) {
      setAnnotations({});
      return;
    }

    let cancelled = false;
    Promise.all(
      images.slice(0, limit).map(async (image) => {
        try {
          return [image.id, await getImageAnnotations(projectId, image.id)] as const;
        } catch {
          return [image.id, []] as const;
        }
      }),
    ).then((entries) => {
      if (!cancelled) {
        setAnnotations(Object.fromEntries(entries));
      }
    });

    return () => {
      cancelled = true;
    };
  }, [projectId, images, limit]);

  return annotations;
}

function ThumbnailAnnotationOverlay({
  image,
  objects,
}: {
  image: DatasetImage;
  objects: AnnotationObject[] | undefined;
}) {
  const visibleObjects = (objects ?? []).filter(
    (object) => (object.type === "bbox" && object.bbox) || (object.type === "polygon" && object.polygon?.length),
  );

  if (visibleObjects.length === 0) {
    return null;
  }

  return (
    <svg
      aria-label={`${image.fileName} 标注预览`}
      className="thumbnail-annotations"
      preserveAspectRatio="xMidYMid slice"
      viewBox={`0 0 ${image.width || 640} ${image.height || 480}`}
    >
      {visibleObjects.map((object) => object.type === "bbox" && object.bbox ? (
        <rect
          aria-label={`${image.fileName} 标注框 ${object.label}`}
          className="thumbnail-annotation-box"
          key={object.id}
          x={object.bbox.x}
          y={object.bbox.y}
          width={object.bbox.width}
          height={object.bbox.height}
        />
      ) : object.polygon ? (
        <polygon
          aria-label={`${image.fileName} 多边形标注 ${object.label}`}
          className="thumbnail-annotation-polygon"
          key={object.id}
          points={object.polygon.map((point) => `${point.x},${point.y}`).join(" ")}
        />
      ) : null)}
    </svg>
  );
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

type ResizeHandle = "nw" | "ne" | "sw" | "se";

function resizeBox(
  original: NonNullable<AnnotationObject["bbox"]>,
  dx: number,
  dy: number,
  handle: ResizeHandle,
  imageWidth: number,
  imageHeight: number,
) {
  const minSize = 3;
  let left = original.x;
  let top = original.y;
  let right = original.x + original.width;
  let bottom = original.y + original.height;

  if (handle.includes("w")) left = clamp(left + dx, 0, right - minSize);
  if (handle.includes("e")) right = clamp(right + dx, left + minSize, imageWidth);
  if (handle.includes("n")) top = clamp(top + dy, 0, bottom - minSize);
  if (handle.includes("s")) bottom = clamp(bottom + dy, top + minSize, imageHeight);

  return {
    x: Number(left.toFixed(1)),
    y: Number(top.toFixed(1)),
    width: Number((right - left).toFixed(1)),
    height: Number((bottom - top).toFixed(1)),
  };
}

function bboxHandles(box: NonNullable<AnnotationObject["bbox"]>) {
  return [
    { handle: "nw" as const, x: box.x, y: box.y },
    { handle: "ne" as const, x: box.x + box.width, y: box.y },
    { handle: "sw" as const, x: box.x, y: box.y + box.height },
    { handle: "se" as const, x: box.x + box.width, y: box.y + box.height },
  ];
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
        {dataset.tags.some((tag) => tag.includes("local-linked")) ? (
          <span className="type-chip local">本机链接</span>
        ) : null}
        {dataset.annotationTypes.map((type) => (
          <span className="type-chip" key={type}>
            {type}
          </span>
        ))}
      </div>
      <div className="tag-list compact">
        {dataset.tags.slice(0, 3).map((tag) => (
          <span key={tag}>{tag}</span>
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
  backendConnection,
  onBackendTasks,
  onCreateDataset,
  onDataSubmit,
  onDatasets,
}: {
  backendConnection: BackendConnection;
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
        <span className={`sync-state ${backendConnection.mode}`}>
          {backendConnection.mode === "unavailable" ? <CircleAlert size={15} /> : <CheckCircle2 size={15} />}
          {backendConnection.label}
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
        <div className="info-path">data/workspaces/default</div>
        <section>
          <h3>
            <Tags size={16} />
            数据工程结构
          </h3>
          {["registry.json", "projects/{projectId}/project.json", "assets/original", "assets/thumbnails", "annotations/native", "snapshots", "exports"].map((item) => (
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
  projects,
  onCancel,
  onDownload,
  onImportImages,
  onImportYolo,
  onOpenLocal,
}: {
  datasets: BuiltinDataset[];
  projects: DatasetProject[];
  onCancel: () => void;
  onDownload: (datasetKey: string) => void;
  onImportImages: (projectId: string, sourcePath: string) => void;
  onImportYolo: (projectId: string, sourcePath: string) => void;
  onOpenLocal: (sourcePath: string, datasetType: string) => void;
}) {
  const [projectId, setProjectId] = useState(projects[0]?.id ?? "");
  const [sourcePath, setSourcePath] = useState("");
  const [localDatasetType, setLocalDatasetType] = useState<"voc-detect" | "yolo-detect">("voc-detect");

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
            <FolderKanban size={18} />
            <div>
              <h3>打开本机标注目录</h3>
              <p>不复制图片，直接索引本机 VOC / YOLO BBox 目录，保存时原地写回 XML 或 TXT。</p>
            </div>
            <button type="button" onClick={() => onOpenLocal(sourcePath, localDatasetType)} disabled={!sourcePath.trim()}>
              打开本机数据集
            </button>
          </article>
          <article className="submit-option">
            <Upload size={18} />
            <div>
              <h3>本地图片或目录</h3>
              <p>输入本机图片目录路径，后端会复制、索引并生成可标注图片列表。</p>
            </div>
            <button type="button" onClick={() => onImportImages(projectId, sourcePath)} disabled={!projectId || !sourcePath.trim()}>
              导入图片目录
            </button>
          </article>
          <article className="submit-option">
            <Database size={18} />
            <div>
              <h3>YOLO 数据集目录</h3>
              <p>导入包含 images/labels 的 YOLO 检测或分割数据集。</p>
            </div>
            <button type="button" onClick={() => onImportYolo(projectId, sourcePath)} disabled={!projectId || !sourcePath.trim()}>
              导入 YOLO 数据集
            </button>
          </article>
        </div>
        <div className="submit-path-form">
          <label>
            <span>目录类型</span>
            <select value={localDatasetType} onChange={(event) => setLocalDatasetType(event.target.value as "voc-detect" | "yolo-detect")}>
              <option value="voc-detect">Pascal VOC BBox XML</option>
              <option value="yolo-detect">YOLO BBox TXT</option>
            </select>
          </label>
          <label>
            <span>目标项目</span>
            <select value={projectId} onChange={(event) => setProjectId(event.target.value)}>
              <option value="">请选择项目</option>
              {projects.map((project) => (
                <option key={project.id} value={project.id}>{project.name}</option>
              ))}
            </select>
          </label>
          <label>
            <span>本地路径</span>
            <input
              placeholder="例如 F:\\datasets\\my-yolo-dataset"
              value={sourcePath}
              onChange={(event) => setSourcePath(event.target.value)}
            />
          </label>
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
  variant = "overlay",
}: {
  tasks: BackendTask[];
  state: "idle" | "loading" | "error";
  message: string | null;
  onClearCompleted: () => void;
  onClose: () => void;
  onRefresh: () => void;
  variant?: "overlay" | "window";
}) {
  const content = (
    <aside aria-label="后端任务托盘" className={`backend-task-tray ${variant}`} role="complementary">
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
  );

  if (variant === "window") {
    return <div className="task-window-shell">{content}</div>;
  }

  return (
    <div className="task-tray-backdrop">
      {content}
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
  const [snapshots, setSnapshots] = useState<DatasetSnapshot[]>([]);
  const [exports, setExports] = useState<DatasetExport[]>([]);
  const [workflowMessage, setWorkflowMessage] = useState<string | null>(null);
  const [tab, setTab] = useState<ProjectTab>("概览");
  const [imagePage, setImagePage] = useState(0);
  const [loadError, setLoadError] = useState<{ title: string; message: string } | null>(null);
  const imageUrls = useImageAssetUrls(projectId, images, images.length);
  const imageAnnotations = useImageAnnotations(projectId, images, images.length);

  useEffect(() => {
    setImagePage(0);
  }, [projectId]);

  useEffect(() => {
    setLoadError(null);
    Promise.all([
      getProjectDetail(projectId),
      listProjectImages(projectId, undefined, {
        offset: imagePage * projectImagePageSize,
        limit: projectImagePageSize,
      }),
      listSnapshots(projectId).catch(() => []),
      listExports(projectId).catch(() => []),
    ])
      .then(([nextDetail, nextImages, nextSnapshots, nextExports]) => {
        setDetail(nextDetail);
        setImages(nextImages);
        setSnapshots(nextSnapshots);
        setExports(nextExports);
      })
      .catch((error) => {
        setDetail(null);
        setImages([]);
        setSnapshots([]);
        setExports([]);
        setLoadError(
          isBackendUnavailableError(error)
            ? { title: "后端未连接", message: "请在 Tauri 桌面环境启动应用。" }
            : { title: "数据集未初始化", message: error instanceof Error ? error.message : String(error) },
        );
      });
  }, [projectId, imagePage]);

  async function handleCreateSnapshot() {
    if (!detail) return;
    const snapshot = await createDatasetSnapshot(
      projectId,
      `${detail.project.name} 快照 ${snapshots.length + 1}`,
    );
    setSnapshots((current) => [snapshot, ...current]);
    setWorkflowMessage(`已创建快照 ${snapshot.name}`);
  }

  async function handleExport(format: "yolo" | "coco") {
    const snapshotId = snapshots[0]?.id;
    if (!snapshotId) {
      setWorkflowMessage("请先创建快照，再执行导出。");
      return;
    }
    const nextExport = await exportDataset(projectId, snapshotId, format);
    setExports((current) => [nextExport, ...current]);
    setWorkflowMessage(`已导出 ${format.toUpperCase()} 数据包`);
  }

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
        <div className="project-sticky-header">
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
        </div>
        <section className="project-surface">
          {renderProjectTab(tab, detail, images, imageUrls, imageAnnotations, {
            exports,
            imagePage,
            imagePageSize: projectImagePageSize,
            snapshots,
            workflowMessage,
            onCreateSnapshot: handleCreateSnapshot,
            onExport: handleExport,
            onImagePageChange: setImagePage,
          })}
        </section>
      </section>
    </main>
  );
}

function renderProjectTab(
  tab: ProjectTab,
  detail: ProjectDetail,
  images: DatasetImage[],
  imageUrls: Record<string, string>,
  imageAnnotations: Record<string, AnnotationObject[]>,
  workflow: {
    snapshots: DatasetSnapshot[];
    exports: DatasetExport[];
    imagePage: number;
    imagePageSize: number;
    workflowMessage: string | null;
    onCreateSnapshot: () => void;
    onExport: (format: "yolo" | "coco") => void;
    onImagePageChange: (page: number) => void;
  },
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
          <div className="tab-header-row">
            <h2>图片浏览</h2>
            <div className="pager-actions">
              <span>
                第 {workflow.imagePage + 1} 页 / {formatNumber(detail.project.imageCount)} 张
              </span>
              <button type="button" onClick={() => workflow.onImagePageChange(Math.max(0, workflow.imagePage - 1))} disabled={workflow.imagePage === 0}>
                上一页
              </button>
              <button
                type="button"
                onClick={() => workflow.onImagePageChange(workflow.imagePage + 1)}
                disabled={(workflow.imagePage + 1) * workflow.imagePageSize >= detail.project.imageCount}
              >
                下一页
              </button>
            </div>
          </div>
          <div className="image-grid">
            {images.map((image) => (
              <div
                className="image-tile"
                key={image.id}
                onDoubleClick={() => navigate(`#/annotate/${detail.project.id}/${image.id}`)}
                role="button"
                tabIndex={0}
                onKeyDown={(event) => {
                  if (event.key === "Enter") navigate(`#/annotate/${detail.project.id}/${image.id}`);
                }}
              >
                <div className="sample-thumb traffic-a">
                  {imageUrls[image.id] ? (
                    <img alt={image.fileName} decoding="async" loading="lazy" src={imageUrls[image.id]} />
                  ) : null}
                  <ThumbnailAnnotationOverlay image={image} objects={imageAnnotations[image.id]} />
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
          <div className="action-row">
            <button type="button" onClick={() => workflow.onExport("yolo")}>导出 YOLO</button>
            <button type="button" onClick={() => workflow.onExport("coco")}>导出 COCO</button>
          </div>
          {workflow.workflowMessage ? <p className="workflow-message">{workflow.workflowMessage}</p> : null}
          {workflow.exports.length === 0 ? <p>暂无导出记录</p> : (
            <div className="export-grid">
              {workflow.exports.map((item) => (
                <article className="export-card" key={item.id}>
                  <h3>{item.format.toUpperCase()}</h3>
                  <p>{item.outputPath}</p>
                  <span>{item.snapshotId}</span>
                  <strong>{item.status}</strong>
                </article>
              ))}
            </div>
          )}
        </div>
      );
    case "快照":
      return (
        <div>
          <div className="tab-header-row">
            <h2>数据集快照</h2>
            <button type="button" onClick={workflow.onCreateSnapshot}>创建快照</button>
          </div>
          {workflow.workflowMessage ? <p className="workflow-message">{workflow.workflowMessage}</p> : null}
          {workflow.snapshots.length === 0 ? <p>暂无快照</p> : (
            <div className="data-table">
              {workflow.snapshots.map((snapshot) => (
                <div className="table-row" key={snapshot.id}>
                  <strong>{snapshot.name}</strong>
                  <span>{snapshot.imageCount} 张图片</span>
                  <span>{snapshot.createdAt}</span>
                </div>
              ))}
            </div>
          )}
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
  const [revision, setRevision] = useState<string | null>(null);
  const [annotationStatus, setAnnotationStatus] = useState("加载中");
  const [saveMessage, setSaveMessage] = useState<string | null>(null);
  const [dirty, setDirty] = useState(false);
  const [saveAndNext, setSaveAndNext] = useState(false);
  const [mode, setMode] = useState<ToolMode>("select");
  const [selectedObjectId, setSelectedObjectId] = useState<string | null>(null);
  const [draftBox, setDraftBox] = useState<{ start: { x: number; y: number }; end: { x: number; y: number } } | null>(null);
  const [dragState, setDragState] = useState<{
    objectId: string;
    kind: "move" | "resize";
    handle?: "nw" | "ne" | "sw" | "se";
    start: { x: number; y: number };
    original: NonNullable<AnnotationObject["bbox"]>;
  } | null>(null);
  const activeImage = images.find((image) => image.id === activeImageId) ?? images[0];
  const filmstripUrls = useImageAssetUrls(projectId, images, 12);
  const selectedObject = objects.find((object) => object.id === selectedObjectId) ?? null;

  useEffect(() => {
    setImagesLoaded(false);
    setLoadError(null);
    listProjectImages(projectId, undefined, { offset: 0, limit: annotationImagePageSize })
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
    getImageAnnotationState(projectId, nextImageId)
      .then((state) => {
        setObjects(state.objects);
        setRevision(state.revision);
        setAnnotationStatus(state.status);
        setSaveMessage(null);
        setDirty(false);
        setSelectedObjectId(state.objects[0]?.id ?? null);
      })
      .catch(() => {
        setObjects([]);
        setRevision(null);
        setAnnotationStatus("未标注");
        setDirty(false);
        setSelectedObjectId(null);
      });
  }, [projectId, activeImageId, imageId]);

  useEffect(() => {
    function handleBeforeUnload(event: BeforeUnloadEvent) {
      if (!dirty) return;
      event.preventDefault();
      event.returnValue = "";
    }

    window.addEventListener("beforeunload", handleBeforeUnload);
    return () => window.removeEventListener("beforeunload", handleBeforeUnload);
  }, [dirty]);

  async function save(options: { next?: boolean } = {}) {
    const targetImageId = activeImageId || imageId;
    if (!targetImageId) return;
    const result = await saveImageAnnotations(projectId, targetImageId, revision, objects);
    setRevision(result.revision);
    setAnnotationStatus("草稿");
    setDirty(false);
    setSaveMessage(`已保存并写回标注文件 ${result.savedAt}`);
    if (options.next || saveAndNext) {
      goToImage(1);
    }
  }

  async function submit() {
    const targetImageId = activeImageId || imageId;
    if (!targetImageId) return;
    await submitImageAnnotations(projectId, targetImageId);
    setAnnotationStatus("待质检");
    setSaveMessage("已提交质检");
  }

  useEffect(() => {
    function handleKeyDown(event: globalThis.KeyboardEvent) {
      if (event.key === "Delete" || event.key === "Backspace") {
        deleteSelectedObject();
      }
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "d") {
        event.preventDefault();
        duplicateSelectedObject();
      }
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "s") {
        event.preventDefault();
        void save();
      }
    }
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [selectedObjectId, objects, revision, activeImageId, imageId]);

  function goToImage(offset: number) {
    if (!activeImage) return;
    if (dirty && !window.confirm("当前标注尚未保存，是否继续切换图片？")) return;
    const index = images.findIndex((image) => image.id === activeImage.id);
    const next = images[index + offset];
    if (next) setActiveImageId(next.id);
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
      setDirty(true);
      setObjects((current) =>
        current.map((object) => {
          if (object.id !== dragState.objectId || !object.bbox || !activeImage) return object;
          if (dragState.kind === "resize" && dragState.handle) {
            return {
              ...object,
              bbox: resizeBox(dragState.original, dx, dy, dragState.handle, activeImage.width, activeImage.height),
            };
          }
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
        setDirty(true);
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
      kind: "move",
      start: {
        x: clamp(((clientX - (rect?.left ?? 0)) / rectWidth) * width, 0, width),
        y: clamp(((clientY - (rect?.top ?? 0)) / rectHeight) * height, 0, height),
      },
      original: object.bbox,
    });
  }

  function beginObjectResize(
    event: MouseEvent<SVGRectElement>,
    object: AnnotationObject,
    handle: "nw" | "ne" | "sw" | "se",
  ) {
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
      kind: "resize",
      handle,
      start: {
        x: clamp(((clientX - (rect?.left ?? 0)) / rectWidth) * width, 0, width),
        y: clamp(((clientY - (rect?.top ?? 0)) / rectHeight) * height, 0, height),
      },
      original: object.bbox,
    });
  }

  function updateSelectedLabel(label: string) {
    if (!selectedObjectId) return;
    setDirty(true);
    setObjects((current) =>
      current.map((object) => (object.id === selectedObjectId ? { ...object, label } : object)),
    );
  }

  function updateSelectedBbox(field: "x" | "y" | "width" | "height", rawValue: string) {
    if (!selectedObjectId || !activeImage) return;
    const value = Number(rawValue);
    if (!Number.isFinite(value)) return;

    setDirty(true);
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
    setDirty(true);
    setObjects((current) => current.filter((object) => object.id !== selectedObjectId));
    setSelectedObjectId(null);
  }

  function duplicateSelectedObject() {
    if (!selectedObject) return;
    const duplicate: AnnotationObject = {
      ...selectedObject,
      id: `ann-${Date.now()}`,
      bbox: selectedObject.bbox
        ? {
            ...selectedObject.bbox,
            x: activeImage ? clamp(selectedObject.bbox.x + 8, 0, activeImage.width - selectedObject.bbox.width) : selectedObject.bbox.x + 8,
            y: activeImage ? clamp(selectedObject.bbox.y + 8, 0, activeImage.height - selectedObject.bbox.height) : selectedObject.bbox.y + 8,
          }
        : undefined,
      polygon: selectedObject.polygon?.map((point) => ({
        x: activeImage ? clamp(point.x + 8, 0, activeImage.width) : point.x + 8,
        y: activeImage ? clamp(point.y + 8, 0, activeImage.height) : point.y + 8,
      })),
      attributes: { ...selectedObject.attributes, source: "copy" },
    };
    setObjects((current) => [...current, duplicate]);
    setSelectedObjectId(duplicate.id);
    setDirty(true);
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
            <span>{projectId} / {activeImage?.fileName ?? "加载图片"} / {annotationStatus}</span>
          </div>
          <div className="annotation-actions">
            {saveMessage ? <span>{saveMessage}</span> : null}
            {dirty ? <span className="dirty-state">未保存</span> : null}
            <label className="inline-toggle">
              <input type="checkbox" checked={saveAndNext} onChange={(event) => setSaveAndNext(event.target.checked)} />
              保存后下一张
            </label>
            <button type="button" onClick={() => goToImage(-1)} disabled={!activeImage || images.findIndex((image) => image.id === activeImage.id) <= 0}>
              上一张
            </button>
            <button type="button" onClick={() => goToImage(1)} disabled={!activeImage || images.findIndex((image) => image.id === activeImage.id) >= images.length - 1}>
              下一张
            </button>
            <button type="button" onClick={submit}>
              <ClipboardCheck size={16} />
              提交质检
            </button>
            <button type="button" onClick={() => save()}>
              <Save size={16} />
              保存标注
            </button>
            <button type="button" onClick={() => save({ next: true })}>
              保存并下一张
            </button>
          </div>
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
                  {object.id === selectedObjectId ? (
                    <>
                      {bboxHandles(object.bbox).map((handle) => (
                        <rect
                          aria-label={`${handle.handle} resize`}
                          className="annotation-handle"
                          key={handle.handle}
                          onMouseDown={(event) => beginObjectResize(event, object, handle.handle)}
                          x={handle.x - 4}
                          y={handle.y - 4}
                          width={8}
                          height={8}
                        />
                      ))}
                    </>
                  ) : null}
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
          <button type="button" onClick={duplicateSelectedObject} disabled={!selectedObjectId}>
            复制对象
          </button>
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
  const [backendConnection, setBackendConnection] = useState<BackendConnection>({
    mode: "checking",
    label: "连接中",
    health: null,
  });

  useEffect(() => {
    const onHashChange = () => setRoute(parseRoute());
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);

  useEffect(() => {
    refreshDatasets({ autoDownload: true });
  }, []);

  useEffect(() => {
    let cancelled = false;
    detectBackendConnection().then((connection) => {
      if (!cancelled) {
        setBackendConnection(connection);
      }
    });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    if (route.name === "backendTasks") {
      loadBackendTasks();
    }
  }, [route.name]);

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
          await listProjectImages(project.id, undefined, {
            offset: 0,
            limit: datasetPreviewLimit,
          }),
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

  async function handleImportImages(projectId: string, sourcePath: string) {
    await importImages(projectId, sourcePath);
    setDataSubmitOpen(false);
    await refreshDatasets({ autoDownload: false });
  }

  async function handleImportYolo(projectId: string, sourcePath: string) {
    await importYoloDataset(projectId, sourcePath);
    setDataSubmitOpen(false);
    await refreshDatasets({ autoDownload: false });
  }

  async function handleOpenLocalDataset(sourcePath: string, datasetType: string) {
    await openLocalDataset(sourcePath, datasetType);
    setDataSubmitOpen(false);
    await refreshDatasets({ autoDownload: false });
  }

  async function annotateProject(project: DatasetProject) {
    const images = await listProjectImages(project.id, undefined, { offset: 0, limit: 1 });
    navigate(`#/annotate/${project.id}/${images[0]?.id ?? ""}`);
  }

  async function openProjectWindow(project: DatasetProject) {
    const images = await listProjectImages(project.id, undefined, { offset: 0, limit: 1 });
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
    try {
      await openBackendTaskTray();
    } catch (error) {
      setTaskTrayOpen(true);
      setTaskTrayState(isBackendUnavailableError(error) ? "error" : "loading");
      setTaskTrayMessage(
        isBackendUnavailableError(error)
          ? "后端未连接，请在 Tauri 桌面环境启动应用。"
          : null,
      );
      if (!isBackendUnavailableError(error)) {
        await loadBackendTasks();
      }
    }
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

  if (route.name === "backendTasks") {
    return (
      <BackendTaskTray
        message={taskTrayMessage}
        onClearCompleted={clearCompletedTasks}
        onClose={() => runDesktopCommand("close_window")}
        onRefresh={loadBackendTasks}
        state={taskTrayState}
        tasks={backendTasks}
        variant="window"
      />
    );
  }

  return (
    <div className="app-shell">
      <TopBar
        backendConnection={backendConnection}
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
          projects={projects}
          onCancel={() => setDataSubmitOpen(false)}
          onDownload={handleSubmitDownload}
          onImportImages={handleImportImages}
          onImportYolo={handleImportYolo}
          onOpenLocal={handleOpenLocalDataset}
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

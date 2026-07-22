import {
  Archive,
  ArrowLeft,
  BoxSelect,
  CheckCircle2,
  CircleAlert,
  ClipboardCheck,
  Database,
  Download,
  Eye,
  FileJson,
  Files,
  FolderKanban,
  FolderOpen,
  Home,
  ImageIcon,
  Layers3,
  Maximize2,
  Minus,
  Move,
  MousePointer2,
  Play,
  Plus,
  Save,
  Settings,
  ShieldCheck,
  Square,
  Tags,
  Upload,
  X,
  ZoomIn,
  ZoomOut,
  Zap,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { DragEvent, KeyboardEvent, MouseEvent, WheelEvent } from "react";
import {
  analyzeDataSource,
  clearCompletedBackendTasks,
  createDatasetProject,
  createDatasetSnapshot,
  detectBackendConnection,
  downloadTestDataset,
  exportDataset,
  getFileAssetUrl,
  getImageAnnotations,
  getImageAnnotationState,
  importFiles,
  importImages,
  importYoloDataset,
  getProjectDetail,
  isBackendUnavailableError,
  listBackendTasks,
  listBuiltinDatasets,
  listClassSamples,
  listDatasetProjects,
  listExports,
  listProjectImages,
  listSnapshots,
  openAnnotationWindow,
  openBackendTaskTray,
  openLocalDataset,
  pickDataSource,
  saveImageAnnotations,
  submitImageAnnotations,
} from "./api/tauri";
import type { BackendConnection } from "./api/tauri";
import type {
  AnnotationObject,
  BackendTask,
  BuiltinDataset,
  ClassSample,
  ClassStat,
  DatasetExport,
  DatasetImage,
  DatasetProject,
  DatasetSnapshot,
  DataSourceAnalysis,
  DataSourceTreeNode,
  ProjectDetail,
} from "./types/domain";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWebview } from "@tauri-apps/api/webview";

type ProjectTab = "概览" | "数据分组" | "图片" | "类别" | "任务" | "质检" | "快照" | "导出";
type ToolMode = "select" | "bbox" | "polygon" | "pan";
type DataRuntimeState = "loading" | "ready" | "downloading" | "backend-unavailable" | "download-error";
type CanvasViewport = {
  scale: number;
  offsetX: number;
  offsetY: number;
};
type CanvasSize = {
  width: number;
  height: number;
};
type DatasetCreationForm = {
  name: string;
  datasetType: "yolo-detect" | "yolo-seg" | "image-classification";
  demoTemplate: "demo-bbox" | "demo-polygon" | "demo-classification" | "empty";
};
type ProjectTopbarContext = {
  project: DatasetProject;
  firstImageId?: string;
};
type Route =
  | { name: "datasets" }
  | { name: "project"; projectId: string; tab?: ProjectTab }
  | { name: "annotate"; projectId: string; imageId?: string }
  | { name: "backendTasks" };

const projectTabs: ProjectTab[] = ["概览", "数据分组", "图片", "类别", "任务", "质检", "快照", "导出"];
const projectTabIcons: Record<ProjectTab, typeof Home> = {
  "概览": Home,
  "数据分组": Tags,
  "图片": ImageIcon,
  "类别": Layers3,
  "任务": ClipboardCheck,
  "质检": ShieldCheck,
  "快照": Archive,
  "导出": Download,
};
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

function beginDesktopWindowDrag(event: MouseEvent<HTMLElement>) {
  if (event.button !== 0) return;
  const target = event.target instanceof HTMLElement ? event.target : null;
  if (target?.closest("[data-no-drag], button, input, textarea, select, a, label")) {
    return;
  }
  void runDesktopCommand("start_drag_window");
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
    const tab = parts[2] ? decodeURIComponent(parts[2]) : undefined;
    return {
      name: "project",
      projectId: parts[1],
      tab: projectTabs.includes(tab as ProjectTab) ? tab as ProjectTab : undefined,
    };
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

function ReadOnlyAnnotationOverlay({
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
      className="preview-annotations"
      preserveAspectRatio="none"
      viewBox={`0 0 ${image.width || 640} ${image.height || 480}`}
    >
      {visibleObjects.map((object) => object.type === "bbox" && object.bbox ? (
        <g key={object.id}>
          <rect
            aria-label={`${image.fileName} 标注框 ${object.label}`}
            className="preview-annotation-box"
            x={object.bbox.x}
            y={object.bbox.y}
            width={object.bbox.width}
            height={object.bbox.height}
          />
          <text x={object.bbox.x + 6} y={Math.max(18, object.bbox.y - 6)}>{object.label}</text>
        </g>
      ) : object.polygon ? (
        <polygon
          aria-label={`${image.fileName} 多边形标注 ${object.label}`}
          className="preview-annotation-polygon"
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

function isEditableShortcutTarget(target: EventTarget | null) {
  if (!(target instanceof HTMLElement)) return false;
  return Boolean(target.closest("input, textarea, select, [contenteditable='true']"));
}

function fitCanvasViewport(imageWidth: number, imageHeight: number, canvasWidth: number, canvasHeight: number): CanvasViewport {
  const padding = 36;
  const safeWidth = Math.max(1, canvasWidth - padding * 2);
  const safeHeight = Math.max(1, canvasHeight - padding * 2);
  const scale = clamp(Math.min(safeWidth / imageWidth, safeHeight / imageHeight, 1), 0.1, 8);
  return {
    scale,
    offsetX: Math.round((canvasWidth - imageWidth * scale) / 2),
    offsetY: Math.round((canvasHeight - imageHeight * scale) / 2),
  };
}

function zoomCanvasViewport(
  viewport: CanvasViewport,
  nextScale: number,
  focalPoint: { x: number; y: number },
): CanvasViewport {
  const scale = clamp(nextScale, 0.1, 8);
  const imageX = (focalPoint.x - viewport.offsetX) / viewport.scale;
  const imageY = (focalPoint.y - viewport.offsetY) / viewport.scale;
  return {
    scale,
    offsetX: focalPoint.x - imageX * scale,
    offsetY: focalPoint.y - imageY * scale,
  };
}

function drawAnnotationCanvas({
  activeImage,
  canvas,
  ctx,
  draftBox,
  imageElement,
  imageReady,
  mode,
  objects,
  selectedObjectId,
  size,
  viewport,
}: {
  activeImage: DatasetImage | undefined;
  canvas: HTMLCanvasElement;
  ctx: CanvasRenderingContext2D;
  draftBox: { start: { x: number; y: number }; end: { x: number; y: number } } | null;
  imageElement: HTMLImageElement | null;
  imageReady: boolean;
  mode: ToolMode;
  objects: AnnotationObject[];
  selectedObjectId: string | null;
  size: CanvasSize;
  viewport: CanvasViewport;
}) {
  const dpr = window.devicePixelRatio || 1;
  const width = Math.max(1, Math.round(size.width));
  const height = Math.max(1, Math.round(size.height));
  const pixelWidth = Math.round(width * dpr);
  const pixelHeight = Math.round(height * dpr);
  if (canvas.width !== pixelWidth || canvas.height !== pixelHeight) {
    canvas.width = pixelWidth;
    canvas.height = pixelHeight;
  }

  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, width, height);
  ctx.fillStyle = "#111827";
  ctx.fillRect(0, 0, width, height);
  ctx.strokeStyle = "rgba(255, 255, 255, 0.05)";
  ctx.lineWidth = 1;
  for (let x = 0; x < width; x += 48) {
    ctx.beginPath();
    ctx.moveTo(x, 0);
    ctx.lineTo(x, height);
    ctx.stroke();
  }
  for (let y = 0; y < height; y += 48) {
    ctx.beginPath();
    ctx.moveTo(0, y);
    ctx.lineTo(width, y);
    ctx.stroke();
  }

  const imageWidth = activeImage?.width || 640;
  const imageHeight = activeImage?.height || 480;
  ctx.save();
  ctx.translate(viewport.offsetX, viewport.offsetY);
  ctx.scale(viewport.scale, viewport.scale);
  ctx.imageSmoothingEnabled = true;
  ctx.imageSmoothingQuality = "high";

  if (imageReady && imageElement) {
    ctx.drawImage(imageElement, 0, 0, imageWidth, imageHeight);
  } else {
    ctx.fillStyle = "#253044";
    ctx.fillRect(0, 0, imageWidth, imageHeight);
    ctx.fillStyle = "#94a3b8";
    ctx.font = `${Math.max(13 / viewport.scale, 10)}px sans-serif`;
    ctx.fillText("加载图片", 18 / viewport.scale, 28 / viewport.scale);
  }

  ctx.strokeStyle = "rgba(255, 255, 255, 0.42)";
  ctx.lineWidth = 1 / viewport.scale;
  ctx.strokeRect(0, 0, imageWidth, imageHeight);

  function drawBox(box: NonNullable<AnnotationObject["bbox"]>, label: string, selected: boolean, draft = false) {
    ctx.save();
    ctx.lineWidth = (selected ? 3 : 2) / viewport.scale;
    ctx.strokeStyle = draft ? "#1769e0" : selected ? "#1769e0" : "#1fa7ff";
    ctx.fillStyle = selected ? "rgba(23, 105, 224, 0.16)" : "rgba(31, 167, 255, 0.08)";
    if (draft) {
      ctx.setLineDash([6 / viewport.scale, 4 / viewport.scale]);
      ctx.fillStyle = "rgba(23, 105, 224, 0.1)";
    }
    ctx.fillRect(box.x, box.y, box.width, box.height);
    ctx.strokeRect(box.x, box.y, box.width, box.height);
    ctx.setLineDash([]);

    if (!draft) {
      const fontSize = Math.max(14 / viewport.scale, 9);
      const labelX = box.x + 4 / viewport.scale;
      const labelY = Math.max(fontSize + 2 / viewport.scale, box.y - 4 / viewport.scale);
      ctx.font = `700 ${fontSize}px sans-serif`;
      ctx.lineJoin = "round";
      ctx.strokeStyle = "rgba(17, 24, 39, 0.86)";
      ctx.lineWidth = 4 / viewport.scale;
      ctx.strokeText(label, labelX, labelY);
      ctx.fillStyle = "#ffffff";
      ctx.fillText(label, labelX, labelY);
    }

    if (selected) {
      const handleSize = 8 / viewport.scale;
      ctx.fillStyle = "#ffffff";
      ctx.strokeStyle = "#1769e0";
      ctx.lineWidth = 2 / viewport.scale;
      bboxHandles(box).forEach((handle) => {
        ctx.fillRect(handle.x - handleSize / 2, handle.y - handleSize / 2, handleSize, handleSize);
        ctx.strokeRect(handle.x - handleSize / 2, handle.y - handleSize / 2, handleSize, handleSize);
      });
    }
    ctx.restore();
  }

  objects.forEach((object) => {
    if (object.type === "bbox" && object.bbox) {
      drawBox(object.bbox, object.label, object.id === selectedObjectId);
    } else if (object.polygon) {
      ctx.save();
      ctx.beginPath();
      object.polygon.forEach((point, index) => {
        if (index === 0) ctx.moveTo(point.x, point.y);
        else ctx.lineTo(point.x, point.y);
      });
      ctx.closePath();
      ctx.fillStyle = "rgba(204, 84, 216, 0.16)";
      ctx.strokeStyle = "#cc54d8";
      ctx.lineWidth = 2 / viewport.scale;
      ctx.fill();
      ctx.stroke();
      ctx.restore();
    }
  });

  if (draftBox) {
    drawBox(normalizeBox(draftBox.start, draftBox.end), "", false, true);
  }

  if (mode === "pan") {
    ctx.restore();
    ctx.fillStyle = "rgba(17, 24, 39, 0.62)";
    ctx.fillRect(12, height - 36, 120, 24);
    ctx.fillStyle = "#ffffff";
    ctx.font = "12px sans-serif";
    ctx.fillText("拖拽平移画布", 24, height - 20);
    return;
  }

  ctx.restore();
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
  activeProjectId,
  activeProjectContext,
  backendConnection,
  onBackendTasks,
  onCreateDataset,
  onDataSubmit,
  onDatasets,
  onProjectAnnotate,
  onProjectOpenWindow,
  onProjectTab,
}: {
  activeProjectId?: string;
  activeProjectContext: ProjectTopbarContext | null;
  backendConnection: BackendConnection;
  onBackendTasks: () => void;
  onCreateDataset: () => void;
  onDataSubmit: () => void;
  onDatasets: () => void;
  onProjectAnnotate: (projectId: string, imageId?: string) => void;
  onProjectOpenWindow: (project: DatasetProject) => void;
  onProjectTab: (projectId: string, tab: ProjectTab) => void;
}) {
  const showWindowControls = backendConnection.mode === "tauri";
  const isProjectScope = Boolean(activeProjectId);
  const activeProject = activeProjectContext?.project ?? null;

  return (
    <header className="topbar" data-tauri-drag-region onMouseDown={beginDesktopWindowDrag}>
      <button
        aria-label={isProjectScope ? "返回数据集列表" : "数据集首页"}
        className="brand"
        data-no-drag
        onClick={onDatasets}
        type="button"
      >
        {isProjectScope ? <ArrowLeft size={19} /> : <ImageIcon size={19} />}
      </button>
      <div className="topbar-context">
        {activeProject ? (
          <>
            <strong>{activeProject.name}</strong>
            <span>{activeProject.description}</span>
          </>
        ) : isProjectScope ? (
          <span>加载数据集...</span>
        ) : null}
      </div>
      <div className="topbar-actions" data-no-drag>
        {isProjectScope && activeProjectId ? (
          <>
            {activeProject ? (
              <>
                <button type="button" onClick={() => onProjectAnnotate(activeProject.id, activeProjectContext?.firstImageId)}>
                  <Play size={16} />
                  开始标注
                </button>
                <button type="button" onClick={() => onProjectOpenWindow(activeProject)}>
                  <Layers3 size={16} />
                  独立窗口标注
                </button>
              </>
            ) : null}
            <button type="button" onClick={onDataSubmit}>
              <Plus size={16} />
              添加数据
            </button>
            <button type="button" onClick={() => onProjectTab(activeProjectId, "快照")}>
              <Save size={16} />
              快照管理
            </button>
            <button className="primary" type="button" onClick={() => onProjectTab(activeProjectId, "导出")}>
              <Download size={16} />
              导出数据集
            </button>
          </>
        ) : (
          <>
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
          </>
        )}
        <span className={`sync-state ${backendConnection.mode}`}>
          {backendConnection.mode === "unavailable" ? <CircleAlert size={15} /> : <CheckCircle2 size={15} />}
          {backendConnection.label}
        </span>
        {showWindowControls ? (
          <>
            <button aria-label="最小化" type="button" onClick={() => runDesktopCommand("minimize_window")}>
              <Minus size={16} />
            </button>
            <button aria-label="最大化" type="button" onClick={() => runDesktopCommand("toggle_maximize_window")}>
              <Maximize2 size={16} />
            </button>
            <button aria-label="关闭到托盘" type="button" onClick={() => runDesktopCommand("close_window")}>
              <X size={16} />
            </button>
          </>
        ) : null}
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

type DataImportAction = "open-local" | "copy-images" | "copy-yolo";

function DataSubmitDialog({
  datasets,
  projects,
  onCancel,
  onAnalyzeSource,
  onDownload,
  onImportFiles,
  onImportImages,
  onImportYolo,
  onOpenLocal,
  onPickSource,
}: {
  datasets: BuiltinDataset[];
  projects: DatasetProject[];
  onCancel: () => void;
  onAnalyzeSource: (sourcePaths: string[]) => Promise<DataSourceAnalysis>;
  onDownload: (datasetKey: string) => void;
  onImportFiles: (projectId: string, sourcePaths: string[]) => void;
  onImportImages: (projectId: string, sourcePath: string) => void;
  onImportYolo: (projectId: string, sourcePath: string) => void;
  onOpenLocal: (sourcePath: string, datasetType: string) => void;
  onPickSource: (selectionType: "folder" | "files") => Promise<string[] | null>;
}) {
  const [projectId, setProjectId] = useState(projects[0]?.id ?? "");
  const [analysis, setAnalysis] = useState<DataSourceAnalysis | null>(null);
  const [datasetType, setDatasetType] = useState<"voc-detect" | "yolo-detect" | "image-directory">("voc-detect");
  const [importAction, setImportAction] = useState<DataImportAction>("open-local");
  const [analyzeState, setAnalyzeState] = useState<"idle" | "loading" | "error">("idle");
  const [message, setMessage] = useState<string | null>(null);
  const [dropActive, setDropActive] = useState(false);

  const analyzeSourcePaths = useCallback(async (sourcePaths: string[]) => {
    if (!sourcePaths.length) return;
    setAnalyzeState("loading");
    setMessage(null);
    try {
      const nextAnalysis = await onAnalyzeSource(sourcePaths);
      setAnalysis(nextAnalysis);
      setDatasetType(normalizeDetectedDatasetType(nextAnalysis.detectedFormat));
      setImportAction(defaultImportAction(nextAnalysis));
      setAnalyzeState("idle");
    } catch (error) {
      setAnalyzeState("error");
      setMessage(error instanceof Error ? error.message : String(error));
    }
  }, [onAnalyzeSource]);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | null = null;
    getCurrentWebview()
      .onDragDropEvent((event) => {
        if (event.payload.type === "enter" || event.payload.type === "over") {
          setDropActive(true);
          return;
        }
        if (event.payload.type === "leave") {
          setDropActive(false);
          return;
        }
        setDropActive(false);
        void analyzeSourcePaths(event.payload.paths);
      })
      .then((nextUnlisten) => {
        if (disposed) {
          nextUnlisten();
          return;
        }
        unlisten = nextUnlisten;
      })
      .catch(() => {
        // In a normal browser there is no Tauri webview event source.
      });

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [analyzeSourcePaths]);

  async function chooseSource(selectionType: "folder" | "files") {
    setAnalyzeState("loading");
    setMessage(null);
    try {
      const sourcePaths = await onPickSource(selectionType);
      if (!sourcePaths?.length) {
        setAnalyzeState("idle");
        return;
      }
      await analyzeSourcePaths(sourcePaths);
    } catch (error) {
      setAnalyzeState("error");
      setMessage(error instanceof Error ? error.message : String(error));
    }
  }

  function handleDragEnter(event: DragEvent<HTMLElement>) {
    event.preventDefault();
    setDropActive(true);
  }

  function handleDragOver(event: DragEvent<HTMLElement>) {
    event.preventDefault();
    event.dataTransfer.dropEffect = "copy";
    setDropActive(true);
  }

  function handleDragLeave(event: DragEvent<HTMLElement>) {
    const nextTarget = event.relatedTarget instanceof Node ? event.relatedTarget : null;
    if (nextTarget && event.currentTarget.contains(nextTarget)) return;
    setDropActive(false);
  }

  function handleDrop(event: DragEvent<HTMLElement>) {
    event.preventDefault();
    setDropActive(false);
    const sourcePaths = pathsFromDrop(event.dataTransfer);
    if (!sourcePaths.length) {
      setAnalyzeState("error");
      setMessage("当前环境无法读取拖拽文件路径，请使用选择文件夹或选择多个文件。");
      return;
    }
    void analyzeSourcePaths(sourcePaths);
  }

  function confirmImport() {
    if (!analysis) return;
    if (importAction === "open-local") {
      onOpenLocal(analysis.rootPath, datasetType === "image-directory" ? "voc-detect" : datasetType);
      return;
    }
    if (importAction === "copy-yolo") {
      onImportYolo(projectId, analysis.rootPath);
      return;
    }
    if (analysis.sourceKind === "files") {
      onImportFiles(projectId, analysis.sourcePaths);
      return;
    }
    onImportImages(projectId, analysis.rootPath);
  }

  const canConfirm = Boolean(
    analysis
      && analysis.imageCount > 0
      && (importAction === "open-local" || projectId),
  );

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
        <section className="source-picker">
          <button type="button" onClick={() => chooseSource("folder")} disabled={analyzeState === "loading"}>
            <FolderOpen size={18} />
            <div>
              <strong>选择文件夹</strong>
              <span>自动识别 VOC / YOLO / 图片目录</span>
            </div>
          </button>
          <button type="button" onClick={() => chooseSource("files")} disabled={analyzeState === "loading"}>
            <Files size={18} />
            <div>
              <strong>选择多个文件</strong>
              <span>适合临时补充图片到当前项目</span>
            </div>
          </button>
        </section>
        <section
          aria-label="拖拽添加数据"
          className={`drop-source ${dropActive ? "active" : ""}`}
          onDragEnter={handleDragEnter}
          onDragLeave={handleDragLeave}
          onDragOver={handleDragOver}
          onDrop={handleDrop}
        >
          <Upload size={20} />
          <div>
            <strong>{dropActive ? "松开后分析数据" : "拖拽文件夹或多个文件到这里"}</strong>
            <span>不会立即导入，系统会先展开导入确认界面。</span>
          </div>
        </section>
        {message ? <div className="inline-error">{message}</div> : null}
        {analysis ? (
          <section className="import-review">
            <div className="import-review-main">
              <div className="analysis-summary">
                <span>{analysis.sourceKind === "folder" ? "文件夹分析" : "多文件分析"}</span>
                <h3>{formatDatasetFormat(analysis.detectedFormat)}</h3>
                <p>{analysis.rootPath}</p>
                <div className="analysis-metrics">
                  <div><strong>{analysis.imageCount}</strong><span>图片</span></div>
                  <div><strong>{analysis.annotationCount}</strong><span>标注文件</span></div>
                  <div><strong>{analysis.classCount}</strong><span>类别</span></div>
                  <div><strong>{analysis.splitCount || 1}</strong><span>分组</span></div>
                </div>
              </div>
              <div className="import-settings">
                <label>
                  <span>导入方式</span>
                  <select value={importAction} onChange={(event) => setImportAction(event.target.value as DataImportAction)}>
                    <option value="open-local">链接本机目录（原地写回标注）</option>
                    <option value="copy-images">复制图片到目标项目</option>
                    {analysis.detectedFormat === "yolo-detect" ? <option value="copy-yolo">复制 YOLO 数据集到目标项目</option> : null}
                  </select>
                </label>
                <label>
                  <span>数据类型</span>
                  <select value={datasetType} onChange={(event) => setDatasetType(event.target.value as typeof datasetType)}>
                    <option value="voc-detect">Pascal VOC BBox XML</option>
                    <option value="yolo-detect">YOLO BBox TXT</option>
                    <option value="image-directory">仅图片目录</option>
                  </select>
                </label>
                {importAction !== "open-local" ? (
                  <label>
                    <span>目标项目</span>
                    <select value={projectId} onChange={(event) => setProjectId(event.target.value)}>
                      <option value="">请选择项目</option>
                      {projects.map((project) => (
                        <option key={project.id} value={project.id}>{project.name}</option>
                      ))}
                    </select>
                  </label>
                ) : null}
              </div>
              {analysis.classes.length ? (
                <div className="class-preview">
                  {analysis.classes.slice(0, 12).map((label) => <span key={label}>{label}</span>)}
                  {analysis.classes.length > 12 ? <span>+{analysis.classes.length - 12}</span> : null}
                </div>
              ) : null}
              {analysis.warnings.length ? (
                <div className="import-warnings">
                  {analysis.warnings.map((warning) => <span key={warning}>{warning}</span>)}
                </div>
              ) : null}
              <div className="dialog-actions">
                <button type="button" onClick={() => setAnalysis(null)}>重新选择</button>
                <button className="primary" type="button" onClick={confirmImport} disabled={!canConfirm}>
                  确认导入
                </button>
              </div>
            </div>
            <aside className="source-tree" aria-label="文件夹结构">
              <h3>文件夹结构</h3>
              {analysis.tree.map((node) => <SourceTreeNode node={node} key={`${node.kind}-${node.path}`} />)}
            </aside>
          </section>
        ) : (
          <section className="empty-analysis">
            <Database size={20} />
            <div>
              <h3>{analyzeState === "loading" ? "正在分析数据结构" : "选择数据来源后确认导入"}</h3>
              <p>选择文件夹或多个文件后，系统会自动识别标注格式并展开导入确认界面。</p>
            </div>
          </section>
        )}
        <details className="builtin-downloads">
          <summary>内置下载</summary>
          <BuiltinDatasetPanel datasets={datasets} onDownload={onDownload} />
        </details>
      </section>
    </div>
  );
}

function SourceTreeNode({ node }: { node: DataSourceTreeNode }) {
  return (
    <div className={`tree-node ${node.kind}`}>
      <div>
        <span>{node.kind === "folder" ? "▸" : ""}</span>
        <strong>{node.name}</strong>
      </div>
      {node.children.length ? (
        <div className="tree-children">
          {node.children.map((child) => <SourceTreeNode node={child} key={`${child.kind}-${child.path}`} />)}
          {node.truncated ? <span className="tree-more">已截断显示</span> : null}
        </div>
      ) : node.truncated ? (
        <span className="tree-more">已截断显示</span>
      ) : null}
    </div>
  );
}

function pathsFromDrop(dataTransfer: DataTransfer) {
  const droppedText = typeof dataTransfer.getData === "function" ? dataTransfer.getData("text/plain") : "";
  const explicitPaths = droppedText
    .split(/\r?\n/)
    .map((path) => path.trim())
    .filter(Boolean);
  if (explicitPaths.length) return explicitPaths;

  return Array.from(dataTransfer.files)
    .map((file) => {
      const pathFile = file as File & { path?: string; webkitRelativePath?: string };
      return pathFile.path || pathFile.webkitRelativePath || "";
    })
    .filter(Boolean);
}

function normalizeDetectedDatasetType(format: DataSourceAnalysis["detectedFormat"]): "voc-detect" | "yolo-detect" | "image-directory" {
  if (format === "yolo-detect") return "yolo-detect";
  if (format === "voc-detect") return "voc-detect";
  return "image-directory";
}

function defaultImportAction(analysis: DataSourceAnalysis): DataImportAction {
  if (analysis.recommendedAction === "open-local") return "open-local";
  return "copy-images";
}

function formatDatasetFormat(format: DataSourceAnalysis["detectedFormat"]) {
  if (format === "voc-detect") return "Pascal VOC BBox";
  if (format === "yolo-detect") return "YOLO BBox";
  if (format === "image-directory") return "图片目录";
  return "未识别";
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
              const demoTemplate =
                datasetType === "yolo-seg"
                  ? "demo-polygon"
                  : datasetType === "image-classification"
                    ? "demo-classification"
                    : "demo-bbox";
              onChange({
                ...form,
                datasetType,
                demoTemplate,
              });
            }}
            value={form.datasetType}
          >
            <option value="yolo-detect">目标检测 / YOLO BBox</option>
            <option value="yolo-seg">实例分割 / YOLO Polygon</option>
            <option value="image-classification">图像分类 / Classification</option>
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
            <option value="demo-classification">Demo Classification：生成 3 张样例图和分类目录</option>
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

function ImagePreviewDialog({
  image,
  imageUrl,
  objects,
  onAnnotate,
  onClose,
}: {
  image: DatasetImage;
  imageUrl: string | undefined;
  objects: AnnotationObject[] | undefined;
  onAnnotate: () => void;
  onClose: () => void;
}) {
  return (
    <div className="modal-backdrop">
      <section aria-labelledby="image-preview-title" className="dataset-dialog image-preview-dialog" role="dialog">
        <div className="detail-header">
          <div>
            <span className="eyebrow">查看弹窗</span>
            <h2 id="image-preview-title">图像预览</h2>
          </div>
          <button aria-label="关闭图像预览" type="button" onClick={onClose}>
            <X size={16} />
          </button>
        </div>
        <div className="image-preview-body">
          <div
            className="image-preview-stage"
            style={{ aspectRatio: `${image.width || 640} / ${image.height || 480}` }}
          >
            {imageUrl ? <img alt={`预览 ${image.fileName}`} src={imageUrl} /> : <div className="road-scene" />}
            <ReadOnlyAnnotationOverlay image={image} objects={objects} />
          </div>
          <aside className="image-preview-meta" aria-label="图像信息">
            <h3>{image.fileName}</h3>
            <dl>
              <dt>尺寸</dt>
              <dd>{image.width} x {image.height}</dd>
              <dt>状态</dt>
              <dd>{image.status}</dd>
              <dt>分组</dt>
              <dd>{image.split}</dd>
              <dt>对象数</dt>
              <dd>{objects?.length ?? 0}</dd>
            </dl>
            <div className="tag-list compact">
              {image.tags.map((tag) => (
                <span key={tag}>{tag}</span>
              ))}
            </div>
          </aside>
        </div>
        <div className="dialog-actions">
          <button type="button" onClick={onClose}>关闭</button>
          <button className="primary" type="button" onClick={onAnnotate}>
            <Tags size={16} />
            标记
          </button>
        </div>
      </section>
    </div>
  );
}

function ProjectWorkspace({
  projectId,
  routeTab,
  onProjectContextChange,
}: {
  projectId: string;
  routeTab?: ProjectTab;
  onProjectContextChange: (context: ProjectTopbarContext | null) => void;
}) {
  const [detail, setDetail] = useState<ProjectDetail | null>(null);
  const [images, setImages] = useState<DatasetImage[]>([]);
  const [snapshots, setSnapshots] = useState<DatasetSnapshot[]>([]);
  const [exports, setExports] = useState<DatasetExport[]>([]);
  const [workflowMessage, setWorkflowMessage] = useState<string | null>(null);
  const [tab, setTab] = useState<ProjectTab>(routeTab ?? "概览");
  const [imagePage, setImagePage] = useState(0);
  const [previewImageId, setPreviewImageId] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<{ title: string; message: string } | null>(null);
  const [selectedClass, setSelectedClass] = useState<ClassStat | null>(null);
  const [classSamples, setClassSamples] = useState<ClassSample[]>([]);
  const [classSamplePage, setClassSamplePage] = useState(0);
  const [classSampleState, setClassSampleState] = useState<"idle" | "loading" | "error">("idle");
  const [classSampleMessage, setClassSampleMessage] = useState<string | null>(null);
  const classSampleImages = useMemo(() => classSamples.map((sample) => sample.image), [classSamples]);
  const imageUrls = useImageAssetUrls(projectId, images, images.length);
  const imageAnnotations = useImageAnnotations(projectId, images, images.length);
  const classSampleUrls = useImageAssetUrls(projectId, classSampleImages, classSampleImages.length);
  const classSampleAnnotations = useImageAnnotations(projectId, classSampleImages, classSampleImages.length);
  const previewImage =
    images.find((image) => image.id === previewImageId)
    ?? classSampleImages.find((image) => image.id === previewImageId)
    ?? null;

  useEffect(() => {
    setImagePage(0);
    setSelectedClass(null);
    setClassSamples([]);
    setClassSamplePage(0);
  }, [projectId]);

  useEffect(() => {
    if (routeTab) {
      setTab(routeTab);
    }
  }, [routeTab]);

  useEffect(() => {
    if (!selectedClass) {
      setClassSamples([]);
      setClassSampleState("idle");
      setClassSampleMessage(null);
      return;
    }

    let cancelled = false;
    setClassSampleState("loading");
    setClassSampleMessage(null);
    listClassSamples(projectId, {
      classId: selectedClass.id,
      label: selectedClass.label,
      offset: classSamplePage * projectImagePageSize,
      limit: projectImagePageSize,
    })
      .then((samples) => {
        if (!cancelled) {
          setClassSamples(samples);
          setClassSampleState("idle");
        }
      })
      .catch((error) => {
        if (!cancelled) {
          setClassSamples([]);
          setClassSampleState("error");
          setClassSampleMessage(error instanceof Error ? error.message : String(error));
        }
      });

    return () => {
      cancelled = true;
    };
  }, [projectId, selectedClass, classSamplePage]);

  useEffect(() => {
    setLoadError(null);
    onProjectContextChange(null);
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
        onProjectContextChange({
          project: nextDetail.project,
          firstImageId: nextImages[0]?.id,
        });
      })
      .catch((error) => {
        setDetail(null);
        setImages([]);
        setSnapshots([]);
        setExports([]);
        onProjectContextChange(null);
        setLoadError(
          isBackendUnavailableError(error)
            ? { title: "后端未连接", message: "请在 Tauri 桌面环境启动应用。" }
            : { title: "数据集未初始化", message: error instanceof Error ? error.message : String(error) },
        );
      });
  }, [projectId, imagePage, onProjectContextChange]);

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

  async function openAnnotationConsole(imageId: string) {
    try {
      await openAnnotationWindow(projectId, imageId);
    } catch (error) {
      if (isBackendUnavailableError(error)) {
        navigate(`#/annotate/${projectId}/${imageId}`);
        return;
      }
      throw error;
    }
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
          <div className="project-tabs" aria-label="项目页面">
            {projectTabs.map((item) => {
              const TabIcon = projectTabIcons[item];
              return (
                <button aria-pressed={item === tab} className={item === tab ? "active" : ""} key={item} onClick={() => setTab(item)} type="button">
                  <TabIcon aria-hidden="true" size={15} />
                  {item}
                </button>
              );
            })}
          </div>
        </div>
        <section className={`project-surface ${tab === "概览" ? "overview-surface" : ""}`}>
          {renderProjectTab(tab, detail, images, imageUrls, imageAnnotations, {
            classSampleAnnotations,
            classSampleMessage,
            classSamplePage,
            classSampleState,
            classSampleUrls,
            classSamples,
            exports,
            imagePage,
            imagePageSize: projectImagePageSize,
            selectedClass,
            snapshots,
            workflowMessage,
            onClassSamplePageChange: setClassSamplePage,
            onCreateSnapshot: handleCreateSnapshot,
            onExport: handleExport,
            onImagePageChange: setImagePage,
            onOpenClassSample: openAnnotationConsole,
            onOpenAnnotation: openAnnotationConsole,
            onPreviewImage: setPreviewImageId,
            onSelectClass: (row) => {
              setSelectedClass(row);
              setClassSamplePage(0);
            },
            onTabChange: setTab,
          })}
        </section>
        {previewImage ? (
          <ImagePreviewDialog
            image={previewImage}
            imageUrl={imageUrls[previewImage.id] ?? classSampleUrls[previewImage.id]}
            objects={imageAnnotations[previewImage.id] ?? classSampleAnnotations[previewImage.id]}
            onAnnotate={() => openAnnotationConsole(previewImage.id)}
            onClose={() => setPreviewImageId(null)}
          />
        ) : null}
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
    selectedClass: ClassStat | null;
    classSamples: ClassSample[];
    classSamplePage: number;
    classSampleState: "idle" | "loading" | "error";
    classSampleMessage: string | null;
    classSampleUrls: Record<string, string>;
    classSampleAnnotations: Record<string, AnnotationObject[]>;
    snapshots: DatasetSnapshot[];
    exports: DatasetExport[];
    imagePage: number;
    imagePageSize: number;
    workflowMessage: string | null;
    onSelectClass: (row: ClassStat) => void;
    onClassSamplePageChange: (page: number) => void;
    onOpenClassSample: (imageId: string) => void;
    onOpenAnnotation: (imageId: string) => void;
    onCreateSnapshot: () => void;
    onExport: (format: "yolo" | "coco") => void;
    onImagePageChange: (page: number) => void;
    onPreviewImage: (imageId: string) => void;
    onTabChange: (tab: ProjectTab) => void;
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
            <div className="tab-title-stack">
              <h2>图片浏览</h2>
              <p>查看图像、预览已有标注，并从预览进入标记控制台。</p>
            </div>
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
              <article
                className="image-tile"
                key={image.id}
                onDoubleClick={() => navigate(`#/annotate/${detail.project.id}/${image.id}`)}
                tabIndex={0}
                onKeyDown={(event) => {
                  if (event.key === "Enter" && event.currentTarget === event.target) {
                    navigate(`#/annotate/${detail.project.id}/${image.id}`);
                  }
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
                <div className="image-tile-actions">
                  <button
                    type="button"
                    onClick={(event) => {
                      event.stopPropagation();
                      workflow.onPreviewImage(image.id);
                    }}
                  >
                    <Eye size={15} />
                    预览 {image.fileName}
                  </button>
                </div>
              </article>
            ))}
          </div>
        </div>
      );
    case "类别":
      const matchedObjectCount = workflow.classSamples.reduce((sum, sample) => sum + sample.matchCount, 0);
      return (
        <div className="class-sample-layout">
          <div>
            <h2>类别体系</h2>
            <div className="data-table">
              {detail.classes.map((row) => (
                <div className="table-row class-row" key={row.label}>
                  <strong>
                    <span className="class-color" style={{ backgroundColor: row.color }} />
                    {row.label}
                  </strong>
                  <span>{row.count} 个对象</span>
                  <span>{row.attributes.join(", ") || "默认属性"}</span>
                  <button
                    aria-label={`查看 ${row.label} 样本`}
                    type="button"
                    onClick={() => workflow.onSelectClass(row)}
                  >
                    查看样本
                  </button>
                </div>
              ))}
            </div>
          </div>
          {workflow.selectedClass ? (
            <section className="class-sample-panel" aria-label={`${workflow.selectedClass.label} 样本`}>
              <div className="tab-header-row">
                <div className="tab-title-stack">
                  <h2>{workflow.selectedClass.label} 样本</h2>
                  <p>{workflow.classSamples.length} 张图片 / {matchedObjectCount} 个匹配对象</p>
                </div>
                <div className="pager-actions">
                  <span>第 {workflow.classSamplePage + 1} 页</span>
                  <button
                    type="button"
                    onClick={() => workflow.onClassSamplePageChange(Math.max(0, workflow.classSamplePage - 1))}
                    disabled={workflow.classSamplePage === 0 || workflow.classSampleState === "loading"}
                  >
                    上一页
                  </button>
                  <button
                    type="button"
                    onClick={() => workflow.onClassSamplePageChange(workflow.classSamplePage + 1)}
                    disabled={workflow.classSamples.length < workflow.imagePageSize || workflow.classSampleState === "loading"}
                  >
                    下一页
                  </button>
                </div>
              </div>
              {workflow.classSampleState === "loading" ? <p className="empty-state">加载类别样本...</p> : null}
              {workflow.classSampleState === "error" ? <p className="empty-state error">{workflow.classSampleMessage}</p> : null}
              {workflow.classSampleState === "idle" && workflow.classSamples.length === 0 ? (
                <p className="empty-state">暂无匹配样本</p>
              ) : null}
              <div className="image-grid">
                {workflow.classSamples.map((sample) => (
                  <article className="image-tile" key={sample.image.id}>
                    <div className="sample-thumb traffic-a">
                      {workflow.classSampleUrls[sample.image.id] ? (
                        <img alt={sample.image.fileName} decoding="async" loading="lazy" src={workflow.classSampleUrls[sample.image.id]} />
                      ) : null}
                      <ThumbnailAnnotationOverlay image={sample.image} objects={workflow.classSampleAnnotations[sample.image.id]} />
                    </div>
                    <span>{sample.image.fileName}</span>
                    <em>{sample.matchCount} 个匹配对象</em>
                    <div className="image-tile-actions">
                      <button type="button" onClick={() => workflow.onPreviewImage(sample.image.id)}>
                        <Eye size={15} />
                        预览 {sample.image.fileName}
                      </button>
                      <button type="button" onClick={() => workflow.onOpenClassSample(sample.image.id)}>
                        标记
                      </button>
                    </div>
                  </article>
                ))}
              </div>
            </section>
          ) : null}
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
        <ProjectOverview
          detail={detail}
          images={images}
          imageAnnotations={imageAnnotations}
          imageUrls={imageUrls}
          onOpenAnnotation={workflow.onOpenAnnotation}
          onPreviewImage={workflow.onPreviewImage}
          onSelectClass={(row) => {
            workflow.onSelectClass(row);
            workflow.onTabChange("类别");
          }}
          onTabChange={workflow.onTabChange}
        />
      );
  }
}

function ProjectOverview({
  detail,
  images,
  imageAnnotations,
  imageUrls,
  onOpenAnnotation,
  onPreviewImage,
  onSelectClass,
  onTabChange,
}: {
  detail: ProjectDetail;
  images: DatasetImage[];
  imageAnnotations: Record<string, AnnotationObject[]>;
  imageUrls: Record<string, string>;
  onOpenAnnotation: (imageId: string) => void;
  onPreviewImage: (imageId: string) => void;
  onSelectClass: (row: ClassStat) => void;
  onTabChange: (tab: ProjectTab) => void;
}) {
  const project = detail.project;
  const annotatedCount = Math.round(project.imageCount * project.annotatedPercent / 100);
  const remainingCount = Math.max(0, project.imageCount - annotatedCount);
  const recentImages = images.slice(0, 6);
  const topClasses = detail.classes.slice(0, 6);
  const largestClass = Math.max(1, ...topClasses.map((row) => row.count));
  const firstImage = images[0];

  return (
    <div className="project-overview">
      <div className="project-overview-grid">
        <section className="overview-section overview-progress" aria-label="生产进度">
          <div className="overview-section-header">
            <div>
              <span className="section-kicker">PRODUCTION</span>
              <h2>生产进度</h2>
              <p>从数据接入到审核交付的当前完成情况</p>
            </div>
            <span className={`overview-status ${remainingCount === 0 ? "complete" : "active"}`}>
              {remainingCount === 0 ? "标注完成" : "生产中"}
            </span>
          </div>
          <div className="overview-progress-summary">
            <div className="overview-progress-value">
              <strong>{project.annotatedPercent}%</strong>
              <span>{formatNumber(annotatedCount)} / {formatNumber(project.imageCount)}</span>
            </div>
            <div
              aria-label="标注完成度"
              aria-valuemax={100}
              aria-valuemin={0}
              aria-valuenow={project.annotatedPercent}
              className="overview-progress-track"
              role="progressbar"
            >
              <span style={{ width: `${project.annotatedPercent}%` }} />
            </div>
          </div>
          <div className="overview-metrics">
            <div><span>图片</span><strong>{formatNumber(project.imageCount)}</strong></div>
            <div><span>类别</span><strong>{formatNumber(project.classCount)}</strong></div>
            <div><span>分组</span><strong>{formatNumber(project.tagGroupCount)}</strong></div>
            <div className={project.issueCount > 0 ? "has-issue" : ""}><span>问题</span><strong>{formatNumber(project.issueCount)}</strong></div>
          </div>
          {firstImage ? (
            <button className="overview-primary-action" type="button" onClick={() => onOpenAnnotation(firstImage.id)}>
              <Play aria-hidden="true" size={15} />
              {remainingCount === 0 ? "复查标注" : "继续标注"}
            </button>
          ) : null}
        </section>

        <section className="overview-section overview-queue" aria-label="工作队列">
          <div className="overview-section-header compact">
            <div>
              <span className="section-kicker">QUEUE</span>
              <h2>工作队列</h2>
            </div>
            <span className="queue-total">{formatNumber(remainingCount + project.reviewCount + project.issueCount)} 项</span>
          </div>
          <div className="overview-queue-list">
            <button type="button" onClick={() => onTabChange("图片")}>
              <span className={`queue-icon ${remainingCount === 0 ? "done" : "pending"}`}>
                {remainingCount === 0 ? <CheckCircle2 size={16} /> : <ImageIcon size={16} />}
              </span>
              <span><strong>{remainingCount === 0 ? "标注已完成" : `${formatNumber(remainingCount)} 张待标注`}</strong><small>图片标注队列</small></span>
              <Eye aria-hidden="true" size={15} />
            </button>
            <button type="button" onClick={() => onTabChange("质检")}>
              <span className={`queue-icon ${project.reviewCount === 0 ? "done" : "review"}`}>
                <ShieldCheck size={16} />
              </span>
              <span><strong>{project.reviewCount === 0 ? "暂无质检待办" : `${formatNumber(project.reviewCount)} 项待审核`}</strong><small>质量审核队列</small></span>
              <Eye aria-hidden="true" size={15} />
            </button>
            <button type="button" onClick={() => onTabChange("质检")}>
              <span className={`queue-icon ${project.issueCount === 0 ? "done" : "issue"}`}>
                <CircleAlert size={16} />
              </span>
              <span><strong>{formatNumber(project.issueCount)} 个质量问题</strong><small>{project.issueCount === 0 ? "当前数据检查正常" : "需要处理后再交付"}</small></span>
              <Eye aria-hidden="true" size={15} />
            </button>
          </div>
        </section>
      </div>

      <div className="project-overview-secondary">
        <section className="overview-section overview-recent" aria-label="最近样本">
          <div className="overview-section-header compact">
            <div>
              <span className="section-kicker">SAMPLES</span>
              <h2>最近样本</h2>
            </div>
            <button className="section-link" type="button" onClick={() => onTabChange("图片")}>
              查看全部图片
            </button>
          </div>
          {recentImages.length > 0 ? (
            <div className="overview-sample-grid">
              {recentImages.map((image) => (
                <button aria-label={`预览 ${image.fileName}`} className="overview-sample" key={image.id} type="button" onClick={() => onPreviewImage(image.id)}>
                  <span className="overview-sample-thumb">
                    {imageUrls[image.id] ? <img alt={image.fileName} decoding="async" loading="lazy" src={imageUrls[image.id]} /> : null}
                    <ThumbnailAnnotationOverlay image={image} objects={imageAnnotations[image.id]} />
                  </span>
                  <span className="overview-sample-meta">
                    <strong>{image.fileName}</strong>
                    <small>{image.status}</small>
                  </span>
                </button>
              ))}
            </div>
          ) : <p className="overview-empty">当前数据集还没有图片</p>}
        </section>

        <section className="overview-section overview-classes" aria-label="类别分布">
          <div className="overview-section-header compact">
            <div>
              <span className="section-kicker">CLASSES</span>
              <h2>类别分布</h2>
            </div>
            <button className="section-link" type="button" onClick={() => onTabChange("类别")}>全部类别</button>
          </div>
          <div className="overview-class-list">
            {topClasses.map((row) => (
              <button aria-label={`查看 ${row.label} 类别`} key={row.label} type="button" onClick={() => onSelectClass(row)}>
                <span className="overview-class-label"><i style={{ backgroundColor: row.color }} /><strong>{row.label}</strong></span>
                <span className="overview-class-track"><i style={{ backgroundColor: row.color, width: `${row.count === 0 ? 0 : Math.max(4, row.count / largestClass * 100)}%` }} /></span>
                <span>{formatNumber(row.count)}</span>
              </button>
            ))}
          </div>
        </section>
      </div>

      <section className="overview-info-band" aria-label="数据集信息">
        <div>
          <span className="section-kicker">DATASET</span>
          <h2>数据集信息</h2>
        </div>
        <p>{project.description}</p>
        <div className="tag-list compact">{project.tags.map((tag) => <span key={tag}>{tag}</span>)}</div>
      </section>
    </div>
  );
}

function AnnotationWorkspace({
  projectId,
  imageId,
  showWindowControls,
}: {
  projectId: string;
  imageId?: string;
  showWindowControls: boolean;
}) {
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
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const canvasShellRef = useRef<HTMLDivElement | null>(null);
  const imageElementRef = useRef<HTMLImageElement | null>(null);
  const selectedLabelInputRef = useRef<HTMLInputElement | null>(null);
  const [canvasSize, setCanvasSize] = useState<CanvasSize>({ width: 800, height: 600 });
  const [viewport, setViewport] = useState<CanvasViewport>({ scale: 1, offsetX: 80, offsetY: 60 });
  const [imageReady, setImageReady] = useState(false);
  const [panState, setPanState] = useState<{
    start: { x: number; y: number };
    original: CanvasViewport;
  } | null>(null);

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
    function measureCanvas() {
      const rect = canvasShellRef.current?.getBoundingClientRect();
      if (!rect || rect.width <= 0 || rect.height <= 0) return;
      setCanvasSize((current) => {
        const width = Math.round(rect.width);
        const height = Math.round(rect.height);
        if (current.width === width && current.height === height) return current;
        return { width, height };
      });
    }

    measureCanvas();
    if (typeof ResizeObserver === "undefined") {
      window.addEventListener("resize", measureCanvas);
      return () => window.removeEventListener("resize", measureCanvas);
    }

    const observer = new ResizeObserver(measureCanvas);
    if (canvasShellRef.current) observer.observe(canvasShellRef.current);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    if (!activeImage) return;
    setViewport(fitCanvasViewport(activeImage.width, activeImage.height, canvasSize.width, canvasSize.height));
  }, [activeImage?.id, activeImage?.width, activeImage?.height, canvasSize.width, canvasSize.height]);

  useEffect(() => {
    if (!assetUrl) {
      imageElementRef.current = null;
      setImageReady(false);
      return;
    }

    let cancelled = false;
    const image = new Image();
    image.decoding = "async";
    image.onload = () => {
      if (cancelled) return;
      imageElementRef.current = image;
      setImageReady(true);
    };
    image.onerror = () => {
      if (cancelled) return;
      imageElementRef.current = null;
      setImageReady(false);
    };
    setImageReady(false);
    image.src = assetUrl;
    return () => {
      cancelled = true;
    };
  }, [assetUrl]);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    let ctx: CanvasRenderingContext2D | null = null;
    try {
      ctx = canvas.getContext("2d");
    } catch {
      ctx = null;
    }
    if (!ctx) return;

    drawAnnotationCanvas({
      activeImage,
      canvas,
      ctx,
      draftBox,
      imageElement: imageElementRef.current,
      imageReady,
      mode,
      objects,
      selectedObjectId,
      size: canvasSize,
      viewport,
    });
  }, [activeImage, canvasSize, draftBox, imageReady, mode, objects, selectedObjectId, viewport]);

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
      const key = event.key.toLowerCase();
      const commandKey = event.ctrlKey || event.metaKey;
      const editableTarget = isEditableShortcutTarget(event.target);

      if (commandKey && key === "s") {
        event.preventDefault();
        void save();
        return;
      }

      if (commandKey && key === "d") {
        event.preventDefault();
        duplicateSelectedObject();
        return;
      }

      if (commandKey && key === "e") {
        event.preventDefault();
        selectedLabelInputRef.current?.focus();
        selectedLabelInputRef.current?.select();
        return;
      }

      if (commandKey && (key === "+" || (key === "=" && event.shiftKey))) {
        event.preventDefault();
        zoomImage(1.25);
        return;
      }

      if (commandKey && key === "-") {
        event.preventDefault();
        zoomImage(0.8);
        return;
      }

      if (commandKey && key === "=") {
        event.preventDefault();
        resetImageZoom();
        return;
      }

      if (commandKey && key === "f") {
        event.preventDefault();
        fitImageToCanvas();
        return;
      }

      if (editableTarget) return;

      if (key === "delete" || key === "backspace") {
        event.preventDefault();
        deleteSelectedObject();
        return;
      }

      if (key === "w") {
        event.preventDefault();
        setMode("bbox");
        return;
      }

      if (key === "a") {
        event.preventDefault();
        goToImage(-1);
        return;
      }

      if (key === "d") {
        event.preventDefault();
        goToImage(1);
        return;
      }

      if (key === " ") {
        event.preventDefault();
        setAnnotationStatus("已验证");
        setSaveMessage("已按 LabelImg 快捷键标记为已验证");
        return;
      }

      const arrowMove: Record<string, { dx: number; dy: number }> = {
        arrowup: { dx: 0, dy: -1 },
        arrowright: { dx: 1, dy: 0 },
        arrowdown: { dx: 0, dy: 1 },
        arrowleft: { dx: -1, dy: 0 },
      };
      const move = arrowMove[key];
      if (move) {
        event.preventDefault();
        const step = event.shiftKey ? 10 : 1;
        moveSelectedObject(move.dx * step, move.dy * step);
      }
    }
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [selectedObjectId, selectedObject, objects, revision, activeImageId, imageId, activeImage, dirty, images, saveAndNext, canvasSize]);

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

  function eventClientPosition(event: MouseEvent<Element> | WheelEvent<Element>) {
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

  function pointFromEvent(event: MouseEvent<HTMLCanvasElement>) {
    const width = activeImage?.width || 640;
    const height = activeImage?.height || 480;
    const rect = event.currentTarget.getBoundingClientRect();
    const { clientX, clientY } = eventClientPosition(event);
    return {
      x: clamp(((clientX - rect.left) - viewport.offsetX) / viewport.scale, 0, width),
      y: clamp(((clientY - rect.top) - viewport.offsetY) / viewport.scale, 0, height),
    };
  }

  function canvasLocalPoint(event: MouseEvent<HTMLCanvasElement> | WheelEvent<HTMLCanvasElement>) {
    const rect = event.currentTarget.getBoundingClientRect();
    const { clientX, clientY } = eventClientPosition(event);
    return {
      x: clientX - rect.left,
      y: clientY - rect.top,
    };
  }

  function hitTestObject(point: { x: number; y: number }) {
    const handleRadius = Math.max(5 / viewport.scale, 3);
    for (const object of [...objects].reverse()) {
      if (!object.bbox) continue;
      if (object.id === selectedObjectId) {
        const handle = bboxHandles(object.bbox).find(
          (item) => Math.abs(point.x - item.x) <= handleRadius && Math.abs(point.y - item.y) <= handleRadius,
        );
        if (handle) return { object, handle: handle.handle };
      }
      const box = object.bbox;
      if (point.x >= box.x && point.x <= box.x + box.width && point.y >= box.y && point.y <= box.y + box.height) {
        return { object, handle: null };
      }
    }
    return null;
  }

  function beginCanvasInteraction(event: MouseEvent<HTMLCanvasElement>) {
    if (event.button !== 0) return;
    const localPoint = canvasLocalPoint(event);
    if (mode === "pan" || event.altKey) {
      setPanState({ start: localPoint, original: viewport });
      return;
    }

    const point = pointFromEvent(event);
    const target = hitTestObject(point);
    if (target?.object.bbox) {
      setMode("select");
      setSelectedObjectId(target.object.id);
      setDragState({
        objectId: target.object.id,
        kind: target.handle ? "resize" : "move",
        handle: target.handle ?? undefined,
        start: point,
        original: target.object.bbox,
      });
      return;
    }

    if (mode !== "bbox") {
      setSelectedObjectId(null);
      return;
    }

    setDraftBox({ start: point, end: point });
  }

  function updateCanvasInteraction(event: MouseEvent<HTMLCanvasElement>) {
    if (panState) {
      const point = canvasLocalPoint(event);
      setViewport({
        ...panState.original,
        offsetX: panState.original.offsetX + point.x - panState.start.x,
        offsetY: panState.original.offsetY + point.y - panState.start.y,
      });
      return;
    }

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

  function finishCanvasInteraction(event: MouseEvent<HTMLCanvasElement>) {
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
    setPanState(null);
  }

  function zoomImage(factor: number) {
    setViewport((current) => zoomCanvasViewport(current, current.scale * factor, {
      x: canvasSize.width / 2,
      y: canvasSize.height / 2,
    }));
  }

  function fitImageToCanvas() {
    if (!activeImage) return;
    setViewport(fitCanvasViewport(activeImage.width, activeImage.height, canvasSize.width, canvasSize.height));
  }

  function resetImageZoom() {
    setViewport({
      scale: 1,
      offsetX: (canvasSize.width - (activeImage?.width || 640)) / 2,
      offsetY: (canvasSize.height - (activeImage?.height || 480)) / 2,
    });
  }

  function handleCanvasWheel(event: WheelEvent<HTMLCanvasElement>) {
    event.preventDefault();
    const localPoint = canvasLocalPoint(event);
    const factor = event.deltaY > 0 ? 0.9 : 1.1;
    setViewport((current) => zoomCanvasViewport(current, current.scale * factor, localPoint));
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

  function moveSelectedObject(dx: number, dy: number) {
    if (!selectedObjectId || !activeImage) return;
    setDirty(true);
    setObjects((current) =>
      current.map((object) => {
        if (object.id !== selectedObjectId || !object.bbox) return object;
        return {
          ...object,
          bbox: {
            ...object.bbox,
            x: Number(clamp(object.bbox.x + dx, 0, activeImage.width - object.bbox.width).toFixed(1)),
            y: Number(clamp(object.bbox.y + dy, 0, activeImage.height - object.bbox.height).toFixed(1)),
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
          { label: "平移", icon: Move, mode: "pan" as ToolMode },
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
        <div className="annotation-toolbar" data-tauri-drag-region onMouseDown={beginDesktopWindowDrag}>
          <div>
            <h1>标注工作台</h1>
            <span>{projectId} / {activeImage?.fileName ?? "加载图片"} / {annotationStatus}</span>
          </div>
          <div className="annotation-actions" data-no-drag>
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
            {showWindowControls ? (
              <span className="annotation-window-controls">
                <button aria-label="最小化标注工作台" type="button" onClick={() => runDesktopCommand("minimize_window")}>
                  <Minus size={16} />
                </button>
                <button aria-label="最大化标注工作台" type="button" onClick={() => runDesktopCommand("toggle_maximize_window")}>
                  <Maximize2 size={16} />
                </button>
                <button aria-label="关闭标注工作台" type="button" onClick={() => runDesktopCommand("close_window")}>
                  <X size={16} />
                </button>
              </span>
            ) : null}
          </div>
        </div>
        <div className="image-stage">
          <div className="canvas-stage-shell" ref={canvasShellRef}>
            <canvas
              aria-label={activeImage ? `${activeImage.fileName} 标注画布` : "标注画布"}
              className={`annotation-canvas ${mode === "bbox" ? "drawing" : ""} ${mode === "pan" || panState ? "panning" : ""}`}
              data-testid="annotation-canvas"
              height={canvasSize.height}
              onMouseDown={beginCanvasInteraction}
              onMouseMove={updateCanvasInteraction}
              onMouseUp={finishCanvasInteraction}
              onMouseLeave={finishCanvasInteraction}
              onWheel={handleCanvasWheel}
              ref={canvasRef}
              width={canvasSize.width}
            />
            <div className="canvas-controls" data-no-drag>
              <button aria-label="缩小图像" type="button" onClick={() => zoomImage(0.8)}>
                <ZoomOut size={16} />
              </button>
              <button aria-label="图像适配窗口" type="button" onClick={fitImageToCanvas}>
                适配
              </button>
              <button aria-label="重置为原始大小" type="button" onClick={resetImageZoom}>
                1:1
              </button>
              <button aria-label="放大图像" type="button" onClick={() => zoomImage(1.25)}>
                <ZoomIn size={16} />
              </button>
              <span className="zoom-readout">{Math.round(viewport.scale * 100)}%</span>
            </div>
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
              ref={selectedLabelInputRef}
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
  const [projectTopbarContext, setProjectTopbarContext] = useState<ProjectTopbarContext | null>(null);
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
    setProjectTopbarContext(null);
  }, [route.name === "project" ? route.projectId : route.name]);

  const updateProjectTopbarContext = useCallback((context: ProjectTopbarContext | null) => {
    setProjectTopbarContext(context);
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

  async function handleImportFiles(projectId: string, sourcePaths: string[]) {
    await importFiles(projectId, sourcePaths);
    setDataSubmitOpen(false);
    await refreshDatasets({ autoDownload: false });
  }

  async function handleOpenLocalDataset(sourcePath: string, datasetType: string) {
    const project = await openLocalDataset(sourcePath, datasetType);
    setDataSubmitOpen(false);
    await refreshDatasets({ autoDownload: false });
    navigate(`#/datasets/${project.id}`);
  }

  async function handlePickDataSource(selectionType: "folder" | "files") {
    return pickDataSource(selectionType);
  }

  async function handleAnalyzeDataSource(sourcePaths: string[]) {
    return analyzeDataSource(sourcePaths);
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

  function openProjectTab(projectId: string, tab: ProjectTab) {
    navigate(`#/datasets/${projectId}/${encodeURIComponent(tab)}`);
  }

  function openProjectAnnotationRoute(projectId: string, imageId?: string) {
    navigate(`#/annotate/${projectId}/${imageId ?? ""}`);
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

  if (route.name === "annotate") {
    return <AnnotationWorkspace imageId={route.imageId} projectId={route.projectId} showWindowControls={backendConnection.mode === "tauri"} />;
  }

  return (
    <div className="app-shell">
      <TopBar
        activeProjectId={route.name === "project" ? route.projectId : undefined}
        activeProjectContext={route.name === "project" ? projectTopbarContext : null}
        backendConnection={backendConnection}
        onBackendTasks={openBackendTasks}
        onCreateDataset={() => setCreateDialogOpen(true)}
        onDataSubmit={() => setDataSubmitOpen(true)}
        onDatasets={() => navigate("#/datasets")}
        onProjectAnnotate={openProjectAnnotationRoute}
        onProjectOpenWindow={openProjectWindow}
        onProjectTab={openProjectTab}
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
        {route.name === "project" && (
          <ProjectWorkspace
            onProjectContextChange={updateProjectTopbarContext}
            projectId={route.projectId}
            routeTab={route.tab}
          />
        )}
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
          onAnalyzeSource={handleAnalyzeDataSource}
          onCancel={() => setDataSubmitOpen(false)}
          onDownload={handleSubmitDownload}
          onImportFiles={handleImportFiles}
          onImportImages={handleImportImages}
          onImportYolo={handleImportYolo}
          onOpenLocal={handleOpenLocalDataset}
          onPickSource={handlePickDataSource}
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

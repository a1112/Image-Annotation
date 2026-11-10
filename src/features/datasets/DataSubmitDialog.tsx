import { Database, Files, FolderOpen, Upload, X } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import type { DragEvent } from "react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import type { BuiltinDataset, DataSourceAnalysis, DataSourceTreeNode, DatasetProject } from "../../types/domain";

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

type DataImportAction = "open-local" | "copy-images" | "copy-yolo";

export function DataSubmitDialog({
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
  initialDatasetId,
}: {
  datasets: BuiltinDataset[];
  projects: DatasetProject[];
  initialDatasetId?: string;
  onCancel: () => void;
  onAnalyzeSource: (sourcePaths: string[]) => Promise<DataSourceAnalysis>;
  onDownload: (datasetKey: string) => void;
  onImportFiles: (projectId: string, sourcePaths: string[]) => void;
  onImportImages: (projectId: string, sourcePath: string) => void;
  onImportYolo: (projectId: string, sourcePath: string) => void;
  onOpenLocal: (sourcePath: string, datasetType: string) => void;
  onPickSource: (selectionType: "folder" | "files") => Promise<string[] | null>;
}) {
  const [projectId, setProjectId] = useState(initialDatasetId ?? projects[0]?.id ?? "");
  const [analysis, setAnalysis] = useState<DataSourceAnalysis | null>(null);
  const [datasetType, setDatasetType] = useState<
    "voc-detect" | "yolo-detect" | "yolo-seg" | "image-classification" | "image-directory"
  >("voc-detect");
  const [importAction, setImportAction] = useState<DataImportAction>("open-local");
  const [analyzeState, setAnalyzeState] = useState<"idle" | "loading" | "error">("idle");
  const [message, setMessage] = useState<string | null>(null);
  const [dropActive, setDropActive] = useState(false);

  useEffect(() => {
    if (initialDatasetId && projects.some((item) => item.id === initialDatasetId)) setProjectId(initialDatasetId);
    else if (!projects.some((item) => item.id === projectId)) setProjectId(projects[0]?.id ?? "");
  }, [initialDatasetId, projects, projectId]);

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
                    {matchesYoloFormat(analysis.detectedFormat) ? <option value="copy-yolo">复制 YOLO 数据集到目标项目</option> : null}
                  </select>
                </label>
                <label>
                  <span>数据类型</span>
                  <select value={datasetType} onChange={(event) => setDatasetType(event.target.value as typeof datasetType)}>
                    <option value="voc-detect">Pascal VOC BBox XML</option>
                    <option value="yolo-detect">YOLO BBox TXT</option>
                    <option value="yolo-seg">YOLO Polygon TXT</option>
                    <option value="image-classification">图像分类目录</option>
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

function normalizeDetectedDatasetType(
  format: DataSourceAnalysis["detectedFormat"],
): "voc-detect" | "yolo-detect" | "yolo-seg" | "image-classification" | "image-directory" {
  if (
    format === "yolo-detect"
    || format === "yolo-seg"
    || format === "voc-detect"
    || format === "image-classification"
  ) {
    return format;
  }
  return "image-directory";
}

function matchesYoloFormat(format: DataSourceAnalysis["detectedFormat"]) {
  return format === "yolo-detect" || format === "yolo-seg";
}

function defaultImportAction(analysis: DataSourceAnalysis): DataImportAction {
  if (analysis.recommendedAction === "open-local") return "open-local";
  return "copy-images";
}

function formatDatasetFormat(format: DataSourceAnalysis["detectedFormat"]) {
  if (format === "voc-detect") return "Pascal VOC BBox";
  if (format === "yolo-detect") return "YOLO BBox";
  if (format === "yolo-seg") return "YOLO Polygon";
  if (format === "image-classification") return "图像分类目录";
  if (format === "image-directory") return "图片目录";
  return "未识别";
}

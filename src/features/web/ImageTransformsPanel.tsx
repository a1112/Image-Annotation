import { Download, History, RefreshCw, Wand2 } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import {
  createWebDataProjectTransform,
  getWebImageTransform,
  getWebImageTransformDownloadUrl,
  getWebImageContentUrl,
  listWebDataProjectImages,
  listWebDataProjectTransforms,
  parseWebDatasetIdentity,
  resolveWebResourceUrl,
  type WebDatasetImage,
  type WebImageTransformRequest,
  type WebImageTransformTask,
} from "../../api/web";
import "./web.css";

type TransformHistoryItem = {
  id: string;
  sourceFileName: string;
  task: WebImageTransformTask;
};

const terminalStatuses = new Set(["succeeded", "failed", "error", "cancelled"]);

function transformError(cause: unknown, fallback: string): string {
  return cause instanceof Error ? cause.message : fallback;
}

function statusLabel(status: string): string {
  switch (status) {
    case "succeeded": return "已完成";
    case "failed":
    case "error": return "失败";
    case "cancelled": return "已取消";
    case "queued": return "排队中";
    case "running":
    case "processing": return "处理中";
    default: return status || "等待中";
  }
}

function randomOperationId(): string {
  const uuid = typeof window !== "undefined" && typeof window.crypto?.randomUUID === "function"
    ? window.crypto.randomUUID()
    : `${Date.now()}-${Math.random().toString(36).slice(2)}`;
  return `web-transform-${uuid}`;
}

function numberOrUndefined(value: string, minimum = 1): number | undefined {
  if (!value.trim()) return undefined;
  const parsed = Number(value);
  return Number.isFinite(parsed) && parsed >= minimum ? Math.round(parsed) : undefined;
}

function sourceFileNameForTask(task: WebImageTransformTask, images: WebDatasetImage[]): string {
  return images.find((image) => image.imageHash === task.sourceHash)?.fileName ?? "原图";
}

function transformDownloadUrl(task: WebImageTransformTask): string {
  return task.downloadUrl ? resolveWebResourceUrl(task.downloadUrl) : getWebImageTransformDownloadUrl(task.taskId);
}

export function ImageTransformsPanel({ datasetId, imageId, accountId }: { datasetId: string; imageId?: string; accountId?: string }) {
  const identity = useMemo(() => parseWebDatasetIdentity(datasetId), [datasetId]);
  const [images, setImages] = useState<WebDatasetImage[]>([]);
  const [selectedImageId, setSelectedImageId] = useState(imageId ?? "");
  const [format, setFormat] = useState<WebImageTransformRequest["format"]>("png");
  const [width, setWidth] = useState("");
  const [height, setHeight] = useState("");
  const [cropX, setCropX] = useState("");
  const [cropY, setCropY] = useState("");
  const [cropWidth, setCropWidth] = useState("");
  const [cropHeight, setCropHeight] = useState("");
  const [rotate, setRotate] = useState<WebImageTransformRequest["rotate"]>(0);
  const [flip, setFlip] = useState<WebImageTransformRequest["flip"]>("none");
  const [quality, setQuality] = useState("85");
  const [loadingImages, setLoadingImages] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const [task, setTask] = useState<WebImageTransformTask | null>(null);
  const [history, setHistory] = useState<TransformHistoryItem[]>([]);
  const [loadingHistory, setLoadingHistory] = useState(false);
  const [collapsed, setCollapsed] = useState(true);
  const requestRef = useRef(0);
  const imagesRef = useRef<WebDatasetImage[]>([]);

  const selectedImage = images.find((item) => item.id === selectedImageId) ?? null;

  useEffect(() => {
    if (!identity) {
      requestRef.current += 1;
      imagesRef.current = [];
      setImages([]);
      setSelectedImageId("");
      setTask(null);
      setHistory([]);
      setBusy(false);
      setLoadingImages(false);
      setLoadingHistory(false);
      setError(null);
      setMessage(null);
      return;
    }
    const request = ++requestRef.current;
    imagesRef.current = [];
    setImages([]);
    setHistory([]);
    setTask(null);
    setBusy(false);
    setCollapsed(true);
    setLoadingImages(true);
    setLoadingHistory(true);
    setError(null);
    setMessage(null);
    void listWebDataProjectImages(datasetId, { limit: 500 })
      .then((result) => {
        if (requestRef.current !== request) return;
        // Do not expose a transform control until the server has supplied the
        // scoped source hash. The browser must never invent one locally.
        const transformable = result.items.filter((item) => typeof item.imageHash === "string" && item.imageHash.length > 0);
        imagesRef.current = transformable;
        setImages(transformable);
        setHistory((current) => current.map((item) => ({
          ...item,
          sourceFileName: sourceFileNameForTask(item.task, transformable),
        })));
        setSelectedImageId((current) => {
          if (current && transformable.some((item) => item.id === current)) return current;
          if (imageId && transformable.some((item) => item.id === imageId)) return imageId;
          return transformable[0]?.id ?? "";
        });
      })
      .catch((cause) => {
        if (requestRef.current === request) setError(transformError(cause, "读取可转换图像失败。"));
      })
      .finally(() => {
        if (requestRef.current === request) setLoadingImages(false);
      });
    void listWebDataProjectTransforms(identity.dataProjectId, identity.datasetKey, { limit: 50 })
      .then((tasks) => {
        if (requestRef.current !== request) return;
        const loaded = tasks.map((next) => ({
          id: next.taskId,
          sourceFileName: sourceFileNameForTask(next, imagesRef.current),
          task: next,
        }));
        setHistory((current) => {
          const currentById = new Map(current.map((item) => [item.id, item]));
          const loadedIds = new Set(loaded.map((item) => item.id));
          const durable = loaded.map((item) => currentById.get(item.id) ?? item);
          const localOnly = current.filter((item) => !loadedIds.has(item.id));
          return [...localOnly, ...durable];
        });
      })
      .catch((cause) => {
        if (requestRef.current === request) setError(transformError(cause, "读取转换历史失败。"));
      })
      .finally(() => {
        if (requestRef.current === request) setLoadingHistory(false);
      });
    return () => {
      if (requestRef.current === request) requestRef.current += 1;
      imagesRef.current = [];
    };
  }, [accountId, datasetId, identity, imageId]);

  function updateHistory(next: WebImageTransformTask, sourceFileName: string) {
    setHistory((current) => {
      const existing = current.find((item) => item.id === next.taskId);
      if (existing) return current.map((item) => item.id === next.taskId ? { ...item, task: next } : item);
      return [{ id: next.taskId, sourceFileName, task: next }, ...current];
    });
  }

  async function pollTask(taskId: string, sourceFileName: string, request: number): Promise<WebImageTransformTask | null> {
    let latest = await getWebImageTransform(taskId);
    if (requestRef.current !== request) return null;
    setTask(latest);
    updateHistory(latest, sourceFileName);
    for (let attempt = 0; attempt < 90 && !terminalStatuses.has(latest.status); attempt += 1) {
      await new Promise<void>((resolve) => window.setTimeout(resolve, 700));
      if (requestRef.current !== request) return null;
      latest = await getWebImageTransform(taskId);
      if (requestRef.current !== request) return null;
      setTask(latest);
      updateHistory(latest, sourceFileName);
    }
    if (!terminalStatuses.has(latest.status)) throw new Error("转换任务轮询超时，请稍后在历史记录中重试查看。");
    return latest;
  }

  async function submitTransform() {
    if (!identity || !selectedImage) {
      setError("请选择要转换的图像。");
      return;
    }
    if (!selectedImage.imageHash) {
      setError("图像信息不完整，请刷新后重试。");
      return;
    }
    const cropValues = [cropX, cropY, cropWidth, cropHeight];
    const hasCrop = cropValues.some((value) => value.trim());
    let crop: WebImageTransformRequest["crop"];
    if (hasCrop) {
      const numbers = [
        numberOrUndefined(cropX, 0),
        numberOrUndefined(cropY, 0),
        numberOrUndefined(cropWidth),
        numberOrUndefined(cropHeight),
      ];
      if (numbers.some((value) => value === undefined)) {
        setError("裁剪区域需要填写有效的 x、y、宽度和高度。");
        return;
      }
      crop = { x: numbers[0]!, y: numbers[1]!, width: numbers[2]!, height: numbers[3]! };
    }
    const parsedQuality = Number(quality);
    if (!Number.isFinite(parsedQuality) || parsedQuality < 1 || parsedQuality > 100) {
      setError("质量需要在 1 到 100 之间。");
      return;
    }
    const request: WebImageTransformRequest = {
      operationId: randomOperationId(),
      sourceHash: selectedImage.imageHash,
      datasetKey: identity.datasetKey,
      format,
      rotate,
      flip,
      quality: Math.round(parsedQuality),
    };
    const parsedWidth = numberOrUndefined(width);
    const parsedHeight = numberOrUndefined(height);
    if (width.trim() && parsedWidth === undefined) {
      setError("缩放宽度需要是正整数。");
      return;
    }
    if (height.trim() && parsedHeight === undefined) {
      setError("缩放高度需要是正整数。");
      return;
    }
    if (parsedWidth !== undefined) request.width = parsedWidth;
    if (parsedHeight !== undefined) request.height = parsedHeight;
    if (crop) request.crop = crop;
    setBusy(true);
    setError(null);
    setMessage(null);
    const requestGeneration = requestRef.current;
    try {
      const ref = await createWebDataProjectTransform(identity.dataProjectId, request);
      if (requestRef.current !== requestGeneration) return;
      const initial: WebImageTransformTask = {
        taskId: ref.taskId,
        operationId: request.operationId,
        sourceHash: request.sourceHash,
        sourceDatasetKey: request.datasetKey,
        format: request.format,
        status: ref.status,
      };
      setTask(initial);
      updateHistory(initial, selectedImage.fileName);
      const completed = await pollTask(ref.taskId, selectedImage.fileName, requestGeneration);
      if (!completed || requestRef.current !== requestGeneration) return;
      if (completed.status === "succeeded") {
        setMessage("转换完成，原图和标签已保留。");
      } else {
        setError(completed.errorMessage || `转换${statusLabel(completed.status)}。`);
      }
    } catch (cause) {
      if (requestRef.current === requestGeneration) setError(transformError(cause, "提交图像转换失败。"));
    } finally {
      if (requestRef.current === requestGeneration) setBusy(false);
    }
  }

  if (!identity) {
    return <section className={`web-transform-panel ${collapsed ? "is-collapsed" : ""}`} aria-label="图像转换">
      <div className="web-transform-header">
        <div><h2><Wand2 size={17} />图像转换</h2>{!collapsed ? <p>生成新文件，保留原图和标签。</p> : null}</div>
        <button className="web-transform-toggle" type="button" aria-expanded={!collapsed} onClick={() => setCollapsed((value) => !value)}>{collapsed ? "展开转换面板" : "收起转换面板"}</button>
      </div>
      {!collapsed ? <div className="web-transform-panel-body"><p className="web-transform-muted">请先将数据集加入项目。</p></div> : null}
    </section>;
  }

  const downloadUrl = task ? transformDownloadUrl(task) : null;

  return (
    <section className={`web-transform-panel ${collapsed ? "is-collapsed" : ""}`} aria-label="图像转换">
      <div className="web-transform-header"><div><h2><Wand2 size={17} />图像转换</h2>{!collapsed ? <p>生成新文件，保留原图和标签。</p> : null}</div><button className="web-transform-toggle" type="button" aria-expanded={!collapsed} aria-controls="web-transform-panel-body" onClick={() => setCollapsed((value) => !value)}>{collapsed ? "展开转换面板" : "收起转换面板"}</button></div>
      {!collapsed ? <div className="web-transform-panel-body" id="web-transform-panel-body">
        {loadingImages ? <p className="web-transform-muted">正在加载图像…</p> : images.length === 0 ? <p className="web-transform-muted">暂无可转换图像。</p> : (
          <>
            <div className="web-transform-form">
              <label><span>转换图像</span><select value={selectedImageId} onChange={(event) => setSelectedImageId(event.target.value)}>{images.map((image) => <option key={image.id} value={image.id}>{image.fileName}</option>)}</select></label>
              <label><span>输出格式</span><select value={format} onChange={(event) => setFormat(event.target.value as WebImageTransformRequest["format"])}><option value="png">PNG</option><option value="jpeg">JPEG</option><option value="webp">WebP</option><option value="tiff">TIFF</option></select></label>
              <label><span>宽度（可选）</span><input inputMode="numeric" min="1" placeholder="保持原宽" value={width} onChange={(event) => setWidth(event.target.value)} /></label>
              <label><span>高度（可选）</span><input inputMode="numeric" min="1" placeholder="保持原高" value={height} onChange={(event) => setHeight(event.target.value)} /></label>
              <label><span>旋转</span><select value={rotate} onChange={(event) => setRotate(Number(event.target.value) as WebImageTransformRequest["rotate"])}><option value={0}>不旋转</option><option value={90}>90°</option><option value={180}>180°</option><option value={270}>270°</option></select></label>
              <label><span>翻转</span><select value={flip} onChange={(event) => setFlip(event.target.value as WebImageTransformRequest["flip"])}><option value="none">不翻转</option><option value="horizontal">水平翻转</option><option value="vertical">垂直翻转</option></select></label>
              <label><span>质量（1–100）</span><input inputMode="numeric" min="1" max="100" value={quality} onChange={(event) => setQuality(event.target.value)} /></label>
              <fieldset><legend>裁剪区域（可选）</legend><div className="web-transform-crop-grid"><input aria-label="裁剪 x" inputMode="numeric" placeholder="x" value={cropX} onChange={(event) => setCropX(event.target.value)} /><input aria-label="裁剪 y" inputMode="numeric" placeholder="y" value={cropY} onChange={(event) => setCropY(event.target.value)} /><input aria-label="裁剪宽度" inputMode="numeric" placeholder="宽" value={cropWidth} onChange={(event) => setCropWidth(event.target.value)} /><input aria-label="裁剪高度" inputMode="numeric" placeholder="高" value={cropHeight} onChange={(event) => setCropHeight(event.target.value)} /></div></fieldset>
            </div>
            {selectedImage ? <div className="web-transform-source-row"><span>源文件：{selectedImage.fileName}</span><span className="web-transform-source-actions"><a href={selectedImage.imageUrl ?? getWebImageContentUrl(datasetId, selectedImage.id)} download target="_blank" rel="noreferrer"><Download size={13} />下载原图</a>{selectedImage.annotationUrl ? <a href={selectedImage.annotationUrl} download target="_blank" rel="noreferrer"><Download size={13} />下载标签</a> : null}</span></div> : null}
            <button className="primary web-transform-submit" disabled={busy || !selectedImage?.imageHash} type="button" onClick={() => void submitTransform()}><Wand2 size={15} />{busy ? "转换处理中…" : "开始转换"}</button>
          </>
        )}
        {error ? <div className="web-alert error" role="alert">{error}</div> : null}
        {message ? <div className="web-alert success" role="status">{message}</div> : null}
        {task ? <div className={`web-transform-task ${task.status}`}><strong>{statusLabel(task.status)}</strong><span>{task.outputMimeType || task.format || "输出"}{task.outputByteSize ? ` · ${task.outputByteSize.toLocaleString("zh-CN")} 字节` : ""}</span>{task.status === "succeeded" && downloadUrl ? <a href={downloadUrl} download target="_blank" rel="noreferrer"><Download size={14} />下载新图像</a> : null}</div> : null}
        <details className="web-transform-history" open={history.length > 0 || loadingHistory}><summary><History size={15} />转换历史（{history.length}）</summary>{loadingHistory ? <p className="web-transform-muted">正在读取转换历史…</p> : history.length ? <div>{history.map((item) => <article key={item.id}><span><strong>{item.sourceFileName}</strong><small>{statusLabel(item.task.status)}</small></span>{item.task.status === "succeeded" ? <a href={transformDownloadUrl(item.task)} download target="_blank" rel="noreferrer"><Download size={13} />下载</a> : <button type="button" disabled={busy} onClick={() => { setTask(item.task); setMessage(null); setError(null); }}><RefreshCw size={13} />查看</button>}</article>)}</div> : <p className="web-transform-muted">暂无转换记录。</p>}</details>
      </div> : null}
    </section>
  );
}

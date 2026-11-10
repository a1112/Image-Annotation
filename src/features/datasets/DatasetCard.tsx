import { MoreHorizontal } from "lucide-react";
import { useEffect, useState } from "react";
import type { KeyboardEvent, ReactNode } from "react";
import type { DatasetImage, DatasetProject } from "../../types/domain";
import "./dataset-card.css";
import { useImageAssetUrls } from "../shared/useImageAssetUrls";

function formatNumber(value: number) {
  return value.toLocaleString("zh-CN");
}

function annotationStatusLabel(status: string): string {
  const normalized = status.trim().toLocaleLowerCase().replace(/[\s_-]+/g, "");
  const labels: Record<string, string> = {
    raw: "未处理",
    unannotated: "未处理",
    inprogress: "处理中",
    draft: "标注中",
    pendingreview: "待审核",
    submitted: "待审核",
    approved: "已通过",
    rejected: "已驳回",
    active: "已导入",
    ready: "已导入",
  };
  return labels[normalized] ?? (status.trim() || "未处理");
}

export function DatasetCard({
  dataset,
  previewImages,
  onOpen,
  onAnnotate,
  onOpenWindow,
  compact = false,
  canAnnotate = true,
  loadPreviewImages,
  compactMenu,
}: {
  dataset: DatasetProject;
  previewImages: DatasetImage[];
  onOpen: () => void;
  onAnnotate: () => void;
  onOpenWindow?: () => void;
  compact?: boolean;
  canAnnotate?: boolean;
  loadPreviewImages?: (datasetId: string) => Promise<DatasetImage[]>;
  compactMenu?: ReactNode;
}) {
  const [cardNode, setCardNode] = useState<HTMLElement | null>(null);
  const [loadedPreviewImages, setLoadedPreviewImages] = useState<DatasetImage[]>([]);
  useEffect(() => {
    if (!compact || previewImages.length > 0 || !loadPreviewImages || !cardNode) return;
    let cancelled = false;
    const load = async () => {
      try {
        const loaded = await loadPreviewImages(dataset.id);
        if (!cancelled && Array.isArray(loaded)) setLoadedPreviewImages(loaded.slice(0, 1));
      } catch {
        // A missing preview should keep the card usable; the source remains
        // available through the dataset actions and can be retried on refresh.
      }
    };
    if (typeof IntersectionObserver === "undefined") {
      void load();
      return () => { cancelled = true; };
    }
    const observer = new IntersectionObserver((entries) => {
      if (entries.some((entry) => entry.isIntersecting)) {
        observer.disconnect();
        void load();
      }
    }, { rootMargin: "120px" });
    observer.observe(cardNode);
    return () => {
      cancelled = true;
      observer.disconnect();
    };
  }, [cardNode, compact, dataset.id, loadPreviewImages, previewImages.length]);
  const effectivePreviewImages = previewImages.length ? previewImages : loadedPreviewImages;
  const previewUrls = useImageAssetUrls(dataset.id, effectivePreviewImages, compact ? 1 : 3);
  const isRemoteCache = dataset.description.trimStart().startsWith("真实 remote:")
    || dataset.tags.some((tag) => tag.trim().toLowerCase() === "format: remote");
  const description = isRemoteCache ? "已缓存的协作项目" : dataset.description;
  const displayTags = isRemoteCache
    ? dataset.tags.filter((tag) => !tag.startsWith("source:") && !tag.startsWith("format:"))
    : dataset.tags;
  const compactStatus = annotationStatusLabel(dataset.status);
  return (
    <article
      aria-label={`${dataset.name} 数据集卡片`}
      className={`dataset-card${compact ? " dataset-card--compact" : ""}`}
      ref={setCardNode}
      onClick={(event) => {
        if (compact && !event.defaultPrevented && !(event.target as HTMLElement).closest("button, input, select, summary, details")) onOpen();
      }}
      onDoubleClick={compact ? undefined : onOpen}
      onKeyDown={(event: KeyboardEvent<HTMLElement>) => { if (event.key === "Enter" && event.target === event.currentTarget) onOpen(); }}
      tabIndex={0}
    >
      <div className="thumbnail-strip" aria-label={`${dataset.name} samples`}>
        {Array.from({ length: compact ? 1 : 3 }, (_, index) => {
          const image = effectivePreviewImages[index];
          return (
            <div className={`sample-thumb traffic-${["a", "b", "c"][index]}`} key={image?.id ?? index}>
              {image && previewUrls[image.id] ? <img draggable={false} alt={`${dataset.name} 预览 ${index + 1}`} src={previewUrls[image.id]} /> : null}
              <span>{index + 1}</span>
            </div>
          );
        })}
      </div>
      <div className="dataset-card__header"><div><h3>{dataset.name}</h3></div><span className="quality-chip ready">{dataset.status === "active" || dataset.status === "ready" ? "已导入" : dataset.status}</span></div>
      <div className="metric-grid">
        {compact ? <>
          <span className="dataset-card-compact-metric"><strong>{formatNumber(dataset.imageCount)}</strong><small>图像</small></span>
          <span className="dataset-card-compact-metric dataset-card-status-tag"><strong>{compactStatus}</strong></span>
          <span className="dataset-card-compact-metric dataset-card-progress-tag"><strong>{dataset.annotatedPercent}%</strong><small>已标注</small></span>
        </> : <>
          <span><strong>{formatNumber(dataset.imageCount)}</strong>图片</span>
          <span><strong>{dataset.annotatedPercent}%</strong>已标注</span>
          <span><strong>{formatNumber(dataset.reviewCount)}</strong>待审核</span>
          <span><strong>{dataset.classCount}</strong>类别</span>
        </>}
      </div>
      <div className="progress-bar" aria-label={`${dataset.name} 标注进度 ${dataset.annotatedPercent}%`}><span style={{ width: `${dataset.annotatedPercent}%` }} /></div>
      {compact ? <div className="card-footer"><div><button type="button" onClick={onOpen}>打开</button><button className="primary" type="button" disabled={!canAnnotate} title={canAnnotate ? undefined : "只读项目"} onClick={onAnnotate}>开始标注</button>{compactMenu ? <details className="dataset-card-menu"><summary role="button" aria-label={`${dataset.name} 管理菜单`} title="管理"><MoreHorizontal size={16} /></summary><div className="dataset-card-menu-content" role="menu">{compactMenu}</div></details> : null}</div></div> : <div className="card-footer">
        <details className="dataset-card-details">
          <summary>详情</summary>
          <div className="dataset-card-details-body">
            {description ? <p>{description}</p> : null}
            <div className="type-row">
              {dataset.tags.some((tag) => tag.includes("local-linked")) ? <span className="type-chip local">本机链接</span> : null}
              {dataset.annotationTypes.map((type) => <span className="type-chip" key={type}>{type}</span>)}
            </div>
            {displayTags.length ? <div className="tag-list compact">{displayTags.slice(0, 3).map((tag) => <span key={tag}>{tag}</span>)}</div> : null}
            <span>{dataset.tagGroupCount} 个保存分组</span>
          </div>
        </details>
        <div><button type="button" onClick={onOpen}>打开</button><button className="primary" type="button" onClick={onAnnotate}>开始标注</button>{onOpenWindow ? <button className="dataset-window-action" type="button" onClick={onOpenWindow}>独立窗口标注</button> : null}</div>
      </div>}
    </article>
  );
}

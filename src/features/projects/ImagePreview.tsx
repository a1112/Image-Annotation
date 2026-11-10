import { Tags, X } from "lucide-react";
import { useEffect, useState } from "react";
import { getImageAnnotations } from "../../api/tauri";
import type { AnnotationObject, DatasetImage } from "../../types/domain";
import { annotationColor } from "../annotation/geometry";
import type { CanvasSize } from "../annotation/types";
import { displayStatusLabel } from "./statusLabels";

export function useImageAnnotations(projectId: string | undefined, images: DatasetImage[], limit = 12) {
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

export function ThumbnailAnnotationOverlay({
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
          style={{
            fill: `${annotationColor(object.label)}20`,
            stroke: annotationColor(object.label),
          }}
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
          style={{
            fill: `${annotationColor(object.label)}26`,
            stroke: annotationColor(object.label),
          }}
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
    <div className="preview-annotation-layer">
      <svg
        aria-label={`${image.fileName} 标注预览`}
        className="preview-annotations"
        preserveAspectRatio="none"
        viewBox={`0 0 ${image.width || 640} ${image.height || 480}`}
      >
        {visibleObjects.map((object) => object.type === "bbox" && object.bbox ? (
          <rect
            aria-label={`${image.fileName} 标注框 ${object.label}`}
            className="preview-annotation-box"
            key={object.id}
            style={{
              fill: `${annotationColor(object.label)}20`,
              stroke: annotationColor(object.label),
            }}
            x={object.bbox.x}
            y={object.bbox.y}
            width={object.bbox.width}
            height={object.bbox.height}
          />
        ) : object.polygon ? (
          <polygon
            aria-label={`${image.fileName} 多边形标注 ${object.label}`}
            className="preview-annotation-polygon"
            key={object.id}
            points={object.polygon.map((point) => `${point.x},${point.y}`).join(" ")}
            style={{
              fill: `${annotationColor(object.label)}26`,
              stroke: annotationColor(object.label),
            }}
          />
        ) : null)}
      </svg>
      <div className="preview-annotation-labels" aria-hidden="true">
        {visibleObjects.map((object) => {
          const anchor = object.bbox
            ? { x: object.bbox.x, y: object.bbox.y }
            : object.polygon?.[0];
          if (!anchor) return null;
          return (
            <span
              key={object.id}
              style={{
                left: `${Math.max(0, Math.min(100, anchor.x / (image.width || 640) * 100))}%`,
                top: `${Math.max(0, Math.min(96, anchor.y / (image.height || 480) * 100))}%`,
              }}
            >
              {object.label}
            </span>
          );
        })}
      </div>
    </div>
  );
}

export function ImagePreviewDialog({
  image,
  imageUrl,
  images,
  imageUrls,
  objects,
  onAnnotate,
  onClose,
  onSelectImage,
}: {
  image: DatasetImage;
  imageUrl: string | undefined;
  images: DatasetImage[];
  imageUrls: Record<string, string>;
  objects: AnnotationObject[] | undefined;
  onAnnotate: () => void;
  onClose: () => void;
  onSelectImage: (imageId: string) => void;
}) {
  const [renderedSize, setRenderedSize] = useState<CanvasSize | null>(null);
  const [hoveredObjectId, setHoveredObjectId] = useState<string | null>(null);
  const [hiddenLabels, setHiddenLabels] = useState<Set<string>>(new Set());
  const visibleObjects = (objects ?? []).filter(
    (object) => (object.type === "bbox" && object.bbox) || (object.type === "polygon" && object.polygon?.length),
  );
  const enabledObjects = visibleObjects.filter((object) => !hiddenLabels.has(object.label));
  const focusedObject = enabledObjects.find((object) => object.id === hoveredObjectId);
  const classStats = Array.from(
    visibleObjects.reduce((stats, object) => {
      stats.set(object.label, (stats.get(object.label) ?? 0) + 1);
      return stats;
    }, new Map<string, number>()),
  );
  const activeImageIndex = images.findIndex((candidate) => candidate.id === image.id);
  const filmstripStart = Math.max(0, Math.min(activeImageIndex - 3, images.length - 7));
  const filmstripImages = images.slice(filmstripStart, filmstripStart + 7);

  useEffect(() => {
    setRenderedSize(null);
    setHoveredObjectId(null);
    setHiddenLabels(new Set());
  }, [image.id]);

  useEffect(() => {
    function closeOnEscape(event: globalThis.KeyboardEvent) {
      if (event.key === "Escape") onClose();
    }
    window.addEventListener("keydown", closeOnEscape);
    return () => window.removeEventListener("keydown", closeOnEscape);
  }, [onClose]);

  return (
    <div className="modal-backdrop preview-backdrop" onMouseDown={(event) => {
      if (event.target === event.currentTarget) onClose();
    }}>
      <section aria-labelledby="image-preview-title" aria-modal="true" className="dataset-dialog image-preview-dialog" role="dialog">
        <div className="detail-header preview-dialog-header">
          <div>
            <span className="eyebrow">IMAGE INSPECTOR</span>
            <h2 id="image-preview-title">图像预览</h2>
            <strong className="preview-file-name">{image.fileName}</strong>
          </div>
          <div className="preview-header-actions">
            <button className="preview-edit-action primary" type="button" onClick={onAnnotate}>
              <Tags size={15} />
              <span>标记</span>
            </button>
            <span className={`preview-status ${["已标注", "approved", "completed"].includes(image.status) ? "complete" : ""}`}>{displayStatusLabel(image.status)}</span>
            <button aria-label="关闭图像预览" type="button" onClick={onClose}>
              <X size={17} />
            </button>
          </div>
        </div>
        <div className="image-preview-body">
          <div className="image-preview-main">
            <div className="image-preview-stage">
              <div className="image-preview-media">
                {imageUrl ? (
                  <img
                    alt={`预览 ${image.fileName}`}
                    decoding="async"
                    onLoad={(event) => setRenderedSize({
                      width: event.currentTarget.naturalWidth,
                      height: event.currentTarget.naturalHeight,
                    })}
                    src={imageUrl}
                  />
                ) : <div className="road-scene" />}
                <div className={focusedObject ? "preview-annotation-muted" : ""}>
                  <ReadOnlyAnnotationOverlay image={image} objects={enabledObjects} />
                </div>
                {focusedObject ? (
                  <div className="preview-annotation-focus">
                    <ReadOnlyAnnotationOverlay image={image} objects={[focusedObject]} />
                  </div>
                ) : null}
              </div>
            </div>
            <div className="preview-filmstrip" aria-label="图像切换">
              <button
                aria-label="上一张图像"
                className="preview-filmstrip-nav"
                disabled={activeImageIndex <= 0}
                type="button"
                onClick={() => onSelectImage(images[activeImageIndex - 1].id)}
              >
                ‹
              </button>
              <div className="preview-filmstrip-track">
                {filmstripImages.map((candidate) => (
                  <button
                    aria-current={candidate.id === image.id ? "true" : undefined}
                    className={candidate.id === image.id ? "active" : ""}
                    key={candidate.id}
                    title={candidate.fileName}
                    type="button"
                    onClick={() => onSelectImage(candidate.id)}
                  >
                    {imageUrls[candidate.id] ? <img alt="" src={imageUrls[candidate.id]} /> : <span />}
                    <small>{candidate.fileName}</small>
                  </button>
                ))}
              </div>
              <button
                aria-label="下一张图像"
                className="preview-filmstrip-nav"
                disabled={activeImageIndex < 0 || activeImageIndex >= images.length - 1}
                type="button"
                onClick={() => onSelectImage(images[activeImageIndex + 1].id)}
              >
                ›
              </button>
            </div>
          </div>
          <aside className="image-preview-meta" aria-label="图像信息">
            <div className="preview-stat-grid">
              <div><span>尺寸</span><strong>{renderedSize?.width ?? image.width} x {renderedSize?.height ?? image.height}</strong></div>
              <div><span>对象数</span><strong>{visibleObjects.length}</strong></div>
              <div><span>分组</span><strong>{image.split}</strong></div>
              <div><span>质检</span><strong>{image.qaStatus || "未质检"}</strong></div>
            </div>
            <div className="preview-object-panel">
              <div className="preview-object-heading">
                <strong>标注对象</strong>
                <span>{visibleObjects.length}</span>
              </div>
              {visibleObjects.length > 0 ? (
                <div className="preview-object-list">
                  {visibleObjects.slice(0, 8).map((object, index) => (
                    <button
                      className={hiddenLabels.has(object.label) ? "hidden" : ""}
                      key={object.id}
                      type="button"
                      onMouseEnter={() => {
                        if (!hiddenLabels.has(object.label)) setHoveredObjectId(object.id);
                      }}
                      onMouseLeave={() => setHoveredObjectId(null)}
                    >
                      <i style={{ backgroundColor: ["#36a3ff", "#20b486", "#f59e0b", "#d45ce0"][index % 4] }} />
                      <strong>{object.label}</strong>
                      <small>{object.type === "bbox" ? "边界框" : "多边形"}</small>
                    </button>
                  ))}
                </div>
              ) : <p>当前图片没有可见标注对象。</p>}
            </div>
            <div className="preview-class-panel">
              <div className="preview-object-heading">
                <strong>类别显示</strong>
                <span>{classStats.length}</span>
              </div>
              <div className="preview-class-tags">
                {classStats.map(([label, count], index) => {
                  const color = ["#36a3ff", "#20b486", "#f59e0b", "#d45ce0"][index % 4];
                  const hidden = hiddenLabels.has(label);
                  return (
                    <button
                      aria-pressed={!hidden}
                      className={hidden ? "hidden" : ""}
                      key={label}
                      style={{ borderColor: color, color }}
                      type="button"
                      onClick={() => setHiddenLabels((current) => {
                        const next = new Set(current);
                        if (next.has(label)) next.delete(label);
                        else next.add(label);
                        return next;
                      })}
                    >
                      <i style={{ backgroundColor: color }} />
                      <span>{label}</span>
                      <strong>{count}</strong>
                    </button>
                  );
                })}
              </div>
            </div>
            <div className="tag-list compact">{image.tags.map((tag) => <span key={tag}>{tag}</span>)}</div>
          </aside>
        </div>
      </section>
    </div>
  );
}

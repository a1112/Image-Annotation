import { ArrowLeft, ChevronLeft, ChevronRight, MoreHorizontal, BoxSelect, CheckCircle2, ClipboardCheck, Eye, Maximize2, Minus, Move, MousePointer2, Redo2, Save, Square, Undo2, X, ZoomIn, ZoomOut, Zap } from "lucide-react";
import "./annotation-workspace.css";
import { useCallback, useEffect, useMemo, useReducer, useRef, useState } from "react";
import type { MouseEvent, PointerEvent as ReactPointerEvent, WheelEvent } from "react";
import { annotationEditorReducer, initialAnnotationEditorState } from "../../annotation/editor";
import { pinchCanvasViewport } from "../../annotation/pointer";
import {
  cancelAiAnnotation,
  exportAnnotationFile,
  getAiJobStatus,
  getFileAssetUrl,
  getImageAnnotationState,
  getProjectDetail,
  isBackendUnavailableError,
  listProjectImages,
  loadExternalAnnotations,
  pickDataSource,
  runAiAnnotation,
  saveImageAnnotations,
  setImageVerified,
  submitImageAnnotations,
} from "../../api/tauri";
import type { AiOptions } from "../../api/tauri";
import type { AnnotationObject, DatasetImage, Point, ProjectDetail } from "../../types/domain";
import { drawAnnotationCanvas } from "./canvas";
import { bboxHandles, clamp, fitCanvasViewport, isEditableShortcutTarget, normalizeBox, pointDistance, pointInPolygon, pointToSegmentDistance, resizeBox, translateShapePoints, zoomCanvasViewport } from "./geometry";
import type { CanvasLocalEvent, CanvasPointEvent, CanvasSize, CanvasViewport, ToolMode } from "./types";
import { RuntimeStatePanel } from "../shared/RuntimeStatePanel";
import { navigate } from "../../shared/navigation";
import { runDesktopCommand } from "../../platform/desktop";
import { beginDesktopWindowDrag } from "../../platform/desktop";
import { useImageAssetUrls } from "../shared/useImageAssetUrls";

const annotationStatusLabels: Record<string, string> = {
  raw: "待标注", unannotated: "待标注", inProgress: "标注中", draft: "草稿",
  pendingReview: "待审核", submitted: "待审核", approved: "已通过", rejected: "已驳回",
};

type UnsavedDecision = "save" | "discard" | "stay";

type UnsavedPromptState = {
  message: string;
  error?: string;
};

type PendingUnsavedDecision = {
  promise: Promise<UnsavedDecision>;
  resolve: (decision: UnsavedDecision) => void;
};

export function AnnotationWorkspace({
  projectId,
  imageId,
  showWindowControls,
  mobileRuntime,
  registerLeaveGuard,
  onBack,
  onNavigateImage,
}: {
  projectId: string;
  imageId?: string;
  showWindowControls: boolean;
  mobileRuntime?: boolean;
  registerLeaveGuard: (guard: (() => Promise<boolean>) | null) => void;
  onBack?: () => void;
  onNavigateImage?: (imageId: string) => void;
}) {
  const [images, setImages] = useState<DatasetImage[]>([]);
  const listingProjectIdRef = useRef(projectId);
  listingProjectIdRef.current = projectId;
  const hasMoreImagesRef = useRef(false);
  const loadingMoreImagesRef = useRef(false);
  const [workspaceDetail, setWorkspaceDetail] = useState<ProjectDetail | null>(null);
  const [imagesLoaded, setImagesLoaded] = useState(false);
  const [loadError, setLoadError] = useState<{ title: string; message: string } | null>(null);
  const [activeImageId, setActiveImageId] = useState(imageId ?? "");
  const [assetUrl, setAssetUrl] = useState("");
  const [editor, dispatchEditor] = useReducer(annotationEditorReducer, initialAnnotationEditorState);
  const objects = editor.objects;
  const objectsRef = useRef(objects);
  objectsRef.current = objects;
  const editObjects = useCallback(
    (update: (current: AnnotationObject[]) => AnnotationObject[]) => dispatchEditor({ type: "commit", update }),
    [],
  );
  const previewObjects = useCallback(
    (update: (current: AnnotationObject[]) => AnnotationObject[]) => dispatchEditor({ type: "preview", update }),
    [],
  );
  const [revision, setRevision] = useState<string | null>(null);
  const [annotationStatus, setAnnotationStatus] = useState("加载中");
  const [isVerified, setIsVerified] = useState(false);
  const [saveMessage, setSaveMessage] = useState<string | null>(null);
  const [mobileInspectorOpen, setMobileInspectorOpen] = useState(false);
  const [pendingVerification, setPendingVerification] = useState<boolean | null>(null);
  const [annotationExportFormat, setAnnotationExportFormat] = useState("labelme");
  const [aiPanelOpen, setAiPanelOpen] = useState(false);
  const [aiProvider, setAiProvider] = useState<"onnx" | "osam">("onnx");
  const [aiModelPath, setAiModelPath] = useState("");
  const [aiModelName, setAiModelName] = useState("sam2:latest");
  const [aiLayout, setAiLayout] = useState<"yolo8" | "yolo5" | "xyxy">("yolo8");
  const [aiOutputFormat, setAiOutputFormat] = useState<"rectangle" | "polygon" | "mask" | "oriented_rectangle" | "circle">("polygon");
  const [aiPromptType, setAiPromptType] = useState<"points" | "text">("points");
  const [aiPromptClassId, setAiPromptClassId] = useState<number>(0);
  const [aiPromptPoints, setAiPromptPoints] = useState<Array<{ point: Point; label: number }>>([]);
  const [aiTexts, setAiTexts] = useState("");
  const [aiScoreThreshold, setAiScoreThreshold] = useState(0.25);
  const [aiIouThreshold, setAiIouThreshold] = useState(0.45);
  const [aiStatus, setAiStatus] = useState("");
  const [aiBusy, setAiBusy] = useState(false);
  const aiJobIdRef = useRef<string | null>(null);
  const canceledAiJobRef = useRef<string | null>(null);
  const activeImageIdRef = useRef(activeImageId);
  const [dirty, setDirty] = useState(false);
  const [unsavedPrompt, setUnsavedPrompt] = useState<UnsavedPromptState | null>(null);
  const [unsavedDecisionBusy, setUnsavedDecisionBusy] = useState(false);
  const unsavedDecisionBusyRef = useRef(false);
  const [saveAndNext, setSaveAndNext] = useState(false);
  const [autoSaveOnSwitch, setAutoSaveOnSwitch] = useState(false);
  const pendingUnsavedDecisionRef = useRef<PendingUnsavedDecision | null>(null);
  const unsavedActionRequestRef = useRef(0);
  const imageRouteRequestRef = useRef(0);
  const imageSwitchRequestRef = useRef(0);
  const mountedRef = useRef(true);
  const savedObjectsRef = useRef<AnnotationObject[]>([]);
  const savedRevisionRef = useRef<string | null>(null);
  const savedStatusRef = useRef("未标注");
  const savedVerifiedRef = useRef(false);
  const [mode, setMode] = useState<ToolMode>("select");
  const [selectedObjectId, setSelectedObjectId] = useState<string | null>(null);
  const [selectedObjectIds, setSelectedObjectIds] = useState<Set<string>>(() => new Set());
  function selectObject(objectId: string | null, additive = false) {
    if (!additive) {
      setSelectedObjectId(objectId);
      setSelectedObjectIds(new Set(objectId ? [objectId] : []));
      return;
    }
    if (!objectId) return;
    const next = new Set(selectedObjectIds);
    if (next.has(objectId)) next.delete(objectId);
    else next.add(objectId);
    setSelectedObjectIds(next);
    setSelectedObjectId(next.has(objectId) ? objectId : (next.values().next().value ?? null));
  }
  const [hiddenObjectIds, setHiddenObjectIds] = useState<Set<string>>(() => new Set());
  const [objectQuery, setObjectQuery] = useState("");
  const [objectSort, setObjectSort] = useState<"drawing" | "class" | "type">("drawing");
  const [draftBox, setDraftBox] = useState<{ start: { x: number; y: number }; end: { x: number; y: number } } | null>(null);
  const [draftPolygon, setDraftPolygon] = useState<Point[]>([]);
  const [maskStroke, setMaskStroke] = useState<Point[]>([]);
  const [maskBrushSize, setMaskBrushSize] = useState(16);
  const maskImagesRef = useRef<Map<string, HTMLImageElement>>(new Map());
  const [maskRenderVersion, setMaskRenderVersion] = useState(0);
  const [dragState, setDragState] = useState<{
    objectId: string;
    kind: "move" | "resize";
    handle?: "nw" | "ne" | "sw" | "se";
    start: { x: number; y: number };
    original: NonNullable<AnnotationObject["bbox"]>;
  } | null>(null);
  const [polygonDragState, setPolygonDragState] = useState<{
    objectId: string;
    geometry: "polygon" | "points";
    kind: "move" | "vertex";
    vertexIndex?: number;
    start: Point;
    original: Point[];
  } | null>(null);
  const activeImage = images.find((image) => image.id === activeImageId) ?? (activeImageId ? undefined : images[0]);
  activeImageIdRef.current = activeImage?.id ?? activeImageId;
  const activeImageIndex = images.findIndex((image) => image.id === activeImage?.id);
  const filmstripStart = Math.min(Math.max(0, activeImageIndex - 5), Math.max(0, images.length - 12));
  const filmstripImages = useMemo(
    () => images.slice(filmstripStart, filmstripStart + 12),
    [images, filmstripStart],
  );
  const filmstripUrls = useImageAssetUrls(projectId, filmstripImages, filmstripImages.length);
  const selectedObject = objects.find((object) => object.id === selectedObjectId) ?? null;
  const displayedObjects = useMemo(() => {
    const query = objectQuery.trim().toLocaleLowerCase();
    const filtered = objects.filter((object) => !query || object.label.toLocaleLowerCase().includes(query)
      || object.type.toLocaleLowerCase().includes(query) || object.id.toLocaleLowerCase().includes(query));
    if (objectSort === "drawing") return filtered;
    return [...filtered].sort((left, right) => (objectSort === "class" ? left.label.localeCompare(right.label) : left.type.localeCompare(right.type))
      || left.id.localeCompare(right.id));
  }, [objects, objectQuery, objectSort]);
  useEffect(() => {
    const valid = new Set([...selectedObjectIds].filter((id) => objects.some((object) => object.id === id)));
    if (valid.size !== selectedObjectIds.size) {
      setSelectedObjectIds(valid);
      if (!selectedObjectId || !valid.has(selectedObjectId)) {
        setSelectedObjectId(valid.values().next().value ?? null);
      }
    }
  }, [objects, selectedObjectIds, selectedObjectId]);
  const classificationObject = objects.find((object) => object.type === "classification") ?? null;
  const isClassification = workspaceDetail?.project.annotationTypes.includes("Classification") ?? false;
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const canvasShellRef = useRef<HTMLDivElement | null>(null);
  const imageElementRef = useRef<HTMLImageElement | null>(null);
  const selectedLabelInputRef = useRef<HTMLInputElement | null>(null);
  const [canvasSize, setCanvasSize] = useState<CanvasSize>({ width: 800, height: 600 });
  const [viewport, setViewport] = useState<CanvasViewport>({ scale: 1, offsetX: 80, offsetY: 60 });
  const [brightness, setBrightness] = useState(100);
  const [imageReady, setImageReady] = useState(false);
  const nextObjectSequence = useRef(0);
  function newAnnotationId() {
    nextObjectSequence.current += 1;
    return `ann-${Date.now()}-${nextObjectSequence.current}`;
  }
  const [panState, setPanState] = useState<{
    start: { x: number; y: number };
    original: CanvasViewport;
  } | null>(null);
  const activePointersRef = useRef(new Map<number, { x: number; y: number }>());
  const pointerEventsSeenRef = useRef(false);
  const pinchStartRef = useRef<{
    left: { x: number; y: number };
    right: { x: number; y: number };
    viewport: CanvasViewport;
  } | null>(null);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      unsavedActionRequestRef.current += 1;
      unsavedDecisionBusyRef.current = false;
      const pending = pendingUnsavedDecisionRef.current;
      pendingUnsavedDecisionRef.current = null;
      // A route guard can be waiting on this promise while React unmounts the
      // workspace. Resolve it as “stay” so no caller remains suspended and no
      // navigation is allowed to continue after the workspace is gone.
      pending?.resolve("stay");
    };
  }, []);

  function beginUnsavedDecision(message: string): { requestId: number; promise: Promise<UnsavedDecision> } {
    const requestId = ++unsavedActionRequestRef.current;
    const existing = pendingUnsavedDecisionRef.current;
    if (existing) return { requestId, promise: existing.promise };
    if (!mountedRef.current) return { requestId, promise: Promise.resolve("stay") };

    let resolveDecision!: (decision: UnsavedDecision) => void;
    const promise = new Promise<UnsavedDecision>((resolve) => {
      resolveDecision = resolve;
    });
    pendingUnsavedDecisionRef.current = { promise, resolve: resolveDecision };
    unsavedDecisionBusyRef.current = false;
    setUnsavedDecisionBusy(false);
    setUnsavedPrompt({ message });
    return { requestId, promise };
  }

  function resolveUnsavedDecision(decision: UnsavedDecision) {
    const pending = pendingUnsavedDecisionRef.current;
    if (!pending) return;
    pendingUnsavedDecisionRef.current = null;
    unsavedDecisionBusyRef.current = false;
    if (mountedRef.current) {
      setUnsavedPrompt(null);
      setUnsavedDecisionBusy(false);
    }
    pending.resolve(decision);
  }

  function discardCurrentChanges() {
    const baseline = savedObjectsRef.current;
    dispatchEditor({ type: "load", objects: baseline });
    setRevision(savedRevisionRef.current);
    setAnnotationStatus(savedStatusRef.current);
    setIsVerified(savedVerifiedRef.current);
    setPendingVerification(null);
    setDirty(false);
    selectObject(baseline[0]?.id ?? null);
    setHiddenObjectIds(new Set());
    setDraftBox(null);
    setDraftPolygon([]);
    setMaskStroke([]);
    setAiPromptPoints([]);
    setSaveMessage("已放弃未保存更改");
  }

  async function handleUnsavedDecision(decision: UnsavedDecision) {
    if (!pendingUnsavedDecisionRef.current || unsavedDecisionBusyRef.current) return;
    if (decision === "save") {
      unsavedDecisionBusyRef.current = true;
      setUnsavedDecisionBusy(true);
      let saved = false;
      try {
        saved = await save({ stay: true });
      } catch {
        saved = false;
      }
      if (!mountedRef.current) return;
      if (!saved) {
        unsavedDecisionBusyRef.current = false;
        setUnsavedDecisionBusy(false);
        setUnsavedPrompt((current) => current ? { ...current, error: "保存失败，请处理错误后再选择下一步。" } : current);
        return;
      }
      unsavedDecisionBusyRef.current = false;
      setUnsavedDecisionBusy(false);
    } else if (decision === "discard") {
      discardCurrentChanges();
    }
    resolveUnsavedDecision(decision);
  }

  useEffect(() => {
    setImagesLoaded(false);
    setLoadError(null);
    hasMoreImagesRef.current = false;
    loadingMoreImagesRef.current = false;
    listProjectImages(projectId, undefined, { offset: 0, limit: 256 })
      .then((items) => {
        setImages(items);
        setImagesLoaded(true);
        hasMoreImagesRef.current = items.length === 256;
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
    if (!imagesLoaded || !hasMoreImagesRef.current || loadingMoreImagesRef.current) return;
    const index = images.findIndex((image) => image.id === activeImageId);
    if (index >= 0 && index < images.length - 24) return;
    loadingMoreImagesRef.current = true;
    listProjectImages(projectId, undefined, { offset: images.length, limit: 256 })
      .then((items) => {
        if (listingProjectIdRef.current !== projectId) return;
        hasMoreImagesRef.current = items.length === 256;
        setImages((current) => [...current, ...items]);
      })
      .catch((error) => {
        if (listingProjectIdRef.current !== projectId) return;
        hasMoreImagesRef.current = false;
        setLoadError({ title: "加载图片列表失败", message: error instanceof Error ? error.message : String(error) });
      })
      .finally(() => { if (listingProjectIdRef.current === projectId) loadingMoreImagesRef.current = false; });
  }, [projectId, imagesLoaded, images, activeImageId]);

  useEffect(() => {
    getProjectDetail(projectId)
      .then(setWorkspaceDetail)
      .catch(() => setWorkspaceDetail(null));
  }, [projectId]);

  useEffect(() => {
    if (workspaceDetail?.classes[0] && !workspaceDetail.classes.some((item) => item.id === aiPromptClassId)) {
      setAiPromptClassId(workspaceDetail.classes[0].id ?? 0);
    }
  }, [workspaceDetail, aiPromptClassId]);

  useEffect(() => {
    const nextImageId = activeImageId;
    if (!nextImageId) return;
    let cancelled = false;
    setLoadError(null);
    getFileAssetUrl(projectId, nextImageId)
      .then((url) => { if (!cancelled) setAssetUrl(url); })
      .catch((error) => {
        if (cancelled) return;
        setAssetUrl("");
        setLoadError({
          title: "图片未找到或数据集未初始化",
          message: error instanceof Error ? error.message : String(error),
        });
      });
    getImageAnnotationState(projectId, nextImageId)
      .then((state) => {
        if (cancelled) return;
        savedObjectsRef.current = state.objects;
        savedRevisionRef.current = state.revision;
        savedStatusRef.current = state.status;
        savedVerifiedRef.current = state.verified === true;
        dispatchEditor({ type: "load", objects: state.objects });
        setRevision(state.revision);
        setAnnotationStatus(state.status);
        setIsVerified(state.verified === true);
        setSaveMessage(null);
        setPendingVerification(null);
        setDirty(false);
        selectObject(state.objects[0]?.id ?? null);
        setHiddenObjectIds(new Set());
        setDraftPolygon([]);
        setAiPromptPoints([]);
      })
      .catch(() => {
        if (cancelled) return;
        savedObjectsRef.current = [];
        savedRevisionRef.current = null;
        savedStatusRef.current = "未标注";
        savedVerifiedRef.current = false;
        dispatchEditor({ type: "load", objects: [] });
        setRevision(null);
        setAnnotationStatus("未标注");
        setIsVerified(false);
        setDirty(false);
        selectObject(null);
        setHiddenObjectIds(new Set());
        setAiPromptPoints([]);
      });
    return () => { cancelled = true; };
  }, [projectId, activeImageId]);

  useEffect(() => {
    if (!imageId || imageId === activeImageId) return;
    const routeRequest = ++imageRouteRequestRef.current;
    void (async () => {
      if (dirty) {
        if (autoSaveOnSwitch) {
          if (!(await save({ stay: true }))) {
            if (routeRequest !== imageRouteRequestRef.current) return;
            if (onNavigateImage) onNavigateImage(activeImageId);
            else navigate(`#/annotate/${projectId}/${activeImageId}`);
            return;
          }
        } else {
          const decisionRequest = beginUnsavedDecision("当前标注尚未保存，是否继续切换图片？");
          const decision = await decisionRequest.promise;
          if (!mountedRef.current || routeRequest !== imageRouteRequestRef.current
            || decisionRequest.requestId !== unsavedActionRequestRef.current) return;
          if (decision === "stay") {
            if (onNavigateImage) onNavigateImage(activeImageId);
            else navigate(`#/annotate/${projectId}/${activeImageId}`);
            return;
          }
        }
        if (!mountedRef.current || routeRequest !== imageRouteRequestRef.current) return;
      }
      if (!mountedRef.current || routeRequest !== imageRouteRequestRef.current) return;
      setActiveImageId(imageId);
    })();
  }, [imageId, projectId]);

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
    const activeMasks = new Set(objects.map((object) => object.maskData).filter((value): value is string => Boolean(value)));
    for (const key of maskImagesRef.current.keys()) {
      if (!activeMasks.has(key)) maskImagesRef.current.delete(key);
    }
    for (const object of objects) {
      if (!object.maskData || maskImagesRef.current.has(object.maskData)) continue;
      const key = object.maskData;
      const image = new Image();
      image.onload = () => {
        if (maskImagesRef.current.get(key) === image) setMaskRenderVersion((current) => current + 1);
      };
      image.src = `data:image/png;base64,${key}`;
      maskImagesRef.current.set(key, image);
    }
  }, [objects]);

  useEffect(() => {
    return () => {
      const jobId = aiJobIdRef.current;
      if (jobId) {
        canceledAiJobRef.current = jobId;
        void cancelAiAnnotation(jobId).catch(() => {});
      }
    };
  }, [projectId, activeImageId]);

  useEffect(() => () => { maskImagesRef.current.clear(); }, []);

  useEffect(() => {
    if (!aiBusy) return;
    const timer = window.setInterval(() => {
      const jobId = aiJobIdRef.current;
      if (!jobId) return;
      void getAiJobStatus(jobId).then((status) => {
        if (!status || aiJobIdRef.current !== jobId || status.event !== "progress") return;
        if (status.filename) {
          const progress = status.bytes_total && status.bytes_total > 0
            ? ` ${Math.round((status.bytes_done ?? 0) / status.bytes_total * 100)}%` : "";
          setAiStatus(`模型下载 ${Number(status.file_index ?? 0) + 1}/${status.file_count ?? 1}：${status.filename}${progress}`);
        }
      }).catch(() => {});
    }, 700);
    return () => window.clearInterval(timer);
  }, [aiBusy]);

  useEffect(() => {
    // Coalesce pointer/zoom updates before the next screen refresh. Cleanup
    // prevents an older state from painting after a newer render or unmount.
    const frame = window.requestAnimationFrame(() => {
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
        draftPolygon,
        aiPromptPoints,
        maskStroke,
        maskBrushSize,
        maskImages: maskImagesRef.current,
        brightness,
        imageElement: imageElementRef.current,
        imageReady,
        mode,
        objects,
        hiddenObjectIds,
        selectedObjectIds,
        size: canvasSize,
        viewport,
      });
    });
    return () => window.cancelAnimationFrame(frame);
  }, [activeImage, canvasSize, draftBox, draftPolygon, aiPromptPoints, maskStroke, maskBrushSize, maskRenderVersion, hiddenObjectIds, imageReady, mode, objects, selectedObjectIds, viewport, brightness]);

  useEffect(() => {
    function handleBeforeUnload(event: BeforeUnloadEvent) {
      if (!dirty) return;
      event.preventDefault();
      event.returnValue = "";
    }

    window.addEventListener("beforeunload", handleBeforeUnload);
    return () => window.removeEventListener("beforeunload", handleBeforeUnload);
  }, [dirty]);

  async function save(options: { next?: boolean; stay?: boolean } = {}): Promise<boolean> {
    const targetImageId = activeImageId || imageId;
    if (!targetImageId) return false;
    const submittedObjects = objects;
    try {
      const result = await saveImageAnnotations(projectId, targetImageId, revision, submittedObjects);
      setRevision(result.revision);
      if (pendingVerification !== null) {
        await setImageVerified(projectId, targetImageId, pendingVerification);
        setIsVerified(pendingVerification);
        setPendingVerification(null);
      }
      savedObjectsRef.current = submittedObjects;
      savedRevisionRef.current = result.revision;
      savedStatusRef.current = "草稿";
      savedVerifiedRef.current = pendingVerification !== null ? pendingVerification : isVerified;
      const changedWhileSaving = objectsRef.current !== submittedObjects;
      setDirty(changedWhileSaving);
      setAnnotationStatus("草稿");
      const saveNotice =
        result.sourceSync === "native-only"
          ? `已保存项目标注；源标签未同步：${result.sourceSyncMessage ?? "目标格式不支持当前形状"}`
          : result.sourceSync === "not-applicable"
            ? "已保存"
            : "已保存并写回标注文件";
      setSaveMessage(changedWhileSaving ? `${saveNotice}；保存期间又有新修改，请再次保存` : saveNotice);
      if (!changedWhileSaving && (options.next || (!options.stay && saveAndNext))) goToImage(1, true);
      return !changedWhileSaving;
    } catch (error) {
      setSaveMessage(`保存失败：${error instanceof Error ? error.message : String(error)}`);
      return false;
    }
  }

  useEffect(() => {
    registerLeaveGuard(async () => {
      if (!dirty) return true;
      if (autoSaveOnSwitch) return save({ stay: true });
      const decisionRequest = beginUnsavedDecision("当前标注尚未保存，是否继续离开工作台？");
      const decision = await decisionRequest.promise;
      if (!mountedRef.current || decisionRequest.requestId !== unsavedActionRequestRef.current) return false;
      return decision !== "stay";
    });
    return () => registerLeaveGuard(null);
  }, [registerLeaveGuard, dirty, autoSaveOnSwitch, objects, revision, pendingVerification, projectId, imageId]);

  async function closeWorkspace() {
    if (dirty) {
      if (autoSaveOnSwitch) {
        if (!(await save({ stay: true }))) return;
      } else {
        const decisionRequest = beginUnsavedDecision("当前标注尚未保存，是否继续关闭工作台？");
        const decision = await decisionRequest.promise;
        if (!mountedRef.current || decisionRequest.requestId !== unsavedActionRequestRef.current || decision === "stay") return;
      }
    }
    await runDesktopCommand("close_window");
  }

  async function toggleVerified() {
    const targetImageId = activeImageId || imageId;
    if (!targetImageId) return;
    try {
      if (dirty && !(await save({ stay: true }))) return;
      const verified = !isVerified;
      await setImageVerified(projectId, targetImageId, verified);
      setIsVerified(verified);
      setSaveMessage(verified ? "已验证并持久化" : "已取消验证");
    } catch (error) {
      setSaveMessage(`验证状态保存失败：${error instanceof Error ? error.message : String(error)}`);
    }
  }

  async function submit() {
    const targetImageId = activeImageId || imageId;
    if (!targetImageId) return;
    if (dirty && !(await save({ stay: true }))) return;
    await submitImageAnnotations(projectId, targetImageId);
    setAnnotationStatus("待质检");
    setSaveMessage("已提交质检");
  }

  async function loadExternalAnnotationFile() {
    const targetImageId = activeImageId || imageId;
    if (!targetImageId) return;
    if (dirty) {
      const decisionRequest = beginUnsavedDecision("当前标注尚未保存，是否加载外部标注并替换？");
      const decision = await decisionRequest.promise;
      if (!mountedRef.current || decisionRequest.requestId !== unsavedActionRequestRef.current || decision === "stay") return;
    }
    try {
      const paths = await pickDataSource("labels");
      if (!paths?.[0]) return;
      const imported = await loadExternalAnnotations(projectId, targetImageId, paths[0]);
      dispatchEditor({ type: "load", objects: imported.objects });
      selectObject(imported.objects[0]?.id ?? null);
      setPendingVerification(imported.verified);
      setIsVerified(imported.verified);
      setAnnotationStatus("草稿");
      setDirty(true);
      setSaveMessage(`已加载 ${imported.objects.length} 个外部标注，请保存以写入项目`);
    } catch (error) {
      setSaveMessage(`加载外部标注失败：${error instanceof Error ? error.message : String(error)}`);
    }
  }

  async function exportCurrentAnnotations() {
    const targetImageId = activeImageId || imageId;
    if (!targetImageId) return;
    if (dirty && !(await save({ stay: true }))) return;
    try {
      const output = await exportAnnotationFile(projectId, targetImageId, annotationExportFormat);
      if (output) setSaveMessage(`标注已另存为 ${output}`);
    } catch (error) {
      setSaveMessage(`导出失败：${error instanceof Error ? error.message : String(error)}`);
    }
  }

  async function chooseAiModel() {
    try {
      const paths = await pickDataSource("model");
      if (paths?.[0]) {
        setAiModelPath(paths[0]);
        setAiStatus(`已选择模型：${paths[0]}`);
      }
    } catch (error) {
      setAiStatus(`选择模型失败：${error instanceof Error ? error.message : String(error)}`);
    }
  }

  async function runAiInference() {
    const targetImageId = activeImage?.id;
    if (!targetImageId || aiBusy) return;
    if (aiProvider === "onnx" && !aiModelPath) { setAiStatus("请先选择 ONNX 模型"); return; }
    if (aiProvider === "osam" && aiPromptType === "points" && aiPromptPoints.length === 0) {
      setAiStatus("请先在画布上添加点或框提示"); return;
    }
    const texts = aiTexts.split(",").map((value) => value.trim()).filter(Boolean);
    if (aiProvider === "osam" && aiPromptType === "text" && texts.length === 0) {
      setAiStatus("请输入项目类别作为文本提示"); return;
    }
    const jobId = `ai-${Date.now()}-${++nextObjectSequence.current}`;
    const options: AiOptions = {
      jobId, provider: aiProvider, modelPath: aiModelPath, model: aiModelName,
      layout: aiLayout, outputFormat: aiOutputFormat, promptType: aiPromptType,
      points: aiPromptPoints.map(({ point }) => [point.x, point.y]),
      pointLabels: aiPromptPoints.map(({ label }) => label), texts,
      defaultClass: workspaceDetail?.classes.find((item) => item.id === aiPromptClassId)?.label,
      scoreThreshold: aiScoreThreshold, iouThreshold: aiIouThreshold,
    };
    aiJobIdRef.current = jobId;
    canceledAiJobRef.current = null;
    setAiBusy(true);
    setAiStatus("正在加载模型并推理…");
    try {
      const result = await runAiAnnotation(projectId, targetImageId, options);
      if (canceledAiJobRef.current === jobId) {
        setAiStatus(activeImageIdRef.current !== targetImageId
          ? "已取消并丢弃旧图片的 AI 结果" : "AI 推理已取消");
        return;
      }
      if (activeImageIdRef.current !== result.imageId) {
        setAiStatus("图片已切换，已丢弃旧图片的 AI 结果");
        return;
      }
      const next = [...objectsRef.current];
      const additions: AnnotationObject[] = [];
      for (const object of result.objects) {
          const duplicate = next.some((existing) => {
            if (existing.classId !== object.classId || existing.type !== object.type) return false;
            if (object.type === "mask") return existing.maskData === object.maskData
              && JSON.stringify(existing.points) === JSON.stringify(object.points);
            if (existing.bbox && object.bbox) {
              const overlapWidth = Math.max(0, Math.min(existing.bbox.x + existing.bbox.width, object.bbox.x + object.bbox.width) - Math.max(existing.bbox.x, object.bbox.x));
              const overlapHeight = Math.max(0, Math.min(existing.bbox.y + existing.bbox.height, object.bbox.y + object.bbox.height) - Math.max(existing.bbox.y, object.bbox.y));
              const overlap = overlapWidth * overlapHeight;
              return overlap / (existing.bbox.width * existing.bbox.height + object.bbox.width * object.bbox.height - overlap) > 0.95;
            }
            return JSON.stringify(existing.polygon ?? existing.points) === JSON.stringify(object.polygon ?? object.points);
          });
          if (!duplicate) { next.push(object); additions.push(object); }
      }
      if (additions.length) {
        editObjects((current) => [...current, ...additions]);
        setDirty(true);
      }
      setAiStatus(`AI 已生成 ${additions.length} 个标注；${result.objects.length - additions.length} 个重复结果已跳过`);
    } catch (error) {
      if (activeImageIdRef.current === targetImageId) setAiStatus(canceledAiJobRef.current === jobId
        ? "AI 推理已取消" : `AI 失败：${error instanceof Error ? error.message : String(error)}`);
    } finally {
      if (aiJobIdRef.current === jobId) {
        aiJobIdRef.current = null;
        setAiBusy(false);
      }
    }
  }

  async function cancelAiInference() {
    const jobId = aiJobIdRef.current;
    if (!jobId) return;
    canceledAiJobRef.current = jobId;
    setAiStatus("正在取消 AI 推理…");
    try { await cancelAiAnnotation(jobId); }
    catch (error) { setAiStatus(`取消失败：${error instanceof Error ? error.message : String(error)}`); }
  }

  useEffect(() => {
    function handleKeyDown(event: globalThis.KeyboardEvent) {
      const key = event.key.toLowerCase();
      const commandKey = event.ctrlKey || event.metaKey;
      const editableTarget = isEditableShortcutTarget(event.target);

      if (commandKey && key === "z") {
        if (editableTarget) return;
        event.preventDefault();
        if (draftPolygon.length && !event.shiftKey) {
          setDraftPolygon((current) => current.slice(0, -1));
          return;
        }
        dispatchEditor({ type: event.shiftKey ? "redo" : "undo" });
        if (event.shiftKey ? editor.future.length : editor.past.length) setDirty(true);
        return;
      }

      if (commandKey && key === "y") {
        if (editableTarget) return;
        event.preventDefault();
        dispatchEditor({ type: "redo" });
        if (editor.future.length) setDirty(true);
        return;
      }

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

      if (commandKey && key === "a") {
        event.preventDefault();
        setSelectedObjectIds(new Set(objects.map((object) => object.id)));
        setSelectedObjectId(objects[0]?.id ?? null);
        return;
      }

      if (key === "escape" && draftPolygon.length) {
        event.preventDefault();
        setDraftPolygon([]);
        setSaveMessage("已取消当前多边形");
        return;
      }

      if (key === "enter" && draftPolygon.length >= (mode === "polygon" || mode === "oriented_rectangle" ? 3 : 2)) {
        event.preventDefault();
        completeDraftPolygon();
        return;
      }

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

      if (key === "p") {
        event.preventDefault();
        setMode("polygon");
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
        if (!event.repeat) void toggleVerified();
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
  }, [selectedObjectId, selectedObject, objects, revision, activeImageId, imageId, activeImage, dirty, images, saveAndNext, canvasSize, draftPolygon, mode, editor.past.length, editor.future.length]);

  async function selectImage(nextImageId: string, skipDirtyCheck = false) {
    if (nextImageId === activeImage?.id) return;
    const switchRequest = ++imageSwitchRequestRef.current;
    if (dirty && !skipDirtyCheck) {
      if (autoSaveOnSwitch) {
        if (!(await save({ stay: true }))) return;
      } else {
        const decisionRequest = beginUnsavedDecision("当前标注尚未保存，是否继续切换图片？");
        const decision = await decisionRequest.promise;
        if (!mountedRef.current || switchRequest !== imageSwitchRequestRef.current
          || decisionRequest.requestId !== unsavedActionRequestRef.current || decision === "stay") return;
      }
    }
    if (!mountedRef.current || switchRequest !== imageSwitchRequestRef.current) return;
    setActiveImageId(nextImageId);
    if (onNavigateImage) onNavigateImage(nextImageId);
    else navigate(`#/annotate/${projectId}/${nextImageId}`);
  }

  function goToImage(offset: number, skipDirtyCheck = false) {
    if (!activeImage) return;
    const index = images.findIndex((image) => image.id === activeImage.id);
    const next = images[index + offset];
    if (next) selectImage(next.id, skipDirtyCheck);
  }

  async function copyPreviousBoxes() {
    const previous = images[activeImageIndex - 1];
    if (!previous || !activeImage) return;
    try {
      const state = await getImageAnnotationState(projectId, previous.id);
      const copied = state.objects.filter((object) => object.type === "bbox" && object.bbox).map((object) => {
        const width = Math.min(object.bbox!.width, activeImage.width);
        const height = Math.min(object.bbox!.height, activeImage.height);
        return {
          ...object,
          id: newAnnotationId(),
          bbox: { x: clamp(object.bbox!.x, 0, activeImage.width - width),
            y: clamp(object.bbox!.y, 0, activeImage.height - height), width, height },
          attributes: { ...object.attributes, source: "previous-image" },
        };
      });
      if (!copied.length) { setSaveMessage("上一张图片没有可复制的矩形框"); return; }
      editObjects((current) => [...current, ...copied]);
      selectObject(copied[0].id);
      setDirty(true);
      setSaveMessage(`已复制上一张图片的 ${copied.length} 个矩形框`);
    } catch (error) {
      setSaveMessage(`复制上一张框失败：${error instanceof Error ? error.message : String(error)}`);
    }
  }

  if (loadError) {
    return (
      <main className={`annotation-page${mobileRuntime ? " mobile-annotation-page" : ""}`}>
        <section className="workspace-area">
          <RuntimeStatePanel title={loadError.title} message={loadError.message} actionLabel="返回数据集" onAction={onBack ?? (() => navigate("#/datasets"))} />
        </section>
      </main>
    );
  }

  if (imagesLoaded && images.length === 0) {
    return (
      <main className={`annotation-page${mobileRuntime ? " mobile-annotation-page" : ""}`}>
        <section className="workspace-area">
          <RuntimeStatePanel title="图片未找到或数据集未初始化" message="请先完成 COCO128 测试数据下载。" actionLabel="返回数据集" onAction={onBack ?? (() => navigate("#/datasets"))} />
        </section>
      </main>
    );
  }

  function eventClientPosition(event: MouseEvent<Element> | ReactPointerEvent<Element> | WheelEvent<Element>) {
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

  function pointFromEvent(event: CanvasPointEvent) {
    const width = activeImage?.width || 640;
    const height = activeImage?.height || 480;
    const rect = event.currentTarget.getBoundingClientRect();
    const { clientX, clientY } = eventClientPosition(event);
    return {
      x: clamp(((clientX - rect.left) - viewport.offsetX) / viewport.scale, 0, width),
      y: clamp(((clientY - rect.top) - viewport.offsetY) / viewport.scale, 0, height),
    };
  }

  function canvasLocalPoint(event: CanvasLocalEvent) {
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
      if (hiddenObjectIds.has(object.id)) continue;
      if (object.bbox) {
        if (object.id === selectedObjectId) {
          const handle = bboxHandles(object.bbox).find(
            (item) => Math.abs(point.x - item.x) <= handleRadius && Math.abs(point.y - item.y) <= handleRadius,
          );
          if (handle) return { object, handle: handle.handle, vertexIndex: null };
        }
        const box = object.bbox;
        if (point.x >= box.x && point.x <= box.x + box.width && point.y >= box.y && point.y <= box.y + box.height) {
          return { object, handle: null, vertexIndex: null };
        }
      }
      if (object.polygon) {
        if (object.id === selectedObjectId) {
          const vertexIndex = object.polygon.findIndex(
            (item) => Math.abs(point.x - item.x) <= handleRadius && Math.abs(point.y - item.y) <= handleRadius,
          );
          if (vertexIndex >= 0) return { object, handle: null, vertexIndex };
        }
        if (pointInPolygon(point, object.polygon)) {
          return { object, handle: null, vertexIndex: null };
        }
      }
      if (object.points?.length) {
        const vertexIndex = object.id === selectedObjectId
          ? object.points.findIndex((item) => pointDistance(point, item) <= handleRadius)
          : -1;
        if (vertexIndex >= 0) return { object, handle: null,
          vertexIndex: object.type === "oriented_rectangle" ? null : vertexIndex };
        const tolerance = 8 / viewport.scale;
        const touchesPoint = object.points.some((item) => pointDistance(point, item) <= tolerance);
        const touchesLine = object.points.slice(1).some((item, index) =>
          pointToSegmentDistance(point, object.points![index], item) <= tolerance);
        const touchesCircle = object.type === "circle" && object.points.length >= 2
          && pointDistance(point, object.points[0]) <= pointDistance(object.points[0], object.points[1]);
        const insideRectangle = object.type === "oriented_rectangle" && pointInPolygon(point, object.points);
        const insideMask = object.type === "mask" && object.points.length === 2
          && point.x >= object.points[0].x && point.x <= object.points[1].x
          && point.y >= object.points[0].y && point.y <= object.points[1].y;
        if (touchesPoint || touchesLine || touchesCircle || insideRectangle || insideMask) {
          return { object, handle: null, vertexIndex: null };
        }
      }
    }
    return null;
  }

  function completeDraftPolygon(points = draftPolygon) {
    const minimum = mode === "polygon" || mode === "oriented_rectangle" ? 3 : 2;
    if (points.length < minimum) return;
    let shapePoints = points;
    if (mode === "oriented_rectangle") {
      const [first, second, third] = points;
      const dx = second.x - first.x;
      const dy = second.y - first.y;
      const length = Math.hypot(dx, dy);
      if (length < 3) return;
      const normal = { x: -dy / length, y: dx / length };
      const height = (third.x - second.x) * normal.x + (third.y - second.y) * normal.y;
      if (Math.abs(height) < 3) return;
      const offset = { x: normal.x * height, y: normal.y * height };
      shapePoints = [first, second,
        { x: second.x + offset.x, y: second.y + offset.y },
        { x: first.x + offset.x, y: first.y + offset.y }];
      if (activeImage && shapePoints.some((item) => item.x < 0 || item.y < 0 || item.x > activeImage.width || item.y > activeImage.height)) {
        setSaveMessage("旋转框超出图片范围，请重新选取第三点");
        return;
      }
    }
    const shapeType = mode === "linestrip" || mode === "points" || mode === "oriented_rectangle" ? mode : "polygon";
    const chosen = workspaceDetail?.classes.find((item) => item.id === selectedObject?.classId) ?? workspaceDetail?.classes[0];
    const object: AnnotationObject = {
      id: newAnnotationId(),
      classId: chosen?.id ?? 0,
      label: chosen?.label ?? "object",
      type: shapeType,
      ...(shapeType === "polygon" ? { polygon: shapePoints } : { points: shapePoints }),
      attributes: { source: "manual" },
    };
    editObjects((current) => [...current, object]);
    setDraftPolygon([]);
    setDirty(true);
    selectObject(object.id);
    setMode("select");
  }

  function commitMaskStroke(stroke: Point[]) {
    if (!activeImage || stroke.length === 0) return;
    const existing = selectedObject?.type === "mask" ? selectedObject : null;
    if (mode === "mask_erase" && !existing) { setSaveMessage("请先选择要擦除的掩码"); return; }
    const existingImage = existing?.maskData ? maskImagesRef.current.get(existing.maskData) : null;
    if (existing && (!existingImage?.complete || !existingImage.naturalWidth)) {
      setSaveMessage("掩码图像尚未加载，请稍后重试"); return;
    }
    const radius = maskBrushSize / 2;
    const x1 = Math.max(0, Math.floor(Math.min(...stroke.map((point) => point.x - radius), existing?.points?.[0].x ?? Infinity)));
    const y1 = Math.max(0, Math.floor(Math.min(...stroke.map((point) => point.y - radius), existing?.points?.[0].y ?? Infinity)));
    const x2 = Math.min(activeImage.width, Math.ceil(Math.max(...stroke.map((point) => point.x + radius), existing?.points?.[1].x ?? -Infinity)));
    const y2 = Math.min(activeImage.height, Math.ceil(Math.max(...stroke.map((point) => point.y + radius), existing?.points?.[1].y ?? -Infinity)));
    if (x2 <= x1 || y2 <= y1) return;
    const bitmap = document.createElement("canvas");
    bitmap.width = x2 - x1;
    bitmap.height = y2 - y1;
    const context = bitmap.getContext("2d");
    if (!context) { setSaveMessage("浏览器不支持掩码画布"); return; }
    if (existing && existingImage && existing.points) {
      context.drawImage(existingImage, existing.points[0].x - x1, existing.points[0].y - y1,
        existing.points[1].x - existing.points[0].x, existing.points[1].y - existing.points[0].y);
    }
    context.globalCompositeOperation = mode === "mask_erase" ? "destination-out" : "source-over";
    context.strokeStyle = "#ffffff";
    context.fillStyle = "#ffffff";
    context.lineCap = "round";
    context.lineJoin = "round";
    context.lineWidth = maskBrushSize;
    context.beginPath();
    context.moveTo(stroke[0].x - x1, stroke[0].y - y1);
    stroke.slice(1).forEach((point) => context.lineTo(point.x - x1, point.y - y1));
    context.stroke();
    if (stroke.length === 1) {
      context.beginPath();
      context.arc(stroke[0].x - x1, stroke[0].y - y1, radius, 0, Math.PI * 2);
      context.fill();
    }
    const dataUrl = bitmap.toDataURL("image/png");
    if (!dataUrl.startsWith("data:image/png;base64,")) { setSaveMessage("掩码 PNG 编码失败"); return; }
    const maskData = dataUrl.slice("data:image/png;base64,".length);
    const points = [{ x: x1, y: y1 }, { x: x2, y: y2 }];
    if (existing) {
      editObjects((current) => current.map((object) => object.id === existing.id ? { ...object, maskData, points } : object));
    } else {
      const chosen = workspaceDetail?.classes.find((item) => item.id === selectedObject?.classId) ?? workspaceDetail?.classes[0];
      const object: AnnotationObject = {
        id: newAnnotationId(), classId: chosen?.id ?? 0, label: chosen?.label ?? "object",
        type: "mask", points, maskData, attributes: { source: "manual" },
      };
      editObjects((current) => [...current, object]);
      selectObject(object.id);
    }
    setDirty(true);
  }

  function beginCanvasInteraction(event: CanvasPointEvent) {
    const isTouchPointer = "pointerType" in event && event.pointerType === "touch";
    if (event.button !== 0 && !isTouchPointer) return;
    const localPoint = canvasLocalPoint(event);
    const point = pointFromEvent(event);
    if (mode === "mask" || mode === "mask_erase") {
      setMaskStroke([point]);
      return;
    }
    if (mode === "ai_point_positive" || mode === "ai_point_negative") {
      setAiPromptPoints((current) => [...current, { point, label: mode === "ai_point_positive" ? 1 : 0 }]);
      return;
    }
    if (mode === "ai_box") {
      setDraftBox({ start: point, end: point });
      return;
    }
    if ((event.altKey || event.ctrlKey) && selectedObject && (selectedObject.polygon || selectedObject.type === "linestrip")) {
      const geometry = selectedObject.polygon ? "polygon" : "points";
      const vertices = selectedObject[geometry] ?? [];
      const tolerance = 8 / viewport.scale;
      if (event.ctrlKey) {
        const vertexIndex = vertices.findIndex((item) => pointDistance(item, point) <= tolerance);
        const minimum = geometry === "polygon" ? 3 : 2;
        if (vertexIndex >= 0 && vertices.length > minimum) {
          editObjects((current) => current.map((object) => object.id === selectedObject.id
            ? { ...object, [geometry]: vertices.filter((_, index) => index !== vertexIndex) }
            : object));
          setDirty(true);
          return;
        }
      }
      if (event.altKey) {
        const edgeCount = geometry === "polygon" ? vertices.length : vertices.length - 1;
        for (let index = 0; index < edgeCount; index += 1) {
          if (pointToSegmentDistance(point, vertices[index], vertices[(index + 1) % vertices.length]) <= tolerance) {
            editObjects((current) => current.map((object) => object.id === selectedObject.id
              ? { ...object, [geometry]: [...vertices.slice(0, index + 1), point, ...vertices.slice(index + 1)] }
              : object));
            setDirty(true);
            return;
          }
        }
      }
    }
    if (mode === "pan" || event.altKey) {
      setPanState({ start: localPoint, original: viewport });
      return;
    }
    if (mode === "point") {
      const chosen = workspaceDetail?.classes.find((item) => item.id === selectedObject?.classId) ?? workspaceDetail?.classes[0];
      const object: AnnotationObject = {
        id: newAnnotationId(),
        classId: chosen?.id ?? 0,
        label: chosen?.label ?? "object",
        type: "point",
        points: [point],
        attributes: { source: "manual" },
      };
      editObjects((current) => [...current, object]);
      selectObject(object.id);
      setDirty(true);
      return;
    }
    if (mode === "line" || mode === "circle") {
      setDraftBox({ start: point, end: point });
      return;
    }
    if (mode === "polygon" || mode === "linestrip" || mode === "points" || mode === "oriented_rectangle") {
      if (
        mode === "polygon"
        &&
        draftPolygon.length >= 3
        && pointDistance(point, draftPolygon[0]) <= Math.max(10 / viewport.scale, 5)
      ) {
        completeDraftPolygon();
      } else if (mode === "oriented_rectangle" && draftPolygon.length === 2) {
        completeDraftPolygon([...draftPolygon, point]);
      } else {
        setDraftPolygon((current) => [...current, point]);
      }
      return;
    }

    const target = hitTestObject(point);
    if (target?.object.bbox) {
      dispatchEditor({ type: "begin-gesture" });
      setMode("select");
      selectObject(target.object.id, event.ctrlKey || event.metaKey);
      setDragState({
        objectId: target.object.id,
        kind: target.handle ? "resize" : "move",
        handle: target.handle ?? undefined,
        start: point,
        original: target.object.bbox,
      });
      return;
    }
    if (target?.object.polygon) {
      dispatchEditor({ type: "begin-gesture" });
      setMode("select");
      selectObject(target.object.id, event.ctrlKey || event.metaKey);
      setPolygonDragState({
        objectId: target.object.id,
        geometry: "polygon",
        kind: target.vertexIndex === null ? "move" : "vertex",
        vertexIndex: target.vertexIndex ?? undefined,
        start: point,
        original: target.object.polygon,
      });
      return;
    }
    if (target?.object.points) {
      dispatchEditor({ type: "begin-gesture" });
      setMode("select");
      selectObject(target.object.id, event.ctrlKey || event.metaKey);
      setPolygonDragState({
        objectId: target.object.id,
        geometry: "points",
        kind: target.vertexIndex === null ? "move" : "vertex",
        vertexIndex: target.vertexIndex ?? undefined,
        start: point,
        original: target.object.points,
      });
      return;
    }

    if (mode !== "bbox") {
      selectObject(null);
      return;
    }

    setDraftBox({ start: point, end: point });
  }

  function updateCanvasInteraction(event: CanvasPointEvent) {
    if (!panState && !draftBox && !dragState && !polygonDragState && !maskStroke.length) return;
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
    if (maskStroke.length) {
      setMaskStroke((current) => [...current, point]);
      return;
    }
    if (draftBox) {
      setDraftBox({ ...draftBox, end: point });
      return;
    }

    if (dragState) {
      const dx = point.x - dragState.start.x;
      const dy = point.y - dragState.start.y;
      setDirty(true);
      previewObjects((current) =>
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
    if (polygonDragState && activeImage) {
      const dx = point.x - polygonDragState.start.x;
      const dy = point.y - polygonDragState.start.y;
      setDirty(true);
      previewObjects((current) =>
        current.map((object) => {
          if (object.id !== polygonDragState.objectId) return object;
          const geometry = polygonDragState.geometry;
          if (geometry === "points" && object.type === "circle" && polygonDragState.original.length === 2) {
            if (polygonDragState.kind === "move" || polygonDragState.vertexIndex === 0) {
              return { ...object, points: translateShapePoints(object, polygonDragState.original, dx, dy, activeImage.width, activeImage.height) };
            }
            const center = polygonDragState.original[0];
            const originalEnd = polygonDragState.original[1];
            const candidate = { x: originalEnd.x + dx, y: originalEnd.y + dy };
            const distance = pointDistance(center, candidate);
            const maximum = Math.max(0, Math.min(center.x, center.y,
              activeImage.width - center.x, activeImage.height - center.y) - 0.1);
            const scale = distance > 0 ? Math.min(1, maximum / distance) : 1;
            return { ...object, points: [center, {
              x: Number((center.x + (candidate.x - center.x) * scale).toFixed(1)),
              y: Number((center.y + (candidate.y - center.y) * scale).toFixed(1)),
            }] };
          }
          if (polygonDragState.kind === "move") {
            return { ...object, [geometry]: translateShapePoints(object, polygonDragState.original, dx, dy, activeImage.width, activeImage.height) };
          }
          return {
            ...object,
            [geometry]: polygonDragState.original.map((originalPoint, index) => {
              if (
                polygonDragState.kind === "vertex"
                && index !== polygonDragState.vertexIndex
              ) {
                return originalPoint;
              }
              return {
                x: Number(clamp(originalPoint.x + dx, 0, activeImage.width).toFixed(1)),
                y: Number(clamp(originalPoint.y + dy, 0, activeImage.height).toFixed(1)),
              };
            }),
          };
        }),
      );
    }
  }

  function finishCanvasInteraction(event: CanvasPointEvent) {
    if (maskStroke.length) {
      const end = pointFromEvent(event);
      commitMaskStroke(pointDistance(maskStroke[maskStroke.length - 1], end) > 0.5 ? [...maskStroke, end] : maskStroke);
      setMaskStroke([]);
      return;
    }
    if (draftBox) {
      const end = pointFromEvent(event);
      const box = normalizeBox(draftBox.start, end);
      setDraftBox(null);
      if (mode === "ai_box" && box.width >= 3 && box.height >= 3) {
        setAiPromptPoints((current) => [
          ...current,
          { point: { x: box.x, y: box.y }, label: 2 },
          { point: { x: box.x + box.width, y: box.y + box.height }, label: 3 },
        ]);
      } else if ((mode === "line" || mode === "circle") && pointDistance(draftBox.start, end) >= 3) {
        const chosen = workspaceDetail?.classes.find((item) => item.id === selectedObject?.classId) ?? workspaceDetail?.classes[0];
        let shapeEnd = end;
        if (mode === "circle" && activeImage) {
          const radius = Math.min(pointDistance(draftBox.start, end), draftBox.start.x, draftBox.start.y,
            activeImage.width - draftBox.start.x, activeImage.height - draftBox.start.y);
          if (radius < 3) return;
          const dx = end.x - draftBox.start.x;
          const dy = end.y - draftBox.start.y;
          const length = Math.hypot(dx, dy);
          shapeEnd = { x: draftBox.start.x + radius * dx / length, y: draftBox.start.y + radius * dy / length };
        }
        const object: AnnotationObject = {
          id: newAnnotationId(),
          classId: chosen?.id ?? 0,
          label: chosen?.label ?? "object",
          type: mode,
          points: [draftBox.start, shapeEnd],
          attributes: { source: "manual" },
        };
        editObjects((current) => [...current, object]);
        setDirty(true);
        selectObject(object.id);
      } else if (mode === "bbox" && box.width >= 3 && box.height >= 3) {
        const chosen = workspaceDetail?.classes.find((item) => item.id === selectedObject?.classId) ?? workspaceDetail?.classes[0];
        const object: AnnotationObject = {
          id: newAnnotationId(),
          classId: chosen?.id ?? 0,
          label: chosen?.label ?? "object",
          type: "bbox",
          bbox: box,
          attributes: { source: "manual" },
        };
        editObjects((current) => [...current, object]);
        setDirty(true);
        selectObject(object.id);
      }
    }

    if (dragState || polygonDragState) dispatchEditor({ type: "end-gesture" });
    setDragState(null);
    setPolygonDragState(null);
    setPanState(null);
  }

  function handleCanvasPointerDown(event: ReactPointerEvent<HTMLCanvasElement>) {
    pointerEventsSeenRef.current = true;
    if (event.button !== 0 && event.pointerType !== "touch") return;
    const localPoint = canvasLocalPoint(event);
    activePointersRef.current.set(event.pointerId, localPoint);
    if (event.currentTarget.setPointerCapture) {
      event.currentTarget.setPointerCapture(event.pointerId);
    }

    if (activePointersRef.current.size >= 2) {
      const [left, right] = [...activePointersRef.current.values()];
      pinchStartRef.current = { left, right, viewport };
      // A second finger changes the gesture from editing to navigation. Do
      // not commit a half-drawn box or mask when that transition happens.
      if (dragState || polygonDragState) dispatchEditor({ type: "end-gesture" });
      setDragState(null);
      setPolygonDragState(null);
      setPanState(null);
      setDraftBox(null);
      setMaskStroke([]);
      return;
    }

    beginCanvasInteraction(event);
  }

  function handleCanvasPointerMove(event: ReactPointerEvent<HTMLCanvasElement>) {
    pointerEventsSeenRef.current = true;
    if (!activePointersRef.current.has(event.pointerId)) return;
    activePointersRef.current.set(event.pointerId, canvasLocalPoint(event));
    const pinch = pinchStartRef.current;
    if (pinch && activePointersRef.current.size >= 2) {
      const [left, right] = [...activePointersRef.current.values()];
      setViewport(pinchCanvasViewport(pinch.viewport, pinch.left, pinch.right, left, right));
      return;
    }
    updateCanvasInteraction(event);
  }

  function handleCanvasPointerUp(event: ReactPointerEvent<HTMLCanvasElement>) {
    pointerEventsSeenRef.current = true;
    const wasPinching = Boolean(pinchStartRef.current);
    activePointersRef.current.delete(event.pointerId);
    if (event.currentTarget.hasPointerCapture?.(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
    if (wasPinching) {
      // The remaining finger can start a fresh one-finger gesture, but should
      // not finish the edit that was cancelled when the pinch began.
      if (activePointersRef.current.size < 2) pinchStartRef.current = null;
      return;
    }
    finishCanvasInteraction(event);
  }

  function handleCanvasPointerCancel(event: ReactPointerEvent<HTMLCanvasElement>) {
    pointerEventsSeenRef.current = true;
    activePointersRef.current.delete(event.pointerId);
    pinchStartRef.current = null;
    if (event.currentTarget.hasPointerCapture?.(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
    finishCanvasInteraction(event);
  }

  // React's Pointer Events cover normal desktop mice, but retaining these
  // compatibility handlers keeps older embedded webviews and existing mouse
  // automation usable when they do not dispatch pointer events.
  function handleCanvasMouseDown(event: MouseEvent<HTMLCanvasElement>) {
    if (!pointerEventsSeenRef.current) beginCanvasInteraction(event);
  }

  function handleCanvasMouseMove(event: MouseEvent<HTMLCanvasElement>) {
    if (!pointerEventsSeenRef.current) updateCanvasInteraction(event);
  }

  function handleCanvasMouseUp(event: MouseEvent<HTMLCanvasElement>) {
    if (!pointerEventsSeenRef.current) finishCanvasInteraction(event);
  }

  function handleCanvasDoubleClick(event: MouseEvent<HTMLCanvasElement>) {
    if (mode !== "polygon" && mode !== "linestrip" && mode !== "points") return;
    event.preventDefault();
    const point = pointFromEvent(event);
    const points =
      draftPolygon.length > 0
      && pointDistance(draftPolygon[draftPolygon.length - 1], point) < 1
        ? draftPolygon
        : [...draftPolygon, point];
    completeDraftPolygon(points);
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

  function fitImageToWidth() {
    if (!activeImage) return;
    const scale = canvasSize.width / activeImage.width;
    setViewport({ scale, offsetX: 0, offsetY: (canvasSize.height - activeImage.height * scale) / 2 });
  }

  function focusSelectedObject() {
    if (!selectedObject) return;
    const geometry = selectedObject.bbox
      ? { x: selectedObject.bbox.x, y: selectedObject.bbox.y,
          width: selectedObject.bbox.width, height: selectedObject.bbox.height }
      : (() => {
          const points = selectedObject.polygon ?? selectedObject.points;
          if (!points?.length) return null;
          const x = Math.min(...points.map((point) => point.x));
          const y = Math.min(...points.map((point) => point.y));
          return { x, y, width: Math.max(...points.map((point) => point.x)) - x,
            height: Math.max(...points.map((point) => point.y)) - y };
        })();
    if (!geometry) return;
    const scale = clamp(Math.min(canvasSize.width / Math.max(geometry.width * 1.4, 20),
      canvasSize.height / Math.max(geometry.height * 1.4, 20)), 0.05, 16);
    setViewport({ scale,
      offsetX: canvasSize.width / 2 - (geometry.x + geometry.width / 2) * scale,
      offsetY: canvasSize.height / 2 - (geometry.y + geometry.height / 2) * scale });
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
    const chosen = workspaceDetail?.classes.find((item) => item.label === label);
    setDirty(true);
    editObjects((current) =>
      current.map((object) => (object.id === selectedObjectId
        ? { ...object, label, classId: chosen?.id ?? object.classId } : object)),
    );
  }

  function updateSelectedClass(classId: number) {
    const chosen = workspaceDetail?.classes.find((item) => item.id === classId);
    if (!selectedObjectId || !chosen) return;
    editObjects((current) => current.map((object) => object.id === selectedObjectId
      ? { ...object, classId, label: chosen.label }
      : object));
    setDirty(true);
  }

  function updateSelectedDifficult(difficult: boolean) {
    if (!selectedObjectId) return;
    editObjects((current) => current.map((object) => object.id === selectedObjectId
      ? { ...object, attributes: { ...object.attributes, difficult } }
      : object));
    setDirty(true);
  }

  function updateSelectedAttribute(key: string, value: unknown) {
    if (!selectedObjectId) return;
    editObjects((current) => current.map((object) => object.id === selectedObjectId
      ? { ...object, attributes: { ...object.attributes, [key]: value } }
      : object));
    setDirty(true);
  }

  function toggleObjectVisibility(objectId: string) {
    setHiddenObjectIds((current) => {
      const next = new Set(current);
      if (next.has(objectId)) next.delete(objectId);
      else next.add(objectId);
      return next;
    });
  }

  function assignClassification(rawClassId: string) {
    if (!workspaceDetail) return;
    const classId = Number(rawClassId);
    if (!Number.isInteger(classId)) {
      editObjects((current) => current.filter((object) => object.type !== "classification"));
      selectObject(null);
      setDirty(true);
      return;
    }
    const selectedClass = workspaceDetail.classes.find((item) => item.id === classId);
    if (!selectedClass) return;
    const object: AnnotationObject = {
      id: classificationObject?.id ?? `classification-${activeImageId || imageId || Date.now()}`,
      classId: selectedClass.id ?? 0,
      label: selectedClass.label,
      type: "classification",
      attributes: { ...classificationObject?.attributes, source: "manual" },
    };
    editObjects((current) => [
      ...current.filter((item) => item.type !== "classification"),
      object,
    ]);
    selectObject(object.id);
    setDirty(true);
  }

  function updateSelectedBbox(field: "x" | "y" | "width" | "height", rawValue: string) {
    if (!selectedObjectId || !activeImage) return;
    const value = Number(rawValue);
    if (!Number.isFinite(value)) return;

    setDirty(true);
    editObjects((current) =>
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
    if (!selectedObjectIds.size || !activeImage) return;
    setDirty(true);
    editObjects((current) =>
      current.map((object) => {
        if (!selectedObjectIds.has(object.id)) return object;
        if (object.polygon || object.points) {
          const geometry = object.polygon ? "polygon" : "points";
          const points = object[geometry] ?? [];
          return { ...object, [geometry]: translateShapePoints(object, points, dx, dy, activeImage.width, activeImage.height) };
        }
        if (!object.bbox) return object;
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
    if (!selectedObjectIds.size) return;
    setDirty(true);
    editObjects((current) => current.filter((object) => !selectedObjectIds.has(object.id)));
    selectObject(null);
  }

  function deleteAllObjects() {
    if (!objects.length) return;
    editObjects(() => []);
    selectObject(null);
    setDirty(true);
  }

  function duplicateSelectedObject() {
    const selected = objects.filter((object) => selectedObjectIds.has(object.id));
    if (!selected.length) return;
    const duplicates: AnnotationObject[] = selected.map((object) => ({
      ...object,
      id: newAnnotationId(),
      bbox: object.bbox
        ? {
            ...object.bbox,
            x: activeImage ? clamp(object.bbox.x + 8, 0, activeImage.width - object.bbox.width) : object.bbox.x + 8,
            y: activeImage ? clamp(object.bbox.y + 8, 0, activeImage.height - object.bbox.height) : object.bbox.y + 8,
          }
        : undefined,
      polygon: object.polygon?.map((point) => ({
        x: activeImage ? clamp(point.x + 8, 0, activeImage.width) : point.x + 8,
        y: activeImage ? clamp(point.y + 8, 0, activeImage.height) : point.y + 8,
      })),
      points: object.points && activeImage
        ? translateShapePoints(object, object.points, 8, 8, activeImage.width, activeImage.height)
        : object.points?.map((point) => ({ x: point.x + 8, y: point.y + 8 })),
      attributes: { ...object.attributes, source: "copy" },
    }));
    editObjects((current) => [...current, ...duplicates]);
    setSelectedObjectIds(new Set(duplicates.map((object) => object.id)));
    setSelectedObjectId(duplicates[0].id);
    setDirty(true);
  }

  return (
    <main className={`annotation-page${mobileRuntime ? " mobile-annotation-page" : ""}`}>
      <aside className="tool-rail" aria-label="Annotation tools">
        {(isClassification
          ? [
              { label: "选择", icon: MousePointer2, mode: "select" as ToolMode },
              { label: "平移", icon: Move, mode: "pan" as ToolMode },
              { label: "显示", icon: Eye, mode: "select" as ToolMode },
            ]
          : [
              { label: "选择", icon: MousePointer2, mode: "select" as ToolMode },
              { label: "BBox", icon: BoxSelect, mode: "bbox" as ToolMode },
              { label: "平移", icon: Move, mode: "pan" as ToolMode },
              { label: "Polygon", icon: Square, mode: "polygon" as ToolMode },
              ...(mobileRuntime ? [] : [{ label: "智能工具", icon: Zap, mode: "select" as ToolMode }]),
              { label: "显示", icon: Eye, mode: "select" as ToolMode },
            ]
        ).map((tool) => {
          const Icon = tool.icon;
          return (
            <button
              aria-label={tool.label}
              aria-pressed={mode === tool.mode}
              className={mode === tool.mode ? "active" : ""}
              key={tool.label}
              onClick={() => {
                if (tool.label === "智能工具") setAiPanelOpen((current) => !current);
                else setMode(tool.mode);
              }}
              type="button"
              title={tool.label}
            >
              <Icon size={20} />
            </button>
          );
        })}
        {!isClassification ? (
          <select aria-label="更多形状工具" onChange={(event) => setMode(event.target.value as ToolMode)} value={["point", "line", "circle", "linestrip", "points", "oriented_rectangle", "mask", "mask_erase"].includes(mode) ? mode : ""}>
            <option value="">更多形状</option>
            <option value="point">点</option>
            <option value="line">线</option>
            <option value="circle">圆</option>
            <option value="linestrip">折线</option>
            <option value="points">多点</option>
            <option value="oriented_rectangle">旋转框</option>
            <option value="mask">掩码笔刷</option>
            <option value="mask_erase">掩码橡皮</option>
          </select>
        ) : null}
        {mode === "mask" || mode === "mask_erase" ? (
          <label className="mask-brush-size">笔刷
            <input aria-label="掩码笔刷大小" type="number" min={2} max={200} value={maskBrushSize} onChange={(event) => setMaskBrushSize(clamp(Number(event.target.value) || 2, 2, 200))} />
          </label>
        ) : null}
      </aside>
      <section className="workspace-area">
        <div
          className="annotation-toolbar"
          data-tauri-drag-region={!mobileRuntime ? true : undefined}
          onMouseDown={mobileRuntime ? undefined : beginDesktopWindowDrag}
        >
          <div className="annotation-toolbar-context">
            {onBack ? <button aria-label="返回数据管理" title="返回数据管理" type="button" onClick={onBack} data-no-drag><ArrowLeft size={17} /></button> : null}
            <h1 aria-label="标注工作台" title={activeImage?.fileName}>{activeImage?.fileName ?? "标注"}</h1>
            <span className="annotation-state-tag">{annotationStatusLabels[annotationStatus] ?? annotationStatus}{isVerified ? " · 已验证" : ""}</span>
            {dirty ? <span className="dirty-state">未保存</span> : null}
          </div>
          <div className="annotation-actions" data-no-drag>
            {saveMessage ? <span className="annotation-save-feedback" role="status" title={saveMessage}>{saveMessage}</span> : null}
            {mobileRuntime ? (
              <button
                aria-controls="annotation-inspector"
                aria-expanded={mobileInspectorOpen}
                className="mobile-inspector-toggle"
                type="button"
                onClick={() => setMobileInspectorOpen((current) => !current)}
              >
                {mobileInspectorOpen ? "关闭属性" : "对象属性"}
              </button>
            ) : null}
            <button aria-label="撤销" title="撤销 (Ctrl+Z)" type="button" disabled={editor.past.length === 0} onClick={() => {
              dispatchEditor({ type: "undo" });
              setDirty(true);
            }}><Undo2 size={16} /></button>
            <button aria-label="重做" title="重做 (Ctrl+Shift+Z)" type="button" disabled={editor.future.length === 0} onClick={() => {
              dispatchEditor({ type: "redo" });
              setDirty(true);
            }}><Redo2 size={16} /></button>
            <button type="button" aria-label="上一张" title="上一张" onClick={() => goToImage(-1)} disabled={!activeImage || images.findIndex((image) => image.id === activeImage.id) <= 0}>
              <ChevronLeft size={16} />
            </button>
            <button type="button" aria-label="下一张" title="下一张" onClick={() => goToImage(1)} disabled={!activeImage || images.findIndex((image) => image.id === activeImage.id) >= images.length - 1}>
              <ChevronRight size={16} />
            </button>
            <button aria-label="保存标注" title="保存 (Ctrl+S)" className="mobile-primary-action mobile-save-action primary" type="button" onClick={() => save()}>
              <Save size={16} />
              保存
            </button>
            <button type="button" aria-label="保存并下一张" title="保存并下一张" onClick={() => save({ next: true })}>
              保存/下一张
            </button>
            <details className="annotation-more-actions">
              <summary aria-label="更多标注操作" title="更多标注操作"><MoreHorizontal size={16} /></summary>
              <div className="annotation-more-menu">
            <label className="inline-toggle">
              <input type="checkbox" checked={saveAndNext} onChange={(event) => setSaveAndNext(event.target.checked)} />
              保存后下一张
            </label>
            <label className="inline-toggle">
              <input type="checkbox" checked={autoSaveOnSwitch} onChange={(event) => setAutoSaveOnSwitch(event.target.checked)} />
              自动保存
            </label>
            <button aria-label="复制上一张框" type="button" onClick={() => void copyPreviousBoxes()} disabled={activeImageIndex <= 0}>复制上一张</button>
            <button className="mobile-primary-action mobile-submit-action" type="button" onClick={submit}>
              <ClipboardCheck size={16} />
              提交质检
            </button>
            <button type="button" aria-label={isVerified ? "取消验证" : "验证图片"} onClick={() => void toggleVerified()}>
              <CheckCircle2 size={16} />
              {isVerified ? "取消验证" : "验证图片"}
            </button>
              </div>
            </details>
            {showWindowControls ? (
              <span className="annotation-window-controls">
                <button aria-label="最小化标注工作台" type="button" onClick={() => runDesktopCommand("minimize_window")}>
                  <Minus size={16} />
                </button>
                <button aria-label="最大化标注工作台" type="button" onClick={() => runDesktopCommand("toggle_maximize_window")}>
                  <Maximize2 size={16} />
                </button>
                <button aria-label="关闭标注工作台" type="button" onClick={() => void closeWorkspace()}>
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
              className={`annotation-canvas ${!["select", "pan"].includes(mode) ? "drawing" : ""} ${mode === "pan" || panState ? "panning" : ""}`}
              data-testid="annotation-canvas"
              onDoubleClick={handleCanvasDoubleClick}
              height={canvasSize.height}
              onPointerDown={handleCanvasPointerDown}
              onPointerMove={handleCanvasPointerMove}
              onPointerUp={handleCanvasPointerUp}
              onPointerCancel={handleCanvasPointerCancel}
              onMouseDown={handleCanvasMouseDown}
              onMouseMove={handleCanvasMouseMove}
              onMouseUp={handleCanvasMouseUp}
              onMouseLeave={handleCanvasMouseUp}
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
              <button aria-label="图像适配宽度" type="button" onClick={fitImageToWidth}>适宽</button>
              <button aria-label="重置为原始大小" type="button" onClick={resetImageZoom}>
                1:1
              </button>
              <button aria-label="放大图像" type="button" onClick={() => zoomImage(1.25)}>
                <ZoomIn size={16} />
              </button>
              <span className="zoom-readout">{Math.round(viewport.scale * 100)}%</span>
              <label className="canvas-brightness">亮度
                <input aria-label="图像亮度" type="range" min={25} max={200} value={brightness} onChange={(event) => setBrightness(Number(event.target.value))} />
                {brightness}%
              </label>
            </div>
          </div>
        </div>
        <div className="filmstrip">
          {filmstripImages.map((image) => (
            <button className={image.id === activeImage?.id ? "active" : ""} key={image.id} type="button" onClick={() => selectImage(image.id)}>
              <div className="mini-thumb">
                {filmstripUrls[image.id] ? <img alt={`${image.fileName} 缩略图`} src={filmstripUrls[image.id]} /> : null}
              </div>
              <span>{image.fileName}</span>
            </button>
          ))}
        </div>
      </section>
      <aside className={`inspector${mobileRuntime ? ` mobile-inspector${mobileInspectorOpen ? " is-mobile-open" : ""}` : ""}`} id="annotation-inspector">
        {mobileRuntime ? (
          <div className="mobile-inspector-header">
            <strong>属性</strong>
            <button
              aria-label="关闭对象属性"
              className="mobile-inspector-close"
              type="button"
              onClick={() => setMobileInspectorOpen(false)}
            >
              关闭
            </button>
          </div>
        ) : null}
        {!mobileRuntime && aiPanelOpen && !isClassification ? (
          <section className="ai-annotation-panel" aria-label="AI 标注">
            <h2>AI 标注</h2>
            <label>推理方式
              <select aria-label="AI 推理方式" value={aiProvider} onChange={(event) => setAiProvider(event.target.value as "onnx" | "osam")}>
                <option value="onnx">ONNX 检测</option>
                <option value="osam">点 / 框 / 文本提示</option>
              </select>
            </label>
            {aiProvider === "onnx" ? (
              <>
                <button type="button" onClick={() => void chooseAiModel()}>选择 ONNX 模型</button>
                <span className="ai-model-path">{aiModelPath || "尚未选择模型"}</span>
                <label>检测模型布局
                  <select aria-label="检测模型布局" value={aiLayout} onChange={(event) => setAiLayout(event.target.value as "yolo8" | "yolo5" | "xyxy")}>
                    <option value="yolo8">YOLOv8 / 11</option>
                    <option value="yolo5">YOLOv5</option>
                    <option value="xyxy">端到端 XYXY</option>
                  </select>
                </label>
              </>
            ) : (
              <>
                <label>OSAM 模型
                  <input aria-label="OSAM 模型" value={aiModelName} onChange={(event) => setAiModelName(event.target.value)} />
                </label>
                <label>输出形状
                  <select aria-label="AI 输出形状" value={aiOutputFormat} onChange={(event) => setAiOutputFormat(event.target.value as typeof aiOutputFormat)}>
                    <option value="polygon">多边形</option>
                    <option value="rectangle">矩形</option>
                    <option value="mask">掩码</option>
                    <option value="oriented_rectangle">旋转框</option>
                    <option value="circle">圆</option>
                  </select>
                </label>
                <label>提示类型
                  <select aria-label="AI 提示类型" value={aiPromptType} onChange={(event) => setAiPromptType(event.target.value as "points" | "text")}>
                    <option value="points">点 / 框</option>
                    <option value="text">文本</option>
                  </select>
                </label>
                {aiPromptType === "points" ? (
                  <div className="ai-prompt-tools">
                    <label>提示结果类别
                      <select aria-label="AI 提示结果类别" value={aiPromptClassId} onChange={(event) => setAiPromptClassId(Number(event.target.value))}>
                        {workspaceDetail?.classes.map((item, index) => <option key={item.id ?? index} value={item.id ?? index}>{item.label}</option>)}
                      </select>
                    </label>
                    <button type="button" aria-pressed={mode === "ai_point_positive"} onClick={() => setMode("ai_point_positive")}>正点</button>
                    <button type="button" aria-pressed={mode === "ai_point_negative"} onClick={() => setMode("ai_point_negative")}>负点</button>
                    <button type="button" aria-pressed={mode === "ai_box"} onClick={() => setMode("ai_box")}>提示框</button>
                    <button type="button" onClick={() => setAiPromptPoints([])}>清除提示</button>
                    <span>已添加 {aiPromptPoints.length} 个提示点</span>
                  </div>
                ) : (
                  <label>文本类别（逗号分隔）
                    <input aria-label="AI 文本提示" value={aiTexts} onChange={(event) => setAiTexts(event.target.value)} />
                  </label>
                )}
              </>
            )}
            <label>置信度阈值
              <input aria-label="AI 置信度阈值" type="number" min="0" max="1" step="0.01" value={aiScoreThreshold} onChange={(event) => setAiScoreThreshold(Number(event.target.value))} />
            </label>
            <label>IoU 阈值
              <input aria-label="AI IoU 阈值" type="number" min="0" max="1" step="0.01" value={aiIouThreshold} onChange={(event) => setAiIouThreshold(Number(event.target.value))} />
            </label>
            {aiStatus ? <p role="status">{aiStatus}</p> : null}
            {aiBusy ? <button type="button" onClick={() => void cancelAiInference()}>取消 AI 推理</button>
              : <button className="primary" type="button" onClick={() => void runAiInference()}>{aiProvider === "osam" ? "下载所需模型并运行 AI 标注" : "运行 AI 标注"}</button>}
          </section>
        ) : null}
        {isClassification ? (
          <>
            <h2>图片分类</h2>
            <label className="classification-field">
              <span>分类标签</span>
              <select
                aria-label="图片分类"
                onChange={(event) => assignClassification(event.target.value)}
                value={classificationObject?.classId ?? ""}
              >
                <option value="">未分类</option>
                {workspaceDetail?.classes.map((item) => (
                  <option key={item.id ?? item.label} value={item.id}>
                    {item.label}
                  </option>
                ))}
              </select>
            </label>
          </>
        ) : null}
        <h2>对象</h2>
        <div className="object-view-actions">
          <button type="button" onClick={() => { setSelectedObjectIds(new Set(objects.map((object) => object.id))); setSelectedObjectId(objects[0]?.id ?? null); }} disabled={!objects.length}>全选</button>
          <button type="button" onClick={deleteAllObjects} disabled={!objects.length}>删除全部</button>
          <button type="button" onClick={() => setHiddenObjectIds(new Set())}>显示全部</button>
          <button type="button" onClick={() => setHiddenObjectIds(new Set(objects.map((object) => object.id)))}>隐藏全部</button>
          <button type="button" onClick={focusSelectedObject} disabled={!selectedObject}>聚焦选中</button>
        </div>
        <div className="object-list-controls">
          <input aria-label="搜索对象" placeholder="搜索类别、形状或 ID" value={objectQuery} onChange={(event) => setObjectQuery(event.target.value)} />
          <select aria-label="对象排序" value={objectSort} onChange={(event) => setObjectSort(event.target.value as typeof objectSort)}>
            <option value="drawing">绘制顺序</option>
            <option value="class">按类别</option>
            <option value="type">按形状</option>
          </select>
        </div>
        {displayedObjects.map((object) => (
          <div className="object-row-shell" key={object.id}>
            <button
              aria-pressed={selectedObjectIds.has(object.id)}
              className={`object-row ${selectedObjectIds.has(object.id) ? "selected" : ""} ${hiddenObjectIds.has(object.id) ? "hidden" : ""}`}
              onClick={(event) => selectObject(object.id, event.ctrlKey || event.metaKey || event.shiftKey)}
              type="button"
            >
              <span className="dot" />
              <span>{object.label}</span>
              <em>{object.type}</em>
            </button>
            <button aria-label={`${hiddenObjectIds.has(object.id) ? "显示" : "隐藏"} ${object.label}`} className="object-visibility" onClick={() => toggleObjectVisibility(object.id)} title={hiddenObjectIds.has(object.id) ? "显示对象" : "隐藏对象"} type="button">
              <Eye size={15} />
            </button>
          </div>
        ))}
        <h2>属性</h2>
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
              disabled={!selectedObject || selectedObject.type === "classification"}
              onChange={(event) => updateSelectedLabel(event.target.value)}
              ref={selectedLabelInputRef}
              value={selectedObject?.label ?? ""}
            />
          </dd>
          <dt>类别</dt>
          <dd>
            <select aria-label="对象类别" disabled={!selectedObject || selectedObject.type === "classification"} onChange={(event) => updateSelectedClass(Number(event.target.value))} value={selectedObject?.classId ?? ""}>
              {workspaceDetail?.classes.map((item) => <option key={item.id} value={item.id}>{item.label}</option>)}
            </select>
          </dd>
        </dl>
        <label className="inline-toggle">
          <input aria-label="困难样本" checked={selectedObject?.attributes.difficult === true} disabled={!selectedObject || selectedObject.type === "classification"} onChange={(event) => updateSelectedDifficult(event.target.checked)} type="checkbox" />
          困难样本
        </label>
        {selectedObject && selectedObject.type !== "classification" ? (
          <details className="annotation-extra-fields">
            <summary>更多属性</summary>
            <div className="annotation-metadata-editor">
              <label>对象分组
                <input aria-label="对象分组" type="number" min={0} value={typeof selectedObject.attributes.groupId === "number" ? selectedObject.attributes.groupId : ""} onChange={(event) => updateSelectedAttribute("groupId", event.target.value === "" ? null : Number(event.target.value))} />
              </label>
              <label>对象描述
                <textarea aria-label="对象描述" value={typeof selectedObject.attributes.description === "string" ? selectedObject.attributes.description : ""} onChange={(event) => updateSelectedAttribute("description", event.target.value)} />
              </label>
              <label>对象标志
                <input aria-label="对象标志" title="用逗号分隔多个标志" value={selectedObject.attributes.flags && typeof selectedObject.attributes.flags === "object" ? Object.entries(selectedObject.attributes.flags as Record<string, unknown>).filter(([, enabled]) => enabled === true).map(([key]) => key).join(", ") : ""} onChange={(event) => updateSelectedAttribute("flags", Object.fromEntries(event.target.value.split(",").map((flag) => flag.trim()).filter(Boolean).map((flag) => [flag, true])))} />
              </label>
              <label>对象颜色
                <input aria-label="对象颜色" type="color" value={typeof selectedObject.attributes.lineColor === "string" && /^#[0-9a-fA-F]{6}$/.test(selectedObject.attributes.lineColor) ? selectedObject.attributes.lineColor : "#cc54d8"} onChange={(event) => updateSelectedAttribute("lineColor", event.target.value)} />
              </label>
            </div>
          </details>
        ) : null}
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
        {!mobileRuntime ? (
          <details className="annotation-export-panel">
            <summary>导入/导出</summary>
            <button type="button" onClick={() => void loadExternalAnnotationFile()}>加载外部标注</button>
            <label className="annotation-export-field">标注导出格式
              <select aria-label="标注导出格式" value={annotationExportFormat} onChange={(event) => setAnnotationExportFormat(event.target.value)}>
                <option value="labelme">LabelMe JSON</option>
                <option value="createml">CreateML JSON</option>
                <option value="voc">Pascal VOC XML</option>
                <option value="yolo">YOLO 检测 TXT</option>
                <option value="yolo-seg">YOLO 分割 TXT</option>
              </select>
            </label>
            <button className="primary" type="button" onClick={() => void exportCurrentAnnotations()}>另存标注文件</button>
          </details>
        ) : null}
      </aside>
      {unsavedPrompt ? (
        <div className="modal-backdrop annotation-unsaved-backdrop">
          <section className="dataset-dialog annotation-unsaved-dialog" role="dialog" aria-modal="true" aria-labelledby="annotation-unsaved-title" aria-describedby="annotation-unsaved-message">
            <div className="dialog-header">
              <h2 id="annotation-unsaved-title">未保存的标注</h2>
            </div>
            <p id="annotation-unsaved-message">{unsavedPrompt.message}</p>
            {unsavedPrompt.error ? <div className="data-projects-feature-error" role="alert">{unsavedPrompt.error}</div> : null}
            <div className="dialog-actions">
              <button type="button" disabled={unsavedDecisionBusy} onClick={() => void handleUnsavedDecision("stay")}>继续标注</button>
              <button type="button" disabled={unsavedDecisionBusy} onClick={() => void handleUnsavedDecision("discard")}>放弃更改</button>
              <button className="primary" type="button" disabled={unsavedDecisionBusy} onClick={() => void handleUnsavedDecision("save")}>{unsavedDecisionBusy ? "保存中…" : "保存后继续"}</button>
            </div>
          </section>
        </div>
      ) : null}
    </main>
  );
}

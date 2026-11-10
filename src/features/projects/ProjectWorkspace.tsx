import { CheckCircle2, CircleAlert, Eye, FolderOpen, ImageIcon, Play, ShieldCheck } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { flushOfflineQueue } from "../../api/mobile";
import { createAnnotationTask, migrateLegacyProjectFolders } from "../../api/tauri";
import {
  createDatasetSnapshot,
  createProjectFolder,
  deleteProjectFolder,
  exportDataset,
  getProjectDetail,
  isBackendUnavailableError,
  listClassSamples,
  listExports,
  listProjectImages,
  listProjectFolders,
  listSnapshots,
  moveImageToProjectFolder,
  openAnnotationWindow,
  renameProjectFolder,
  upgradeDatasetSnapshotBridge,
} from "../../api/tauri";
import { HybridProjectPanel } from "./HybridProjectPanel";
import { ImagePreviewDialog, ThumbnailAnnotationOverlay, useImageAnnotations } from "./ImagePreview";
import { displayStatusLabel } from "./statusLabels";
import type { ClassSample, ClassStat, DatasetExport, DatasetImage, DatasetSnapshot, FolderWorkspace, AnnotationObject, ProjectDetail } from "../../types/domain";
import { annotationColor } from "../annotation/geometry";
import { RuntimeStatePanel } from "../shared/RuntimeStatePanel";
import { useImageAssetUrls } from "../shared/useImageAssetUrls";
import { ProjectTabs } from "./ProjectTabs";
import type { ProjectTopbarContext } from "./types";
import { projectTabs, type ProjectTab } from "../../shared/routes";
import { navigate } from "../../shared/navigation";
import { datasetViewStateKey, readDatasetViewState, saveDatasetViewState } from "./viewState";
import "./project-workspace.css";

function formatNumber(value: number) { return value.toLocaleString("zh-CN"); }
const projectImagePageSize = 48;
type TaskSelectionMode = "all" | "custom";

async function listAllProjectImageIds(projectId: string, imageCount: number): Promise<string[]> {
  const ids: string[] = [];
  for (let offset = 0; offset < imageCount; offset += projectImagePageSize) {
    const page = await listProjectImages(projectId, undefined, {
      offset,
      limit: projectImagePageSize,
    });
    if (page.length === 0) break;
    ids.push(...page.map((image) => image.id));
    if (page.length < projectImagePageSize) break;
  }
  return Array.from(new Set(ids));
}

type ImageFolderState = {
  names: string[];
  assignments: Record<string, string>;
};

function readImageFolderState(projectId: string): ImageFolderState {
  try {
    const stored = window.localStorage.getItem(`image-folders:${projectId}`);
    if (!stored) return { names: [], assignments: {} };
    const parsed = JSON.parse(stored) as Partial<ImageFolderState>;
    return {
      names: Array.isArray(parsed.names) ? parsed.names.filter((name): name is string => typeof name === "string") : [],
      assignments: parsed.assignments && typeof parsed.assignments === "object"
        ? parsed.assignments as Record<string, string>
        : {},
    };
  } catch {
    return { names: [], assignments: {} };
  }
}

export function ProjectWorkspace({
  projectId,
  routeTab,
  mobileRuntime,
  onProjectContextChange,
  browserRuntime = false,
  onAnnotate,
  onBack,
  onTabChange,
  viewScopeKey,
}: {
  projectId: string;
  routeTab?: ProjectTab;
  mobileRuntime?: boolean;
  onProjectContextChange: (context: ProjectTopbarContext | null) => void;
  browserRuntime?: boolean;
  onAnnotate?: (datasetId: string, imageId?: string) => void;
  onBack?: () => void;
  onTabChange?: (tab: ProjectTab) => void;
  viewScopeKey?: string;
}) {
  const viewKey = datasetViewStateKey(projectId, viewScopeKey);
  const initialView = readDatasetViewState(viewKey);
  const projectMainRef = useRef<HTMLElement | null>(null);
  const restoreScrollRef = useRef<number | null>(initialView.scrollTop);
  const activeProjectIdRef = useRef(projectId);
  activeProjectIdRef.current = projectId;
  const upgradeRequestRef = useRef<{ projectId: string; snapshotId: string } | null>(null);
  if (upgradeRequestRef.current?.projectId !== projectId) {
    upgradeRequestRef.current = null;
  }
  const [detail, setDetail] = useState<ProjectDetail | null>(null);
  const [images, setImages] = useState<DatasetImage[]>([]);
  const [snapshots, setSnapshots] = useState<DatasetSnapshot[]>([]);
  const [upgradingSnapshotId, setUpgradingSnapshotId] = useState<string | null>(null);
  const [exports, setExports] = useState<DatasetExport[]>([]);
  const [workflowMessage, setWorkflowMessage] = useState<string | null>(null);
  const [tab, setTab] = useState<ProjectTab>(routeTab ?? "概览");
  const [imagePage, setImagePage] = useState(initialView.page);
  const [previewImageId, setPreviewImageId] = useState<string | null>(null);
  const [selectedImageId, setSelectedImageId] = useState<string | null>(initialView.selectedImageId);
  const [imageClassFilter, setImageClassFilter] = useState(initialView.classFilter);
  const [imageStatusFilter, setImageStatusFilter] = useState(initialView.statusFilter);
  const [imageFolderFilter, setImageFolderFilter] = useState(initialView.folderFilter);
  const [imageFolderWorkspace, setImageFolderWorkspace] = useState<FolderWorkspace>({ folders: [], members: [] });
  const [loadError, setLoadError] = useState<{ title: string; message: string } | null>(null);
  const [selectedClass, setSelectedClass] = useState<ClassStat | null>(null);
  const [classSamples, setClassSamples] = useState<ClassSample[]>([]);
  const [classSamplePage, setClassSamplePage] = useState(0);
  const [classSampleState, setClassSampleState] = useState<"idle" | "loading" | "error">("idle");
  const [classSampleMessage, setClassSampleMessage] = useState<string | null>(null);
  const [taskName, setTaskName] = useState("");
  const [taskImageIds, setTaskImageIds] = useState<string[]>([]);
  const [taskAllImageIds, setTaskAllImageIds] = useState<string[]>([]);
  const [taskSelectionMode, setTaskSelectionMode] = useState<TaskSelectionMode>("all");
  const [taskImageCatalogReady, setTaskImageCatalogReady] = useState(false);
  const [taskCreationState, setTaskCreationState] = useState<"idle" | "creating" | "created" | "queued" | "error">("idle");
  const [taskCreationMessage, setTaskCreationMessage] = useState<string | null>(null);
  const taskSelectionModeRef = useRef<TaskSelectionMode>("all");
  const classSampleImages = useMemo(() => classSamples.map((sample) => sample.image), [classSamples]);
  // Overview renders six thumbnails and twelve density samples. Hidden tabs
  // must not fan out a whole page of filesystem/IPC requests.
  const previewingPage = previewImageId !== null && images.some((image) => image.id === previewImageId);
  const pageVisible = tab === "图片" || previewingPage;
  const imageUrls = useImageAssetUrls(projectId, images, pageVisible ? images.length : tab === "概览" ? 6 : 0);
  const imageAnnotations = useImageAnnotations(projectId, images, pageVisible ? images.length : tab === "概览" ? 12 : 0);
  const classSamplesVisible = tab === "类别" || (previewImageId !== null && !previewingPage);
  const classSampleUrls = useImageAssetUrls(projectId, classSampleImages, classSamplesVisible ? classSampleImages.length : 0);
  const classSampleAnnotations = useImageAnnotations(projectId, classSampleImages, classSamplesVisible ? classSampleImages.length : 0);
  const previewImage =
    images.find((image) => image.id === previewImageId)
    ?? classSampleImages.find((image) => image.id === previewImageId)
    ?? null;
  const safeImageFolderWorkspace = imageFolderWorkspace ?? { folders: [], members: [] };
  const folderForImage = (image: DatasetImage) =>
    safeImageFolderWorkspace.members.find((member) => member.imageId === image.id)?.folderId ?? "";
  const imageFolders = safeImageFolderWorkspace.folders;
  const folderFilteredImages = imageFolderFilter === "all"
    ? images
    : images.filter((image) => folderForImage(image) === imageFolderFilter);

  useEffect(() => {
    setUpgradingSnapshotId(null);
    const saved = readDatasetViewState(viewKey);
    setImagePage(saved.page);
    setSelectedImageId(saved.selectedImageId);
    setImageClassFilter(saved.classFilter);
    setImageStatusFilter(saved.statusFilter);
    setImageFolderFilter(saved.folderFilter);
    restoreScrollRef.current = saved.scrollTop;
    setSelectedClass(null);
    setClassSamples([]);
    setClassSamplePage(0);
    if (browserRuntime) {
      setImageFolderWorkspace({ folders: [], members: [] });
      return;
    }
    const legacyFolders = mobileRuntime ? { names: [], assignments: {} } : readImageFolderState(projectId);
    const migrationKey = `image-folders-migrated:${projectId}`;
    const needsMigration = !mobileRuntime && window.localStorage.getItem(migrationKey) !== "1"
      && (legacyFolders.names.length > 0 || Object.keys(legacyFolders.assignments).length > 0);
    const loadFolders = needsMigration
      ? migrateLegacyProjectFolders(projectId, legacyFolders.names, legacyFolders.assignments)
          .then((workspace) => {
            if (!mobileRuntime) window.localStorage.setItem(migrationKey, "1");
            return workspace;
          })
      : listProjectFolders(projectId);
    loadFolders
      .then((workspace) => setImageFolderWorkspace(workspace ?? { folders: [], members: [] }))
      .catch((error) => setWorkflowMessage(error instanceof Error ? error.message : String(error)));
  }, [browserRuntime, mobileRuntime, projectId, viewKey]);

  useEffect(() => {
    saveDatasetViewState(viewKey, {
      page: imagePage, classFilter: imageClassFilter, statusFilter: imageStatusFilter,
      folderFilter: imageFolderFilter, selectedImageId,
      scrollTop: restoreScrollRef.current ?? projectMainRef.current?.scrollTop ?? 0,
    });
  }, [imagePage, imageClassFilter, imageStatusFilter, imageFolderFilter, selectedImageId, viewKey]);

  useEffect(() => {
    taskSelectionModeRef.current = "all";
    setTaskSelectionMode("all");
    setTaskImageIds([]);
    setTaskAllImageIds([]);
    setTaskImageCatalogReady(false);
  }, [projectId]);

  useEffect(() => {
    if (tab !== "任务" || detail?.project.id !== projectId) return;

    let cancelled = false;
    setTaskImageCatalogReady(false);
    const expectedImageCount = Math.max(0, detail.project.imageCount);
    if (expectedImageCount === 0) {
      setTaskAllImageIds([]);
      setTaskImageIds((current) => taskSelectionModeRef.current === "all" ? [] : current);
      setTaskImageCatalogReady(true);
      return;
    }

    void listAllProjectImageIds(projectId, expectedImageCount)
      .then((ids) => {
        if (cancelled) return;
        setTaskAllImageIds(ids);
        setTaskImageIds((current) => taskSelectionModeRef.current === "all"
          ? ids
          : current.filter((imageId) => ids.includes(imageId)));
        setTaskImageCatalogReady(true);
      })
      .catch(() => {
        if (cancelled) return;
        // Keep the native all-project default available even if the optional
        // catalog read fails. Custom selection remains limited to the page
        // already loaded, while the untouched default still sends no IDs.
        const fallbackIds = images.map((image) => image.id);
        setTaskAllImageIds(fallbackIds);
        setTaskImageIds((current) => taskSelectionModeRef.current === "all"
          ? fallbackIds
          : current.filter((imageId) => fallbackIds.includes(imageId)));
        setTaskImageCatalogReady(true);
      });

    return () => {
      cancelled = true;
    };
  }, [detail?.project.id, detail?.project.imageCount, projectId, tab]);

  useEffect(() => {
    if (routeTab) {
      setTab(mobileRuntime && routeTab === "导出" ? "概览" : routeTab);
    }
  }, [mobileRuntime, routeTab]);

  const visibleProjectTabs = browserRuntime
    ? projectTabs.filter((item) => ["图片", "类别", "任务", "质检"].includes(item))
    : mobileRuntime
    ? projectTabs.filter((item) => item !== "导出")
    : projectTabs;

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
    let cancelled = false;
    setLoadError(null);
    onProjectContextChange(null);
    Promise.all([
      getProjectDetail(projectId),
      listProjectImages(projectId, undefined, {
        offset: imagePage * projectImagePageSize,
        limit: projectImagePageSize,
      }),
      browserRuntime ? Promise.resolve([]) : listSnapshots(projectId).catch(() => []),
      browserRuntime ? Promise.resolve([]) : listExports(projectId).catch(() => []),
    ])
      .then(([nextDetail, nextImages, nextSnapshots, nextExports]) => {
        if (cancelled) return;
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
        if (cancelled) return;
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
    return () => { cancelled = true; };
  }, [projectId, imagePage, onProjectContextChange, browserRuntime]);

  useEffect(() => {
    if (!detail || detail.project.id !== projectId || restoreScrollRef.current === null) return;
    if (projectMainRef.current) projectMainRef.current.scrollTop = restoreScrollRef.current;
    restoreScrollRef.current = null;
  }, [detail, projectId]);

  async function handleCreateSnapshot() {
    if (!detail) return;
    const snapshot = await createDatasetSnapshot(
      projectId,
      `${detail.project.name} 快照 ${snapshots.length + 1}`,
    );
    setSnapshots((current) => [snapshot, ...current]);
    setWorkflowMessage(`已创建快照 ${snapshot.name}`);
  }

  async function refreshProjectDetail() {
    const nextDetail = await getProjectDetail(projectId);
    if (activeProjectIdRef.current === projectId) {
      setDetail(nextDetail);
    }
  }

  async function handleCreateTask() {
    const name = taskName.trim();
    const allTaskImagesSelected = taskSelectionMode === "all"
      || (taskAllImageIds.length > 0
        && taskImageIds.length === taskAllImageIds.length
        && taskAllImageIds.every((imageId) => taskImageIds.includes(imageId)));
    const selectedImageIds = allTaskImagesSelected ? undefined : taskImageIds;
    if (!name) {
      setTaskCreationState("error");
      setTaskCreationMessage("请输入任务名称。");
      return;
    }
    if (!allTaskImagesSelected && taskImageIds.length === 0) {
      setTaskCreationState("error");
      setTaskCreationMessage("至少选择一张图片。");
      return;
    }

    setTaskCreationState("creating");
    setTaskCreationMessage(null);
    try {
      await createAnnotationTask(projectId, name, selectedImageIds);
    } catch (error) {
      setTaskCreationState("error");
      setTaskCreationMessage(`任务创建失败：${error instanceof Error ? error.message : String(error)}`);
      return;
    }

    setTaskName("");
    try {
      const syncStatus = await flushOfflineQueue(projectId);
      if (syncStatus.lastError || syncStatus.queuedOperations > 0) {
        setTaskCreationState("queued");
        setTaskCreationMessage(`任务已保存，待同步（队列 ${syncStatus.queuedOperations} 项）${syncStatus.lastError ? `：${syncStatus.lastError}` : ""}`);
      } else {
        setTaskCreationState("created");
        setTaskCreationMessage("任务已创建并同步。");
      }
    } catch (error) {
      setTaskCreationState("queued");
      setTaskCreationMessage(`任务已保存，待同步：${error instanceof Error ? error.message : String(error)}`);
    }

    try {
      await refreshProjectDetail();
    } catch {
      setTaskCreationMessage((current) => `${current ?? "任务已保存"}；任务列表刷新失败，请稍后刷新页面。`);
    }
  }

  async function handleUpgradeSnapshotBridge(snapshotId: string) {
    if (upgradeRequestRef.current) {
      return;
    }
    const requestProjectId = projectId;
    const request = { projectId: requestProjectId, snapshotId };
    upgradeRequestRef.current = request;
    setUpgradingSnapshotId(snapshotId);
    setWorkflowMessage(null);
    try {
      const upgraded = await upgradeDatasetSnapshotBridge(requestProjectId, snapshotId);
      if (
        activeProjectIdRef.current !== requestProjectId
        || upgradeRequestRef.current !== request
      ) {
        return;
      }
      setSnapshots((current) =>
        current.map((snapshot) => snapshot.id === upgraded.id ? upgraded : snapshot),
      );
      setWorkflowMessage(`训练桥接已就绪：${upgraded.name}`);
    } catch (error) {
      if (
        activeProjectIdRef.current !== requestProjectId
        || upgradeRequestRef.current !== request
      ) {
        return;
      }
      setWorkflowMessage(
        `训练桥接升级失败：${error instanceof Error ? error.message : String(error)}`,
      );
    } finally {
      if (upgradeRequestRef.current !== request) {
        return;
      }
      upgradeRequestRef.current = null;
      if (activeProjectIdRef.current === requestProjectId) {
        setUpgradingSnapshotId(null);
      }
    }
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
    if (onAnnotate) {
      onAnnotate(projectId, imageId);
      return;
    }
    if (mobileRuntime) {
      navigate(`#/annotate/${projectId}/${imageId}`);
      return;
    }
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

  async function createImageFolder() {
    const name = window.prompt("输入新文件夹名称")?.trim();
    if (!name || name === "all" || imageFolders.some((folder) => folder.name === name)) return;
    try {
      const next = await createProjectFolder(projectId, name);
      setImageFolderWorkspace(next);
      setImageFolderFilter(next.folders.find((folder) => folder.name === name)?.id ?? "all");
    } catch (error) {
      setWorkflowMessage(error instanceof Error ? error.message : String(error));
    }
  }

  async function renameImageFolder() {
    if (imageFolderFilter === "all") return;
    const folder = imageFolders.find((item) => item.id === imageFolderFilter);
    if (!folder) return;
    const previousName = folder.name;
    const nextName = window.prompt("重命名文件夹", previousName)?.trim();
    if (!nextName || nextName === previousName || nextName === "all" || imageFolders.some((item) => item.name === nextName)) return;
    try {
      setImageFolderWorkspace(await renameProjectFolder(projectId, folder.id, nextName));
    } catch (error) {
      setWorkflowMessage(error instanceof Error ? error.message : String(error));
    }
  }

  async function deleteImageFolder() {
    if (imageFolderFilter === "all") return;
    const folder = imageFolders.find((item) => item.id === imageFolderFilter);
    if (!folder || !window.confirm(`删除文件夹“${folder.name}”？其中图片将移至“未分组”。`)) return;
    try {
      setImageFolderWorkspace(await deleteProjectFolder(projectId, folder.id));
      setImageFolderFilter("all");
    } catch (error) {
      setWorkflowMessage(error instanceof Error ? error.message : String(error));
    }
  }

  async function moveSelectedImageToFolder(folderId: string) {
    if (!selectedImageId || !folderId) return;
    try {
      setImageFolderWorkspace(await moveImageToProjectFolder(projectId, selectedImageId, folderId));
      const folderName = imageFolders.find((folder) => folder.id === folderId)?.name ?? folderId;
      setWorkflowMessage(`已将选中图片移动到“${folderName}”`);
    } catch (error) {
      setWorkflowMessage(error instanceof Error ? error.message : String(error));
    }
  }

  if (loadError) {
    return (
      <main className="project-page">
        <section className="project-main">
          <RuntimeStatePanel title={loadError.title} message={loadError.message} actionLabel="返回数据管理" onAction={onBack ?? (() => navigate("#/datasets"))} />
        </section>
      </main>
    );
  }

  if (!detail) {
    return <main className="project-page"><section className="project-main">加载数据集...</section></main>;
  }

  return (
    <main className="project-page">
      <section className="project-main" ref={projectMainRef} onScroll={() => saveDatasetViewState(viewKey, { page: imagePage, classFilter: imageClassFilter, statusFilter: imageStatusFilter, folderFilter: imageFolderFilter, selectedImageId, scrollTop: projectMainRef.current?.scrollTop ?? 0 })}>
        <div className="project-sticky-header">
          <ProjectTabs tabs={visibleProjectTabs} activeTab={tab} onChange={(next) => { setTab(next); onTabChange?.(next); }} />
        </div>
        <section className={`project-surface ${tab === "概览" ? "overview-surface" : ""}`}>
          {tab === "图片" && !browserRuntime ? (
            <div className="image-folder-toolbar" aria-label="图片文件夹操作">
              <div className="image-folder-list" role="tablist" aria-label="图片文件夹">
                <button
                  aria-selected={imageFolderFilter === "all"}
                  className={imageFolderFilter === "all" ? "active" : ""}
                  role="tab"
                  type="button"
                  onClick={() => setImageFolderFilter("all")}
                >
                  <FolderOpen size={14} />
                  全部
                  <strong>{images.length}</strong>
                </button>
                {imageFolders.map((folder) => (
                  <button
                    aria-selected={imageFolderFilter === folder.id}
                    className={imageFolderFilter === folder.id ? "active" : ""}
                    key={folder.id}
                    role="tab"
                    type="button"
                    onClick={() => setImageFolderFilter(folder.id)}
                  >
                    <FolderOpen size={14} />
                    {folder.name}
                    <strong>{folder.imageCount}</strong>
                  </button>
                ))}
              </div>
              <div className="image-folder-actions">
                <button type="button" onClick={createImageFolder}>新建文件夹</button>
                <button disabled={imageFolderFilter === "all"} type="button" onClick={renameImageFolder}>重命名</button>
                <button disabled={imageFolderFilter === "all"} type="button" onClick={deleteImageFolder}>删除</button>
                <select
                  aria-label="移动选中图片到文件夹"
                  disabled={!selectedImageId || imageFolders.length === 0}
                  value=""
                  onChange={(event) => moveSelectedImageToFolder(event.target.value)}
                >
                  <option value="">移动选中图片至…</option>
                  {imageFolders.map((folder) => <option key={folder.id} value={folder.id}>{folder.name}</option>)}
                </select>
              </div>
            </div>
          ) : null}
          {renderProjectTab(
            tab,
            detail,
            tab === "图片" ? folderFilteredImages : images,
            imageUrls,
            imageAnnotations,
            {
            classSampleAnnotations,
            classSampleMessage,
            classSamplePage,
            classSampleState,
            classSampleUrls,
            classSamples,
            exports,
            imageClassFilter,
            imagePage,
            imagePageSize: projectImagePageSize,
            imageStatusFilter,
            selectedImageId,
            selectedClass,
            snapshots,
            taskCreationMessage,
            taskCreationState,
            taskImageCatalogReady,
            taskImageCount: detail.project.imageCount,
            taskImageIds,
            taskSelectionMode,
            taskName,
            upgradingSnapshotId,
            workflowMessage,
            onClassSamplePageChange: setClassSamplePage,
            onCreateSnapshot: handleCreateSnapshot,
            onUpgradeSnapshotBridge: handleUpgradeSnapshotBridge,
            onExport: handleExport,
            onImageClassFilterChange: setImageClassFilter,
            onImagePageChange: setImagePage,
            onImageStatusFilterChange: setImageStatusFilter,
            onOpenClassSample: openAnnotationConsole,
            onOpenAnnotation: openAnnotationConsole,
            onPreviewImage: setPreviewImageId,
            onSelectImage: setSelectedImageId,
            onCreateTask: handleCreateTask,
            onTaskImageToggle: (imageId, checked) => {
              if (!taskImageCatalogReady) return;
              taskSelectionModeRef.current = "custom";
              setTaskSelectionMode("custom");
              setTaskImageIds((current) => checked
                ? (current.includes(imageId) ? current : [...current, imageId])
                : current.filter((id) => id !== imageId));
            },
            onTaskNameChange: setTaskName,
            onTaskSelectAll: () => {
              if (!taskImageCatalogReady) return;
              taskSelectionModeRef.current = "all";
              setTaskSelectionMode("all");
              setTaskImageIds(taskAllImageIds);
            },
            onTaskClearAll: () => {
              taskSelectionModeRef.current = "custom";
              setTaskSelectionMode("custom");
              setTaskImageIds([]);
            },
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
            images={images.some((candidate) => candidate.id === previewImage.id) ? images : classSampleImages}
            imageUrls={{ ...imageUrls, ...classSampleUrls }}
            objects={imageAnnotations[previewImage.id] ?? classSampleAnnotations[previewImage.id]}
            onAnnotate={() => openAnnotationConsole(previewImage.id)}
            onClose={() => setPreviewImageId(null)}
            onSelectImage={setPreviewImageId}
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
    upgradingSnapshotId: string | null;
    exports: DatasetExport[];
    imageClassFilter: string;
    imagePage: number;
    imagePageSize: number;
    imageStatusFilter: string;
    selectedImageId: string | null;
    taskCreationMessage: string | null;
    taskCreationState: "idle" | "creating" | "created" | "queued" | "error";
    taskImageCatalogReady: boolean;
    taskImageCount: number;
    taskImageIds: string[];
    taskSelectionMode: TaskSelectionMode;
    taskName: string;
    workflowMessage: string | null;
    onSelectClass: (row: ClassStat) => void;
    onClassSamplePageChange: (page: number) => void;
    onOpenClassSample: (imageId: string) => void;
    onOpenAnnotation: (imageId: string) => void;
    onCreateSnapshot: () => void;
    onCreateTask: () => void;
    onUpgradeSnapshotBridge: (snapshotId: string) => void;
    onExport: (format: "yolo" | "coco") => void;
    onImageClassFilterChange: (value: string) => void;
    onImagePageChange: (page: number) => void;
    onImageStatusFilterChange: (value: string) => void;
    onPreviewImage: (imageId: string) => void;
    onSelectImage: (imageId: string | null) => void;
    onTaskImageToggle: (imageId: string, checked: boolean) => void;
    onTaskNameChange: (value: string) => void;
    onTaskSelectAll: () => void;
    onTaskClearAll: () => void;
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
    case "图片": {
      const classCounts = new Map<string, number>();
      const pageObjectCount = images.reduce((total, image) => {
        const imageObjects = imageAnnotations[image.id] ?? [];
        imageObjects.forEach((object) => {
          classCounts.set(object.label, (classCounts.get(object.label) ?? 0) + 1);
        });
        return total + imageObjects.length;
      }, 0);
      const classOptions = Array.from(classCounts.entries())
        .sort((left, right) => right[1] - left[1] || left[0].localeCompare(right[0]));
      const statusOptions = Array.from(new Set(images.map((image) => image.status))).sort();
      const filteredImages = images.filter((image) => {
        const matchesStatus = workflow.imageStatusFilter === "all" || image.status === workflow.imageStatusFilter;
        const matchesClass = workflow.imageClassFilter === "all"
          || (imageAnnotations[image.id] ?? []).some((object) => object.label === workflow.imageClassFilter);
        return matchesStatus && matchesClass;
      });

      return (
        <div className="image-browser">
          <div className="image-browser-header">
            <div className="image-browser-toolbar">
              <div className="image-browser-heading">
                <h2>图片</h2>
                <div className="image-browser-stats" aria-label="图片统计">
                <span><strong>{formatNumber(detail.project.imageCount)}</strong><small>图像</small></span>
                <span><strong>{filteredImages.length}</strong><small>结果</small></span>
                <span><strong>{pageObjectCount}</strong><small>对象</small></span>
                <span><strong>{classOptions.length}</strong><small>类别</small></span>
                </div>
              </div>
              <div aria-label="图片筛选和分页" className="image-browser-controls">
                <div aria-label="按状态筛选" className="image-status-filter">
                  <button
                    aria-pressed={workflow.imageStatusFilter === "all"}
                    className={workflow.imageStatusFilter === "all" ? "active" : ""}
                    type="button"
                    onClick={() => workflow.onImageStatusFilterChange("all")}
                  >
                    全部状态
                  </button>
                  {statusOptions.map((status) => (
                    <button
                      aria-pressed={workflow.imageStatusFilter === status}
                      className={workflow.imageStatusFilter === status ? "active" : ""}
                      key={status}
                      type="button"
                      onClick={() => workflow.onImageStatusFilterChange(status)}
                    >
                      {displayStatusLabel(status)}
                    </button>
                  ))}
                </div>
                <div aria-label="按类别筛选" className="image-class-filter">
                  <button
                    aria-pressed={workflow.imageClassFilter === "all"}
                    className={workflow.imageClassFilter === "all" ? "active" : ""}
                    type="button"
                    onClick={() => workflow.onImageClassFilterChange("all")}
                  >
                    全部
                    <strong>{images.length}</strong>
                  </button>
                  {classOptions.map(([label, count]) => (
                    <button
                      aria-pressed={workflow.imageClassFilter === label}
                      className={workflow.imageClassFilter === label ? "active" : ""}
                      key={label}
                      type="button"
                      onClick={() => workflow.onImageClassFilterChange(label)}
                    >
                      <i style={{ backgroundColor: annotationColor(label) }} />
                      {label}
                      <strong>{count}</strong>
                    </button>
                  ))}
                </div>
                <button
                  className="image-filter-reset"
                  disabled={workflow.imageClassFilter === "all" && workflow.imageStatusFilter === "all"}
                  type="button"
                  onClick={() => {
                    workflow.onImageClassFilterChange("all");
                    workflow.onImageStatusFilterChange("all");
                  }}
                >
                  重置
                </button>
                <div className="pager-actions">
                  <span>第 {workflow.imagePage + 1} 页 / {formatNumber(detail.project.imageCount)} 张</span>
                  <button type="button" onClick={() => workflow.onImagePageChange(Math.max(0, workflow.imagePage - 1))} disabled={workflow.imagePage === 0}>上一页</button>
                  <button
                    type="button"
                    onClick={() => workflow.onImagePageChange(workflow.imagePage + 1)}
                    disabled={(workflow.imagePage + 1) * workflow.imagePageSize >= detail.project.imageCount}
                  >
                    下一页
                  </button>
                </div>
              </div>
            </div>
          </div>
          {filteredImages.length > 0 ? (
          <div aria-label="图片选择列表" className="image-grid selectable-image-grid" role="listbox">
            {filteredImages.map((image) => {
              const imageObjects = imageAnnotations[image.id] ?? [];
              const labels = Array.from(new Set(imageObjects.map((object) => object.label)));
              const selected = workflow.selectedImageId === image.id;
              return (
              <article
                aria-label={`${image.fileName}，${imageObjects.length} 个检测对象`}
                aria-selected={selected}
                className={`image-tile ${selected ? "selected" : ""}`}
                key={image.id}
                onClick={() => workflow.onSelectImage(image.id)}
                onDoubleClick={() => workflow.onPreviewImage(image.id)}
                tabIndex={0}
                role="option"
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    workflow.onPreviewImage(image.id);
                  } else if (event.key === " ") {
                    event.preventDefault();
                    workflow.onSelectImage(image.id);
                  }
                }}
              >
                <div className="sample-thumb traffic-a">
                  {imageUrls[image.id] ? (
                    <img alt={image.fileName} decoding="async" loading="lazy" src={imageUrls[image.id]} />
                  ) : null}
                  <ThumbnailAnnotationOverlay image={image} objects={imageAnnotations[image.id]} />
                </div>
                <div className="image-tile-meta">
                  <span title={image.fileName}>{image.fileName}</span>
                </div>
                <div className="image-tile-classes">
                  {labels.slice(0, 3).map((label) => (
                    <span key={label}><i style={{ backgroundColor: annotationColor(label) }} />{label}</span>
                  ))}
                  {labels.length > 3 ? <span>+{labels.length - 3}</span> : null}
                  {labels.length === 0 ? <span className="empty">无检测对象</span> : null}
                  <strong aria-label={`${imageObjects.length} 个对象`} title={`${imageObjects.length} 个对象`}>{imageObjects.length}</strong>
                </div>
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
                  <button
                    type="button"
                    onClick={(event) => {
                      event.stopPropagation();
                      workflow.onOpenAnnotation(image.id);
                    }}
                  >
                    标记
                  </button>
                </div>
              </article>
              );
            })}
          </div>
          ) : (
            <div className="image-filter-empty">
              <ImageIcon size={22} />
              <strong>没有符合筛选条件的图片</strong>
              <span>重置类别或状态筛选后继续浏览。</span>
            </div>
          )}
        </div>
      );
    }
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
    case "任务": {
      const selectedImageCount = workflow.taskSelectionMode === "all"
        ? workflow.taskImageCount
        : workflow.taskImageIds.length;
      const allImagesSelected = workflow.taskSelectionMode === "all"
        || (workflow.taskImageCount > 0 && selectedImageCount === workflow.taskImageCount);
      return (
        <div className="task-workspace">
          <div className="tab-title-stack">
            <h2>生产任务</h2>
          </div>
          <form className="task-creation-card" aria-label="创建生产任务" onSubmit={(event) => {
            event.preventDefault();
            workflow.onCreateTask();
          }}>
            <div className="task-creation-header">
              <div>
                <span className="eyebrow">TASK SETUP</span>
                <h3>创建任务</h3>
              </div>
              <span className="task-selection-count">已选 {selectedImageCount} / {workflow.taskImageCount} 张</span>
            </div>
            <label className="task-name-field" htmlFor="project-task-name">
              任务名称
              <input
                id="project-task-name"
                value={workflow.taskName}
                onChange={(event) => workflow.onTaskNameChange(event.target.value)}
                placeholder="例如：第一轮标注"
              />
            </label>
            <div className="task-image-selection-header">
              <strong>选择图片</strong>
              <div className="action-row">
                <button type="button" onClick={workflow.onTaskSelectAll} disabled={!workflow.taskImageCatalogReady || workflow.taskImageCount === 0 || allImagesSelected}>全选</button>
                <button type="button" onClick={workflow.onTaskClearAll} disabled={!workflow.taskImageCatalogReady || selectedImageCount === 0}>清空</button>
              </div>
            </div>
            {!workflow.taskImageCatalogReady ? <p className="empty-state">正在读取项目图片…</p> : workflow.taskImageCount === 0 ? <p className="empty-state">当前项目暂无可分配图片。</p> : images.length === 0 ? <p className="empty-state">当前页没有图片，可使用默认的全项目选择创建任务。</p> : (
              <div className="task-image-picker" role="group" aria-label="任务图片选择">
                {images.map((image) => (
                  <label className="task-image-option" key={image.id}>
                    <input
                      type="checkbox"
                      aria-label={`选择 ${image.fileName}`}
                      checked={workflow.taskSelectionMode === "all" || workflow.taskImageIds.includes(image.id)}
                      disabled={!workflow.taskImageCatalogReady}
                      onChange={(event) => workflow.onTaskImageToggle(image.id, event.target.checked)}
                    />
                    <span>{image.fileName}</span>
                  </label>
                ))}
              </div>
            )}
            <div className="task-creation-actions">
              <button className="primary" type="submit" disabled={workflow.taskCreationState === "creating" || !workflow.taskImageCatalogReady || workflow.taskImageCount === 0}>
                {workflow.taskCreationState === "creating" ? "创建中…" : "创建任务"}
              </button>
              {workflow.taskCreationMessage ? (
                <p className={`workflow-message task-creation-message is-${workflow.taskCreationState}`} role="status">
                  {workflow.taskCreationMessage}
                </p>
              ) : null}
            </div>
          </form>
          <div className="data-table" aria-label="生产任务列表">
            {detail.tasks.length === 0 ? <p className="empty-state">暂无生产任务</p> : null}
            {detail.tasks.map((task) => (
              <div className="table-row" key={task.name}>
                <strong>{task.name}</strong>
                <span>{task.owner}</span>
                <span>{displayStatusLabel(task.status)}</span>
                <span>{task.progress}%</span>
              </div>
            ))}
          </div>
        </div>
      );
    }
    case "质检":
      return (
        <div>
          <h2>质检队列</h2>
          {detail.project.issueCount === 0 ? <p>暂无质检问题</p> : null}
          <HybridProjectPanel projectId={detail.project.id} />
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
                  <strong>{displayStatusLabel(item.status)}</strong>
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
                  <span>{snapshot.bridgeStatus === "ready" ? "训练桥接已就绪" : "训练桥接待生成"}</span>
                  {snapshot.bridgeStatus !== "ready" ? (
                    <button
                      disabled={workflow.upgradingSnapshotId !== null}
                      type="button"
                      onClick={() => workflow.onUpgradeSnapshotBridge(snapshot.id)}
                    >
                      {workflow.upgradingSnapshotId === snapshot.id
                        ? "正在生成训练桥接…"
                        : snapshot.bridgeStatus === "invalid"
                          ? "重新生成训练桥接"
                          : "升级训练桥接"}
                    </button>
                  ) : null}
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
  const densitySamples = images.slice(0, 12);
  const densityValues = densitySamples.map((image) => imageAnnotations[image.id]?.length ?? 0);
  const densityMax = Math.max(1, ...densityValues);
  const densityAverage = densityValues.length > 0
    ? densityValues.reduce((sum, value) => sum + value, 0) / densityValues.length
    : 0;
  const densityPoints = densityValues.map((value, index) => ({
    x: 18 + index * (604 / Math.max(1, densityValues.length - 1)),
    y: 116 - value / densityMax * 86,
  }));
  const densityLine = densityPoints.reduce((path, point, index) => {
    if (index === 0) return `M ${point.x.toFixed(1)} ${point.y.toFixed(1)}`;
    const previous = densityPoints[index - 1];
    const controlX = (previous.x + point.x) / 2;
    return `${path} C ${controlX.toFixed(1)} ${previous.y.toFixed(1)}, ${controlX.toFixed(1)} ${point.y.toFixed(1)}, ${point.x.toFixed(1)} ${point.y.toFixed(1)}`;
  }, "");
  const densityArea = densityPoints.length > 0
    ? `${densityLine} L ${densityPoints[densityPoints.length - 1].x.toFixed(1)} 124 L ${densityPoints[0].x.toFixed(1)} 124 Z`
    : "";
  const completionAngle = Math.max(0, Math.min(360, project.annotatedPercent * 3.6));

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

      <section className="overview-section overview-analytics" aria-label="数据趋势">
        <div className="overview-section-header compact">
          <div>
            <span className="section-kicker">ANALYTICS</span>
            <h2>数据趋势</h2>
          </div>
          <span className="chart-scope">当前加载样本</span>
        </div>
        <div className="overview-chart-grid">
          <article className="overview-chart-card">
            <div className="chart-card-heading">
              <div>
                <strong>对象密度</strong>
                <span>最近 {densitySamples.length} 个样本的标注对象数</span>
              </div>
              <div className="chart-value">
                <strong>{densityAverage.toFixed(1)}</strong>
                <span>平均对象 / 图</span>
              </div>
            </div>
            <div className="density-chart">
              <svg aria-label="对象密度曲线" role="img" viewBox="0 0 640 132">
                <defs>
                  <linearGradient id="density-chart-fill" x1="0" x2="0" y1="0" y2="1">
                    <stop offset="0%" stopColor="#1769e0" stopOpacity="0.24" />
                    <stop offset="100%" stopColor="#1769e0" stopOpacity="0.02" />
                  </linearGradient>
                </defs>
                <line className="chart-grid-line" x1="18" x2="622" y1="30" y2="30" />
                <line className="chart-grid-line" x1="18" x2="622" y1="73" y2="73" />
                <line className="chart-grid-line" x1="18" x2="622" y1="116" y2="116" />
                {densityArea ? <path className="density-area" d={densityArea} /> : null}
                {densityLine ? <path className="density-line" d={densityLine} /> : null}
                {densityPoints.length > 0 ? (
                  <circle
                    className="density-last-point"
                    cx={densityPoints[densityPoints.length - 1].x}
                    cy={densityPoints[densityPoints.length - 1].y}
                    r="4"
                  />
                ) : null}
              </svg>
              <div className="chart-axis">
                <span>样本 1</span>
                <span>样本 {Math.max(1, densitySamples.length)}</span>
              </div>
            </div>
          </article>

          <article className="overview-chart-card status-chart-card">
            <div className="chart-card-heading">
              <div>
                <strong>工作状态</strong>
                <span>标注、待处理与审核分布</span>
              </div>
            </div>
            <div className="status-chart">
              <div
                aria-label={`标注完成度 ${project.annotatedPercent}%`}
                className="status-donut"
                style={{ background: `conic-gradient(#1769e0 0deg ${completionAngle}deg, #e8edf4 ${completionAngle}deg 360deg)` }}
              >
                <span><strong>{project.annotatedPercent}%</strong><small>完成</small></span>
              </div>
              <div className="status-chart-legend">
                <span><i className="complete" /><b>已标注</b><strong>{formatNumber(annotatedCount)}</strong></span>
                <span><i className="pending" /><b>待标注</b><strong>{formatNumber(remainingCount)}</strong></span>
                <span><i className="review" /><b>待审核</b><strong>{formatNumber(project.reviewCount)}</strong></span>
              </div>
            </div>
          </article>
        </div>
      </section>

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
                    <small>{displayStatusLabel(image.status)}</small>
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

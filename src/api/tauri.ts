import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import type {
  AnnotationObject,
  AnnotationSaveResult,
  AnnotationState,
  DatasetExport,
  BackendTask,
  BuiltinDataset,
  ClassSample,
  DatasetImage,
  DatasetProject,
  DatasetSnapshot,
  DataSourceAnalysis,
  DownloadJob,
  ProjectDetail,
} from "../types/domain";

export class BackendUnavailableError extends Error {
  readonly cause: unknown;

  constructor(command: string, cause: unknown) {
    super(`Tauri backend unavailable while calling ${command}`);
    this.name = "BackendUnavailableError";
    this.cause = cause;
  }
}

const localBackendBaseUrl = "http://127.0.0.1:17310";

export type BackendRuntime = "tauri-desktop" | "standalone-backend";

export type BackendHealth = {
  status: string;
  service: string;
  version: string;
  runtime: BackendRuntime;
  capabilities: string[];
};

export type BackendConnection =
  | { mode: "checking"; label: string; health: null }
  | { mode: "tauri"; label: string; health: BackendHealth }
  | { mode: "web-local-desktop"; label: string; health: BackendHealth }
  | { mode: "web-standalone-backend"; label: string; health: BackendHealth }
  | { mode: "unavailable"; label: string; health: null };

export function isBackendUnavailableError(error: unknown): error is BackendUnavailableError {
  return error instanceof BackendUnavailableError;
}

export async function detectBackendConnection(): Promise<BackendConnection> {
  try {
    const health = await invoke<BackendHealth>("backend_health");
    return { mode: "tauri", label: "Tauri 内部", health };
  } catch (error) {
    if (!looksLikeMissingTauriBackend(error)) {
      return { mode: "unavailable", label: "后端未连接", health: null };
    }
  }

  try {
    const response = await fetch(`${localBackendBaseUrl}/api/health`, { method: "GET" });
    const payload = await response.json();
    if (!response.ok || payload?.ok === false) {
      throw new Error(payload?.error ?? `HTTP ${response.status}`);
    }
    const health = payload.data as BackendHealth;
    if (health.runtime === "tauri-desktop") {
      return { mode: "web-local-desktop", label: "已连接桌面后台", health };
    }
    return { mode: "web-standalone-backend", label: "已连接本地后台", health };
  } catch {
    return { mode: "unavailable", label: "后端未连接", health: null };
  }
}

export async function listBuiltinDatasets(): Promise<BuiltinDataset[]> {
  return invokeRequired("list_builtin_datasets");
}

export async function downloadTestDataset(datasetKey: string): Promise<DownloadJob> {
  return invokeRequired("download_test_dataset", { datasetKey });
}

export async function listDatasetProjects(): Promise<DatasetProject[]> {
  return invokeRequired("list_dataset_projects");
}

export async function getProjectDetail(projectId: string): Promise<ProjectDetail> {
  return invokeRequired("get_project_detail", { projectId });
}

export async function listProjectImages(
  projectId: string,
  groupId?: string,
  page?: { offset?: number; limit?: number },
): Promise<DatasetImage[]> {
  return invokeRequired("list_project_images", {
    projectId,
    groupId: groupId ?? null,
    offset: page?.offset ?? null,
    limit: page?.limit ?? null,
  });
}

export async function listClassSamples(
  projectId: string,
  query: { classId?: number; label: string; offset?: number; limit?: number },
): Promise<ClassSample[]> {
  return invokeRequired("list_class_samples", {
    projectId,
    classId: query.classId ?? null,
    label: query.label,
    offset: query.offset ?? null,
    limit: query.limit ?? null,
  });
}

export async function getFileAssetUrl(projectId: string, imageId: string): Promise<string> {
  try {
    const path = await invoke<string>("get_file_asset_path", { projectId, imageId });
    return convertFileSrc(path);
  } catch (error) {
    if (looksLikeMissingTauriBackend(error)) {
      return `${localBackendBaseUrl}/api/assets/${encodeURIComponent(projectId)}/${encodeURIComponent(imageId)}`;
    }
    throw error;
  }
}

export async function getImageAnnotations(
  projectId: string,
  imageId: string,
): Promise<AnnotationObject[]> {
  return invokeRequired("get_image_annotations", { projectId, imageId });
}

export async function getImageAnnotationState(
  projectId: string,
  imageId: string,
): Promise<AnnotationState> {
  return invokeRequired("get_image_annotation_state", { projectId, imageId });
}

export async function saveImageAnnotations(
  projectId: string,
  imageId: string,
  revision: string | null,
  objects: AnnotationObject[],
): Promise<AnnotationSaveResult> {
  return invokeRequired("save_image_annotations", { projectId, imageId, revision, objects });
}

export async function submitImageAnnotations(projectId: string, imageId: string): Promise<void> {
  await invokeRequired("submit_image_annotations", { projectId, imageId });
}

export async function openAnnotationWindow(projectId: string, imageId?: string): Promise<void> {
  await invokeRequired("open_annotation_window", { projectId, imageId: imageId ?? null });
}

export async function createDatasetProject(
  name: string,
  datasetType: string,
  demoTemplate: string,
): Promise<DatasetProject> {
  return invokeRequired("create_dataset_project", { name, datasetType, demoTemplate });
}

export async function createProject(name: string, datasetType: string): Promise<DatasetProject> {
  return invokeRequired("create_project", { name, datasetType });
}

export async function importImages(
  projectId: string,
  sourcePath: string,
): Promise<DatasetProject> {
  return invokeRequired("import_images", { projectId, sourcePath });
}

export async function importYoloDataset(
  projectId: string,
  sourcePath: string,
): Promise<DatasetProject> {
  return invokeRequired("import_yolo_dataset", { projectId, sourcePath });
}

export async function pickDataSource(selectionType: "folder" | "files"): Promise<string[] | null> {
  return invokeRequired("pick_data_source", { selectionType });
}

export async function analyzeDataSource(sourcePaths: string[]): Promise<DataSourceAnalysis> {
  return invokeRequired("analyze_data_source", { sourcePaths });
}

export async function importFiles(
  projectId: string,
  sourcePaths: string[],
): Promise<DatasetProject> {
  return invokeRequired("import_files", { projectId, sourcePaths });
}

export async function openLocalDataset(
  sourcePath: string,
  datasetType: string,
): Promise<DatasetProject> {
  return invokeRequired("open_local_dataset", { sourcePath, datasetType });
}

export async function rescanProjectAssets(projectId: string): Promise<DatasetProject> {
  return invokeRequired("rescan_project_assets", { projectId });
}

export async function generateThumbnails(projectId: string): Promise<number> {
  return invokeRequired("generate_thumbnails", { projectId });
}

export async function listBackendTasks(): Promise<BackendTask[]> {
  return invokeRequired("list_backend_tasks");
}

export async function clearCompletedBackendTasks(): Promise<void> {
  await invokeRequired("clear_completed_backend_tasks");
}

export async function getBackendTask(taskId: string): Promise<BackendTask | null> {
  return invokeRequired("get_backend_task", { taskId });
}

export async function retryBackendTask(taskId: string): Promise<void> {
  await invokeRequired("retry_backend_task", { taskId });
}

export async function openBackendTaskTray(): Promise<void> {
  await invokeRequired("open_backend_task_tray");
}

export async function listSnapshots(projectId: string): Promise<DatasetSnapshot[]> {
  return invokeRequired("list_snapshots", { projectId });
}

export async function createDatasetSnapshot(
  projectId: string,
  name: string,
): Promise<DatasetSnapshot> {
  return invokeRequired("create_dataset_snapshot", { projectId, name });
}

export async function listExports(projectId: string): Promise<DatasetExport[]> {
  return invokeRequired("list_exports", { projectId });
}

export async function exportDataset(
  projectId: string,
  snapshotId: string,
  format: "yolo" | "coco",
): Promise<DatasetExport> {
  return invokeRequired("export_dataset", { projectId, snapshotId, format });
}

async function invokeRequired<T>(
  command: string,
  args?: Record<string, unknown>,
): Promise<T> {
  try {
    return args === undefined ? await invoke<T>(command) : await invoke<T>(command, args);
  } catch (error) {
    if (looksLikeMissingTauriBackend(error)) {
      return invokeLocalBackend<T>(command, args, error);
    }
    throw error;
  }
}

async function invokeLocalBackend<T>(
  command: string,
  args: Record<string, unknown> | undefined,
  tauriCause: unknown,
): Promise<T> {
  try {
    const response = await fetch(`${localBackendBaseUrl}/api/invoke/${command}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(args ?? {}),
    });
    const payload = await response.json();
    if (!response.ok || payload?.ok === false) {
      throw new Error(payload?.error ?? `HTTP ${response.status}`);
    }
    return payload.data as T;
  } catch (error) {
    throw new BackendUnavailableError(command, {
      tauri: tauriCause,
      http: error,
    });
  }
}

function looksLikeMissingTauriBackend(error: unknown) {
  const message = error instanceof Error ? error.message : String(error);
  return /tauri|__TAURI__|ipc|invoke|not available|unavailable/i.test(message);
}

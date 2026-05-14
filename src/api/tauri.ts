import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import type {
  AnnotationObject,
  BackendTask,
  BuiltinDataset,
  DatasetImage,
  DatasetProject,
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

export function isBackendUnavailableError(error: unknown): error is BackendUnavailableError {
  return error instanceof BackendUnavailableError;
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
): Promise<DatasetImage[]> {
  return invokeRequired("list_project_images", { projectId, groupId: groupId ?? null });
}

export async function getFileAssetUrl(projectId: string, imageId: string): Promise<string> {
  const path = await invokeRequired<string>("get_file_asset_path", { projectId, imageId });
  return convertFileSrc(path);
}

export async function getImageAnnotations(
  projectId: string,
  imageId: string,
): Promise<AnnotationObject[]> {
  return invokeRequired("get_image_annotations", { projectId, imageId });
}

export async function saveImageAnnotations(
  projectId: string,
  imageId: string,
  objects: AnnotationObject[],
): Promise<void> {
  await invokeRequired("save_image_annotations", { projectId, imageId, objects });
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

async function invokeRequired<T>(
  command: string,
  args?: Record<string, unknown>,
): Promise<T> {
  try {
    return args === undefined ? await invoke<T>(command) : await invoke<T>(command, args);
  } catch (error) {
    if (looksLikeMissingTauriBackend(error)) {
      throw new BackendUnavailableError(command, error);
    }
    throw error;
  }
}

function looksLikeMissingTauriBackend(error: unknown) {
  const message = error instanceof Error ? error.message : String(error);
  return /tauri|__TAURI__|ipc|invoke|not available|unavailable/i.test(message);
}

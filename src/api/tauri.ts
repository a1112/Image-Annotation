import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { getBackendProfile } from "./backend-profile";
import type {
  AnnotationObject,
  AnnotationSaveResult,
  AnnotationState,
  DatasetExport,
  DatasetFormat,
  ExportOptions,
  BackendTask,
  BuiltinDataset,
  ClassSample,
  DatasetImage,
  DatasetProject,
  DatasetSnapshot,
  DataSourceAnalysis,
  DownloadJob,
  ProjectDetail,
  RemoteImport,
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

export type BackendRuntime = "tauri-desktop" | "standalone-backend" | "standalone";

export type BackendHealth = {
  status: string;
  service: string;
  version: string;
  runtime: BackendRuntime;
  capabilities: string[];
};

export type SourceSyncResult = {
  path: string;
  sourceVersion: string;
};

type RemoteSample = {
  id: string;
  fileName: string;
  width: number;
  height: number;
  split: string;
  status: string;
  qaStatus: string;
  reviewNote: string | null;
  tags: string[];
  classes: Array<{ id: number; label: string; objectCount: number }>;
};

type RemoteSamplePage = {
  offset: number;
  limit: number;
  total: number;
  items: RemoteSample[];
};

type RemoteEnvelope<T> = {
  ok?: boolean;
  data?: T;
  error?: { code?: string; message?: string };
  requestId?: string;
};

export type BackendConnection =
  | { mode: "checking"; label: string; health: null }
  | { mode: "tauri"; label: string; health: BackendHealth }
  | { mode: "web-local-desktop"; label: string; health: BackendHealth }
  | { mode: "web-standalone-backend"; label: string; health: BackendHealth }
  | { mode: "remote"; label: string; health: BackendHealth; role: string }
  | { mode: "unavailable"; label: string; health: null };

export class BackendApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    readonly requestId: string | null,
    message: string,
  ) {
    super(message);
    this.name = "BackendApiError";
  }
}

export class BackendRevisionConflictError extends BackendApiError {
  constructor(requestId: string | null, message: string) {
    super(409, "revision_conflict", requestId, message);
    this.name = "BackendRevisionConflictError";
  }
}

export class DesktopOnlyActionError extends Error {
  constructor(action: string) {
    super(`${action} 仅支持本机桌面后端`);
    this.name = "DesktopOnlyActionError";
  }
}

export function isBackendUnavailableError(error: unknown): error is BackendUnavailableError {
  return error instanceof BackendUnavailableError;
}

export async function detectBackendConnection(): Promise<BackendConnection> {
  const profile = getBackendProfile();
  if (profile.mode === "remote") {
    return {
      mode: "remote",
      label: `远程服务 · ${profile.role}`,
      role: profile.role,
      health: {
        status: "ok",
        service: "image-annotation-server",
        version: "remote",
        runtime: "standalone",
        capabilities: ["remote-samples", "authenticated"],
      },
    };
  }
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
  if (isRemoteProfile()) return [];
  return invokeRequired("list_builtin_datasets");
}

export async function downloadTestDataset(datasetKey: string): Promise<DownloadJob> {
  requireLocalProfile("内置数据集下载");
  return invokeRequired("download_test_dataset", { datasetKey });
}

export async function listDatasetProjects(): Promise<DatasetProject[]> {
  if (isRemoteProfile()) return remoteJson<DatasetProject[]>("/projects");
  return invokeRequired("list_dataset_projects");
}

export async function getProjectDetail(projectId: string): Promise<ProjectDetail> {
  if (isRemoteProfile()) {
    const project = await remoteJson<DatasetProject>(`/projects/${segment(projectId)}`);
    const samples = await remoteJson<RemoteSamplePage>(
      `/projects/${segment(projectId)}/samples?offset=0&limit=500`,
    );
    const classes = new Map<number, { label: string; count: number }>();
    for (const sample of samples.items) {
      for (const item of sample.classes) {
        const current = classes.get(item.id) ?? { label: item.label, count: 0 };
        current.count += item.objectCount;
        classes.set(item.id, current);
      }
    }
    return {
      project,
      tagGroups: [],
      classes: Array.from(classes.entries()).map(([id, item]) => ({
        id,
        label: item.label,
        color: classColor(id),
        count: item.count,
        attributes: [],
      })),
      tasks: [],
      qualityChecks: [],
      exportPresets: [],
    };
  }
  return invokeRequired("get_project_detail", { projectId });
}

export async function listProjectImages(
  projectId: string,
  groupId?: string,
  page?: { offset?: number; limit?: number },
): Promise<DatasetImage[]> {
  if (isRemoteProfile()) {
    const query = new URLSearchParams();
    if (groupId) query.set("split", groupId);
    if (page?.offset !== undefined) query.set("offset", String(page.offset));
    if (page?.limit !== undefined) query.set("limit", String(page.limit));
    const result = await remoteJson<RemoteSamplePage>(
      `/projects/${segment(projectId)}/samples?${query.toString()}`,
    );
    return result.items.map(remoteSampleImage);
  }
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
  if (isRemoteProfile()) {
    const params = new URLSearchParams({ label: query.label });
    if (query.classId !== undefined) params.set("classId", String(query.classId));
    if (query.offset !== undefined) params.set("offset", String(query.offset));
    if (query.limit !== undefined) params.set("limit", String(query.limit));
    const result = await remoteJson<RemoteSamplePage>(
      `/projects/${segment(projectId)}/samples?${params.toString()}`,
    );
    return result.items.map((sample) => ({
      image: remoteSampleImage(sample),
      matchCount:
        sample.classes.find((item) =>
          query.classId !== undefined ? item.id === query.classId : item.label === query.label,
        )?.objectCount ?? 0,
    }));
  }
  return invokeRequired("list_class_samples", {
    projectId,
    classId: query.classId ?? null,
    label: query.label,
    offset: query.offset ?? null,
    limit: query.limit ?? null,
  });
}

export async function getFileAssetUrl(projectId: string, imageId: string): Promise<string> {
  if (isRemoteProfile()) {
    const response = await remoteFetch(
      `/projects/${segment(projectId)}/samples/${segment(imageId)}/content`,
      { method: "GET" },
    );
    return URL.createObjectURL(await response.blob());
  }
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
  if (isRemoteProfile()) {
    return (await getImageAnnotationState(projectId, imageId)).objects;
  }
  return invokeRequired("get_image_annotations", { projectId, imageId });
}

export async function getImageAnnotationState(
  projectId: string,
  imageId: string,
): Promise<AnnotationState> {
  if (isRemoteProfile()) {
    return remoteJson(
      `/projects/${segment(projectId)}/samples/${segment(imageId)}/annotations`,
    );
  }
  return invokeRequired("get_image_annotation_state", { projectId, imageId });
}

export async function saveImageAnnotations(
  projectId: string,
  imageId: string,
  revision: string | null,
  objects: AnnotationObject[],
): Promise<AnnotationSaveResult> {
  if (isRemoteProfile()) {
    const state = await remoteJson<AnnotationState>(
      `/projects/${segment(projectId)}/samples/${segment(imageId)}/annotations`,
      {
        method: "PUT",
        headers: revision ? { "If-Match": `"${revision}"` } : undefined,
        body: JSON.stringify({ revision, objects }),
      },
    );
    return {
      revision: state.revision ?? "",
      savedAt: state.updatedAt ?? new Date().toISOString(),
      auditEventId: "remote",
    };
  }
  return invokeRequired("save_image_annotations", { projectId, imageId, revision, objects });
}

export async function submitImageAnnotations(projectId: string, imageId: string): Promise<void> {
  if (isRemoteProfile()) {
    await remoteJson(`/projects/${segment(projectId)}/samples/${segment(imageId)}/submit`, {
      method: "POST",
    });
    return;
  }
  await invokeRequired("submit_image_annotations", { projectId, imageId });
}

export async function openAnnotationWindow(projectId: string, imageId?: string): Promise<void> {
  requireLocalProfile("独立标注窗口");
  await invokeRequired("open_annotation_window", { projectId, imageId: imageId ?? null });
}

export async function createDatasetProject(
  name: string,
  datasetType: string,
  demoTemplate: string,
): Promise<DatasetProject> {
  if (isRemoteProfile()) {
    return remoteJson("/projects", {
      method: "POST",
      body: JSON.stringify({ name, datasetType }),
    });
  }
  return invokeRequired("create_dataset_project", { name, datasetType, demoTemplate });
}

export async function createProject(name: string, datasetType: string): Promise<DatasetProject> {
  if (isRemoteProfile()) {
    return remoteJson("/projects", {
      method: "POST",
      body: JSON.stringify({ name, datasetType }),
    });
  }
  return invokeRequired("create_project", { name, datasetType });
}

export async function importImages(
  projectId: string,
  sourcePath: string,
): Promise<DatasetProject> {
  requireLocalProfile("本机目录导入");
  return invokeRequired("import_images", { projectId, sourcePath });
}

export async function importYoloDataset(
  projectId: string,
  sourcePath: string,
): Promise<DatasetProject> {
  requireLocalProfile("本机 YOLO 导入");
  return invokeRequired("import_yolo_dataset", { projectId, sourcePath });
}

export async function pickDataSource(selectionType: "folder" | "files"): Promise<string[] | null> {
  requireLocalProfile("系统文件选择器");
  return invokeRequired("pick_data_source", { selectionType });
}

export async function analyzeDataSource(
  sourcePaths: string[],
  formatOverride?: DataSourceAnalysis["detectedFormat"],
): Promise<DataSourceAnalysis> {
  requireLocalProfile("本机数据分析");
  return invokeRequired("analyze_data_source", {
    sourcePaths,
    ...(formatOverride ? { formatOverride } : {}),
  });
}

export async function importFiles(
  projectId: string,
  sourcePaths: string[],
): Promise<DatasetProject> {
  requireLocalProfile("本机文件导入");
  return invokeRequired("import_files", { projectId, sourcePaths });
}

export async function openLocalDataset(
  sourcePath: string,
  datasetType: string,
): Promise<DatasetProject> {
  requireLocalProfile("打开本机数据集");
  return invokeRequired("open_local_dataset", { sourcePath, datasetType });
}

export async function rescanProjectAssets(projectId: string): Promise<DatasetProject> {
  requireLocalProfile("重新扫描本机资源");
  return invokeRequired("rescan_project_assets", { projectId });
}

export async function generateThumbnails(projectId: string): Promise<number> {
  requireLocalProfile("本机缩略图生成");
  return invokeRequired("generate_thumbnails", { projectId });
}

export async function listBackendTasks(): Promise<BackendTask[]> {
  if (isRemoteProfile()) return [];
  return invokeRequired("list_backend_tasks");
}

export async function clearCompletedBackendTasks(): Promise<void> {
  requireLocalProfile("本机后台任务");
  await invokeRequired("clear_completed_backend_tasks");
}

export async function getBackendTask(taskId: string): Promise<BackendTask | null> {
  requireLocalProfile("本机后台任务");
  return invokeRequired("get_backend_task", { taskId });
}

export async function retryBackendTask(taskId: string): Promise<void> {
  requireLocalProfile("本机后台任务");
  await invokeRequired("retry_backend_task", { taskId });
}

export async function openBackendTaskTray(): Promise<void> {
  requireLocalProfile("本机后台任务窗口");
  await invokeRequired("open_backend_task_tray");
}

export async function listSnapshots(projectId: string): Promise<DatasetSnapshot[]> {
  if (isRemoteProfile()) return [];
  return invokeRequired("list_snapshots", { projectId });
}

export async function createDatasetSnapshot(
  projectId: string,
  name: string,
): Promise<DatasetSnapshot> {
  requireLocalProfile("数据集快照");
  return invokeRequired("create_dataset_snapshot", { projectId, name });
}

export async function listExports(projectId: string): Promise<DatasetExport[]> {
  if (isRemoteProfile()) return [];
  return invokeRequired("list_exports", { projectId });
}

export async function exportDataset(
  projectId: string,
  snapshotId: string,
  options: ExportOptions,
): Promise<DatasetExport> {
  requireLocalProfile("数据集导出");
  return invokeRequired("export_dataset", { projectId, snapshotId, options });
}

export async function syncDatasetSource(projectId: string): Promise<SourceSyncResult> {
  requireLocalProfile("本机源标注同步");
  return invokeRequired("sync_dataset_source", { projectId });
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

export async function uploadRemoteImport(
  projectId: string,
  files: File[],
): Promise<RemoteImport> {
  if (!isRemoteProfile()) {
    throw new BackendApiError(400, "remote_profile_required", null, "请先连接远程服务");
  }
  if (!files.length) {
    throw new BackendApiError(400, "validation", null, "请选择要上传的文件");
  }
  const body = new FormData();
  for (const file of files) {
    const relativePath = (file as File & { webkitRelativePath?: string }).webkitRelativePath;
    body.append("files", file, relativePath || file.name);
  }
  return remoteJson(`/projects/${segment(projectId)}/imports`, {
    method: "POST",
    body,
  });
}

export async function commitRemoteImport(
  importId: string,
  format: DatasetFormat,
): Promise<RemoteImport> {
  return remoteJson(`/imports/${segment(importId)}/commit`, {
    method: "POST",
    body: JSON.stringify({ format }),
  });
}

export async function cancelRemoteImport(importId: string): Promise<RemoteImport> {
  return remoteJson(`/imports/${segment(importId)}`, { method: "DELETE" });
}

function isRemoteProfile(): boolean {
  return getBackendProfile().mode === "remote";
}

function requireLocalProfile(action: string): void {
  if (isRemoteProfile()) throw new DesktopOnlyActionError(action);
}

function classColor(id: number): string {
  const colors = ["#1769e0", "#14814c", "#b45309", "#9f3bb5", "#c2415d", "#0f766e"];
  return colors[Math.abs(id) % colors.length];
}

function segment(value: string): string {
  return encodeURIComponent(value);
}

function remoteSampleImage(sample: RemoteSample): DatasetImage {
  return {
    id: sample.id,
    fileName: sample.fileName,
    width: sample.width,
    height: sample.height,
    split: sample.split,
    status: sample.status,
    qaStatus: sample.qaStatus,
    reviewNote: sample.reviewNote,
    tags: sample.tags,
  };
}

async function remoteFetch(path: string, init: RequestInit = {}): Promise<Response> {
  const profile = getBackendProfile();
  if (profile.mode !== "remote") {
    throw new BackendUnavailableError("remote_request", "remote profile is not active");
  }
  const headers = new Headers(init.headers);
  headers.set("Authorization", `Bearer ${profile.token}`);
  if (init.body !== undefined && !(init.body instanceof FormData) && !headers.has("Content-Type")) {
    headers.set("Content-Type", "application/json");
  }
  const response = await fetch(`${profile.apiBaseUrl}${path}`, { ...init, headers });
  if (response.ok) return response;

  let envelope: RemoteEnvelope<unknown> = {};
  try {
    envelope = (await response.json()) as RemoteEnvelope<unknown>;
  } catch {
    // Preserve the HTTP status when a proxy returns a non-JSON error page.
  }
  const code = envelope.error?.code ?? "http_error";
  const message = envelope.error?.message ?? `HTTP ${response.status}`;
  const requestId = envelope.requestId ?? response.headers.get("x-request-id");
  if (response.status === 409 && code === "revision_conflict") {
    throw new BackendRevisionConflictError(requestId, message);
  }
  throw new BackendApiError(response.status, code, requestId, message);
}

async function remoteJson<T>(path: string, init: RequestInit = {}): Promise<T> {
  const response = await remoteFetch(path, init);
  const envelope = (await response.json()) as RemoteEnvelope<T>;
  if (envelope.ok === false || envelope.data === undefined) {
    const code = envelope.error?.code ?? "invalid_response";
    const message = envelope.error?.message ?? "远程服务返回了无效响应";
    if (code === "revision_conflict") {
      throw new BackendRevisionConflictError(envelope.requestId ?? null, message);
    }
    throw new BackendApiError(response.status, code, envelope.requestId ?? null, message);
  }
  return envelope.data;
}

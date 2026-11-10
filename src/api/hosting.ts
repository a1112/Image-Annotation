import { convertFileSrc } from "@tauri-apps/api/core";
import { invokeBackendCommand } from "./tauri";
import { remoteRequest } from "./mobile";
import { isTauriRuntime } from "../platform/runtime";
import type {
  CreateHostedDatasetInput,
  CreateHostedVersionInput,
  HostedArchiveUploadInput,
  HostedDatasetDetail,
  HostedDatasetMember,
  HostedDatasetVersion,
  HostedDatasetVersionDetail,
  HostedDownloadResult,
  HostedJobExportInput,
  HostedProjectCaptureInput,
  HostedDataset,
  HostingJob,
  HostingQuota,
  HostingRole,
  ReuseHostedVersionInput,
  UpdateHostedDatasetInput,
} from "../types/hosting";

/** Native archive uploads are deliberately capped before they reach IPC. */
export const HOSTING_ARCHIVE_MAX_BYTES = 64 * 1024 * 1024;

const DATASETS_PATH = "/api/v1/datasets";
const HOSTING_OPERATION_STORAGE_KEY = "image-annotation:hosting-operation-metadata";

export type HostingOperationContext = {
  accountScope: string;
  datasetId: string;
  action: string;
  payload: unknown;
};

export type HostingOperationHandle = {
  key: string;
  operationId: string;
};

type StoredHostingOperation = HostingOperationHandle & {
  accountScope: string;
  datasetId: string;
  action: string;
  payloadFingerprint: string;
  updatedAt: number;
};

let volatileHostingOperations: Record<string, StoredHostingOperation> = {};

function pathSegment(value: string): string {
  return encodeURIComponent(value);
}

function operationId(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  // The native command validates operation IDs as UUIDs even on older WebView
  // runtimes that do not expose crypto.randomUUID().
  return "xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx".replace(/[xy]/g, (token) => {
    const random = Math.floor(Math.random() * 16);
    const value = token === "x" ? random : (random & 0x3) | 0x8;
    return value.toString(16);
  });
}

function stableSerialize(value: unknown): string {
  if (value === null) return "null";
  if (value === undefined) return "undefined";
  if (typeof value === "string") return JSON.stringify(value);
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  if (Array.isArray(value)) return `[${value.map(stableSerialize).join(",")}]`;
  if (typeof value === "object") {
    return `{${Object.entries(value as Record<string, unknown>)
      .sort(([left], [right]) => left.localeCompare(right))
      .map(([key, item]) => `${JSON.stringify(key)}:${stableSerialize(item)}`)
      .join(",")}}`;
  }
  return String(value);
}

/**
 * A compact non-secret fingerprint used only as a sessionStorage lookup key.
 * The operation payload itself is not persisted, which is important for ZIP
 * imports where the payload may contain tens of MiB of image bytes.
 */
function payloadFingerprint(value: unknown): string {
  const input = stableSerialize(value);
  let first = 0x811c9dc5;
  let second = 0x9e3779b1;
  for (let index = 0; index < input.length; index += 1) {
    const code = input.charCodeAt(index);
    first = Math.imul(first ^ code, 0x01000193);
    second = Math.imul(second ^ code, 0x85ebca6b);
  }
  return `${(first >>> 0).toString(16)}${(second >>> 0).toString(16)}`;
}

function operationStorage(): Storage | null {
  try {
    return typeof globalThis.sessionStorage === "undefined" ? null : globalThis.sessionStorage;
  } catch {
    return null;
  }
}

function readStoredOperations(): Record<string, StoredHostingOperation> {
  const storage = operationStorage();
  if (!storage) return volatileHostingOperations;
  try {
    const value = JSON.parse(storage.getItem(HOSTING_OPERATION_STORAGE_KEY) ?? "{}");
    return value && typeof value === "object" ? value as Record<string, StoredHostingOperation> : {};
  } catch {
    return {};
  }
}

function writeStoredOperations(value: Record<string, StoredHostingOperation>): void {
  volatileHostingOperations = value;
  const storage = operationStorage();
  if (!storage) return;
  try {
    storage.setItem(HOSTING_OPERATION_STORAGE_KEY, JSON.stringify(value));
  } catch {
    // Private browsing/WebView policies may deny sessionStorage. The caller
    // still gets an in-memory operation ID for the current user action.
  }
}

function operationKey(context: HostingOperationContext): { key: string; fingerprint: string } {
  const accountScope = context.accountScope.trim() || "anonymous";
  const fingerprint = payloadFingerprint(context.payload);
  return {
    key: `${accountScope}\u0000${context.datasetId}\u0000${context.action}\u0000${fingerprint}`,
    fingerprint,
  };
}

/**
 * Reserve one idempotency key for a user intent. This is metadata only; no
 * access or refresh credential is ever written to sessionStorage.
 */
export function beginStableHostingOperation(context: HostingOperationContext): HostingOperationHandle {
  const { key, fingerprint } = operationKey(context);
  const stored = readStoredOperations();
  const existing = stored[key];
  if (existing?.operationId) return { key, operationId: existing.operationId };
  const handle = { key, operationId: operationId() };
  stored[key] = {
    ...handle,
    accountScope: context.accountScope.trim() || "anonymous",
    datasetId: context.datasetId,
    action: context.action,
    payloadFingerprint: fingerprint,
    updatedAt: Date.now(),
  };
  writeStoredOperations(stored);
  return handle;
}

/** Call only after the server/native bridge has confirmed the operation. */
export function completeStableHostingOperation(handle: HostingOperationHandle): void {
  const stored = readStoredOperations();
  if (!stored[handle.key] || stored[handle.key].operationId !== handle.operationId) return;
  delete stored[handle.key];
  writeStoredOperations(stored);
}

/**
 * A 409 is a known outcome (revision/idempotency conflict), so a new user
 * intent may receive a fresh operation ID. Transport/5xx errors deliberately
 * leave the metadata in place for a later explicit retry.
 */
export function isHostingConflict(error: unknown): boolean {
  const message = error instanceof Error ? error.message : String(error);
  return /(?:\b409\b|conflict|operation[_-]?id[_-]?reused|revision[_-]?conflict)/i.test(message);
}

function itemsOf<T>(value: unknown): T[] {
  if (Array.isArray(value)) return value as T[];
  if (value && typeof value === "object" && Array.isArray((value as { items?: unknown }).items)) {
    return (value as { items: T[] }).items;
  }
  return [];
}

function isRetryableReadError(error: unknown): boolean {
  const message = error instanceof Error ? error.message : String(error);
  // Only transient HTTP responses and transport failures are retried.  A
  // generic `!known-error` check would retry deterministic 400/405 responses
  // and could make a permission or contract error look like a flaky server.
  const status = message.match(/remote request failed\s*\((\d{3})\)/i)?.[1];
  if (status) {
    const code = Number(status);
    return code === 408 || code === 425 || code === 429 || code >= 500;
  }
  return !/(no authenticated|forbidden|permission|not found|invalid|revision|unauthorized|conflict|bad request)/i.test(message);
}

async function readRemote<T>(path: string): Promise<Awaited<ReturnType<typeof remoteRequest<T>>>> {
  let lastError: unknown;
  for (let attempt = 0; attempt < 2; attempt += 1) {
    try {
      return await remoteRequest<T>({ method: "GET", path });
    } catch (error) {
      lastError = error;
      if (attempt === 0 && isRetryableReadError(error)) {
        await new Promise<void>((resolve) => globalThis.setTimeout(resolve, 150));
        continue;
      }
      throw error;
    }
  }
  throw lastError instanceof Error ? lastError : new Error(String(lastError));
}

function numberOf(value: unknown, fallback = 0): number {
  return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}

function stringOf(value: unknown, fallback = ""): string {
  return typeof value === "string" ? value : fallback;
}

function nullableStringOf(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

function normalizeDataset(value: unknown): HostedDataset {
  const item = (value && typeof value === "object" ? value : {}) as Record<string, unknown>;
  return {
    id: stringOf(item.id),
    name: stringOf(item.name, "未命名数据集"),
    description: stringOf(item.description),
    revision: numberOf(item.revision, 1),
    itemCount: numberOf(item.itemCount ?? item.item_count),
    totalBytes: numberOf(item.totalBytes ?? item.total_bytes),
    role: stringOf(item.role, "viewer"),
    latestVersionId: nullableStringOf(item.latestVersionId ?? item.latest_version_id),
    createdAt: stringOf(item.createdAt ?? item.created_at),
    updatedAt: stringOf(item.updatedAt ?? item.updated_at),
    status: stringOf(item.status, "active"),
  };
}

function normalizeMember(value: unknown): HostedDatasetMember {
  const item = (value && typeof value === "object" ? value : {}) as Record<string, unknown>;
  const userId = stringOf(item.userId ?? item.user_id);
  return {
    // The server's member list uses userId as the stable path identifier.
    id: stringOf(item.id ?? item.memberId ?? item.member_id, userId),
    userId,
    email: nullableStringOf(item.email),
    displayName: nullableStringOf(item.displayName ?? item.display_name),
    role: stringOf(item.role, "viewer"),
    createdAt: nullableStringOf(item.createdAt ?? item.created_at),
  };
}

function normalizeVersion(value: unknown): HostedDatasetVersion {
  const item = (value && typeof value === "object" ? value : {}) as Record<string, unknown>;
  return {
    id: stringOf(item.id ?? item.versionId ?? item.version_id),
    datasetId: nullableStringOf(item.datasetId ?? item.dataset_id) ?? undefined,
    name: stringOf(item.name, "未命名版本"),
    versionNumber: numberOf(item.versionNumber ?? item.version_number ?? item.version, 0),
    revision: numberOf(item.revision, 0),
    itemCount: numberOf(item.itemCount ?? item.item_count),
    totalBytes: numberOf(item.totalBytes ?? item.total_bytes),
    manifestHash: nullableStringOf(item.manifestHash ?? item.manifest_hash),
    createdAt: stringOf(item.createdAt ?? item.created_at),
    createdBy: nullableStringOf(item.createdBy ?? item.created_by),
    status: nullableStringOf(item.status),
  };
}

function normalizeVersionDetail(value: unknown): HostedDatasetVersionDetail {
  const normalized = normalizeVersion(value);
  const item = (value && typeof value === "object" ? value : {}) as Record<string, unknown>;
  return { ...normalized, manifest: item.manifest };
}

function normalizeQuota(value: unknown): HostingQuota {
  const item = (value && typeof value === "object" ? value : {}) as Record<string, unknown>;
  return {
    limitBytes: numberOf(item.limitBytes ?? item.limit_bytes),
    usedBytes: numberOf(item.usedBytes ?? item.used_bytes),
    reservedBytes: numberOf(item.reservedBytes ?? item.reserved_bytes),
    maxAssets: numberOf(item.maxAssets ?? item.max_assets),
    usedAssets: numberOf(item.usedAssets ?? item.used_assets),
    canManage: Boolean(item.canManage ?? item.can_manage),
  };
}

function normalizeDownload(value: unknown): HostedDownloadResult {
  const item = (value && typeof value === "object" ? value : {}) as Record<string, unknown>;
  const path = stringOf(item.path);
  return {
    path,
    fileName: stringOf(item.fileName ?? item.file_name, "dataset-export.zip"),
    byteSize: numberOf(item.byteSize ?? item.byte_size),
    sha256: stringOf(item.sha256),
    url: isTauriRuntime() && path ? convertFileSrc(path) : path || undefined,
  };
}

function normalizeJob(value: unknown): HostingJob {
  const item = (value && typeof value === "object" ? value : {}) as Record<string, unknown>;
  const result = item.result ? normalizeDownload(item.result) : null;
  const rawStatus = stringOf(item.status, "queued");
  const status = rawStatus === "succeeded" ? "completed" : rawStatus === "error" ? "failed" : rawStatus;
  const progress = item.progress ?? item.percent;
  const kind = stringOf(item.kind ?? item.type, "import");
  const byteSize = item.byteSize ?? item.byte_size
    ?? (kind === "export" ? item.outputByteSize ?? item.output_byte_size : item.inputByteSize ?? item.input_byte_size);
  const sha256 = item.sha256 ?? (kind === "export"
    ? item.outputContentHash ?? item.output_content_hash
    : item.inputContentHash ?? item.input_content_hash);
  const error = item.error ?? item.errorMessage ?? item.error_message;
  return {
    id: stringOf(item.id),
    kind,
    status,
    progress: progress === undefined
      ? status === "completed" ? 100 : status === "running" ? 50 : 0
      : Math.min(100, Math.max(0, numberOf(progress))),
    datasetId: nullableStringOf(item.datasetId ?? item.dataset_id),
    versionId: nullableStringOf(item.versionId ?? item.version_id),
    format: nullableStringOf(item.format),
    fileName: nullableStringOf(item.fileName ?? item.file_name),
    byteSize: numberOf(byteSize),
    sha256: nullableStringOf(sha256),
    message: nullableStringOf(item.message ?? error),
    error: nullableStringOf(error),
    result,
    createdAt: nullableStringOf(item.createdAt ?? item.created_at),
    updatedAt: nullableStringOf(item.updatedAt ?? item.updated_at),
  };
}

export async function listHostedDatasets(): Promise<HostedDataset[]> {
  const response = await readRemote<unknown>(DATASETS_PATH);
  return itemsOf(response.data).map(normalizeDataset);
}

export async function createHostedDataset(input: CreateHostedDatasetInput): Promise<HostedDataset> {
  const response = await remoteRequest<unknown>({
    method: "POST",
    path: DATASETS_PATH,
    body: { name: input.name.trim(), description: input.description?.trim() ?? "" },
  });
  return normalizeDataset(response.data);
}

export async function getHostedDataset(datasetId: string): Promise<HostedDatasetDetail> {
  const response = await readRemote<unknown>(`${DATASETS_PATH}/${pathSegment(datasetId)}`);
  const dataset = normalizeDataset(response.data);
  const value = (response.data && typeof response.data === "object" ? response.data : {}) as Record<string, unknown>;
  return {
    ...dataset,
    members: itemsOf<unknown>(value.members).map(normalizeMember),
    versions: itemsOf<unknown>(value.versions).map(normalizeVersion),
  };
}

export async function updateHostedDataset(datasetId: string, input: UpdateHostedDatasetInput): Promise<HostedDataset> {
  const body = {
    ...(input.name === undefined ? {} : { name: input.name.trim() }),
    ...(input.description === undefined ? {} : { description: input.description.trim() }),
    baseRevision: input.baseRevision,
  };
  const response = await remoteRequest<unknown>({ method: "PATCH", path: `${DATASETS_PATH}/${pathSegment(datasetId)}`, body });
  return normalizeDataset(response.data);
}

export async function archiveHostedDataset(datasetId: string): Promise<void> {
  await remoteRequest({ method: "DELETE", path: `${DATASETS_PATH}/${pathSegment(datasetId)}` });
}

export async function listHostedMembers(datasetId: string): Promise<HostedDatasetMember[]> {
  const response = await readRemote<unknown>(`${DATASETS_PATH}/${pathSegment(datasetId)}/members`);
  return itemsOf(response.data).map(normalizeMember);
}

export async function addHostedMember(datasetId: string, memberIdentity: string, role: HostingRole): Promise<HostedDatasetMember> {
  const identity = memberIdentity.trim();
  const response = await remoteRequest<unknown>({
    method: "POST",
    path: `${DATASETS_PATH}/${pathSegment(datasetId)}/members`,
    // The server accepts a user UUID for backwards compatibility, but normal
    // users should be able to invite by email without discovering an internal
    // account id first.
    body: identity.includes("@") ? { email: identity, role } : { userId: identity, role },
  });
  return normalizeMember(response.data);
}

export async function removeHostedMember(datasetId: string, memberId: string): Promise<void> {
  await remoteRequest({
    method: "DELETE",
    path: `${DATASETS_PATH}/${pathSegment(datasetId)}/members/${pathSegment(memberId)}`,
  });
}

export async function captureHostedProject(
  datasetId: string,
  input: HostedProjectCaptureInput,
): Promise<HostedDataset> {
  const response = await remoteRequest<unknown>({
    method: "POST",
    path: `${DATASETS_PATH}/${pathSegment(datasetId)}/capture`,
    body: input,
  });
  const dataset = normalizeDataset(response.data);
  return dataset.id === datasetId && dataset.name !== "未命名数据集"
    ? dataset
    : getHostedDataset(datasetId);
}

export async function listHostedVersions(datasetId: string): Promise<HostedDatasetVersion[]> {
  const response = await readRemote<unknown>(`${DATASETS_PATH}/${pathSegment(datasetId)}/versions`);
  return itemsOf(response.data).map(normalizeVersion);
}

export async function createHostedVersion(
  datasetId: string,
  input: CreateHostedVersionInput,
): Promise<HostedDatasetVersion> {
  const response = await remoteRequest<unknown>({
    method: "POST",
    path: `${DATASETS_PATH}/${pathSegment(datasetId)}/versions`,
    body: input,
  });
  const version = normalizeVersion(response.data);
  if (!version.id) throw new Error("服务器未返回版本 ID");
  return version;
}

export async function getHostedVersion(datasetId: string, versionId: string): Promise<HostedDatasetVersionDetail> {
  const response = await readRemote<unknown>(`${DATASETS_PATH}/${pathSegment(datasetId)}/versions/${pathSegment(versionId)}`);
  return normalizeVersionDetail(response.data);
}

export async function reuseHostedVersion(
  datasetId: string,
  input: ReuseHostedVersionInput,
): Promise<{ projectId: string; reusedAssets: number }> {
  const response = await remoteRequest<unknown>({
    method: "POST",
    path: `${DATASETS_PATH}/${pathSegment(datasetId)}/reuse`,
    body: input,
  });
  const value = (response.data && typeof response.data === "object" ? response.data : {}) as Record<string, unknown>;
  const reusedValue = value.reusedAssets ?? value.reused_assets;
  return {
    projectId: stringOf(value.projectId ?? value.project_id),
    reusedAssets: Array.isArray(reusedValue) ? reusedValue.length : numberOf(reusedValue),
  };
}

export async function getHostingQuota(): Promise<HostingQuota> {
  const response = await readRemote<unknown>("/api/v1/storage/quota");
  return normalizeQuota(response.data);
}

export async function updateHostingQuota(input: { limitBytes: number; maxAssets: number }): Promise<HostingQuota> {
  const response = await remoteRequest<unknown>({
    method: "PATCH",
    path: "/api/v1/storage/quota",
    body: input,
  });
  return normalizeQuota(response.data);
}

/**
 * The asynchronous server job is intentionally kept behind one small adapter.
 * The upload/download bytes themselves use native commands, while job state is
 * still server-owned and therefore queried through the authenticated request
 * bridge.
 */
export async function listHostingJobs(datasetId: string): Promise<HostingJob[]> {
  const response = await readRemote<unknown>(`${DATASETS_PATH}/${pathSegment(datasetId)}/jobs`);
  return itemsOf(response.data).map(normalizeJob);
}

export async function getHostingJob(jobId: string): Promise<HostingJob> {
  const response = await readRemote<unknown>(`/api/v1/dataset-jobs/${pathSegment(jobId)}`);
  return normalizeJob(response.data);
}

export async function startHostedExport(datasetId: string, input: HostedJobExportInput): Promise<HostingJob> {
  const response = await remoteRequest<unknown>({
    method: "POST",
    path: `${DATASETS_PATH}/${pathSegment(datasetId)}/exports`,
    body: input,
  });
  return normalizeJob(response.data);
}

export async function uploadHostedArchive(input: HostedArchiveUploadInput): Promise<HostingJob> {
  const bytes = Math.floor((input.base64.length * 3) / 4) - (input.base64.endsWith("==") ? 2 : input.base64.endsWith("=") ? 1 : 0);
  if (bytes > HOSTING_ARCHIVE_MAX_BYTES) {
    throw new Error("数据集压缩包不能超过 64 MiB");
  }
  const result = await invokeBackendCommand<unknown>("hosting_upload_archive", {
    datasetId: input.datasetId,
    format: input.format,
    fileName: input.fileName,
    base64: input.base64,
    operationId: input.operationId ?? operationId(),
  });
  return normalizeJob(result);
}

export async function downloadHostedExport(jobId: string): Promise<HostedDownloadResult> {
  const result = await invokeBackendCommand<unknown>("hosting_download_export", { jobId });
  return normalizeDownload(result);
}

/**
 * Ask the native shell to reveal/share the already-downloaded export. The
 * native side resolves the job to its scoped file path; callers never provide
 * an arbitrary path from the WebView.
 */
export async function openHostedExport(jobId: string): Promise<void> {
  await invokeBackendCommand("hosting_open_export", { jobId });
}

/** Read a native archive into the bridge-safe base64 payload. */
export async function readHostingArchive(file: File): Promise<string> {
  if (file.size > HOSTING_ARCHIVE_MAX_BYTES) {
    throw new Error("数据集压缩包不能超过 64 MiB");
  }
  const buffer = typeof file.arrayBuffer === "function"
    ? await file.arrayBuffer()
    : await new Promise<ArrayBuffer>((resolve, reject) => {
      const reader = new FileReader();
      reader.onerror = () => reject(reader.error ?? new Error("读取压缩包失败"));
      reader.onload = () => resolve(reader.result as ArrayBuffer);
      reader.readAsArrayBuffer(file);
    });
  const bytes = new Uint8Array(buffer);
  let binary = "";
  const chunkSize = 0x8000;
  for (let offset = 0; offset < bytes.length; offset += chunkSize) {
    binary += String.fromCharCode(...bytes.subarray(offset, Math.min(offset + chunkSize, bytes.length)));
  }
  return btoa(binary);
}

export function newHostingOperationId(): string {
  return operationId();
}

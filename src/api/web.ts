import type { RemoteRequestMethod } from "../types/mobile";
import type { DatasetImage } from "../types/domain";
import { dispatchSessionChange } from "../shared/sessionEvents";

/**
 * Browser transport for the server-owned workspace.
 *
 * The desktop app deliberately keeps using Tauri IPC.  This module is only
 * selected by `main.tsx` for the web build (or after an explicit web transport
 * configuration in a test).  In particular, it never falls back to the
 * desktop loopback service.
 */

export type WebSessionUser = {
  id: string;
  email?: string | null;
  displayName?: string | null;
  organizationId?: string | null;
};

export type WebSession = {
  authenticated: boolean;
  user: WebSessionUser | null;
  deviceId: string | null;
};

export type WebHealth = {
  status: string;
  service: string;
  version: string;
  runtime: "web-remote-service";
  capabilities: string[];
};

export class WebApiError extends Error {
  readonly status: number;
  readonly code: string | null;
  readonly details: unknown;

  constructor(status: number, message: string, code: string | null = null, details: unknown = null) {
    super(message);
    this.name = "WebApiError";
    this.status = status;
    this.code = code;
    this.details = details;
  }
}

function browserApiBaseUrl(): string {
  const configured = String(import.meta.env.VITE_REMOTE_API_URL ?? "").trim();
  if (configured) return configured.replace(/\/+$/, "");
  if (typeof window === "undefined") return "";

  // Production deployment serves the app at /image-annotation/app/.  The API
  // is mounted one level above the app route, so same-origin cookies remain
  // valid and no localhost default can accidentally leak into a release.
  const path = window.location.pathname
    .replace(/\/app(?:\/index\.html)?\/?$/, "")
    .replace(/\/+$/, "");
  return `${window.location.origin}${path}`;
}

export function getWebApiBaseUrl(): string {
  return browserApiBaseUrl();
}

function joinUrl(path: string): string {
  const base = browserApiBaseUrl();
  if (!base) return path;
  return `${base}/${path.replace(/^\/+/, "")}`;
}

/** Resolve a URL returned by the server against the deployed service base.
 *
 * Server DTOs intentionally use root-relative paths such as `/api/v1/...`.
 * In a deployment mounted below `/image-annotation/app/`, assigning those
 * paths directly to an image/link would incorrectly target the browser root.
 */
export function resolveWebResourceUrl(value: string): string {
  if (/^(?:[a-z][a-z\d+.-]*:|\/\/)/i.test(value)) return value;
  return joinUrl(value);
}

const resourceUrlFields = new Set(["imageUrl", "annotationUrl", "downloadUrl", "pollUrl"]);

/** Normalize URL fields in server envelopes without changing their DTO shape. */
function normalizeResourceUrls<T>(value: T): T {
  if (Array.isArray(value)) return value.map((item) => normalizeResourceUrls(item)) as T;
  if (!value || typeof value !== "object") return value;
  const result = { ...(value as Record<string, unknown>) };
  for (const [key, item] of Object.entries(result)) {
    if (resourceUrlFields.has(key) && typeof item === "string") {
      result[key] = resolveWebResourceUrl(item);
    } else if (item && typeof item === "object") {
      result[key] = normalizeResourceUrls(item);
    }
  }
  return result as T;
}

function messageFromPayload(payload: unknown, fallback: string): { message: string; code: string | null; details: unknown } {
  if (payload && typeof payload === "object") {
    const value = payload as Record<string, unknown>;
    const error = value.error && typeof value.error === "object"
      ? value.error as Record<string, unknown>
      : value;
    const message = typeof error.message === "string"
      ? error.message
      : typeof error.error === "string"
        ? error.error
        : fallback;
    return {
      message,
      code: typeof error.code === "string" ? error.code : null,
      details: error.details ?? null,
    };
  }
  return { message: fallback, code: null, details: null };
}

async function parseResponse(response: Response): Promise<unknown> {
  const text = await response.text();
  if (!text) return null;
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return text;
  }
}

const sessionEndpoints = new Set([
  "/api/v1/web/auth/login",
  "/api/v1/web/auth/refresh",
  "/api/v1/web/auth/logout",
]);

let refreshInFlight: Promise<WebSession> | null = null;

function isSessionEndpoint(path: string): boolean {
  // All internal callers pass a relative API path.  Keeping the query string
  // out of this comparison also makes the guard safe for future auth routes
  // that add a cache-busting parameter.
  const pathname = path.split("?", 1)[0].replace(/\/+$/, "") || "/";
  return sessionEndpoints.has(pathname);
}

async function requestWithoutRefresh<T>(path: string, init: RequestInit = {}): Promise<T> {
  let response: Response;
  try {
    response = await fetch(joinUrl(path), {
      ...init,
      credentials: "include",
      headers: {
        Accept: "application/json",
        ...(init.body instanceof FormData ? {} : { "Content-Type": "application/json" }),
        ...(init.headers ?? {}),
      },
    });
  } catch (error) {
    throw new WebApiError(0, `远端服务不可用：${error instanceof Error ? error.message : String(error)}`);
  }

  const payload = await parseResponse(response);
  if (!response.ok) {
    const parsed = messageFromPayload(payload, `远端服务请求失败（HTTP ${response.status}）`);
    throw new WebApiError(response.status, parsed.message, parsed.code, parsed.details);
  }
  return payload as T;
}

async function refreshSessionSingleFlight(): Promise<WebSession> {
  if (refreshInFlight) return refreshInFlight;

  const request = refreshWebSession()
    .then((session) => {
      if (!session.authenticated || !session.user) {
        throw new WebApiError(401, "浏览器会话已过期");
      }
      return session;
    })
    .catch((error) => {
      // One failed refresh promise is shared by every request that observed
      // the expired access cookie, so this invalidation is emitted once per
      // refresh attempt rather than once per subrequest.
      dispatchSessionChange({ authenticated: false });
      throw error;
    })
    .finally(() => {
      refreshInFlight = null;
    });
  refreshInFlight = request;
  return request;
}

export async function webRequest<T = unknown>(
  path: string,
  init: RequestInit = {},
): Promise<T> {
  try {
    return await requestWithoutRefresh<T>(path, init);
  } catch (error) {
    if (!(error instanceof WebApiError) || error.status !== 401 || isSessionEndpoint(path)) {
      throw error;
    }

    // Auth endpoints are deliberately excluded above and the retry below
    // bypasses webRequest, so an expired session can never recursively refresh
    // itself or retry a protected request more than once.
    try {
      await refreshSessionSingleFlight();
    } catch {
      // Keep the original protected-request error for callers (and its 401
      // status for the UI), while the shared refresh path already broadcast
      // the account-scoped invalidation event.
      throw error;
    }
    return requestWithoutRefresh<T>(path, init);
  }
}

function unwrapCommandPayload<T>(payload: unknown): T {
  if (payload && typeof payload === "object" && "ok" in payload) {
    const value = payload as { ok?: boolean; data?: T; error?: unknown };
    if (value.ok === false) {
      const parsed = messageFromPayload(value.error, "远端命令失败");
      throw new WebApiError(400, parsed.message, parsed.code, parsed.details);
    }
    return value.data as T;
  }
  return payload as T;
}

/** Invoke the server's DTO-compatible command surface. */
export async function invokeWebCommand<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  const payload = await webRequest<unknown>(`/api/v1/web/commands/${encodeURIComponent(command)}`, {
    method: "POST",
    body: JSON.stringify(args ?? {}),
  });
  return unwrapCommandPayload<T>(payload);
}

export async function getWebHealth(): Promise<WebHealth> {
  const payload = await webRequest<Record<string, unknown>>("/health");
  return {
    status: typeof payload.status === "string" ? payload.status : "ok",
    service: typeof payload.service === "string" ? payload.service : "image-annotation-web",
    version: typeof payload.version === "string" ? payload.version : "unknown",
    runtime: "web-remote-service",
    capabilities: Array.isArray(payload.capabilities)
      ? payload.capabilities.filter((item): item is string => typeof item === "string")
      : [],
  };
}

function sessionFromPayload(payload: unknown): WebSession {
  const value = payload && typeof payload === "object" ? payload as Record<string, unknown> : {};
  const userValue = value.user && typeof value.user === "object" ? value.user as Record<string, unknown> : null;
  const user = userValue && typeof userValue.id === "string"
    ? {
      id: userValue.id,
      email: typeof userValue.email === "string" ? userValue.email : null,
      displayName: typeof userValue.displayName === "string" ? userValue.displayName : null,
      organizationId: typeof userValue.organizationId === "string" ? userValue.organizationId : null,
    }
    : null;
  return {
    authenticated: value.authenticated === true || Boolean(user),
    user,
    deviceId: typeof value.deviceId === "string" ? value.deviceId : null,
  };
}

export async function getWebSession(): Promise<WebSession> {
  try {
    return sessionFromPayload(await webRequest("/api/v1/web/auth/session"));
  } catch (error) {
    // The server deliberately protects this endpoint with the same cookie
    // extractor used by workspace commands.  An expired/absent cookie is a
    // normal signed-out state, not a transport error that should strand the
    // login screen behind a spinner.
    if (error instanceof WebApiError && error.status === 401) {
      return { authenticated: false, user: null, deviceId: null };
    }
    throw error;
  }
}

export async function loginWeb(input: { email: string; password: string; deviceId: string; deviceName: string }): Promise<WebSession> {
  return sessionFromPayload(await webRequest("/api/v1/web/auth/login", {
    method: "POST",
    body: JSON.stringify(input),
  }));
}

export async function refreshWebSession(): Promise<WebSession> {
  return sessionFromPayload(await webRequest("/api/v1/web/auth/refresh", { method: "POST", body: "{}" }));
}

export async function logoutWeb(): Promise<void> {
  await webRequest("/api/v1/web/auth/logout", { method: "POST", body: "{}" });
}

export async function webRemoteRequest<T = unknown>(request: { method: RemoteRequestMethod; path: string; body?: unknown }): Promise<T> {
  return webRequest<T>(request.path, {
    method: request.method,
    body: request.body === undefined ? undefined : JSON.stringify(request.body),
  });
}

export async function createWebDataset(input: {
  name: string;
  datasetType: string;
  demoTemplate: string;
  dataProjectId?: string | null;
}): Promise<unknown> {
  return invokeWebCommand("create_dataset_project", input);
}

export type WebDataProjectExport = {
  id: string;
  dataProjectId: string;
  branch: string;
  format: string;
  status: string;
  manifest: unknown;
};

/** Export the server-owned project tree as a browser-downloadable manifest. */
export async function exportWebDataProject(
  dataProjectId: string,
  options: { branch?: string | null; format?: string } = {},
): Promise<WebDataProjectExport> {
  const payload = payloadData<unknown>(await invokeWebCommand<unknown>("export_data_project", {
    dataProjectId,
    branch: options.branch ?? null,
    format: options.format ?? "manifest-json",
  }));
  if (!payload || typeof payload !== "object") {
    throw new WebApiError(502, "远端导出响应格式无效");
  }
  const value = payload as Record<string, unknown>;
  if (
    typeof value.id !== "string"
    || typeof value.dataProjectId !== "string"
    || typeof value.branch !== "string"
    || typeof value.format !== "string"
    || typeof value.status !== "string"
    || !("manifest" in value)
  ) {
    throw new WebApiError(502, "远端导出响应缺少项目清单");
  }
  return {
    id: value.id,
    dataProjectId: value.dataProjectId,
    branch: value.branch,
    format: value.format,
    status: value.status,
    manifest: value.manifest,
  };
}

export async function uploadWebDatasetImages(datasetId: string, files: File[]): Promise<unknown[]> {
  const results: unknown[] = [];
  for (const file of files) {
    results.push(normalizeResourceUrls(await webRequest(
      `/api/v1/web/datasets/${encodeURIComponent(datasetId)}/images/upload?fileName=${encodeURIComponent(file.name)}`,
      {
        method: "PUT",
        headers: { "Content-Type": file.type || "application/octet-stream" },
        body: file,
      },
    )));
  }
  return results;
}

/** The browser dataset identity is data-project UUID + canonical dataset key. */
export function parseWebDatasetIdentity(datasetId: string): { dataProjectId: string; datasetKey: string } | null {
  const separator = datasetId.indexOf("::");
  if (separator <= 0 || separator === datasetId.length - 2) return null;
  return {
    dataProjectId: datasetId.slice(0, separator),
    datasetKey: datasetId.slice(separator + 2),
  };
}

export type WebDatasetImage = DatasetImage & {
  imageHash: string;
  annotationHash?: string;
  revision?: number;
  datasetKey?: string;
  imageUrl?: string | null;
  annotationUrl?: string | null;
};

function normalizeWebDatasetImage(value: unknown): WebDatasetImage | null {
  if (!value || typeof value !== "object") return null;
  const item = value as Record<string, unknown>;
  if (typeof item.id !== "string" || typeof item.fileName !== "string" || typeof item.imageHash !== "string") return null;
  return {
    id: item.id,
    fileName: item.fileName,
    width: typeof item.width === "number" ? item.width : 0,
    height: typeof item.height === "number" ? item.height : 0,
    split: typeof item.split === "string" ? item.split : "train",
    status: typeof item.status === "string" ? item.status : "raw",
    qaStatus: typeof item.qaStatus === "string" ? item.qaStatus : "pending",
    reviewNote: typeof item.reviewNote === "string" ? item.reviewNote : null,
    tags: Array.isArray(item.tags) ? item.tags.filter((tag): tag is string => typeof tag === "string") : [],
    imageHash: item.imageHash,
    annotationHash: typeof item.annotationHash === "string" ? item.annotationHash : undefined,
    revision: typeof item.revision === "number" ? item.revision : undefined,
    datasetKey: typeof item.datasetKey === "string" ? item.datasetKey : undefined,
    imageUrl: typeof item.imageUrl === "string" ? resolveWebResourceUrl(item.imageUrl) : null,
    annotationUrl: typeof item.annotationUrl === "string" ? resolveWebResourceUrl(item.annotationUrl) : null,
  };
}

export async function listWebDataProjectImages(
  datasetId: string,
  options: { cursor?: string | null; limit?: number } = {},
): Promise<{ items: WebDatasetImage[]; nextCursor: string | null }> {
  const payload = await invokeWebCommand<unknown>("list_data_project_images", {
    datasetId,
    cursor: options.cursor ?? null,
    limit: options.limit ?? 500,
  });
  const value = payload && typeof payload === "object" ? payload as Record<string, unknown> : {};
  const rawItems = Array.isArray(payload) ? payload : Array.isArray(value.items) ? value.items : [];
  return {
    items: rawItems.map(normalizeWebDatasetImage).filter((item): item is WebDatasetImage => item !== null),
    nextCursor: typeof value.nextCursor === "string" ? value.nextCursor : null,
  };
}

export async function getWebDataProjectImage(datasetId: string, imageId: string): Promise<WebDatasetImage> {
  const payload = await invokeWebCommand<unknown>("get_data_project_image", { datasetId, imageId });
  const image = normalizeWebDatasetImage(payload);
  if (!image) throw new WebApiError(502, "远端图像响应缺少 sourceHash");
  return image;
}

export type WebImageTransformRequest = {
  operationId: string;
  sourceHash: string;
  datasetKey?: string;
  format: "jpeg" | "png" | "webp" | "tiff";
  width?: number;
  height?: number;
  crop?: { x: number; y: number; width: number; height: number };
  rotate: 0 | 90 | 180 | 270;
  flip: "none" | "horizontal" | "vertical";
  quality?: number;
};

export type WebImageTransformTaskRef = {
  taskId: string;
  status: string;
  created: boolean;
  pollUrl?: string;
};

export type WebImageTransformTask = {
  taskId: string;
  operationId?: string;
  sourceHash?: string;
  sourceDatasetKey?: string | null;
  format?: string;
  status: string;
  outputHash?: string | null;
  outputMimeType?: string | null;
  outputByteSize?: number | null;
  outputWidth?: number | null;
  outputHeight?: number | null;
  downloadUrl?: string | null;
  errorCode?: string | null;
  errorMessage?: string | null;
  createdAt?: string;
  updatedAt?: string;
  finishedAt?: string | null;
};

function payloadData<T>(payload: unknown): T {
  if (payload && typeof payload === "object" && "ok" in payload) {
    const value = payload as { ok?: boolean; data?: T; error?: unknown };
    if (value.ok === false) {
      const parsed = messageFromPayload(value.error, "远端请求失败");
      throw new WebApiError(400, parsed.message, parsed.code, parsed.details);
    }
    return value.data as T;
  }
  return payload as T;
}

export async function createWebDataProjectTransform(
  dataProjectId: string,
  request: WebImageTransformRequest,
): Promise<WebImageTransformTaskRef> {
  const payload = await webRequest<unknown>(`/api/v1/data-projects/${encodeURIComponent(dataProjectId)}/image-transforms`, {
    method: "POST",
    body: JSON.stringify(request),
  });
  return normalizeResourceUrls(payloadData<WebImageTransformTaskRef>(payload));
}

export async function getWebImageTransform(taskId: string): Promise<WebImageTransformTask> {
  const value = payloadData<unknown>(await webRequest<unknown>(`/api/v1/image-transforms/${encodeURIComponent(taskId)}`));
  if (!value || typeof value !== "object" || typeof (value as Record<string, unknown>).taskId !== "string") {
    throw new WebApiError(502, "远端转换任务响应格式无效");
  }
  const task = value as WebImageTransformTask;
  return {
    ...task,
    downloadUrl: typeof task.downloadUrl === "string" ? resolveWebResourceUrl(task.downloadUrl) : task.downloadUrl,
  };
}

/** Load the durable transform history for one data-project dataset. */
export async function listWebDataProjectTransforms(
  dataProjectId: string,
  datasetKey: string,
  options: { limit?: number } = {},
): Promise<WebImageTransformTask[]> {
  const params = new URLSearchParams({
    datasetKey,
    limit: String(options.limit ?? 50),
  });
  const payload = payloadData<unknown>(await webRequest<unknown>(
    `/api/v1/data-projects/${encodeURIComponent(dataProjectId)}/image-transforms?${params.toString()}`,
  ));
  if (!Array.isArray(payload)) {
    throw new WebApiError(502, "远端转换历史响应格式无效");
  }
  return payload.map((item) => {
    if (!item || typeof item !== "object" || typeof (item as Record<string, unknown>).taskId !== "string") {
      throw new WebApiError(502, "远端转换历史包含无效任务记录");
    }
    const task = item as WebImageTransformTask;
    return {
      ...task,
      downloadUrl: typeof task.downloadUrl === "string" ? resolveWebResourceUrl(task.downloadUrl) : task.downloadUrl,
    };
  });
}

export function getWebImageTransformDownloadUrl(taskId: string): string {
  return resolveWebResourceUrl(`/api/v1/image-transforms/${encodeURIComponent(taskId)}/download`);
}

export function getWebImageContentUrl(datasetId: string, imageId: string): string {
  return resolveWebResourceUrl(`/api/v1/web/datasets/${encodeURIComponent(datasetId)}/images/${encodeURIComponent(imageId)}/content`);
}

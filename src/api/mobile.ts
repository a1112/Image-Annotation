import { getProjectSyncSummary, syncProject, invokeBackendCommand } from "./tauri";
import { isTauriRuntime } from "../platform/runtime";
import type {
  AccountProfile,
  AcceptInvitationInput,
  AuthLoginInput,
  AuthSession,
  DeviceInfo,
  OfflineStatus,
  RemoteProjectOpenResult,
  RemoteProjectSummary,
  RemoteInvitation,
  RemoteRequestInput,
  RemoteResponse,
  RemoteReviewItem,
  RemoteTask,
  RemoteTaskItem,
} from "../types/mobile";

/** Default endpoint used by the native mobile shell when no project override
 * has been configured yet. The actual bearer session remains Rust-owned. */
export const DEFAULT_REMOTE_SERVER_URL = "https://175.178.16.90/image-annotation";

export function defaultRemoteServerUrl(): string {
  return import.meta.env.VITE_REMOTE_SERVER_URL || DEFAULT_REMOTE_SERVER_URL;
}

type NativeSessionPayload = AuthSession & {
  accountId?: string | null;
  accessExpiresAt?: number | null;
};

function normalizeSession(payload: NativeSessionPayload): AuthSession {
  const accountId = payload.accountId ?? payload.userId ?? null;
  return {
    ...payload,
    userId: payload.userId ?? accountId,
    displayName: payload.displayName ?? accountId,
    expiresAt: payload.expiresAt
      ?? (typeof payload.accessExpiresAt === "number" ? new Date(payload.accessExpiresAt * 1000).toISOString() : null),
    serverUrl: payload.serverUrl ?? null,
  };
}

export async function authStatus(): Promise<AuthSession> {
  return normalizeSession(await invokeBackendCommand<NativeSessionPayload>("auth_status"));
}

export async function authLogin(input: AuthLoginInput): Promise<AuthSession> {
  return normalizeSession(await invokeBackendCommand<NativeSessionPayload>("auth_login", {
    serverUrl: input.serverUrl,
    email: input.username,
    password: input.password,
    deviceName: input.deviceName ?? "Image Annotation Mobile",
  }));
}

/** Accept a one-time project invitation through Rust so credentials remain in
 * the native credential store just like a regular login. */
export async function authAcceptInvitation(input: AcceptInvitationInput): Promise<AuthSession> {
  return normalizeSession(await invokeBackendCommand<NativeSessionPayload>("auth_accept_invitation", {
    serverUrl: input.serverUrl,
    token: input.token,
    displayName: input.displayName,
    password: input.password,
    deviceName: input.deviceName ?? "Image Annotation Mobile",
  }));
}

export async function authRefresh(): Promise<AuthSession> {
  return normalizeSession(await invokeBackendCommand<NativeSessionPayload>("auth_refresh"));
}

export async function authLogout(): Promise<void> {
  await invokeBackendCommand<void>("auth_logout");
}

/**
 * Make an authenticated remote call through the Rust core. Do not add token
 * headers here: native key storage, refresh and offline queuing are Rust
 * responsibilities and this wrapper deliberately only carries request data.
 */
export async function remoteRequest<T = unknown>(
  request: RemoteRequestInput,
): Promise<RemoteResponse<T>> {
  const payload = await invokeBackendCommand<RemoteResponse<T> | T>("remote_request", {
    method: request.method,
    path: request.path,
    body: request.body ?? null,
  });
  if (payload && typeof payload === "object" && "data" in payload && "status" in payload) {
    return payload as RemoteResponse<T>;
  }
  return { status: 200, data: payload as T };
}

/** Open a project and ask Rust to hydrate/return its local cache snapshot. */
export async function remoteProjectOpen(projectId: string): Promise<RemoteProjectOpenResult> {
  const session = await authStatus();
  if (!session.serverUrl) throw new Error("no authenticated session");
  const payload = await invokeBackendCommand<Omit<RemoteProjectOpenResult, "offline">>("remote_project_open", {
    serverUrl: session.serverUrl,
    remoteProjectId: projectId,
    mode: "cloud_linked",
    cachePolicy: "on_demand",
  });
  const localProjectId = payload.localProjectId ?? payload.projectId ?? projectId;
  return {
    ...payload,
    projectId: localProjectId,
    localProjectId,
    remoteProjectId: payload.remoteProjectId ?? projectId,
    offline: false,
  };
}

function arrayPayload<T>(value: T[] | { items?: T[] } | unknown): T[] {
  if (Array.isArray(value)) return value;
  if (value && typeof value === "object" && Array.isArray((value as { items?: unknown }).items)) {
    return (value as { items: T[] }).items;
  }
  return [];
}

export async function listRemoteProjects(): Promise<RemoteProjectSummary[]> {
  const response = await remoteRequest<RemoteProjectSummary[]>({ method: "GET", path: "/api/v1/projects" });
  return arrayPayload<RemoteProjectSummary>(response.data);
}

export async function getAccountProfile(): Promise<AccountProfile> {
  const response = await remoteRequest<AccountProfile>({ method: "GET", path: "/api/v1/auth/me" });
  return response.data;
}

export async function createRemoteInvitation(
  remoteProjectId: string,
  email: string,
  role: string,
): Promise<RemoteInvitation> {
  const response = await remoteRequest<RemoteInvitation>({
    method: "POST",
    path: `/api/v1/projects/${encodeURIComponent(remoteProjectId)}/invitations`,
    body: { email: email.trim(), role },
  });
  return response.data;
}

export async function revokeRemoteDevice(deviceId: string): Promise<void> {
  await remoteRequest({ method: "DELETE", path: `/api/v1/auth/devices/${encodeURIComponent(deviceId)}` });
}

export async function listRemoteTasks(remoteProjectId: string): Promise<RemoteTask[]> {
  const response = await remoteRequest<RemoteTask[] | { items?: RemoteTask[] }>({
    method: "GET",
    path: `/api/v1/projects/${encodeURIComponent(remoteProjectId)}/tasks`,
  });
  return arrayPayload<RemoteTask>(response.data);
}

export async function listRemoteTaskItems(remoteTaskId: string): Promise<RemoteTaskItem[]> {
  const response = await remoteRequest<RemoteTaskItem[] | { items?: RemoteTaskItem[] }>({
    method: "GET",
    path: `/api/v1/tasks/${encodeURIComponent(remoteTaskId)}/items`,
  });
  return arrayPayload<RemoteTaskItem>(response.data);
}

export async function claimRemoteTaskItem(remoteTaskId: string, imageId: string): Promise<void> {
  await remoteRequest({
    method: "POST",
    path: `/api/v1/tasks/${encodeURIComponent(remoteTaskId)}/items/${encodeURIComponent(imageId)}/claim`,
  });
}

export async function releaseRemoteTaskItem(remoteTaskId: string, imageId: string): Promise<void> {
  await remoteRequest({
    method: "DELETE",
    path: `/api/v1/tasks/${encodeURIComponent(remoteTaskId)}/items/${encodeURIComponent(imageId)}/claim`,
  });
}

export async function listRemoteReviewQueue(remoteProjectId: string): Promise<RemoteReviewItem[]> {
  const response = await remoteRequest<RemoteReviewItem[] | { items?: RemoteReviewItem[] }>({
    method: "GET",
    path: `/api/v1/projects/${encodeURIComponent(remoteProjectId)}/review-queue`,
  });
  return arrayPayload<RemoteReviewItem>(response.data);
}

export async function reviewRemoteItem(
  remoteProjectId: string,
  imageId: string,
  decision: string,
  note = "",
  annotationRevision?: string | number | null,
): Promise<void> {
  const protocolDecision = decision === "approve" ? "approved" : decision === "reject" ? "rejected" : decision;
  let revision = annotationRevision == null ? null : Number(annotationRevision);
  if (revision == null || !Number.isInteger(revision) || revision < 0) {
    const current = await remoteRequest<{ revision: number }>({
      method: "GET",
      path: `/api/v1/projects/${encodeURIComponent(remoteProjectId)}/images/${encodeURIComponent(imageId)}/annotation`,
    });
    revision = current.data.revision;
  }
  await remoteRequest({
    method: "POST",
    path: `/api/v1/projects/${encodeURIComponent(remoteProjectId)}/images/${encodeURIComponent(imageId)}/review`,
    body: { decision: protocolDecision, note, annotationRevision: revision, operationId: crypto.randomUUID() },
  });
}

export async function deviceInfo(): Promise<DeviceInfo> {
  try {
    return await invokeBackendCommand<DeviceInfo>("device_info");
  } catch {
    const session = await authStatus();
    const userAgent = typeof navigator === "undefined" ? "" : navigator.userAgent;
    return {
      deviceId: session.deviceId ?? "unknown-device",
      platform: /Android/i.test(userAgent)
        ? "android"
        : /iPhone|iPad|iPod/i.test(userAgent)
          ? "ios"
          : "unknown",
      appVersion: null,
      online: Boolean(session.authenticated),
    };
  }
}

export async function offlineStatus(projectId?: string): Promise<OfflineStatus> {
  try {
    return await invokeBackendCommand<OfflineStatus>("offline_status", {
      projectId: projectId ?? null,
    });
  } catch (error) {
    if (isTauriRuntime()) throw error;
    if (!projectId) {
      const session = await authStatus();
      return { ...emptyOfflineStatus(), online: session.authenticated };
    }
    const summary = await getProjectSyncSummary(projectId);
    return {
      online: summary.projectMode !== "local_only",
      queuedOperations: summary.pendingOperations + summary.failedOperations,
      conflictCount: summary.conflictCount,
      cachedAssets: 0,
      lastSyncAt: summary.lastPushedAt ?? summary.lastPulledAt,
      lastError: null,
    };
  }
}

export async function flushOfflineQueue(projectId?: string): Promise<OfflineStatus> {
  try {
    return await invokeBackendCommand<OfflineStatus>("flush_offline_queue", {
      projectId: projectId ?? null,
    });
  } catch (error) {
    if (isTauriRuntime()) throw error;
    if (!projectId) return offlineStatus(projectId);
    await syncProject(projectId);
    return offlineStatus(projectId);
  }
}

function emptyOfflineStatus(): OfflineStatus {
  return {
    online: false,
    queuedOperations: 0,
    conflictCount: 0,
    cachedAssets: 0,
    lastSyncAt: null,
    lastError: null,
  };
}

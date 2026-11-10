import type { DatasetImage, DatasetProject, ProjectDetail } from "./domain";

/** The session state is intentionally opaque to the web client.
 *
 * On native builds the Rust core owns the access/refresh credentials and
 * injects the authenticated session into `remote_request`.  The UI only
 * receives display-safe status fields and never persists a token itself.
 */
export type AuthSessionStatus = {
  authenticated: boolean;
  serverUrl: string | null;
  userId?: string | null;
  accountId?: string | null;
  displayName?: string | null;
  expiresAt?: string | null;
  accessExpiresAt?: number | null;
  deviceId?: string | null;
};

export type AuthLoginInput = {
  serverUrl: string;
  username: string;
  password: string;
  deviceName?: string;
};

export type AcceptInvitationInput = {
  serverUrl: string;
  token: string;
  displayName: string;
  password: string;
  deviceName?: string;
};

export type AuthSession = AuthSessionStatus;

export type RemoteRequestMethod = "GET" | "POST" | "PUT" | "PATCH" | "DELETE";

export type RemoteRequestInput = {
  method: RemoteRequestMethod;
  path: string;
  body?: unknown;
};

export type RemoteResponse<T = unknown> = {
  status: number;
  data: T;
  headers?: Record<string, string>;
};

export type RemoteProjectOpenResult = {
  projectId: string;
  /** Local cache identifier returned by the native bootstrap command. */
  localProjectId?: string | null;
  remoteProjectId?: string | null;
  project?: DatasetProject | null;
  detail?: ProjectDetail | null;
  images?: DatasetImage[];
  cursor?: string | null;
  offline?: boolean;
  sync?: unknown;
};

export type RemoteProjectSummary = {
  id: string;
  name: string;
  description?: string | null;
  mode?: string | null;
  status?: string | null;
  role?: string | null;
  revision?: number | null;
  updatedAt?: string | null;
};

/** The one-time token returned when a project manager creates an invitation.
 * The token is intentionally displayed for manual sharing only; it is never
 * persisted in the browser or sent through a second messaging channel. */
export type RemoteInvitation = {
  id?: string;
  projectId?: string;
  email: string;
  role: string;
  token: string;
  expiresAt?: string | null;
  acceptUrl?: string | null;
};

export type SessionDevice = {
  id: string;
  deviceId: string;
  platform: string;
  name: string;
  createdAt?: string | null;
  lastSeenAt?: string | null;
  revokedAt?: string | null;
};

export type AccountProfile = {
  id: string;
  email: string;
  displayName: string;
  organizationId?: string | null;
  status?: string | null;
  createdAt?: string | null;
  devices: SessionDevice[];
};

export type RemoteTask = {
  id: string;
  name: string;
  status: string;
  createdAt?: string | null;
  updatedAt?: string | null;
  itemCount?: number;
  completedCount?: number;
};

export type RemoteTaskItem = {
  id: string;
  clientImageId?: string | null;
  assigneeId?: string | null;
  taskId?: string;
  imageId: string;
  status?: string | null;
  qaStatus?: string | null;
  reviewNote?: string | null;
  lockedAt?: string | null;
};

export type RemoteReviewItem = {
  imageId: string;
  annotationStatus?: string | null;
  clientImageId?: string | null;
  fileName?: string | null;
  status?: string | null;
  objects?: unknown[];
  annotationRevision?: string | number | null;
  updatedAt?: string | null;
};

export type DeviceInfo = {
  deviceId: string;
  platform: "android" | "ios" | "macos" | "windows" | "linux" | "web" | "unknown";
  model?: string | null;
  appVersion?: string | null;
  online?: boolean;
};

export type OfflineStatus = {
  online: boolean;
  queuedOperations: number;
  conflictCount: number;
  cachedAssets: number;
  lastSyncAt: string | null;
  lastError: string | null;
};

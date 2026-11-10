/**
 * Cloud dataset hosting contracts.
 *
 * These types intentionally describe the display-safe API returned by the
 * server.  Access tokens and object-store credentials never cross this
 * boundary; calls are made through the Rust-owned `remoteRequest` bridge.
 */

export type HostingFormat = "native" | "coco" | "yolo" | "labelme";

export type HostingRole = "owner" | "manager" | "viewer";

export type HostedDataset = {
  id: string;
  name: string;
  description: string;
  revision: number;
  itemCount: number;
  totalBytes: number;
  role: string;
  latestVersionId: string | null;
  createdAt: string;
  updatedAt: string;
  status: string;
};

export type HostedDatasetDetail = HostedDataset & {
  members?: HostedDatasetMember[];
  versions?: HostedDatasetVersion[];
};

export type HostedDatasetMember = {
  id: string;
  userId: string;
  email?: string | null;
  displayName?: string | null;
  role: HostingRole | string;
  createdAt?: string | null;
};

export type HostedDatasetVersion = {
  id: string;
  datasetId?: string;
  name: string;
  versionNumber?: number | null;
  revision?: number | null;
  itemCount: number;
  totalBytes?: number | null;
  manifestHash?: string | null;
  createdAt: string;
  createdBy?: string | null;
  status?: string | null;
};

export type HostedDatasetVersionDetail = HostedDatasetVersion & {
  manifest?: unknown;
};

export type HostingQuota = {
  limitBytes: number;
  usedBytes: number;
  reservedBytes: number;
  maxAssets: number;
  usedAssets: number;
  canManage: boolean;
};

export type HostingJobStatus = "queued" | "running" | "completed" | "failed" | "cancelled" | string;

export type HostedDownloadResult = {
  path: string;
  fileName: string;
  byteSize: number;
  sha256: string;
  url?: string;
};

export type HostingJob = {
  id: string;
  kind: "import" | "export" | string;
  status: HostingJobStatus;
  progress: number;
  datasetId?: string | null;
  versionId?: string | null;
  format?: HostingFormat | string | null;
  fileName?: string | null;
  byteSize?: number | null;
  sha256?: string | null;
  message?: string | null;
  error?: string | null;
  result?: HostedDownloadResult | null;
  createdAt?: string | null;
  updatedAt?: string | null;
};

export type HostingListResponse<T> = {
  items: T[];
};

export type CreateHostedDatasetInput = {
  name: string;
  description?: string;
};

export type UpdateHostedDatasetInput = {
  name?: string;
  description?: string;
  baseRevision: number;
};

export type CreateHostedVersionInput = {
  name: string;
  operationId: string;
  baseRevision?: number;
};

export type ReuseHostedVersionInput = {
  versionId: string;
  targetProjectId?: string;
  projectName?: string;
  operationId: string;
};

export type HostedProjectCaptureInput = {
  projectId: string;
  operationId: string;
};

export type HostedJobExportInput = {
  versionId: string;
  format: HostingFormat;
  operationId: string;
};

export type HostedJobImportInput = {
  datasetId: string;
  format: HostingFormat;
  fileName: string;
};

export type HostedArchiveUploadInput = HostedJobImportInput & {
  base64: string;
  /** Stable id used when a native upload is retried after a transport error. */
  operationId?: string;
};

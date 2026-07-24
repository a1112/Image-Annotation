export type DatasetFormat = "yolo-detect" | "yolo-seg" | "voc-detect" | "image-classification";

export type BuiltinDataset = {
  key: string;
  name: string;
  description: string;
  taskType: string;
  format: DatasetFormat;
  downloaded: boolean;
  projectId: string | null;
};

export type DownloadJob = {
  id: string;
  datasetKey: string;
  status: string;
  progress: number;
  message: string;
  projectId: string | null;
};

export type DataSourceTreeNode = {
  name: string;
  path: string;
  kind: "folder" | "file";
  children: DataSourceTreeNode[];
  truncated: boolean;
};

export type DataSourceAnalysis = {
  sourcePaths: string[];
  rootPath: string;
  sourceKind: "folder" | "files";
  detectedFormat: DatasetFormat | "image-directory" | "unknown";
  recommendedAction: "open-local" | "copy-images";
  imageCount: number;
  annotationCount: number;
  classCount: number;
  classes: string[];
  splitCount: number;
  warnings: string[];
  tree: DataSourceTreeNode[];
};

export type BackendTask = {
  id: string;
  title: string;
  kind: string;
  status: string;
  progress: number;
  message: string;
  startedAt: string;
  finishedAt: string | null;
};

export type DatasetProject = {
  id: string;
  name: string;
  description: string;
  annotationTypes: string[];
  imageCount: number;
  annotatedPercent: number;
  reviewCount: number;
  issueCount: number;
  classCount: number;
  tagGroupCount: number;
  status: string;
  tags: string[];
};

export type DatasetImage = {
  id: string;
  fileName: string;
  width: number;
  height: number;
  split: string;
  status: string;
  qaStatus: string;
  reviewNote: string | null;
  tags: string[];
};

export type ClassSample = {
  image: DatasetImage;
  matchCount: number;
};

export type BBox = {
  x: number;
  y: number;
  width: number;
  height: number;
};

export type Point = {
  x: number;
  y: number;
};

export type AnnotationObject = {
  id: string;
  classId: number;
  label: string;
  type: "bbox" | "polygon" | "classification";
  bbox?: BBox;
  polygon?: Point[];
  attributes: Record<string, string | number | boolean>;
};

export type AnnotationState = {
  imageId: string;
  revision: string | null;
  objects: AnnotationObject[];
  status: string;
  updatedAt: string | null;
};

export type AnnotationSaveResult = {
  revision: string;
  savedAt: string;
  auditEventId: string;
};

export type AnnotationVersion = {
  id: string;
  imageId: string;
  revision: string;
  objects: AnnotationObject[];
  createdAt: string;
};

export type AnnotationTask = {
  id: string;
  name: string;
  status: string;
  createdAt: string;
  updatedAt: string;
};

export type TaskItem = {
  id: string;
  taskId: string;
  imageId: string;
  status: string;
  qaStatus: string;
  reviewNote: string | null;
  lockedAt: string | null;
};

export type DatasetSnapshot = {
  id: string;
  name: string;
  imageCount: number;
  manifestPath: string;
  createdAt: string;
};

export type DatasetExport = {
  id: string;
  snapshotId: string;
  format: string;
  status: string;
  outputPath: string;
  createdAt: string;
};

export type TagGroup = {
  id: string;
  name: string;
  conditions: string[];
  imageCount: number;
  annotatedPercent: number;
  issueCount: number;
  exportEnabled: boolean;
};

export type ClassStat = {
  id?: number;
  label: string;
  color: string;
  count: number;
  attributes: string[];
};

export type TaskSummary = {
  name: string;
  owner: string;
  status: string;
  progress: number;
};

export type QualityCheck = {
  name: string;
  severity: string;
  count: number;
};

export type ExportPreset = {
  name: string;
  format: string;
  scope: string;
  status: string;
};

export type ProjectDetail = {
  project: DatasetProject;
  tagGroups: TagGroup[];
  classes: ClassStat[];
  tasks: TaskSummary[];
  qualityChecks: QualityCheck[];
  exportPresets: ExportPreset[];
};

export type ProjectManifest = {
  id: string;
  name: string;
  sourceDatasetKey: string;
  format: DatasetFormat;
  rootPath: string;
  createdAt: string;
  classCount: number;
  imageCount: number;
};

export type IssueSeverity = "blocker" | "critical" | "major" | "minor" | "suggestion";
export type IssueStatus = "open" | "in_progress" | "resolved" | "pending_review" | "closed" | "reopened";

export type IssueRecord = {
  id: string;
  projectId: string;
  imageId: string;
  annotationObjectId: string | null;
  title: string;
  description: string;
  severity: IssueSeverity;
  status: IssueStatus;
  source: string;
  reporterId: string;
  assigneeId: string | null;
  dueAt: string | null;
  revision: number;
  createdAt: string;
  updatedAt: string;
  resolvedAt: string | null;
  deletedAt: string | null;
};

export type IssueComment = {
  id: string;
  issueId: string;
  authorId: string;
  content: string;
  createdAt: string;
  updatedAt: string;
};

export type SyncSummary = {
  projectId: string;
  projectMode: "local_only" | "cloud_linked" | "mirrored";
  pendingOperations: number;
  failedOperations: number;
  conflictCount: number;
  lastPulledAt: string | null;
  lastPushedAt: string | null;
};

export type RemoteProjectConfig = {
  projectId: string;
  serverUrl: string;
  remoteProjectId: string;
  deviceId: string;
  cachePolicy: "thumbnail_only" | "on_demand" | "full_mirror";
  autoSync: boolean;
};

export type AssetCacheRecord = {
  assetId: string;
  contentHash: string;
  localPath: string;
  cacheKind: string;
  byteSize: number;
  lastAccessedAt: string;
  verifiedAt: string | null;
  pinned: boolean;
};

export type AssetCacheSummary = {
  entryCount: number;
  byteSize: number;
  pinnedCount: number;
};

export type AssetCacheCleanupResult = {
  removedCount: number;
  removedBytes: number;
  remaining: AssetCacheSummary;
};

export type HybridDiagnostics = {
  projectId: string;
  projectMode: "local_only" | "cloud_linked" | "mirrored";
  serverUrl: string | null;
  remoteProjectId: string | null;
  deviceId: string | null;
  cachePolicy: "thumbnail_only" | "on_demand" | "full_mirror" | null;
  autoSync: boolean;
  cursor: string | null;
  lastPulledAt: string | null;
  lastPushedAt: string | null;
  pendingOperations: number;
  retryingOperations: number;
  failedOperations: number;
  oldestPendingAt: string | null;
  conflictCount: number;
  cacheEntries: number;
  cacheBytes: number;
  lastError: string | null;
};

export type ProjectRole = "owner" | "manager" | "annotator" | "reviewer" | "viewer";

export type RemoteProjectMember = {
  userId: string;
  role: ProjectRole;
  joinedAt: string | null;
};

export type PublishProjectResult = {
  remoteProjectId: string;
  createdRemoteProject: boolean;
  uploadedAssets: number;
  reusedAssets: number;
  initializedAnnotations: number;
  conflicts: number;
  sync: SyncRunResult;
};

export type SyncRunResult = {
  projectId: string;
  pushed: number;
  pulled: number;
  conflicts: number;
  failed: number;
  cursor: string;
};

export type SyncConflict = {
  id: string;
  projectId: string;
  entityType: string;
  entityId: string;
  base: unknown | null;
  local: unknown;
  remote: unknown;
  status: string;
  createdAt: string;
  resolvedAt: string | null;
};

export type FolderRecord = {
  id: string;
  projectId: string;
  parentId: string | null;
  name: string;
  sortOrder: number;
  revision: number;
  imageCount: number;
  syncStatus: string;
};

export type FolderMemberRecord = {
  folderId: string;
  imageId: string;
  revision: number;
  syncStatus: string;
};

export type FolderWorkspace = {
  folders: FolderRecord[];
  members: FolderMemberRecord[];
};

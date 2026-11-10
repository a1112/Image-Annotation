/**
 * A data project is the versioning/organisational layer above the existing
 * dataset project. DatasetProject remains the annotation workbench boundary;
 * these types deliberately contain only the data-project metadata and its
 * working tree.
 */
export type DataProject = {
  id: string;
  name: string;
  description: string;
  datasetIds: string[];
  createdAt: string;
  updatedAt: string;
  accountScope: string | null;
  role?: string;
};

/** Project-level organization; dataset keys stay canonical across clients. */
export type DatasetGroup = {
  id: string;
  name: string;
  datasetKeys: string[];
  position: number;
};

export type DataProjectMetadata = {
  schemaVersion: 1;
  datasetGroups: DatasetGroup[];
  [key: string]: unknown;
};

export type DataProjectGroupState = {
  dataProjectId: string;
  branch: string;
  revision: number;
  projectMetadata: DataProjectMetadata;
  canEdit: boolean;
};

export type DataProjectSection = "data" | "versions" | "collaboration" | "settings";

export type DataProjectCounts = {
  raw: number;
  inProgress: number;
  pendingReview: number;
  approved: number;
  rejected: number;
  total: number;
};

export type DataProjectChange = {
  kind: string;
  datasetId: string;
  imageId?: string | null;
  fileName?: string | null;
  detail: string;
};

export type DataProjectWorkingTree = {
  dataProjectId: string;
  counts: DataProjectCounts;
  changes: DataProjectChange[];
  headCommitId: string | null;
  projectMetadata?: DataProjectMetadata;
};

export type DataProjectCommitEntry = {
  datasetId: string;
  imageId: string;
  fileName: string;
  imageHash: string;
  annotationHash: string;
  status: string;
  qaStatus: string;
  split: string;
  width: number;
  height: number;
};

export type DataProjectCommit = {
  id: string;
  dataProjectId: string;
  parentId: string | null;
  /** The second parent is present on a merge commit. Older linear commits omit it. */
  secondParentId: string | null;
  branch: string | null;
  message: string;
  createdAt: string;
  datasetIds: string[];
  datasetMetadata: Record<string, string>;
  projectMetadata?: DataProjectMetadata;
  entries: DataProjectCommitEntry[];
};

export type DataProjectBranch = {
  dataProjectId: string;
  name: string;
  headCommitId: string | null;
  current: boolean;
  createdAt: string;
  updatedAt: string;
};

export type DataProjectMergeConflict = {
  key: string;
  kind: string;
  /** Optional human-readable labels supplied by the native merge planner. */
  displayName?: string | null;
  baseDescription?: string | null;
  oursDescription?: string | null;
  theirsDescription?: string | null;
  base: unknown | null;
  ours: unknown | null;
  theirs: unknown | null;
};

export type DataProjectMergePreview = {
  sessionId: string | null;
  commit: DataProjectCommit | null;
  conflicts: DataProjectMergeConflict[];
};

export type DataProjectMergeResult = {
  sessionId: string | null;
  commit: DataProjectCommit | null;
  conflicts: DataProjectMergeConflict[];
};

export type DataProjectRevertResult = DataProjectCommit;

export type DataProjectCheckout = {
  dataProjectId: string;
  branch: string;
  commitId: string | null;
  path: string;
  dirty: boolean;
};

export type DataProjectSyncReport = {
  localProjectId: string;
  remoteProjectId: string;
  remoteRevision: number;
  localHead: string | null;
  remoteHead: string | null;
  outcome: string;
  trackingRef: string | null;
};

export type DataProjectSyncStatus = {
  localProjectId: string;
  remoteProjectId: string | null;
  remoteRevision: number | null;
  localHead: string | null;
  remoteHead: string | null;
  dirty: boolean;
  state: string;
  trackingRef: string | null;
};

export type RemoteDataProject = {
  id: string;
  organizationId: string | null;
  /** Web discovery intentionally omits owner identity; native discovery supplies it. */
  ownerId: string | null;
  name: string;
  description: string;
  status: string;
  revision: number;
  role: string;
  datasetCount: number;
  refCount: number;
  commitCount: number;
  updatedAt: string;
};

export type RemoteDatasetMembership = {
  datasetKey: string;
  datasetId: string | null;
  metadata: unknown;
  position: number;
  updatedAt: string;
};

export type RemoteObjectDescriptor = {
  hash: string;
  byteSize: number;
  kind: string;
  mimeType: string;
  createdAt: string;
};

export type RemoteCommit = {
  commitId: string;
  parentIds: string[];
  message: string;
  tree: unknown;
  metadata: unknown;
  createdBy: string;
  createdAt: string;
};

export type RemoteRef = {
  name: string;
  commitId: string | null;
  revision: number;
  updatedBy: string;
  updatedAt: string;
};

export type DataProjectGraphBundle = {
  project: RemoteDataProject;
  members: unknown[];
  datasets: RemoteDatasetMembership[];
  commits: RemoteCommit[];
  refs: RemoteRef[];
  objects: RemoteObjectDescriptor[];
  remoteRevision: number;
};

export type DataProjectCloudMember = {
  userId: string;
  role: string;
  joinedAt: string | null;
  email: string | null;
  displayName: string | null;
};

export type DataProjectCloudMemberRole = {
  userId: string;
  role: string;
};

export type DataProjectCloudMemberRemoval = {
  userId: string;
  deleted: boolean;
};

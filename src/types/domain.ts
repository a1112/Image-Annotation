export type DatasetFormat = "yolo-detect" | "yolo-seg" | "voc-detect";

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
  type: "bbox" | "polygon";
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

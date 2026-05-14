export type DatasetFormat = "yolo-detect" | "yolo-seg";

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
  tags: string[];
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

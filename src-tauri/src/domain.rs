use crate::{
    exporters::{self, ExportOptions, SnapshotData},
    importers::{adapter::SourceSyncResult, coco, labelme, voc_adapter, yolo_adapter},
    project_fs, storage,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatasetProject {
    pub id: String,
    pub name: String,
    pub description: String,
    pub annotation_types: Vec<String>,
    pub image_count: u32,
    pub annotated_percent: u8,
    pub review_count: u32,
    pub issue_count: u32,
    pub class_count: u16,
    pub tag_group_count: u16,
    pub status: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatasetImage {
    pub id: String,
    pub file_name: String,
    pub width: u32,
    pub height: u32,
    pub split: String,
    pub status: String,
    pub qa_status: String,
    pub review_note: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassSample {
    pub image: DatasetImage,
    pub match_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BBox {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnnotationObject {
    pub id: String,
    pub class_id: u32,
    pub label: String,
    #[serde(rename = "type")]
    pub object_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bbox: Option<BBox>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub polygon: Option<Vec<Point>>,
    pub attributes: BTreeMap<String, Value>,
}

impl AnnotationObject {
    pub fn bbox(id: String, class_id: u32, label: String, bbox: BBox) -> Self {
        Self {
            id,
            class_id,
            label,
            object_type: "bbox".to_string(),
            bbox: Some(bbox),
            polygon: None,
            attributes: BTreeMap::new(),
        }
    }

    pub fn polygon(id: String, class_id: u32, label: String, polygon: Vec<Point>) -> Self {
        Self {
            id,
            class_id,
            label,
            object_type: "polygon".to_string(),
            bbox: None,
            polygon: Some(polygon),
            attributes: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TagGroup {
    pub id: String,
    pub name: String,
    pub conditions: Vec<String>,
    pub image_count: u32,
    pub annotated_percent: u8,
    pub issue_count: u32,
    pub export_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassStat {
    pub id: u32,
    pub label: String,
    pub color: String,
    pub count: u32,
    pub attributes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSummary {
    pub name: String,
    pub owner: String,
    pub status: String,
    pub progress: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityCheck {
    pub name: String,
    pub severity: String,
    pub count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportPreset {
    pub name: String,
    pub format: String,
    pub scope: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnnotationState {
    pub image_id: String,
    pub revision: Option<String>,
    pub objects: Vec<AnnotationObject>,
    pub status: String,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnnotationSaveResult {
    pub revision: String,
    pub saved_at: String,
    pub audit_event_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnnotationVersion {
    pub id: String,
    pub image_id: String,
    pub revision: String,
    pub objects: Vec<AnnotationObject>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnnotationTask {
    pub id: String,
    pub name: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskItem {
    pub id: String,
    pub task_id: String,
    pub image_id: String,
    pub status: String,
    pub qa_status: String,
    pub review_note: Option<String>,
    pub locked_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatasetSnapshot {
    pub id: String,
    pub name: String,
    pub image_count: u32,
    pub manifest_path: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatasetExport {
    pub id: String,
    pub snapshot_id: String,
    pub format: String,
    pub status: String,
    pub output_path: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectDetail {
    pub project: DatasetProject,
    pub tag_groups: Vec<TagGroup>,
    pub classes: Vec<ClassStat>,
    pub tasks: Vec<TaskSummary>,
    pub quality_checks: Vec<QualityCheck>,
    pub export_presets: Vec<ExportPreset>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BackendTask {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub progress: u8,
    pub message: String,
    pub started_at: String,
    pub finished_at: Option<String>,
}

impl BackendTask {
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        kind: impl Into<String>,
        status: impl Into<String>,
        progress: u8,
        message: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            kind: kind.into(),
            status: status.into(),
            progress,
            message: message.into(),
            started_at: now_unix_string(),
            finished_at: None,
        }
    }

    pub fn finished(mut self) -> Self {
        self.finished_at = Some(now_unix_string());
        self
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendLayer {
    pub name: String,
    pub responsibility: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendDesign {
    pub layers: Vec<BackendLayer>,
    pub storage_plan: String,
    pub command_plan: Vec<String>,
}

#[derive(Debug, Default)]
pub struct SampleRepository;

impl SampleRepository {
    pub fn new() -> Self {
        Self
    }

    pub fn dataset_projects(&self) -> Vec<DatasetProject> {
        self.dataset_projects_from(
            project_fs::list_project_manifests(),
            project_fs::project_paths,
        )
    }

    pub fn workspace_dataset_projects(&self) -> Vec<DatasetProject> {
        self.dataset_projects_from(
            project_fs::list_workspace_project_manifests(),
            project_fs::workspace_project_paths,
        )
    }

    fn dataset_projects_from(
        &self,
        manifests: Vec<project_fs::ProjectManifest>,
        project_paths: fn(&str) -> project_fs::ProjectPaths,
    ) -> Vec<DatasetProject> {
        let mut projects: Vec<_> = manifests
            .into_iter()
            .map(|manifest| {
                let paths = project_paths(&manifest.id);
                let indexed_manifest = storage::read_project_manifest(&paths.sqlite)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| manifest.clone());
                let indexed_images = storage::read_images(&paths.sqlite, None).unwrap_or_default();
                let indexed_classes = storage::read_classes(&paths.sqlite).unwrap_or_default();
                let image_count = if indexed_images.is_empty() {
                    indexed_manifest.image_count
                } else {
                    indexed_images.len() as u32
                };
                let annotated_count = indexed_images
                    .iter()
                    .filter(|image| image.status != "未标注" && image.status != "草稿")
                    .count() as u32;
                let review_count = indexed_images
                    .iter()
                    .filter(|image| image.qa_status == "待质检")
                    .count() as u32;
                let issue_count = indexed_images
                    .iter()
                    .filter(|image| image.qa_status == "驳回")
                    .count() as u32;
                let annotation_types = match indexed_manifest.format.as_str() {
                    "yolo-seg" => vec!["Polygon".to_string(), "BBox".to_string()],
                    "image-classification" => vec!["Classification".to_string()],
                    _ => vec!["BBox".to_string()],
                };
                let is_local_linked = indexed_manifest.source_dataset_key == "local-linked";
                DatasetProject {
                    id: indexed_manifest.id.clone(),
                    name: indexed_manifest.name.clone(),
                    description: if is_local_linked {
                        format!("本机目录 {}", indexed_manifest.root_path)
                    } else {
                        format!("真实 {} 测试数据集", indexed_manifest.source_dataset_key)
                    },
                    annotation_types,
                    image_count,
                    annotated_percent: if image_count > 0 {
                        ((annotated_count * 100) / image_count) as u8
                    } else {
                        0
                    },
                    review_count,
                    issue_count,
                    class_count: if indexed_classes.is_empty() {
                        indexed_manifest.class_count as u16
                    } else {
                        indexed_classes.len() as u16
                    },
                    tag_group_count: 3,
                    status: "已导入".to_string(),
                    tags: vec![
                        if is_local_linked {
                            "source: local-linked".to_string()
                        } else {
                            "source: ultralytics".to_string()
                        },
                        format!("format: {}", indexed_manifest.format),
                        "split: train".to_string(),
                    ],
                }
            })
            .collect();

        projects.sort_by(|left, right| left.name.cmp(&right.name));
        projects
    }

    pub fn project_detail(&self, project_id: &str) -> Option<ProjectDetail> {
        let project = self
            .dataset_projects()
            .into_iter()
            .find(|project| project.id == project_id)?;
        let images = self.project_images(project_id, None);
        let train_count = images.iter().filter(|image| image.split == "train").count() as u32;
        let val_count = images.iter().filter(|image| image.split == "val").count() as u32;
        let labels = coco_labels();
        let stored_classes = storage::read_classes(&project_fs::project_paths(project_id).sqlite)
            .unwrap_or_default();
        let paths = project_fs::project_paths(project_id);
        let task_summaries = storage::list_task_records(&paths.sqlite)
            .unwrap_or_default()
            .into_iter()
            .map(|task| TaskSummary {
                name: task.name,
                owner: "本地工作台".to_string(),
                status: task.status,
                progress: project.annotated_percent,
            })
            .collect::<Vec<_>>();
        let review_count = images
            .iter()
            .filter(|image| image.qa_status == "待质检")
            .count() as u32;
        let rejected_count = images
            .iter()
            .filter(|image| image.qa_status == "驳回")
            .count() as u32;
        let export_presets = storage::list_export_records(&paths.sqlite)
            .unwrap_or_default()
            .into_iter()
            .map(|item| ExportPreset {
                name: item.id,
                format: item.format,
                scope: item.snapshot_id,
                status: item.status,
            })
            .collect::<Vec<_>>();
        let project_progress = project.annotated_percent;

        Some(ProjectDetail {
            project,
            tag_groups: vec![
                TagGroup {
                    id: "train".to_string(),
                    name: "train".to_string(),
                    conditions: vec!["split=train".to_string()],
                    image_count: train_count,
                    annotated_percent: if train_count > 0 { 100 } else { 0 },
                    issue_count: 0,
                    export_enabled: true,
                },
                TagGroup {
                    id: "val".to_string(),
                    name: "val".to_string(),
                    conditions: vec!["split=val".to_string()],
                    image_count: val_count,
                    annotated_percent: if val_count > 0 { 100 } else { 0 },
                    issue_count: 0,
                    export_enabled: true,
                },
                TagGroup {
                    id: "unreviewed".to_string(),
                    name: "待审核".to_string(),
                    conditions: vec!["status=已标注".to_string()],
                    image_count: images.len() as u32,
                    annotated_percent: 100,
                    issue_count: 0,
                    export_enabled: false,
                },
            ],
            classes: if stored_classes.is_empty() {
                labels
                    .into_iter()
                    .take(12)
                    .enumerate()
                    .map(|(index, label)| ClassStat {
                        id: index as u32,
                        label,
                        color: class_color(index),
                        count: 0,
                        attributes: Vec::new(),
                    })
                    .collect()
            } else {
                stored_classes
                    .into_iter()
                    .take(12)
                    .map(|class| ClassStat {
                        id: class.id,
                        label: class.label,
                        color: class.color,
                        count: 0,
                        attributes: Vec::new(),
                    })
                    .collect()
            },
            tasks: if task_summaries.is_empty() {
                vec![TaskSummary {
                    name: "默认本地标注任务".to_string(),
                    owner: "本地工作台".to_string(),
                    status: "进行中".to_string(),
                    progress: project_progress,
                }]
            } else {
                task_summaries
            },
            quality_checks: [
                ("待质检样本", "info", review_count),
                ("驳回样本", "warning", rejected_count),
            ]
            .into_iter()
            .filter(|(_, _, count)| *count > 0)
            .map(|(name, severity, count)| QualityCheck {
                name: name.to_string(),
                severity: severity.to_string(),
                count,
            })
            .collect(),
            export_presets,
        })
    }

    pub fn project_images(&self, project_id: &str, group_id: Option<String>) -> Vec<DatasetImage> {
        self.project_images_paged(project_id, group_id, None, None)
    }

    pub fn class_samples(
        &self,
        project_id: &str,
        class_id: Option<u32>,
        label: &str,
        offset: Option<u32>,
        limit: Option<u32>,
    ) -> Vec<ClassSample> {
        let offset = offset.unwrap_or(0) as usize;
        let limit = limit.unwrap_or(u32::MAX) as usize;
        self.project_images(project_id, None)
            .into_iter()
            .filter_map(|image| {
                let match_count = self
                    .image_annotation_state(project_id, &image.id)
                    .objects
                    .into_iter()
                    .filter(|object| {
                        class_id.map(|id| object.class_id == id).unwrap_or(false)
                            || object.label == label
                    })
                    .count() as u32;
                (match_count > 0).then_some(ClassSample { image, match_count })
            })
            .skip(offset)
            .take(limit)
            .collect()
    }

    pub fn project_images_paged(
        &self,
        project_id: &str,
        group_id: Option<String>,
        offset: Option<u32>,
        limit: Option<u32>,
    ) -> Vec<DatasetImage> {
        let paths = project_fs::project_paths(project_id);
        let indexed_images = if let Some(limit) = limit {
            storage::read_images_page(
                &paths.sqlite,
                group_id.as_deref(),
                offset.unwrap_or(0),
                limit,
            )
            .unwrap_or_default()
        } else {
            storage::read_images(&paths.sqlite, group_id.as_deref()).unwrap_or_default()
        };
        if !indexed_images.is_empty() {
            return indexed_images
                .into_iter()
                .map(|image| DatasetImage {
                    id: image.id,
                    file_name: image.file_name,
                    width: image.width,
                    height: image.height,
                    split: image.split.clone(),
                    status: image.status,
                    qa_status: image.qa_status,
                    review_note: image.review_note,
                    tags: vec![format!("split={}", image.split)],
                })
                .collect();
        }
        let mut images = Vec::new();

        let asset_root = project_asset_root(project_id, &paths);
        let offset = offset.unwrap_or(0) as usize;
        let limit = limit.unwrap_or(u32::MAX) as usize;
        let mut skipped = 0usize;
        for entry in WalkDir::new(&asset_root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.file_type().is_file() && is_image_path(&entry.path().to_path_buf())
            })
        {
            let path = entry.path().to_path_buf();
            let split = split_for_path(&path);
            if let Some(group_id) = &group_id {
                if group_id != &split {
                    continue;
                }
            }
            if skipped < offset {
                skipped += 1;
                continue;
            }
            if images.len() >= limit {
                break;
            }

            let (width, height) = image::image_dimensions(&path).unwrap_or((0, 0));
            let file_name = path
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_else(|| "image.jpg".to_string());
            let id = path
                .file_stem()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_else(|| file_name.clone());

            images.push(DatasetImage {
                id,
                file_name,
                width,
                height,
                split: split.clone(),
                status: "已标注".to_string(),
                qa_status: String::new(),
                review_note: None,
                tags: vec![format!("split={split}")],
            });
        }

        images.sort_by(|left, right| left.file_name.cmp(&right.file_name));
        images
    }

    pub fn image_path(&self, project_id: &str, image_id: &str) -> Option<PathBuf> {
        let paths = project_fs::project_paths(project_id);
        let asset_root = project_asset_root(project_id, &paths);
        if let Ok(images) = storage::read_images(&paths.sqlite, None) {
            if let Some(image) = images.into_iter().find(|image| image.id == image_id) {
                let path = asset_root.join(image.file_name);
                if path.exists() {
                    return Some(path);
                }
            }
        }

        WalkDir::new(&asset_root)
            .into_iter()
            .filter_map(Result::ok)
            .find(|entry| {
                entry.file_type().is_file()
                    && is_image_path(&entry.path().to_path_buf())
                    && image_id_matches(&asset_root, entry.path(), image_id)
            })
            .map(|entry| entry.path().to_path_buf())
    }

    pub fn image_annotations(&self, project_id: &str, image_id: &str) -> Vec<AnnotationObject> {
        self.image_annotation_state(project_id, image_id).objects
    }

    pub fn image_annotation_state(&self, project_id: &str, image_id: &str) -> AnnotationState {
        let paths = project_fs::project_paths(project_id);
        if let Ok(Some(payload)) = storage::read_annotation_payload(&paths.sqlite, image_id) {
            let objects = serde_json::from_str::<Vec<AnnotationObject>>(&payload.object_json)
                .unwrap_or_default();
            return AnnotationState {
                image_id: image_id.to_string(),
                revision: Some(payload.revision),
                objects,
                status: image_status(project_id, image_id).unwrap_or_else(|| "草稿".to_string()),
                updated_at: Some(payload.updated_at),
            };
        }

        let native_path = paths.annotations.join(format!("{image_id}.json"));
        if let Ok(data) = fs::read_to_string(native_path) {
            if let Ok(state) = serde_json::from_str::<AnnotationState>(&data) {
                return state;
            }
            if let Ok(objects) = serde_json::from_str::<Vec<AnnotationObject>>(&data) {
                return AnnotationState {
                    image_id: image_id.to_string(),
                    revision: None,
                    objects,
                    status: image_status(project_id, image_id)
                        .unwrap_or_else(|| "草稿".to_string()),
                    updated_at: None,
                };
            }
        }

        let Some(image_path) = self.image_path(project_id, image_id) else {
            return AnnotationState {
                image_id: image_id.to_string(),
                revision: None,
                objects: Vec::new(),
                status: "图片未找到".to_string(),
                updated_at: None,
            };
        };
        let labels = storage::read_classes(&paths.sqlite)
            .unwrap_or_default()
            .into_iter()
            .map(|class| class.label)
            .collect::<Vec<_>>();
        let labels = if labels.is_empty() {
            coco_labels()
        } else {
            labels
        };
        let manifest = project_manifest(project_id);
        let root = manifest
            .as_ref()
            .map(|manifest| PathBuf::from(&manifest.root_path))
            .unwrap_or_else(|| paths.raw.clone());
        let format = manifest
            .as_ref()
            .map(|manifest| manifest.format.as_str())
            .unwrap_or("yolo-detect");
        let objects = match format {
            "voc-detect" => voc_adapter::load_annotations(&root, &image_path, &labels),
            "yolo-detect" | "yolo-seg" => {
                yolo_adapter::load_annotations(&root, &image_path, format, &labels)
            }
            "labelme" => {
                labelme::load_annotations(&root, &image_path, &labels).map(|loaded| loaded.objects)
            }
            "coco" => {
                let source = storage::read_dataset_source(&paths.sqlite)
                    .ok()
                    .flatten()
                    .ok_or_else(|| "COCO dataset source mapping not found".to_string());
                let mapping = storage::read_image_source(&paths.sqlite, image_id)
                    .ok()
                    .flatten()
                    .ok_or_else(|| format!("COCO image source mapping not found: {image_id}"));
                source.and_then(|source| {
                    mapping.and_then(|mapping| {
                        let annotation_path = source
                            .annotation_path
                            .map(|path| root.join(path))
                            .ok_or_else(|| "COCO annotation path not found".to_string())?;
                        let external_id = mapping.external_id.ok_or_else(|| {
                            format!("COCO external image id not found: {image_id}")
                        })?;
                        coco::load_image_annotations(&root, &annotation_path, &external_id)
                    })
                })
            }
            _ => Ok(Vec::new()),
        }
        .unwrap_or_default();
        let default_status = if objects.is_empty() {
            "未标注".to_string()
        } else {
            "已标注".to_string()
        };
        AnnotationState {
            image_id: image_id.to_string(),
            revision: None,
            objects,
            status: image_status(project_id, image_id).unwrap_or(default_status),
            updated_at: None,
        }
    }

    pub fn save_image_annotations(
        &self,
        project_id: &str,
        image_id: &str,
        objects: Vec<AnnotationObject>,
    ) -> Result<AnnotationSaveResult, String> {
        self.save_image_annotations_with_revision(project_id, image_id, None, objects)
    }

    pub fn save_image_annotations_with_revision(
        &self,
        project_id: &str,
        image_id: &str,
        revision: Option<String>,
        objects: Vec<AnnotationObject>,
    ) -> Result<AnnotationSaveResult, String> {
        let paths = project_fs::ensure_project_dirs(project_id)?;
        let object_json = serde_json::to_string(&objects).map_err(|err| err.to_string())?;
        let result = storage::save_annotation_payload(
            &paths.sqlite,
            image_id,
            revision.as_deref(),
            &object_json,
        )?;
        let state = AnnotationState {
            image_id: image_id.to_string(),
            revision: Some(result.revision.clone()),
            objects,
            status: "草稿".to_string(),
            updated_at: Some(result.saved_at.clone()),
        };
        let data = serde_json::to_string_pretty(&state).map_err(|err| err.to_string())?;
        fs::write(paths.annotations.join(format!("{image_id}.json")), data)
            .map_err(|err| err.to_string())?;
        if let (Some(image_path), Some(manifest)) = (
            self.image_path(project_id, image_id),
            project_manifest(project_id),
        ) {
            let root = PathBuf::from(&manifest.root_path);
            let source = storage::read_image_source(&paths.sqlite, image_id).unwrap_or_default();
            let expected_version = source
                .as_ref()
                .map(|mapping| mapping.source_version.as_str())
                .filter(|value| !value.is_empty());
            let synced = match manifest.format.as_str() {
                "voc-detect" => Some(voc_adapter::sync_annotations(
                    &root,
                    &image_path,
                    &state.objects,
                    expected_version,
                )?),
                "yolo-detect" => Some(yolo_adapter::sync_annotations(
                    &root,
                    &image_path,
                    "yolo-detect",
                    &state.objects,
                    expected_version,
                )?),
                "yolo-seg" => Some(yolo_adapter::sync_annotations(
                    &root,
                    &image_path,
                    "yolo-seg",
                    &state.objects,
                    expected_version,
                )?),
                "labelme" => Some(labelme::sync_annotations(
                    &root,
                    &image_path,
                    &state.objects,
                    expected_version,
                )?),
                _ => None,
            };
            if let Some(synced) = synced {
                let relative_path = image_path
                    .strip_prefix(&root)
                    .unwrap_or(&image_path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let annotation_path = synced
                    .path
                    .strip_prefix(&root)
                    .unwrap_or(&synced.path)
                    .to_string_lossy()
                    .replace('\\', "/");
                storage::write_image_source(
                    &paths.sqlite,
                    &storage::StoredImageSource {
                        image_id: image_id.to_string(),
                        relative_path,
                        external_id: source.and_then(|mapping| mapping.external_id),
                        annotation_path: Some(annotation_path),
                        source_version: synced.source_version,
                    },
                )?;
            }
        }
        Ok(AnnotationSaveResult {
            revision: result.revision,
            saved_at: result.saved_at,
            audit_event_id: result.audit_event_id,
        })
    }

    pub fn submit_image_annotations(&self, project_id: &str, image_id: &str) -> Result<(), String> {
        let paths = project_fs::project_paths(project_id);
        storage::submit_image_for_review(&paths.sqlite, image_id)
    }

    pub fn sync_dataset_source(&self, project_id: &str) -> Result<SourceSyncResult, String> {
        let paths = project_fs::project_paths(project_id);
        let source = storage::read_dataset_source(&paths.sqlite)?
            .ok_or_else(|| format!("dataset source mapping not found: {project_id}"))?;
        if source.format != "coco" {
            return Err(format!(
                "dataset-level source sync is not supported for {}",
                source.format
            ));
        }
        let root = PathBuf::from(&source.root_path);
        let annotation_path = source
            .annotation_path
            .as_ref()
            .map(|path| root.join(path))
            .ok_or_else(|| "COCO annotation path not found".to_string())?;
        let mut mappings = storage::read_image_sources(&paths.sqlite)?;
        let expected_version = mappings
            .iter()
            .map(|mapping| mapping.source_version.as_str())
            .find(|value| !value.is_empty());
        let mut objects_by_image = BTreeMap::new();
        for mapping in &mappings {
            let external_id = mapping
                .external_id
                .as_ref()
                .ok_or_else(|| format!("COCO external image id not found: {}", mapping.image_id))?
                .clone();
            objects_by_image.insert(
                external_id,
                self.image_annotation_state(project_id, &mapping.image_id)
                    .objects,
            );
        }
        let result =
            coco::sync_dataset(&root, &annotation_path, &objects_by_image, expected_version)?;
        for mapping in &mut mappings {
            mapping.source_version = result.source_version.clone();
        }
        storage::replace_image_sources(&paths.sqlite, &mappings)?;
        Ok(result)
    }

    pub fn annotation_history(
        &self,
        project_id: &str,
        image_id: &str,
    ) -> Result<Vec<AnnotationVersion>, String> {
        let paths = project_fs::project_paths(project_id);
        storage::read_annotation_versions(&paths.sqlite, image_id)?
            .into_iter()
            .map(|record| {
                Ok(AnnotationVersion {
                    id: record.id,
                    image_id: record.image_id,
                    revision: record.revision,
                    objects: serde_json::from_str(&record.object_json)
                        .map_err(|err| err.to_string())?,
                    created_at: record.created_at,
                })
            })
            .collect()
    }

    pub fn restore_annotation_version(
        &self,
        project_id: &str,
        image_id: &str,
        revision: &str,
    ) -> Result<AnnotationSaveResult, String> {
        let version = self
            .annotation_history(project_id, image_id)?
            .into_iter()
            .find(|version| version.revision == revision)
            .ok_or_else(|| format!("annotation revision not found: {revision}"))?;
        let current_revision = self.image_annotation_state(project_id, image_id).revision;
        self.save_image_annotations_with_revision(
            project_id,
            image_id,
            current_revision,
            version.objects,
        )
    }

    pub fn create_annotation_task(
        &self,
        project_id: &str,
        name: &str,
    ) -> Result<AnnotationTask, String> {
        let paths = project_fs::project_paths(project_id);
        let images = self.project_images(project_id, None);
        let image_ids: Vec<_> = images.iter().map(|image| image.id.as_str()).collect();
        let record = storage::create_annotation_task_record(&paths.sqlite, name, &image_ids)?;
        Ok(task_from_record(record))
    }

    pub fn annotation_tasks(&self, project_id: &str) -> Result<Vec<AnnotationTask>, String> {
        let paths = project_fs::project_paths(project_id);
        Ok(storage::list_task_records(&paths.sqlite)?
            .into_iter()
            .map(task_from_record)
            .collect())
    }

    pub fn task_items(&self, project_id: &str, task_id: &str) -> Result<Vec<TaskItem>, String> {
        let paths = project_fs::project_paths(project_id);
        Ok(storage::list_task_item_records(&paths.sqlite, task_id)?
            .into_iter()
            .map(task_item_from_record)
            .collect())
    }

    pub fn claim_task_item(
        &self,
        project_id: &str,
        task_id: &str,
        image_id: &str,
    ) -> Result<(), String> {
        let paths = project_fs::project_paths(project_id);
        storage::claim_task_item(&paths.sqlite, task_id, image_id)
    }

    pub fn release_task_item(
        &self,
        project_id: &str,
        task_id: &str,
        image_id: &str,
    ) -> Result<(), String> {
        let paths = project_fs::project_paths(project_id);
        storage::release_task_item(&paths.sqlite, task_id, image_id)
    }

    pub fn review_task_item(
        &self,
        project_id: &str,
        image_id: &str,
        decision: &str,
        note: &str,
    ) -> Result<(), String> {
        let paths = project_fs::project_paths(project_id);
        storage::review_image(&paths.sqlite, image_id, decision, note)
    }

    pub fn review_queue(&self, project_id: &str) -> Result<Vec<DatasetImage>, String> {
        let paths = project_fs::project_paths(project_id);
        Ok(storage::read_review_queue(&paths.sqlite)?
            .into_iter()
            .map(|image| DatasetImage {
                id: image.id,
                file_name: image.file_name,
                width: image.width,
                height: image.height,
                split: image.split.clone(),
                status: image.status,
                qa_status: image.qa_status,
                review_note: image.review_note,
                tags: vec![format!("split={}", image.split)],
            })
            .collect())
    }

    pub fn create_dataset_snapshot(
        &self,
        project_id: &str,
        name: &str,
    ) -> Result<DatasetSnapshot, String> {
        let paths = project_fs::ensure_project_dirs(project_id)?;
        let images = self.project_images(project_id, None);
        let classes = storage::read_classes(&paths.sqlite)?
            .into_iter()
            .map(|class| class.label)
            .collect::<Vec<_>>();
        let annotations: Vec<_> = images
            .iter()
            .map(|image| {
                let state = self.image_annotation_state(project_id, &image.id);
                json!({
                    "imageId": image.id,
                    "fileName": image.file_name,
                    "width": image.width,
                    "height": image.height,
                    "split": image.split,
                    "status": image.status,
                    "revision": state.revision,
                    "objects": state.objects,
                })
            })
            .collect();
        let manifest = json!({
            "projectId": project_id,
            "name": name,
            "imageCount": images.len(),
            "classes": classes,
            "annotations": annotations,
        });
        let manifest_json =
            serde_json::to_string_pretty(&manifest).map_err(|err| err.to_string())?;
        let record = storage::create_snapshot_record(
            &paths.sqlite,
            name,
            &manifest_json,
            images.len() as u32,
        )?;
        let snapshot_dir = paths.snapshots.join(&record.id);
        fs::create_dir_all(&snapshot_dir).map_err(|err| err.to_string())?;
        let manifest_path = snapshot_dir.join("manifest.json");
        fs::write(&manifest_path, manifest_json).map_err(|err| err.to_string())?;
        Ok(DatasetSnapshot {
            id: record.id,
            name: record.name,
            image_count: record.image_count,
            manifest_path: manifest_path.to_string_lossy().to_string(),
            created_at: record.created_at,
        })
    }

    pub fn dataset_snapshots(&self, project_id: &str) -> Result<Vec<DatasetSnapshot>, String> {
        let paths = project_fs::project_paths(project_id);
        Ok(storage::list_snapshot_records(&paths.sqlite)?
            .into_iter()
            .map(|record| DatasetSnapshot {
                manifest_path: paths
                    .snapshots
                    .join(&record.id)
                    .join("manifest.json")
                    .to_string_lossy()
                    .to_string(),
                id: record.id,
                name: record.name,
                image_count: record.image_count,
                created_at: record.created_at,
            })
            .collect())
    }

    pub fn export_dataset(
        &self,
        project_id: &str,
        snapshot_id: &str,
        options: &ExportOptions,
    ) -> Result<DatasetExport, String> {
        let paths = project_fs::ensure_project_dirs(project_id)?;
        let output_dir = paths
            .exports
            .join(format!("{snapshot_id}-{}", options.format));
        let pending_dir = paths
            .exports
            .join(format!(".{snapshot_id}-{}.pending", options.format));
        let manifest_path = paths.snapshots.join(snapshot_id).join("manifest.json");
        let manifest_json = fs::read_to_string(&manifest_path).map_err(|err| err.to_string())?;
        let mut snapshot: SnapshotData =
            serde_json::from_str(&manifest_json).map_err(|err| err.to_string())?;
        if snapshot.classes.is_empty() {
            snapshot.classes = storage::read_classes(&paths.sqlite)?
                .into_iter()
                .map(|class| class.label)
                .collect();
        }
        let indexed_images = storage::read_images(&paths.sqlite, None)?;
        for image in &mut snapshot.images {
            if let Some(indexed) = indexed_images.iter().find(|item| item.id == image.image_id) {
                if image.width == 0 {
                    image.width = indexed.width;
                }
                if image.height == 0 {
                    image.height = indexed.height;
                }
                if image.split.is_empty() {
                    image.split.clone_from(&indexed.split);
                }
            }
        }
        let source_root = project_asset_root(project_id, &paths);
        if pending_dir.exists() {
            fs::remove_dir_all(&pending_dir).map_err(|err| err.to_string())?;
        }
        let export_result =
            exporters::export_snapshot(&snapshot, &source_root, &pending_dir, options);
        if let Err(error) = export_result {
            let _ = fs::remove_dir_all(&pending_dir);
            return Err(error);
        }
        fs::write(pending_dir.join("snapshot-manifest.json"), &manifest_json)
            .map_err(|err| err.to_string())?;
        if output_dir.exists() {
            fs::remove_dir_all(&output_dir).map_err(|err| err.to_string())?;
        }
        fs::rename(&pending_dir, &output_dir).map_err(|err| err.to_string())?;
        let record = storage::create_export_record(
            &paths.sqlite,
            snapshot_id,
            &options.format,
            &output_dir.to_string_lossy(),
        )?;
        Ok(DatasetExport {
            id: record.id,
            snapshot_id: record.snapshot_id,
            format: record.format,
            status: record.status,
            output_path: record.output_path,
            created_at: record.created_at,
        })
    }

    pub fn dataset_exports(&self, project_id: &str) -> Result<Vec<DatasetExport>, String> {
        let paths = project_fs::project_paths(project_id);
        Ok(storage::list_export_records(&paths.sqlite)?
            .into_iter()
            .map(|record| DatasetExport {
                id: record.id,
                snapshot_id: record.snapshot_id,
                format: record.format,
                status: record.status,
                output_path: record.output_path,
                created_at: record.created_at,
            })
            .collect())
    }
}

pub fn is_image_path(path: &PathBuf) -> bool {
    path.extension()
        .map(|extension| {
            matches!(
                extension.to_string_lossy().to_ascii_lowercase().as_str(),
                "jpg" | "jpeg" | "png" | "bmp" | "webp"
            )
        })
        .unwrap_or(false)
}

fn split_for_path(path: &PathBuf) -> String {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    if lower.contains("val") {
        "val".to_string()
    } else if lower.contains("test") {
        "test".to_string()
    } else {
        "train".to_string()
    }
}

fn project_asset_root(project_id: &str, paths: &project_fs::ProjectPaths) -> PathBuf {
    project_manifest(project_id)
        .filter(|manifest| manifest.source_dataset_key == "local-linked")
        .map(|manifest| PathBuf::from(manifest.root_path))
        .filter(|path| path.exists())
        .unwrap_or_else(|| paths.raw.clone())
}

fn project_manifest(project_id: &str) -> Option<project_fs::ProjectManifest> {
    let paths = project_fs::project_paths(project_id);
    storage::read_project_manifest(&paths.sqlite)
        .ok()
        .flatten()
        .or_else(|| project_fs::read_manifest(project_id))
}

fn image_id_matches(root: &Path, image_path: &Path, image_id: &str) -> bool {
    if image_path
        .file_stem()
        .map(|value| value.to_string_lossy() == image_id)
        .unwrap_or(false)
    {
        return true;
    }
    let relative = image_path
        .strip_prefix(root)
        .map(|value| value.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    image_id_from_relative(&relative) == image_id
}

fn image_id_from_relative(relative: &str) -> String {
    Path::new(relative)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/")
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn image_status(project_id: &str, image_id: &str) -> Option<String> {
    let path = project_fs::project_paths(project_id).sqlite;
    storage::read_images(&path, None)
        .ok()?
        .into_iter()
        .find(|image| image.id == image_id)
        .map(|image| image.status)
}

pub fn coco_labels() -> Vec<String> {
    [
        "person",
        "bicycle",
        "car",
        "motorcycle",
        "airplane",
        "bus",
        "train",
        "truck",
        "boat",
        "traffic light",
        "fire hydrant",
        "stop sign",
        "parking meter",
        "bench",
        "bird",
        "cat",
        "dog",
        "horse",
        "sheep",
        "cow",
        "elephant",
        "bear",
        "zebra",
        "giraffe",
        "backpack",
        "umbrella",
        "handbag",
        "tie",
        "suitcase",
        "frisbee",
        "skis",
        "snowboard",
        "sports ball",
        "kite",
        "baseball bat",
        "baseball glove",
        "skateboard",
        "surfboard",
        "tennis racket",
        "bottle",
        "wine glass",
        "cup",
        "fork",
        "knife",
        "spoon",
        "bowl",
        "banana",
        "apple",
        "sandwich",
        "orange",
        "broccoli",
        "carrot",
        "hot dog",
        "pizza",
        "donut",
        "cake",
        "chair",
        "couch",
        "potted plant",
        "bed",
        "dining table",
        "toilet",
        "tv",
        "laptop",
        "mouse",
        "remote",
        "keyboard",
        "cell phone",
        "microwave",
        "oven",
        "toaster",
        "sink",
        "refrigerator",
        "book",
        "clock",
        "vase",
        "scissors",
        "teddy bear",
        "hair drier",
        "toothbrush",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn class_color(index: usize) -> String {
    const COLORS: [&str; 8] = [
        "#1fa7ff", "#cc54d8", "#f59e0b", "#22c55e", "#8b5cf6", "#ef4444", "#14b8a6", "#64748b",
    ];
    COLORS[index % COLORS.len()].to_string()
}

fn task_from_record(record: storage::TaskRecord) -> AnnotationTask {
    AnnotationTask {
        id: record.id,
        name: record.name,
        status: record.status,
        created_at: record.created_at,
        updated_at: record.updated_at,
    }
}

fn task_item_from_record(record: storage::TaskItemRecord) -> TaskItem {
    TaskItem {
        id: record.id,
        task_id: record.task_id,
        image_id: record.image_id,
        status: record.status,
        qa_status: record.qa_status,
        review_note: record.review_note,
        locked_at: record.locked_at,
    }
}

pub fn backend_design() -> BackendDesign {
    BackendDesign {
        layers: vec![
            BackendLayer {
                name: "Command API".to_string(),
                responsibility: "Tauri commands expose real dataset downloads, project indexing, annotation persistence, and independent annotation windows.".to_string(),
            },
            BackendLayer {
                name: "Project FS".to_string(),
                responsibility: "The local data/workspaces/default workspace stores project manifests, original assets, native annotations, thumbnails, snapshots, exports, and SQLite databases; data/test_data remains reserved for builtin test datasets.".to_string(),
            },
            BackendLayer {
                name: "Importer".to_string(),
                responsibility: "YOLO detection and segmentation labels are converted into internal bbox and polygon annotation objects.".to_string(),
            },
            BackendLayer {
                name: "Repository".to_string(),
                responsibility: "Repositories scan the local project structure and persist edited annotations in portable JSON sidecars.".to_string(),
            },
        ],
        storage_plan: "Use data/workspaces/default/projects/{projectId} for production projects and data/test_data/projects/{projectId} only for builtin demo datasets.".to_string(),
        command_plan: vec![
            "backend_health".to_string(),
            "list_builtin_datasets".to_string(),
            "download_test_dataset".to_string(),
            "create_project".to_string(),
            "pick_data_source".to_string(),
            "analyze_data_source".to_string(),
            "open_local_dataset".to_string(),
            "import_files".to_string(),
            "import_images".to_string(),
            "import_yolo_dataset".to_string(),
            "list_dataset_projects".to_string(),
            "get_project_detail".to_string(),
            "list_project_images".to_string(),
            "get_image_annotation_state".to_string(),
            "get_image_annotations".to_string(),
            "save_image_annotations".to_string(),
            "submit_image_annotations".to_string(),
            "create_dataset_snapshot".to_string(),
            "export_dataset".to_string(),
            "sync_dataset_source".to_string(),
            "open_annotation_window".to_string(),
            "list_backend_tasks".to_string(),
            "clear_completed_backend_tasks".to_string(),
        ],
    }
}

fn now_unix_string() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_images_that_contain_selected_class_with_match_counts() {
        let repository = SampleRepository::new();
        let project_id = "class-sample-unit";
        let paths = project_fs::project_paths(project_id);
        let _ = std::fs::remove_dir_all(&paths.root);
        project_fs::ensure_workspace_project_dirs(project_id).unwrap();
        storage::initialize_project_database(&paths.sqlite).unwrap();

        let manifest = project_fs::ProjectManifest {
            id: project_id.to_string(),
            name: "Class Sample Unit".to_string(),
            source_dataset_key: "local-demo".to_string(),
            format: "yolo-detect".to_string(),
            root_path: paths.root.to_string_lossy().to_string(),
            created_at: now_unix_string(),
            class_count: 2,
            image_count: 2,
        };
        let images = vec![
            storage::StoredImage {
                id: "image-a".to_string(),
                file_name: "image-a.png".to_string(),
                width: 640,
                height: 480,
                split: "train".to_string(),
                status: "已标注".to_string(),
                qa_status: String::new(),
                review_note: None,
            },
            storage::StoredImage {
                id: "image-b".to_string(),
                file_name: "image-b.png".to_string(),
                width: 640,
                height: 480,
                split: "train".to_string(),
                status: "已标注".to_string(),
                qa_status: String::new(),
                review_note: None,
            },
        ];
        let classes = vec![
            storage::StoredClass {
                id: 0,
                label: "person".to_string(),
                color: "#1fa7ff".to_string(),
            },
            storage::StoredClass {
                id: 1,
                label: "car".to_string(),
                color: "#cc54d8".to_string(),
            },
        ];
        storage::upsert_project_index(&paths.sqlite, &manifest, &images, &classes).unwrap();

        repository
            .save_image_annotations_with_revision(
                project_id,
                "image-a",
                None,
                vec![
                    AnnotationObject::bbox(
                        "ann-1".to_string(),
                        0,
                        "person".to_string(),
                        BBox {
                            x: 1.0,
                            y: 1.0,
                            width: 10.0,
                            height: 10.0,
                        },
                    ),
                    AnnotationObject::bbox(
                        "ann-2".to_string(),
                        0,
                        "person".to_string(),
                        BBox {
                            x: 2.0,
                            y: 2.0,
                            width: 10.0,
                            height: 10.0,
                        },
                    ),
                ],
            )
            .unwrap();
        repository
            .save_image_annotations_with_revision(
                project_id,
                "image-b",
                None,
                vec![AnnotationObject::bbox(
                    "ann-3".to_string(),
                    1,
                    "car".to_string(),
                    BBox {
                        x: 1.0,
                        y: 1.0,
                        width: 10.0,
                        height: 10.0,
                    },
                )],
            )
            .unwrap();

        let samples = repository.class_samples(project_id, Some(0), "person", Some(0), Some(48));

        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].image.id, "image-a");
        assert_eq!(samples[0].match_count, 2);

        let _ = std::fs::remove_dir_all(paths.root);
    }

    #[test]
    fn repository_exports_snapshot_with_explicit_polygon_policy() {
        let name = format!("export-fixture-{}", now_unix_string());
        let project =
            crate::datasets::create_dataset_project(&name, "yolo-seg", "demo-polygon").unwrap();
        let repository = SampleRepository::new();
        let snapshot = repository
            .create_dataset_snapshot(&project.id, "export fixture")
            .unwrap();

        let blocked = repository.export_dataset(
            &project.id,
            &snapshot.id,
            &ExportOptions {
                format: "yolo-detect".to_string(),
                polygon_policy: None,
                include_images: false,
            },
        );
        assert!(blocked.unwrap_err().contains("polygonPolicy"));

        let exported = repository
            .export_dataset(
                &project.id,
                &snapshot.id,
                &ExportOptions {
                    format: "yolo-detect".to_string(),
                    polygon_policy: Some("bbox".to_string()),
                    include_images: true,
                },
            )
            .unwrap();
        assert!(Path::new(&exported.output_path)
            .join("labels")
            .join("train")
            .join("demo_001.txt")
            .exists());
        assert!(Path::new(&exported.output_path)
            .join("export-manifest.json")
            .exists());
        let retained_marker = Path::new(&exported.output_path).join("retained.txt");
        fs::write(&retained_marker, "previous export").unwrap();
        let blocked_again = repository.export_dataset(
            &project.id,
            &snapshot.id,
            &ExportOptions {
                format: "yolo-detect".to_string(),
                polygon_policy: None,
                include_images: false,
            },
        );
        assert!(blocked_again.unwrap_err().contains("polygonPolicy"));
        assert_eq!(
            fs::read_to_string(&retained_marker).unwrap(),
            "previous export"
        );

        let _ = fs::remove_dir_all(project_fs::project_paths(&project.id).root);
    }

    #[test]
    fn repository_enriches_legacy_snapshot_before_export() {
        let name = format!("legacy-export-fixture-{}", now_unix_string());
        let project =
            crate::datasets::create_dataset_project(&name, "yolo-seg", "demo-polygon").unwrap();
        let repository = SampleRepository::new();
        let snapshot = repository
            .create_dataset_snapshot(&project.id, "legacy export fixture")
            .unwrap();
        let manifest_path = Path::new(&snapshot.manifest_path);
        let mut manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(manifest_path).unwrap()).unwrap();
        manifest.as_object_mut().unwrap().remove("classes");
        for image in manifest["annotations"].as_array_mut().unwrap() {
            let image = image.as_object_mut().unwrap();
            image.remove("width");
            image.remove("height");
            image.remove("split");
        }
        fs::write(
            manifest_path,
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let exported = repository
            .export_dataset(
                &project.id,
                &snapshot.id,
                &ExportOptions {
                    format: "yolo-seg".to_string(),
                    polygon_policy: None,
                    include_images: false,
                },
            )
            .unwrap();

        let output = Path::new(&exported.output_path);
        assert!(output
            .join("labels")
            .join("train")
            .join("demo_001.txt")
            .exists());
        assert!(!fs::read_to_string(output.join("classes.txt"))
            .unwrap()
            .trim()
            .is_empty());

        let _ = fs::remove_dir_all(project_fs::project_paths(&project.id).root);
    }
}

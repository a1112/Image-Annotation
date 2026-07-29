use crate::{
    bridge::{self, BridgeBuildInput, BridgeObject, BridgePoint, BridgeSourceSample, BridgeSplit},
    importers::{voc, yolo},
    project_fs, storage,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Component, Path, PathBuf},
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

    pub fn classification(id: String, class_id: u32, label: String) -> Self {
        Self {
            id,
            class_id,
            label,
            object_type: "classification".to_string(),
            bbox: None,
            polygon: None,
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
    #[serde(default)]
    pub bridge_manifest_path: Option<String>,
    #[serde(default = "legacy_bridge_status")]
    pub bridge_status: String,
}

fn legacy_bridge_status() -> String {
    "legacy".to_string()
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
        let mut projects: Vec<_> = project_fs::list_project_manifests()
            .into_iter()
            .map(|manifest| {
                let paths = project_fs::project_paths(&manifest.id);
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
        if is_classification_project(project_id) {
            let classes = storage::read_classes(&paths.sqlite).unwrap_or_default();
            if let Some((class_id, label)) = classification_for_image(&image_path, &classes) {
                return AnnotationState {
                    image_id: image_id.to_string(),
                    revision: None,
                    objects: vec![AnnotationObject::classification(
                        format!("classification-{image_id}"),
                        class_id,
                        label,
                    )],
                    status: image_status(project_id, image_id)
                        .unwrap_or_else(|| "已标注".to_string()),
                    updated_at: None,
                };
            }
            return AnnotationState {
                image_id: image_id.to_string(),
                revision: None,
                objects: Vec::new(),
                status: image_status(project_id, image_id).unwrap_or_else(|| "未标注".to_string()),
                updated_at: None,
            };
        }
        if is_voc_project(project_id) {
            let label_path = image_path.with_extension("xml");
            if let Ok(xml) = fs::read_to_string(label_path) {
                let labels = storage::read_classes(&paths.sqlite)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|class| class.label)
                    .collect::<Vec<_>>();
                let objects = voc::parse_voc_annotations(&xml, &labels).unwrap_or_default();
                return AnnotationState {
                    image_id: image_id.to_string(),
                    revision: None,
                    objects,
                    status: image_status(project_id, image_id)
                        .unwrap_or_else(|| "已标注".to_string()),
                    updated_at: None,
                };
            }
            return AnnotationState {
                image_id: image_id.to_string(),
                revision: None,
                objects: Vec::new(),
                status: image_status(project_id, image_id).unwrap_or_else(|| "未标注".to_string()),
                updated_at: None,
            };
        }
        let Some(label_path) = yolo_label_path_for_image(project_id, &image_path) else {
            return AnnotationState {
                image_id: image_id.to_string(),
                revision: None,
                objects: Vec::new(),
                status: image_status(project_id, image_id).unwrap_or_else(|| "未标注".to_string()),
                updated_at: None,
            };
        };
        let Ok(label_data) = fs::read_to_string(label_path) else {
            return AnnotationState {
                image_id: image_id.to_string(),
                revision: None,
                objects: Vec::new(),
                status: image_status(project_id, image_id).unwrap_or_else(|| "未标注".to_string()),
                updated_at: None,
            };
        };

        let (width, height) = image::image_dimensions(&image_path).unwrap_or((0, 0));
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
        let prefer_polygon = project_fs::read_manifest(project_id)
            .map(|manifest| manifest.format == "yolo-seg")
            .unwrap_or(false);

        let objects = label_data
            .lines()
            .enumerate()
            .filter_map(|(index, line)| {
                yolo::line_to_annotation(line, width, height, &labels, index, prefer_polygon).ok()
            })
            .collect();
        AnnotationState {
            image_id: image_id.to_string(),
            revision: None,
            objects,
            status: image_status(project_id, image_id).unwrap_or_else(|| "已标注".to_string()),
            updated_at: None,
        }
    }

    fn snapshot_annotation_state_strict(
        &self,
        paths: &project_fs::ProjectPaths,
        project: &project_fs::ProjectManifest,
        classes: &[storage::StoredClass],
        source_asset_root: &Path,
        image: &DatasetImage,
    ) -> Result<AnnotationState, String> {
        let image_path = resolve_snapshot_image_path(
            source_asset_root,
            &paths.raw,
            &image.file_name,
            &image.id,
        )?;
        let dimensions =
            crate::datasets::oriented_image_dimensions(&image_path).map_err(|error| {
                format!(
                    "read oriented image dimensions for '{}' at {}: {error}",
                    image.id,
                    image_path.display()
                )
            })?;
        let (image_width, image_height) = dimensions;
        if (image_width, image_height) != (image.width, image.height) {
            return Err(format!(
                "image '{}' dimensions changed from {}x{} to {}x{}; rescan project assets before creating a snapshot",
                image.id, image.width, image.height, image_width, image_height
            ));
        }

        if let Some(payload) = storage::read_annotation_payload(&paths.sqlite, &image.id)? {
            let objects = serde_json::from_str::<Vec<AnnotationObject>>(&payload.object_json)
                .map_err(|error| {
                    format!(
                        "invalid SQLite annotation payload for image '{}': {error}",
                        image.id
                    )
                })?;
            return Ok(AnnotationState {
                image_id: image.id.clone(),
                revision: Some(payload.revision),
                objects,
                status: image.status.clone(),
                updated_at: Some(payload.updated_at),
            });
        }

        let native_path = paths.annotations.join(format!("{}.json", image.id));
        if native_path.exists() {
            let data = fs::read_to_string(&native_path).map_err(|error| {
                format!(
                    "read native annotation for image '{}' at {}: {error}",
                    image.id,
                    native_path.display()
                )
            })?;
            if let Ok(state) = serde_json::from_str::<AnnotationState>(&data) {
                return Ok(state);
            }
            let objects =
                serde_json::from_str::<Vec<AnnotationObject>>(&data).map_err(|error| {
                    format!(
                        "invalid native annotation JSON for image '{}' at {}: {error}",
                        image.id,
                        native_path.display()
                    )
                })?;
            return Ok(AnnotationState {
                image_id: image.id.clone(),
                revision: None,
                objects,
                status: image.status.clone(),
                updated_at: None,
            });
        }

        if project.format == "image-classification" {
            let objects = classification_for_image(&image_path, classes)
                .map(|(class_id, label)| {
                    vec![AnnotationObject::classification(
                        format!("classification-{}", image.id),
                        class_id,
                        label,
                    )]
                })
                .unwrap_or_default();
            return Ok(AnnotationState {
                image_id: image.id.clone(),
                revision: None,
                objects,
                status: image.status.clone(),
                updated_at: None,
            });
        }

        let labels = classes
            .iter()
            .map(|class| class.label.clone())
            .collect::<Vec<_>>();
        if project.format == "voc-detect" {
            let label_path = image_path.with_extension("xml");
            let objects = if label_path.exists() {
                let xml = fs::read_to_string(&label_path).map_err(|error| {
                    format!(
                        "read VOC annotation for image '{}' at {}: {error}",
                        image.id,
                        label_path.display()
                    )
                })?;
                voc::parse_voc_annotations(&xml, &labels).map_err(|error| {
                    format!(
                        "invalid VOC annotation for image '{}' at {}: {error}",
                        image.id,
                        label_path.display()
                    )
                })?
            } else {
                Vec::new()
            };
            return Ok(AnnotationState {
                image_id: image.id.clone(),
                revision: None,
                objects,
                status: image.status.clone(),
                updated_at: None,
            });
        }

        let label_path = [Path::new(&project.root_path), paths.raw.as_path()]
            .into_iter()
            .filter_map(|root| fs::canonicalize(root).ok())
            .find_map(|root| label_path_for_image(&root, &image_path))
            .or_else(|| {
                let adjacent = image_path.with_extension("txt");
                adjacent.is_file().then_some(adjacent)
            });
        let Some(label_path) = label_path else {
            return Ok(AnnotationState {
                image_id: image.id.clone(),
                revision: None,
                objects: Vec::new(),
                status: image.status.clone(),
                updated_at: None,
            });
        };
        let label_data = fs::read_to_string(&label_path).map_err(|error| {
            format!(
                "read YOLO annotation for image '{}' at {}: {error}",
                image.id,
                label_path.display()
            )
        })?;
        let prefer_polygon = project.format == "yolo-seg";
        let mut objects = Vec::new();
        for (index, line) in label_data.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let object = yolo::line_to_annotation(
                line,
                image_width,
                image_height,
                &labels,
                index,
                prefer_polygon,
            )
            .map_err(|error| {
                format!(
                    "invalid YOLO annotation for image '{}' at line {}: {error}",
                    image.id,
                    index + 1
                )
            })?;
            objects.push(object);
        }
        Ok(AnnotationState {
            image_id: image.id.clone(),
            revision: None,
            objects,
            status: image.status.clone(),
            updated_at: None,
        })
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
        if is_voc_project(project_id) {
            if let Some(image_path) = self.image_path(project_id, image_id) {
                let (width, height) = image::image_dimensions(&image_path).unwrap_or((0, 0));
                let xml = voc::annotations_to_voc_xml(&image_path, width, height, &state.objects)?;
                fs::write(image_path.with_extension("xml"), xml).map_err(|err| err.to_string())?;
            }
        }
        if let Some(yolo_format) = yolo_project_format(project_id) {
            if let Some(image_path) = self.image_path(project_id, image_id) {
                let (width, height) = image::image_dimensions(&image_path).unwrap_or((0, 0));
                let label_path = yolo_label_write_path_for_image(project_id, &image_path);
                if let Some(parent) = label_path.parent() {
                    fs::create_dir_all(parent).map_err(|err| err.to_string())?;
                }
                let label_data = if yolo_format == "yolo-seg" {
                    yolo::annotations_to_yolo_polygon_lines(&state.objects, width, height)?
                } else {
                    yolo::annotations_to_yolo_lines(&state.objects, width, height)?
                };
                fs::write(label_path, label_data).map_err(|err| err.to_string())?;
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

    pub fn project_issues(
        &self,
        project_id: &str,
        include_closed: bool,
    ) -> Result<Vec<crate::hybrid::IssueRecord>, String> {
        let paths = project_fs::project_paths(project_id);
        storage::list_issue_records(&paths.sqlite, project_id, include_closed)
    }

    pub fn create_project_issue(
        &self,
        project_id: &str,
        image_id: &str,
        annotation_object_id: Option<&str>,
        title: &str,
        description: &str,
        severity: &str,
        assignee_id: Option<&str>,
    ) -> Result<crate::hybrid::IssueRecord, String> {
        let paths = project_fs::project_paths(project_id);
        storage::create_issue_record(
            &paths.sqlite,
            project_id,
            image_id,
            annotation_object_id,
            title,
            description,
            severity,
            assignee_id,
        )
    }

    pub fn transition_project_issue(
        &self,
        project_id: &str,
        issue_id: &str,
        next_status: &str,
    ) -> Result<crate::hybrid::IssueRecord, String> {
        let paths = project_fs::project_paths(project_id);
        storage::transition_issue_record(&paths.sqlite, project_id, issue_id, next_status)
    }

    pub fn add_project_issue_comment(
        &self,
        project_id: &str,
        issue_id: &str,
        content: &str,
    ) -> Result<crate::hybrid::IssueCommentRecord, String> {
        let paths = project_fs::project_paths(project_id);
        storage::add_issue_comment_record(&paths.sqlite, project_id, issue_id, content)
    }

    pub fn project_issue_comments(
        &self,
        project_id: &str,
        issue_id: &str,
    ) -> Result<Vec<crate::hybrid::IssueCommentRecord>, String> {
        let paths = project_fs::project_paths(project_id);
        storage::list_issue_comment_records(&paths.sqlite, issue_id)
    }

    pub fn project_sync_summary(
        &self,
        project_id: &str,
    ) -> Result<crate::hybrid::SyncSummary, String> {
        let paths = project_fs::project_paths(project_id);
        storage::read_sync_summary(&paths.sqlite, project_id)
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
        let project = storage::read_project_manifest(&paths.sqlite)?
            .or_else(|| project_fs::read_manifest(project_id))
            .ok_or_else(|| format!("project manifest not found: {project_id}"))?;
        let classes = storage::read_classes(&paths.sqlite)?;
        let source_asset_root = PathBuf::from(&project.root_path);
        let source_asset_root = if source_asset_root.exists() {
            source_asset_root
        } else {
            paths.raw.clone()
        };
        let images = storage::read_images(&paths.sqlite, None)?
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
            .collect::<Vec<_>>();
        let snapshot_sources: Vec<_> = images
            .iter()
            .map(|image| -> Result<_, String> {
                let state = self.snapshot_annotation_state_strict(
                    &paths,
                    &project,
                    &classes,
                    &source_asset_root,
                    image,
                )?;
                let annotation = json!({
                    "imageId": image.id,
                    "fileName": image.file_name,
                    "width": image.width,
                    "height": image.height,
                    "split": image.split,
                    "status": image.status,
                    "revision": state.revision,
                    "objects": state.objects,
                });
                Ok((image.clone(), state, annotation))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let annotations = snapshot_sources
            .iter()
            .map(|(_, _, annotation)| annotation.clone())
            .collect::<Vec<_>>();
        let manifest = json!({
            "projectId": project_id,
            "name": name,
            "imageCount": images.len(),
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
        let manifest_path = snapshot_dir.join("manifest.json");
        let mut snapshot_dir_created = false;
        let bridge_result = (|| {
            fs::create_dir(&snapshot_dir).map_err(|err| err.to_string())?;
            snapshot_dir_created = true;
            fs::write(&manifest_path, manifest_json).map_err(|err| err.to_string())?;

            let snapshot_asset_root = snapshot_dir.join("assets");
            fs::create_dir_all(&snapshot_asset_root).map_err(|err| err.to_string())?;
            let mut target_paths = BTreeSet::new();
            let mut bridge_samples = Vec::with_capacity(snapshot_sources.len());
            for (image, state, _) in &snapshot_sources {
                let source_path = resolve_snapshot_image_path(
                    &source_asset_root,
                    &paths.raw,
                    &image.file_name,
                    &image.id,
                )?;
                let relative_path = stable_snapshot_asset_path(&image.id, &source_path)?;
                if !target_paths.insert(relative_path.clone()) {
                    return Err(format!(
                        "duplicate snapshot asset target for image '{}': {relative_path}",
                        image.id
                    ));
                }
                let target_path = snapshot_asset_root.join(Path::new(&relative_path));
                copy_file_and_sync(&source_path, &target_path)?;
                verify_snapshot_asset_dimensions(&target_path, image.width, image.height)?;
                bridge_samples.push(BridgeSourceSample {
                    id: image.id.clone(),
                    relative_path,
                    width: image.width,
                    height: image.height,
                    split: bridge_split(&image.split)?,
                    revision: state.revision.clone(),
                    objects: state
                        .objects
                        .iter()
                        .map(bridge_object)
                        .collect::<Result<Vec<_>, _>>()?,
                });
            }

            bridge::write_bridge_manifest(
                &snapshot_dir,
                BridgeBuildInput {
                    project: &project,
                    snapshot_id: &record.id,
                    snapshot_name: &record.name,
                    created_at: &record.created_at,
                    asset_root: &snapshot_asset_root,
                    classes: &classes,
                    samples: &bridge_samples,
                },
            )
        })();
        let _bridge_manifest_path = match bridge_result {
            Ok(path) => path,
            Err(error) => {
                let directory_cleanup = snapshot_dir_created
                    .then(|| fs::remove_dir_all(&snapshot_dir))
                    .transpose();
                let record_cleanup = storage::delete_snapshot_record(&paths.sqlite, &record.id);
                let mut cleanup_errors = Vec::new();
                if let Err(cleanup_error) = directory_cleanup {
                    cleanup_errors.push(format!("remove snapshot directory: {cleanup_error}"));
                }
                if let Err(cleanup_error) = record_cleanup {
                    cleanup_errors.push(format!("delete snapshot record: {cleanup_error}"));
                }
                if cleanup_errors.is_empty() {
                    return Err(error);
                }
                return Err(format!(
                    "{error}; cleanup failed: {}",
                    cleanup_errors.join("; ")
                ));
            }
        };
        let bridge_manifest_api_path = bridge_manifest_relative_path(&record.id);
        Ok(DatasetSnapshot {
            id: record.id,
            name: record.name,
            image_count: record.image_count,
            manifest_path: manifest_path.to_string_lossy().to_string(),
            created_at: record.created_at,
            bridge_manifest_path: Some(bridge_manifest_api_path),
            bridge_status: "ready".to_string(),
        })
    }

    pub fn dataset_snapshots(&self, project_id: &str) -> Result<Vec<DatasetSnapshot>, String> {
        let paths = project_fs::project_paths(project_id);
        Ok(storage::list_snapshot_records(&paths.sqlite)?
            .into_iter()
            .map(|record| {
                let snapshot_dir = paths.snapshots.join(&record.id);
                let bridge_manifest_path = snapshot_dir.join("visualai-bridge.json");
                let bridge_exists = bridge_manifest_path.is_file();
                let bridge_ready = bridge_exists
                    && fs::read_to_string(&bridge_manifest_path)
                        .ok()
                        .and_then(|text| {
                            serde_json::from_str::<crate::bridge::BridgeManifest>(&text).ok()
                        })
                        .is_some_and(|manifest| {
                            manifest.validate().is_ok()
                                && manifest.project_id == project_id
                                && manifest.snapshot_id == record.id
                        });
                let bridge_manifest_api_path = bridge_manifest_relative_path(&record.id);
                DatasetSnapshot {
                    manifest_path: snapshot_dir
                        .join("manifest.json")
                        .to_string_lossy()
                        .to_string(),
                    id: record.id,
                    name: record.name,
                    image_count: record.image_count,
                    created_at: record.created_at,
                    bridge_manifest_path: bridge_ready.then_some(bridge_manifest_api_path),
                    bridge_status: if bridge_ready {
                        "ready".to_string()
                    } else if bridge_exists {
                        "invalid".to_string()
                    } else {
                        legacy_bridge_status()
                    },
                }
            })
            .collect())
    }

    pub fn export_dataset(
        &self,
        project_id: &str,
        snapshot_id: &str,
        format: &str,
    ) -> Result<DatasetExport, String> {
        let paths = project_fs::ensure_project_dirs(project_id)?;
        let output_dir = paths.exports.join(format!("{snapshot_id}-{format}"));
        fs::create_dir_all(&output_dir).map_err(|err| err.to_string())?;
        let manifest_path = paths.snapshots.join(snapshot_id).join("manifest.json");
        let manifest_json = fs::read_to_string(&manifest_path).map_err(|err| err.to_string())?;
        fs::write(output_dir.join("manifest.json"), &manifest_json)
            .map_err(|err| err.to_string())?;
        if format == "coco" {
            self.write_coco_export(project_id, &paths, &manifest_json, &output_dir)?;
        } else {
            fs::write(
                output_dir.join("dataset.yaml"),
                format!(
                    "path: {}\ntrain: images\nnames: []\n",
                    paths.raw.to_string_lossy()
                ),
            )
            .map_err(|err| err.to_string())?;
        }
        let record = storage::create_export_record(
            &paths.sqlite,
            snapshot_id,
            format,
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

    fn write_coco_export(
        &self,
        project_id: &str,
        paths: &project_fs::ProjectPaths,
        manifest_json: &str,
        output_dir: &Path,
    ) -> Result<(), String> {
        let manifest: Value = serde_json::from_str(manifest_json)
            .map_err(|err| format!("invalid snapshot: {err}"))?;
        let snapshot_images = manifest
            .get("annotations")
            .and_then(Value::as_array)
            .ok_or_else(|| "snapshot does not contain an annotations array".to_string())?;
        let indexed_images = storage::read_images(&paths.sqlite, None)?
            .into_iter()
            .map(|image| (image.id.clone(), image))
            .collect::<BTreeMap<_, _>>();
        let mut category_labels = storage::read_classes(&paths.sqlite)?
            .into_iter()
            .map(|class| (class.id, class.label))
            .collect::<BTreeMap<_, _>>();
        let project_manifest = storage::read_project_manifest(&paths.sqlite)?;
        let asset_root = project_manifest
            .as_ref()
            .map(|project| PathBuf::from(&project.root_path))
            .filter(|path| path.exists())
            .unwrap_or_else(|| paths.raw.clone());
        let images_dir = output_dir.join("images");
        fs::create_dir_all(&images_dir).map_err(|err| err.to_string())?;

        let mut coco_images = Vec::new();
        let mut coco_annotations = Vec::new();
        let mut image_categories = Vec::new();
        let mut annotation_id = 1u64;

        for (image_index, snapshot_image) in snapshot_images.iter().enumerate() {
            let image_id = snapshot_image
                .get("imageId")
                .and_then(Value::as_str)
                .ok_or_else(|| "snapshot image is missing imageId".to_string())?;
            let file_name = snapshot_image
                .get("fileName")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("snapshot image '{image_id}' is missing fileName"))?;
            let indexed = indexed_images.get(image_id);
            let width = snapshot_image
                .get("width")
                .and_then(Value::as_u64)
                .or_else(|| indexed.map(|image| image.width as u64))
                .ok_or_else(|| format!("snapshot image '{image_id}' is missing width"))?;
            let height = snapshot_image
                .get("height")
                .and_then(Value::as_u64)
                .or_else(|| indexed.map(|image| image.height as u64))
                .ok_or_else(|| format!("snapshot image '{image_id}' is missing height"))?;
            if width == 0 || height == 0 {
                return Err(format!(
                    "snapshot image '{image_id}' has invalid dimensions"
                ));
            }
            let coco_image_id = (image_index + 1) as u64;
            let split = snapshot_image
                .get("split")
                .and_then(Value::as_str)
                .or_else(|| indexed.map(|image| image.split.as_str()))
                .unwrap_or("train");
            let relative_path = safe_export_relative_path(file_name)?;
            let exported_file_name = relative_path.to_string_lossy().replace('\\', "/");
            coco_images.push(json!({
                "id": coco_image_id,
                "file_name": exported_file_name,
                "width": width,
                "height": height,
                "split": split,
                "source_id": image_id,
            }));

            let source_path = resolve_export_image_path(&asset_root, &paths.raw, file_name)
                .ok_or_else(|| format!("image asset not found for '{file_name}'"))?;
            let target_path = images_dir.join(relative_path);
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent).map_err(|err| err.to_string())?;
            }
            fs::copy(&source_path, &target_path).map_err(|err| {
                format!(
                    "copy image {} to {}: {err}",
                    source_path.display(),
                    target_path.display()
                )
            })?;

            let objects: Vec<AnnotationObject> = serde_json::from_value(
                snapshot_image
                    .get("objects")
                    .cloned()
                    .unwrap_or_else(|| json!([])),
            )
            .map_err(|err| format!("invalid objects for image '{image_id}': {err}"))?;

            for object in objects {
                category_labels
                    .entry(object.class_id)
                    .or_insert_with(|| object.label.clone());
                let category_id = u64::from(object.class_id) + 1;
                if object.object_type == "classification" {
                    image_categories.push(json!({
                        "image_id": coco_image_id,
                        "category_id": category_id,
                    }));
                    continue;
                }

                let Some((bbox, segmentation, area)) = coco_geometry(&object)? else {
                    continue;
                };
                let is_crowd = object
                    .attributes
                    .get("iscrowd")
                    .and_then(|value| {
                        value
                            .as_u64()
                            .or_else(|| value.as_bool().map(|enabled| u64::from(enabled)))
                    })
                    .unwrap_or(0);
                coco_annotations.push(json!({
                    "id": annotation_id,
                    "image_id": coco_image_id,
                    "category_id": category_id,
                    "bbox": bbox,
                    "segmentation": segmentation,
                    "area": area,
                    "iscrowd": is_crowd,
                    "attributes": object.attributes,
                    "source_id": object.id,
                }));
                annotation_id += 1;
            }
        }

        let categories = category_labels
            .into_iter()
            .map(|(class_id, name)| {
                json!({
                    "id": u64::from(class_id) + 1,
                    "name": name,
                    "supercategory": "",
                })
            })
            .collect::<Vec<_>>();
        let coco = json!({
            "info": {
                "description": manifest.get("name").and_then(Value::as_str).unwrap_or(project_id),
                "version": "1.0",
                "contributor": "Image Annotation",
            },
            "licenses": [],
            "images": coco_images,
            "categories": categories,
            "annotations": coco_annotations,
            "image_categories": image_categories,
        });
        let coco_json = serde_json::to_string_pretty(&coco).map_err(|err| err.to_string())?;
        fs::write(output_dir.join("annotations.json"), coco_json).map_err(|err| err.to_string())
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

fn bridge_manifest_relative_path(snapshot_id: &str) -> String {
    format!("snapshots/{snapshot_id}/visualai-bridge.json")
}

fn safe_export_relative_path(file_name: &str) -> Result<PathBuf, String> {
    let relative = Path::new(file_name)
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value),
            _ => None,
        })
        .collect::<PathBuf>();
    if relative.as_os_str().is_empty() {
        Err(format!("invalid image file name '{file_name}'"))
    } else {
        Ok(relative)
    }
}

fn stable_snapshot_asset_path(sample_id: &str, source_path: &Path) -> Result<String, String> {
    if sample_id.is_empty()
        || sample_id
            .chars()
            .any(|character| character.is_control() || character == '/' || character == '\\')
    {
        return Err(format!("invalid snapshot sample id: {sample_id:?}"));
    }
    let extension = source_path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .filter(|value| matches!(value.as_str(), "jpg" | "jpeg" | "png" | "bmp" | "webp"))
        .ok_or_else(|| {
            format!(
                "snapshot asset has an unsupported extension: {}",
                source_path.display()
            )
        })?;
    let stable_name = format!("{:x}", Sha256::digest(sample_id.as_bytes()));
    Ok(format!("images/{stable_name}.{extension}"))
}

fn resolve_snapshot_image_path(
    asset_root: &Path,
    fallback_root: &Path,
    file_name: &str,
    sample_id: &str,
) -> Result<PathBuf, String> {
    let relative = Path::new(file_name);
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "snapshot image path must be normalized and relative: {file_name:?}"
        ));
    }

    let roots = [asset_root, fallback_root];
    let mut matches = BTreeSet::new();
    for (root, candidate) in [
        (asset_root, asset_root.join(relative)),
        (asset_root, asset_root.join("images").join(relative)),
        (fallback_root, fallback_root.join(relative)),
        (fallback_root, fallback_root.join("images").join(relative)),
    ] {
        if !candidate.is_file() {
            continue;
        }
        let canonical_root = fs::canonicalize(root)
            .map_err(|error| format!("resolve snapshot asset root {}: {error}", root.display()))?;
        let canonical_candidate = fs::canonicalize(&candidate).map_err(|error| {
            format!(
                "resolve snapshot image asset {}: {error}",
                candidate.display()
            )
        })?;
        if canonical_candidate.starts_with(&canonical_root) {
            matches.insert(canonical_candidate);
        }
    }
    if matches.len() == 1 {
        return Ok(matches.into_iter().next().unwrap());
    }
    if matches.len() > 1 {
        return Err(format!(
            "multiple image assets matched '{}' for sample '{}'",
            file_name, sample_id
        ));
    }

    if relative.components().count() == 1 {
        let expected_name = relative.file_name();
        for root in roots {
            let Ok(canonical_root) = fs::canonicalize(root) else {
                continue;
            };
            for entry in WalkDir::new(root)
                .follow_links(false)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_file())
            {
                if entry.path().file_name() != expected_name
                    || !image_id_matches(root, entry.path(), sample_id)
                {
                    continue;
                }
                let canonical_candidate = fs::canonicalize(entry.path()).map_err(|error| {
                    format!(
                        "resolve snapshot image asset {}: {error}",
                        entry.path().display()
                    )
                })?;
                if canonical_candidate.starts_with(&canonical_root) {
                    matches.insert(canonical_candidate);
                }
            }
        }
    }

    match matches.len() {
        0 => Err(format!("image asset not found for '{file_name}'")),
        1 => Ok(matches.into_iter().next().unwrap()),
        _ => Err(format!(
            "multiple image assets matched '{}' for sample '{}'",
            file_name, sample_id
        )),
    }
}

fn copy_file_and_sync(source: &Path, target: &Path) -> Result<(), String> {
    let parent = target
        .parent()
        .ok_or_else(|| format!("snapshot asset has no parent: {}", target.display()))?;
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "create snapshot asset directory {}: {error}",
            parent.display()
        )
    })?;
    if target.exists() {
        return Err(format!(
            "snapshot asset target already exists: {}",
            target.display()
        ));
    }
    let file_name = target
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            format!(
                "snapshot asset target is not valid UTF-8: {}",
                target.display()
            )
        })?;
    let temporary_path = parent.join(format!(".{file_name}.tmp"));
    match fs::remove_file(&temporary_path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "remove stale snapshot asset {}: {error}",
                temporary_path.display()
            ))
        }
    }

    let copy_result = (|| {
        let mut input = File::open(source)
            .map_err(|error| format!("open snapshot source {}: {error}", source.display()))?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
            .map_err(|error| {
                format!(
                    "create snapshot asset temporary file {}: {error}",
                    temporary_path.display()
                )
            })?;
        io::copy(&mut input, &mut output).map_err(|error| {
            format!(
                "copy snapshot asset {} to {}: {error}",
                source.display(),
                target.display()
            )
        })?;
        output
            .flush()
            .map_err(|error| format!("flush snapshot asset {}: {error}", target.display()))?;
        output
            .sync_all()
            .map_err(|error| format!("sync snapshot asset {}: {error}", target.display()))?;
        drop(output);
        fs::rename(&temporary_path, target)
            .map_err(|error| format!("publish snapshot asset {}: {error}", target.display()))
    })();
    if let Err(error) = copy_result {
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    Ok(())
}

fn verify_snapshot_asset_dimensions(
    path: &Path,
    expected_width: u32,
    expected_height: u32,
) -> Result<(), String> {
    let actual = crate::datasets::oriented_image_dimensions(path)?;
    if actual != (expected_width, expected_height) {
        return Err(format!(
            "copied snapshot asset {} dimensions changed: expected {}x{}, got {}x{}",
            path.display(),
            expected_width,
            expected_height,
            actual.0,
            actual.1
        ));
    }
    Ok(())
}

fn bridge_split(split: &str) -> Result<Option<BridgeSplit>, String> {
    match split {
        "train" => Ok(Some(BridgeSplit::Train)),
        "val" => Ok(Some(BridgeSplit::Val)),
        "test" => Ok(Some(BridgeSplit::Test)),
        "" | "local" | "unassigned" => Ok(None),
        value => Err(format!("unsupported bridge split: {value}")),
    }
}

fn bridge_object(object: &AnnotationObject) -> Result<BridgeObject, String> {
    match object.object_type.as_str() {
        "bbox" => {
            let bbox = object
                .bbox
                .as_ref()
                .ok_or_else(|| format!("bbox annotation '{}' has no bbox", object.id))?;
            Ok(BridgeObject::Bbox {
                id: object.id.clone(),
                class_id: object.class_id.to_string(),
                x: bbox.x,
                y: bbox.y,
                width: bbox.width,
                height: bbox.height,
            })
        }
        "classification" => Ok(BridgeObject::Classification {
            id: object.id.clone(),
            class_id: object.class_id.to_string(),
        }),
        "polygon" => {
            let points = object
                .polygon
                .as_ref()
                .ok_or_else(|| format!("polygon annotation '{}' has no points", object.id))?
                .iter()
                .map(|point| BridgePoint {
                    x: point.x,
                    y: point.y,
                })
                .collect();
            Ok(BridgeObject::Polygon {
                id: object.id.clone(),
                class_id: object.class_id.to_string(),
                points,
            })
        }
        object_type => Err(format!(
            "unsupported annotation type '{}' for object '{}'",
            object_type, object.id
        )),
    }
}

fn resolve_export_image_path(
    asset_root: &Path,
    fallback_root: &Path,
    file_name: &str,
) -> Option<PathBuf> {
    [
        asset_root.join(file_name),
        asset_root.join("images").join(file_name),
        fallback_root.join(file_name),
        fallback_root.join("images").join(file_name),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

fn coco_geometry(object: &AnnotationObject) -> Result<Option<(Vec<f64>, Value, f64)>, String> {
    if let Some(polygon) = object.polygon.as_ref() {
        if polygon.len() < 3 {
            return Err(format!(
                "polygon annotation '{}' must contain at least 3 points",
                object.id
            ));
        }
        if polygon
            .iter()
            .any(|point| !point.x.is_finite() || !point.y.is_finite())
        {
            return Err(format!(
                "polygon annotation '{}' contains invalid coordinates",
                object.id
            ));
        }
        let min_x = polygon
            .iter()
            .map(|point| point.x)
            .fold(f64::INFINITY, f64::min);
        let max_x = polygon
            .iter()
            .map(|point| point.x)
            .fold(f64::NEG_INFINITY, f64::max);
        let min_y = polygon
            .iter()
            .map(|point| point.y)
            .fold(f64::INFINITY, f64::min);
        let max_y = polygon
            .iter()
            .map(|point| point.y)
            .fold(f64::NEG_INFINITY, f64::max);
        let flat = polygon
            .iter()
            .flat_map(|point| [point.x, point.y])
            .collect::<Vec<_>>();
        return Ok(Some((
            vec![min_x, min_y, max_x - min_x, max_y - min_y],
            json!([flat]),
            polygon_area(polygon),
        )));
    }

    if let Some(bbox) = object.bbox.as_ref() {
        if !bbox.x.is_finite()
            || !bbox.y.is_finite()
            || !bbox.width.is_finite()
            || !bbox.height.is_finite()
        {
            return Err(format!(
                "bbox annotation '{}' contains invalid coordinates",
                object.id
            ));
        }
        let width = bbox.width.max(0.0);
        let height = bbox.height.max(0.0);
        return Ok(Some((
            vec![bbox.x, bbox.y, width, height],
            json!([]),
            width * height,
        )));
    }

    Ok(None)
}

fn polygon_area(points: &[Point]) -> f64 {
    points
        .iter()
        .zip(points.iter().cycle().skip(1))
        .take(points.len())
        .map(|(left, right)| left.x * right.y - right.x * left.y)
        .sum::<f64>()
        .abs()
        / 2.0
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

fn label_path_for_image(
    raw_root: &std::path::Path,
    image_path: &std::path::Path,
) -> Option<PathBuf> {
    let relative = image_path.strip_prefix(raw_root).ok()?;
    let mut parts: Vec<_> = relative.components().collect();
    let image_index = parts
        .iter()
        .position(|component| component.as_os_str().to_string_lossy() == "images")?;
    parts[image_index] = std::path::Component::Normal(std::ffi::OsStr::new("labels"));
    let mut label = raw_root.to_path_buf();
    for component in parts {
        label.push(component.as_os_str());
    }
    label.set_extension("txt");
    label.exists().then_some(label)
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

fn is_voc_project(project_id: &str) -> bool {
    project_manifest(project_id)
        .map(|manifest| manifest.format == "voc-detect")
        .unwrap_or(false)
}

fn is_classification_project(project_id: &str) -> bool {
    project_manifest(project_id)
        .map(|manifest| manifest.format == "image-classification")
        .unwrap_or(false)
}

fn classification_for_image(
    image_path: &Path,
    classes: &[storage::StoredClass],
) -> Option<(u32, String)> {
    let label = image_path.parent()?.file_name()?.to_string_lossy();
    classes
        .iter()
        .find(|class| class.label == label)
        .map(|class| (class.id, class.label.clone()))
}

fn yolo_project_format(project_id: &str) -> Option<String> {
    project_manifest(project_id).and_then(|manifest| {
        matches!(manifest.format.as_str(), "yolo-detect" | "yolo-seg").then_some(manifest.format)
    })
}

fn yolo_label_path_for_image(project_id: &str, image_path: &Path) -> Option<PathBuf> {
    let manifest_root = project_manifest(project_id)
        .map(|manifest| PathBuf::from(manifest.root_path))
        .unwrap_or_else(|| project_fs::project_paths(project_id).raw);

    label_path_for_image(&manifest_root, image_path).or_else(|| {
        image_path
            .with_extension("txt")
            .exists()
            .then(|| image_path.with_extension("txt"))
    })
}

fn yolo_label_write_path_for_image(project_id: &str, image_path: &Path) -> PathBuf {
    let manifest_root = project_manifest(project_id)
        .map(|manifest| PathBuf::from(manifest.root_path))
        .unwrap_or_else(|| project_fs::project_paths(project_id).raw);

    yolo_label_path_candidate(&manifest_root, image_path)
        .unwrap_or_else(|| image_path.with_extension("txt"))
}

fn yolo_label_path_candidate(root: &Path, image_path: &Path) -> Option<PathBuf> {
    let relative = image_path.strip_prefix(root).ok()?;
    let mut parts: Vec<_> = relative.components().collect();
    let image_index = parts
        .iter()
        .position(|component| component.as_os_str().to_string_lossy() == "images")?;
    parts[image_index] = std::path::Component::Normal(std::ffi::OsStr::new("labels"));
    let mut label = root.to_path_buf();
    for component in parts {
        label.push(component.as_os_str());
    }
    label.set_extension("txt");
    Some(label)
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
    fn demo_detection_project_publishes_ready_snapshot_bridge() {
        let name = format!(
            "Bridge Detection Demo {}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let project =
            crate::datasets::create_dataset_project(&name, "yolo-detect", "demo-bbox").unwrap();
        let paths = project_fs::project_paths(&project.id);
        let snapshot = SampleRepository::new()
            .create_dataset_snapshot(&project.id, "training")
            .unwrap();
        let bridge_path = paths
            .root
            .join(snapshot.bridge_manifest_path.as_ref().unwrap());
        let bridge: crate::bridge::BridgeManifest =
            serde_json::from_str(&std::fs::read_to_string(bridge_path).unwrap()).unwrap();

        assert_eq!(snapshot.bridge_status, "ready");
        assert_eq!(bridge.task_type, crate::bridge::BridgeTaskType::Detection);
        assert_eq!(bridge.samples.len(), 3);
        assert!(bridge.samples.iter().all(|sample| paths
            .snapshots
            .join(&snapshot.id)
            .join(&bridge.asset_root)
            .join(&sample.relative_path)
            .is_file()));
        let _ = std::fs::remove_dir_all(paths.root);
    }

    #[test]
    fn demo_classification_project_publishes_ready_snapshot_bridge() {
        let name = format!(
            "Bridge Classification Demo {}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let project = crate::datasets::create_dataset_project(
            &name,
            "image-classification",
            "demo-classification",
        )
        .unwrap();
        let paths = project_fs::project_paths(&project.id);
        let snapshot = SampleRepository::new()
            .create_dataset_snapshot(&project.id, "training")
            .unwrap();
        let bridge_path = paths
            .root
            .join(snapshot.bridge_manifest_path.as_ref().unwrap());
        let bridge: crate::bridge::BridgeManifest =
            serde_json::from_str(&std::fs::read_to_string(bridge_path).unwrap()).unwrap();

        assert_eq!(snapshot.bridge_status, "ready");
        assert_eq!(
            bridge.task_type,
            crate::bridge::BridgeTaskType::Classification
        );
        assert_eq!(bridge.samples.len(), 3);
        assert!(!bridge.samples.is_empty());
        assert!(bridge.samples.iter().all(|sample| {
            sample.objects.len() == 1
                && matches!(sample.objects[0], BridgeObject::Classification { .. })
        }));
        let _ = std::fs::remove_dir_all(paths.root);
    }

    #[test]
    fn legacy_basename_index_resolves_one_nested_asset_but_rejects_ambiguity() {
        let repository = SampleRepository::new();
        let project_id = "bridge-legacy-nested-unit";
        let paths = project_fs::project_paths(project_id);
        let _ = std::fs::remove_dir_all(&paths.root);
        project_fs::ensure_workspace_project_dirs(project_id).unwrap();
        storage::initialize_project_database(&paths.sqlite).unwrap();
        std::fs::create_dir_all(paths.raw.join("images/train")).unwrap();
        image::RgbImage::from_pixel(10, 10, image::Rgb([1, 2, 3]))
            .save(paths.raw.join("images/train/nested.png"))
            .unwrap();
        let manifest = project_fs::ProjectManifest {
            id: project_id.to_string(),
            name: "Legacy Nested".to_string(),
            source_dataset_key: "downloaded".to_string(),
            format: "yolo-detect".to_string(),
            root_path: paths.root.to_string_lossy().to_string(),
            created_at: now_unix_string(),
            class_count: 0,
            image_count: 1,
        };
        let images = vec![storage::StoredImage {
            id: "nested".to_string(),
            file_name: "nested.png".to_string(),
            width: 10,
            height: 10,
            split: "train".to_string(),
            status: "已标注".to_string(),
            qa_status: String::new(),
            review_note: None,
        }];
        storage::upsert_project_index(&paths.sqlite, &manifest, &images, &[]).unwrap();

        let snapshot = repository
            .create_dataset_snapshot(project_id, "unique")
            .unwrap();
        assert_eq!(snapshot.bridge_status, "ready");

        std::fs::create_dir_all(paths.raw.join("images/val")).unwrap();
        image::RgbImage::from_pixel(10, 10, image::Rgb([4, 5, 6]))
            .save(paths.raw.join("images/val/nested.png"))
            .unwrap();
        let ambiguous = repository.create_dataset_snapshot(project_id, "ambiguous");
        assert!(ambiguous.is_err());
        assert_eq!(repository.dataset_snapshots(project_id).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(paths.root);
    }

    #[test]
    fn legacy_dataset_snapshot_json_defaults_bridge_fields() {
        let snapshot: DatasetSnapshot = serde_json::from_value(json!({
            "id": "snapshot-legacy",
            "name": "Legacy",
            "imageCount": 1,
            "manifestPath": "snapshots/snapshot-legacy/manifest.json",
            "createdAt": "1785312000"
        }))
        .unwrap();

        assert_eq!(snapshot.bridge_manifest_path, None);
        assert_eq!(snapshot.bridge_status, "legacy");
    }

    #[test]
    fn malformed_bridge_file_is_not_listed_as_ready() {
        let repository = SampleRepository::new();
        let project_id = "bridge-malformed-list-unit";
        let paths = project_fs::project_paths(project_id);
        let _ = std::fs::remove_dir_all(&paths.root);
        project_fs::ensure_workspace_project_dirs(project_id).unwrap();
        storage::initialize_project_database(&paths.sqlite).unwrap();
        let record = storage::create_snapshot_record(&paths.sqlite, "Malformed", "{}", 0).unwrap();
        let snapshot_dir = paths.snapshots.join(&record.id);
        std::fs::create_dir_all(&snapshot_dir).unwrap();
        std::fs::write(
            snapshot_dir.join("visualai-bridge.json"),
            b"{\"schema_version\":\"not-supported\"}",
        )
        .unwrap();

        let snapshots = repository.dataset_snapshots(project_id).unwrap();

        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].bridge_status, "invalid");
        assert_eq!(snapshots[0].bridge_manifest_path, None);
        let _ = std::fs::remove_dir_all(paths.root);
    }

    #[test]
    fn logically_invalid_bridge_manifests_are_not_listed_as_ready() {
        let repository = SampleRepository::new();
        let project_id = "bridge-logically-invalid-list-unit";
        let paths = project_fs::project_paths(project_id);
        let _ = std::fs::remove_dir_all(&paths.root);
        project_fs::ensure_workspace_project_dirs(project_id).unwrap();
        storage::initialize_project_database(&paths.sqlite).unwrap();
        let base: serde_json::Value =
            serde_json::from_str(include_str!("../../docs/protocol/fixtures/detection.json"))
                .unwrap();
        let mut snapshot_ids = Vec::new();

        for (name, mutate) in [
            ("duplicate-class", "duplicate_class"),
            ("duplicate-sample-path", "duplicate_sample"),
            ("duplicate-object", "duplicate_object"),
            ("unknown-class", "unknown_class"),
            ("task-object-mismatch", "task_mismatch"),
            ("wrong-annotation-format", "wrong_format"),
        ] {
            let record = storage::create_snapshot_record(&paths.sqlite, name, "{}", 1).unwrap();
            let mut value = base.clone();
            value["project_id"] = json!(project_id);
            value["snapshot_id"] = json!(record.id);
            match mutate {
                "duplicate_class" => {
                    let duplicate = value["classes"][0].clone();
                    value["classes"].as_array_mut().unwrap().push(duplicate);
                }
                "duplicate_sample" => {
                    let duplicate = value["samples"][0].clone();
                    value["samples"].as_array_mut().unwrap().push(duplicate);
                }
                "duplicate_object" => {
                    let duplicate = value["samples"][0]["objects"][0].clone();
                    value["samples"][0]["objects"]
                        .as_array_mut()
                        .unwrap()
                        .push(duplicate);
                }
                "unknown_class" => {
                    value["samples"][0]["objects"][0]["class_id"] = json!("missing-class");
                }
                "task_mismatch" => {
                    value["task_type"] = json!("classification");
                }
                "wrong_format" => {
                    value["annotation_format"] = json!("other.normalized/v1");
                }
                _ => unreachable!(),
            }
            let snapshot_dir = paths.snapshots.join(&record.id);
            std::fs::create_dir_all(&snapshot_dir).unwrap();
            std::fs::write(
                snapshot_dir.join("visualai-bridge.json"),
                serde_json::to_vec_pretty(&value).unwrap(),
            )
            .unwrap();
            snapshot_ids.push(record.id);
        }

        let snapshots = repository.dataset_snapshots(project_id).unwrap();
        for snapshot_id in snapshot_ids {
            let snapshot = snapshots
                .iter()
                .find(|snapshot| snapshot.id == snapshot_id)
                .unwrap();
            assert_eq!(
                snapshot.bridge_status, "invalid",
                "{} unexpectedly ready",
                snapshot.name
            );
            assert_eq!(snapshot.bridge_manifest_path, None);
        }
        let _ = std::fs::remove_dir_all(paths.root);
    }

    #[test]
    fn snapshot_rejects_malformed_yolo_line_and_compensates() {
        let name = format!(
            "Bridge Malformed YOLO {}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let project =
            crate::datasets::create_dataset_project(&name, "yolo-detect", "demo-bbox").unwrap();
        let paths = project_fs::project_paths(&project.id);
        std::fs::write(
            paths.raw.join("labels/train/demo_001.txt"),
            "0 0.5 0.5 0.2 0.2\nthis line is malformed\n",
        )
        .unwrap();
        let manifest = storage::read_project_manifest(&paths.sqlite)
            .unwrap()
            .unwrap();
        let classes = storage::read_classes(&paths.sqlite).unwrap();
        let image = SampleRepository::new()
            .project_images(&project.id, None)
            .into_iter()
            .next()
            .unwrap();
        let strict = SampleRepository::new()
            .snapshot_annotation_state_strict(&paths, &manifest, &classes, &paths.raw, &image);
        assert!(
            strict.is_err(),
            "strict loader unexpectedly returned {strict:?}"
        );

        let result =
            SampleRepository::new().create_dataset_snapshot(&project.id, "must-fail-strict");

        assert!(result.is_err());
        assert!(storage::list_snapshot_records(&paths.sqlite)
            .unwrap()
            .is_empty());
        assert_eq!(std::fs::read_dir(&paths.snapshots).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(paths.root);
    }

    #[test]
    fn snapshot_rejects_malformed_voc_xml_and_compensates() {
        let source_root = std::env::temp_dir().join(format!(
            "bridge-malformed-voc-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&source_root).unwrap();
        image::RgbImage::from_pixel(10, 10, image::Rgb([1, 2, 3]))
            .save(source_root.join("sample.png"))
            .unwrap();
        std::fs::write(source_root.join("sample.xml"), "<annotation><broken>").unwrap();
        let project =
            crate::datasets::open_local_dataset(&source_root.to_string_lossy(), "voc-detect")
                .unwrap();
        let paths = project_fs::project_paths(&project.id);

        let result =
            SampleRepository::new().create_dataset_snapshot(&project.id, "must-fail-strict");

        assert!(result.is_err());
        assert!(storage::list_snapshot_records(&paths.sqlite)
            .unwrap()
            .is_empty());
        assert_eq!(std::fs::read_dir(&paths.snapshots).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(paths.root);
        let _ = std::fs::remove_dir_all(source_root);
    }

    #[test]
    fn snapshot_rejects_malformed_sqlite_annotation_payload_and_compensates() {
        let name = format!(
            "Bridge Malformed SQLite {}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let project =
            crate::datasets::create_dataset_project(&name, "yolo-detect", "demo-bbox").unwrap();
        let paths = project_fs::project_paths(&project.id);
        let image = storage::read_images(&paths.sqlite, None)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        storage::save_annotation_payload(&paths.sqlite, &image.id, None, "{malformed-json")
            .unwrap();

        let result =
            SampleRepository::new().create_dataset_snapshot(&project.id, "must-fail-strict");

        assert!(result.is_err());
        assert!(storage::list_snapshot_records(&paths.sqlite)
            .unwrap()
            .is_empty());
        assert_eq!(std::fs::read_dir(&paths.snapshots).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(paths.root);
    }

    #[test]
    fn snapshot_rejects_unreadable_image_content() {
        let repository = SampleRepository::new();
        let project_id = "bridge-unreadable-image-unit";
        let paths = project_fs::project_paths(project_id);
        let _ = std::fs::remove_dir_all(&paths.root);
        project_fs::ensure_workspace_project_dirs(project_id).unwrap();
        storage::initialize_project_database(&paths.sqlite).unwrap();
        std::fs::create_dir_all(paths.raw.join("images/train")).unwrap();
        std::fs::write(
            paths.raw.join("images/train/broken.png"),
            b"not a real image",
        )
        .unwrap();
        let manifest = project_fs::ProjectManifest {
            id: project_id.to_string(),
            name: "Unreadable Image".to_string(),
            source_dataset_key: "downloaded".to_string(),
            format: "yolo-detect".to_string(),
            root_path: paths.root.to_string_lossy().to_string(),
            created_at: now_unix_string(),
            class_count: 0,
            image_count: 1,
        };
        let images = vec![storage::StoredImage {
            id: "images_train_broken".to_string(),
            file_name: "images/train/broken.png".to_string(),
            width: 10,
            height: 10,
            split: "train".to_string(),
            status: "未标注".to_string(),
            qa_status: String::new(),
            review_note: None,
        }];
        storage::upsert_project_index(&paths.sqlite, &manifest, &images, &[]).unwrap();

        let result = repository.create_dataset_snapshot(project_id, "must-fail-image");

        assert!(result.is_err());
        assert!(storage::list_snapshot_records(&paths.sqlite)
            .unwrap()
            .is_empty());
        let _ = std::fs::remove_dir_all(paths.root);
    }

    #[test]
    fn snapshot_rejects_changed_image_dimensions_before_creating_record() {
        let name = format!(
            "Bridge Changed Dimensions {}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let project =
            crate::datasets::create_dataset_project(&name, "yolo-detect", "demo-bbox").unwrap();
        let paths = project_fs::project_paths(&project.id);
        let image = storage::read_images(&paths.sqlite, None)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        image::RgbImage::from_pixel(13, 7, image::Rgb([1, 2, 3]))
            .save(paths.raw.join(&image.file_name))
            .unwrap();

        let result =
            SampleRepository::new().create_dataset_snapshot(&project.id, "must-rescan-first");

        assert!(result.unwrap_err().to_lowercase().contains("rescan"));
        assert!(storage::list_snapshot_records(&paths.sqlite)
            .unwrap()
            .is_empty());
        assert_eq!(std::fs::read_dir(&paths.snapshots).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(paths.root);
    }

    #[test]
    fn copied_snapshot_asset_dimensions_are_verified() {
        let root = std::env::temp_dir().join(format!(
            "bridge-copy-dimensions-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("asset.png");
        image::RgbImage::from_pixel(9, 5, image::Rgb([1, 2, 3]))
            .save(&target)
            .unwrap();

        let result = verify_snapshot_asset_dimensions(&target, 10, 5);

        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn maps_annotation_domain_objects_to_strict_bridge_variants() {
        let bbox = bridge_object(&AnnotationObject::bbox(
            "bbox-1".to_string(),
            1,
            "box".to_string(),
            BBox {
                x: 1.0,
                y: 2.0,
                width: 3.0,
                height: 4.0,
            },
        ))
        .unwrap();
        let classification = bridge_object(&AnnotationObject::classification(
            "classification-1".to_string(),
            2,
            "class".to_string(),
        ))
        .unwrap();
        let polygon = bridge_object(&AnnotationObject::polygon(
            "polygon-1".to_string(),
            3,
            "region".to_string(),
            vec![
                Point { x: 0.0, y: 0.0 },
                Point { x: 2.0, y: 0.0 },
                Point { x: 1.0, y: 2.0 },
            ],
        ))
        .unwrap();

        assert!(matches!(
            bbox,
            BridgeObject::Bbox {
                id,
                class_id,
                x: 1.0,
                y: 2.0,
                width: 3.0,
                height: 4.0,
            } if id == "bbox-1" && class_id == "1"
        ));
        assert!(matches!(
            classification,
            BridgeObject::Classification { id, class_id }
                if id == "classification-1" && class_id == "2"
        ));
        assert!(matches!(
            polygon,
            BridgeObject::Polygon { id, class_id, points }
                if id == "polygon-1" && class_id == "3" && points.len() == 3
        ));
    }

    #[test]
    fn maps_local_linked_split_to_unassigned_bridge_split() {
        assert_eq!(bridge_split("local").unwrap(), None);
    }

    #[test]
    fn creates_snapshot_with_ready_bridge_and_collision_safe_assets() {
        let repository = SampleRepository::new();
        let project_id = "bridge-snapshot-unit";
        let paths = project_fs::project_paths(project_id);
        let _ = std::fs::remove_dir_all(&paths.root);
        project_fs::ensure_workspace_project_dirs(project_id).unwrap();
        storage::initialize_project_database(&paths.sqlite).unwrap();
        std::fs::create_dir_all(paths.raw.join("train")).unwrap();
        std::fs::create_dir_all(paths.raw.join("val")).unwrap();
        image::RgbImage::from_pixel(32, 16, image::Rgb([1, 2, 3]))
            .save(paths.raw.join("train/shared.png"))
            .unwrap();
        image::RgbImage::from_pixel(24, 12, image::Rgb([4, 5, 6]))
            .save(paths.raw.join("val/shared.png"))
            .unwrap();

        let manifest = project_fs::ProjectManifest {
            id: project_id.to_string(),
            name: "Bridge Snapshot Unit".to_string(),
            source_dataset_key: "local".to_string(),
            format: "yolo-detect".to_string(),
            root_path: paths.raw.to_string_lossy().to_string(),
            created_at: now_unix_string(),
            class_count: 2,
            image_count: 2,
        };
        let images = vec![
            storage::StoredImage {
                id: "image-z".to_string(),
                file_name: "train/shared.png".to_string(),
                width: 32,
                height: 16,
                split: "train".to_string(),
                status: "已标注".to_string(),
                qa_status: String::new(),
                review_note: None,
            },
            storage::StoredImage {
                id: "image-a".to_string(),
                file_name: "val/shared.png".to_string(),
                width: 24,
                height: 12,
                split: "val".to_string(),
                status: "已标注".to_string(),
                qa_status: String::new(),
                review_note: None,
            },
        ];
        let classes = vec![
            storage::StoredClass {
                id: 2,
                label: "zebra".to_string(),
                color: "#ffffff".to_string(),
            },
            storage::StoredClass {
                id: 1,
                label: "antelope".to_string(),
                color: "#000000".to_string(),
            },
        ];
        storage::upsert_project_index(&paths.sqlite, &manifest, &images, &classes).unwrap();
        repository
            .save_image_annotations_with_revision(
                project_id,
                "image-a",
                None,
                vec![AnnotationObject::bbox(
                    "object-a".to_string(),
                    1,
                    "antelope".to_string(),
                    BBox {
                        x: 1.0,
                        y: 2.0,
                        width: 3.0,
                        height: 4.0,
                    },
                )],
            )
            .unwrap();

        let snapshot = repository
            .create_dataset_snapshot(project_id, "bridge-ready")
            .unwrap();

        assert_eq!(snapshot.bridge_status, "ready");
        let bridge_manifest_path = snapshot.bridge_manifest_path.as_ref().unwrap();
        assert_eq!(
            bridge_manifest_path,
            &format!("snapshots/{}/visualai-bridge.json", snapshot.id)
        );
        assert!(!Path::new(bridge_manifest_path).is_absolute());
        assert!(!bridge_manifest_path.contains(':'));
        assert!(!bridge_manifest_path.contains('\\'));
        let bridge_path = paths.root.join(bridge_manifest_path);
        assert!(bridge_path.is_file());
        let bridge: crate::bridge::BridgeManifest =
            serde_json::from_str(&std::fs::read_to_string(&bridge_path).unwrap()).unwrap();
        assert_eq!(bridge.project_id, project_id);
        assert_eq!(bridge.snapshot_id, snapshot.id);
        assert_eq!(bridge.asset_root, "assets");
        assert_eq!(
            bridge
                .samples
                .iter()
                .map(|sample| sample.id.as_str())
                .collect::<Vec<_>>(),
            vec!["image-a", "image-z"]
        );
        assert_ne!(
            bridge.samples[0].relative_path,
            bridge.samples[1].relative_path
        );
        let snapshot_dir = bridge_path.parent().unwrap();
        for sample in &bridge.samples {
            assert!(snapshot_dir
                .join(&bridge.asset_root)
                .join(&sample.relative_path)
                .is_file());
        }
        let listed = repository.dataset_snapshots(project_id).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].bridge_status, "ready");
        assert_eq!(
            listed[0].bridge_manifest_path,
            snapshot.bridge_manifest_path
        );

        let _ = std::fs::remove_dir_all(paths.root);
    }

    #[test]
    fn failed_bridge_publish_compensates_snapshot_record_and_directory() {
        let repository = SampleRepository::new();
        let project_id = "bridge-snapshot-failure-unit";
        let paths = project_fs::project_paths(project_id);
        let _ = std::fs::remove_dir_all(&paths.root);
        project_fs::ensure_workspace_project_dirs(project_id).unwrap();
        storage::initialize_project_database(&paths.sqlite).unwrap();
        let manifest = project_fs::ProjectManifest {
            id: project_id.to_string(),
            name: "Bridge Failure Unit".to_string(),
            source_dataset_key: "local".to_string(),
            format: "yolo-detect".to_string(),
            root_path: paths.raw.to_string_lossy().to_string(),
            created_at: now_unix_string(),
            class_count: 0,
            image_count: 1,
        };
        let images = vec![storage::StoredImage {
            id: "missing-image".to_string(),
            file_name: "missing.png".to_string(),
            width: 10,
            height: 10,
            split: "train".to_string(),
            status: "已标注".to_string(),
            qa_status: String::new(),
            review_note: None,
        }];
        storage::upsert_project_index(&paths.sqlite, &manifest, &images, &[]).unwrap();

        let result = repository.create_dataset_snapshot(project_id, "must-fail");

        assert!(result.is_err());
        assert!(storage::list_snapshot_records(&paths.sqlite)
            .unwrap()
            .is_empty());
        assert_eq!(std::fs::read_dir(&paths.snapshots).unwrap().count(), 0);

        let _ = std::fs::remove_dir_all(paths.root);
    }

    #[test]
    fn snapshot_creation_rejects_source_path_traversal_and_compensates() {
        let repository = SampleRepository::new();
        let project_id = "bridge-snapshot-traversal-unit";
        let paths = project_fs::project_paths(project_id);
        let _ = std::fs::remove_dir_all(&paths.root);
        project_fs::ensure_workspace_project_dirs(project_id).unwrap();
        storage::initialize_project_database(&paths.sqlite).unwrap();
        image::RgbImage::from_pixel(10, 10, image::Rgb([7, 8, 9]))
            .save(paths.root.join("outside.png"))
            .unwrap();
        let manifest = project_fs::ProjectManifest {
            id: project_id.to_string(),
            name: "Bridge Traversal Unit".to_string(),
            source_dataset_key: "local".to_string(),
            format: "yolo-detect".to_string(),
            root_path: paths.raw.to_string_lossy().to_string(),
            created_at: now_unix_string(),
            class_count: 0,
            image_count: 1,
        };
        let images = vec![storage::StoredImage {
            id: "escaping-image".to_string(),
            file_name: "../../outside.png".to_string(),
            width: 10,
            height: 10,
            split: "train".to_string(),
            status: "已标注".to_string(),
            qa_status: String::new(),
            review_note: None,
        }];
        storage::upsert_project_index(&paths.sqlite, &manifest, &images, &[]).unwrap();

        let result = repository.create_dataset_snapshot(project_id, "must-reject-traversal");

        assert!(result.is_err());
        assert!(storage::list_snapshot_records(&paths.sqlite)
            .unwrap()
            .is_empty());
        assert_eq!(std::fs::read_dir(&paths.snapshots).unwrap().count(), 0);

        let _ = std::fs::remove_dir_all(paths.root);
    }

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
    fn exports_snapshot_as_coco_with_bbox_polygon_and_images() {
        let repository = SampleRepository::new();
        let project_id = "coco-export-unit";
        let paths = project_fs::project_paths(project_id);
        let _ = std::fs::remove_dir_all(&paths.root);
        project_fs::ensure_workspace_project_dirs(project_id).unwrap();
        storage::initialize_project_database(&paths.sqlite).unwrap();
        std::fs::create_dir_all(paths.raw.join("train")).unwrap();
        image::RgbImage::new(100, 80)
            .save(paths.raw.join("train/sample.png"))
            .unwrap();

        let manifest = project_fs::ProjectManifest {
            id: project_id.to_string(),
            name: "COCO Export Unit".to_string(),
            source_dataset_key: "local".to_string(),
            format: "yolo-seg".to_string(),
            root_path: paths.raw.to_string_lossy().to_string(),
            created_at: now_unix_string(),
            class_count: 2,
            image_count: 1,
        };
        let images = vec![storage::StoredImage {
            id: "image-a".to_string(),
            file_name: "train/sample.png".to_string(),
            width: 100,
            height: 80,
            split: "train".to_string(),
            status: "已标注".to_string(),
            qa_status: String::new(),
            review_note: None,
        }];
        let classes = vec![
            storage::StoredClass {
                id: 0,
                label: "box".to_string(),
                color: "#1fa7ff".to_string(),
            },
            storage::StoredClass {
                id: 1,
                label: "region".to_string(),
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
                        "bbox-a".to_string(),
                        0,
                        "box".to_string(),
                        BBox {
                            x: 10.0,
                            y: 12.0,
                            width: 20.0,
                            height: 15.0,
                        },
                    ),
                    AnnotationObject::polygon(
                        "polygon-a".to_string(),
                        1,
                        "region".to_string(),
                        vec![
                            Point { x: 40.0, y: 20.0 },
                            Point { x: 70.0, y: 20.0 },
                            Point { x: 70.0, y: 50.0 },
                            Point { x: 40.0, y: 50.0 },
                        ],
                    ),
                ],
            )
            .unwrap();

        let snapshot = repository
            .create_dataset_snapshot(project_id, "release-1")
            .unwrap();
        let export = repository
            .export_dataset(project_id, &snapshot.id, "coco")
            .unwrap();
        let output_dir = PathBuf::from(export.output_path);
        let coco: Value = serde_json::from_str(
            &std::fs::read_to_string(output_dir.join("annotations.json")).unwrap(),
        )
        .unwrap();

        assert_eq!(coco["images"][0]["width"], 100);
        assert_eq!(coco["images"][0]["height"], 80);
        assert_eq!(coco["categories"][0]["name"], "box");
        assert_eq!(coco["categories"][1]["name"], "region");
        assert_eq!(coco["annotations"].as_array().unwrap().len(), 2);
        assert_eq!(
            coco["annotations"][0]["bbox"],
            json!([10.0, 12.0, 20.0, 15.0])
        );
        assert_eq!(coco["annotations"][0]["area"], 300.0);
        assert_eq!(
            coco["annotations"][1]["segmentation"][0],
            json!([40.0, 20.0, 70.0, 20.0, 70.0, 50.0, 40.0, 50.0])
        );
        assert_eq!(coco["annotations"][1]["area"], 900.0);
        assert!(output_dir.join("images/train/sample.png").is_file());

        let _ = std::fs::remove_dir_all(paths.root);
    }
}

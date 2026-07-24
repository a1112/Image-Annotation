use crate::{
    importers::{voc, yolo},
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
        let images = self.project_images(project_id, None);
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

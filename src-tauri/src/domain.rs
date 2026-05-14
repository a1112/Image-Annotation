use crate::{importers::yolo, project_fs, storage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, fs, path::PathBuf};
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
    pub tags: Vec<String>,
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
                let annotation_types = if indexed_manifest.format == "yolo-seg" {
                    vec!["Polygon".to_string(), "BBox".to_string()]
                } else {
                    vec!["BBox".to_string()]
                };
                DatasetProject {
                    id: indexed_manifest.id.clone(),
                    name: indexed_manifest.name.clone(),
                    description: format!("真实 {} 测试数据集", indexed_manifest.source_dataset_key),
                    annotation_types,
                    image_count: if indexed_images.is_empty() {
                        indexed_manifest.image_count
                    } else {
                        indexed_images.len() as u32
                    },
                    annotated_percent: if indexed_manifest.image_count > 0 {
                        100
                    } else {
                        0
                    },
                    review_count: 0,
                    issue_count: 0,
                    class_count: if indexed_classes.is_empty() {
                        indexed_manifest.class_count as u16
                    } else {
                        indexed_classes.len() as u16
                    },
                    tag_group_count: 3,
                    status: "已导入".to_string(),
                    tags: vec![
                        "source: ultralytics".to_string(),
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
                        label: class.label,
                        color: class.color,
                        count: 0,
                        attributes: Vec::new(),
                    })
                    .collect()
            },
            tasks: vec![TaskSummary {
                name: "测试数据标注校验".to_string(),
                owner: "数据生产平台".to_string(),
                status: "Active".to_string(),
                progress: 100,
            }],
            quality_checks: Vec::new(),
            export_presets: Vec::new(),
        })
    }

    pub fn project_images(&self, project_id: &str, group_id: Option<String>) -> Vec<DatasetImage> {
        let paths = project_fs::project_paths(project_id);
        let indexed_images =
            storage::read_images(&paths.sqlite, group_id.as_deref()).unwrap_or_default();
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
                    tags: vec![format!("split={}", image.split)],
                })
                .collect();
        }
        let mut images = Vec::new();

        for entry in WalkDir::new(&paths.raw).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() || !is_image_path(&entry.path().to_path_buf()) {
                continue;
            }

            let path = entry.path().to_path_buf();
            let split = split_for_path(&path);
            if let Some(group_id) = &group_id {
                if group_id != &split {
                    continue;
                }
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
                tags: vec![format!("split={split}")],
            });
        }

        images.sort_by(|left, right| left.file_name.cmp(&right.file_name));
        images
    }

    pub fn image_path(&self, project_id: &str, image_id: &str) -> Option<PathBuf> {
        let paths = project_fs::project_paths(project_id);
        WalkDir::new(paths.raw)
            .into_iter()
            .filter_map(Result::ok)
            .find(|entry| {
                entry.file_type().is_file()
                    && is_image_path(&entry.path().to_path_buf())
                    && entry
                        .path()
                        .file_stem()
                        .map(|value| value.to_string_lossy() == image_id)
                        .unwrap_or(false)
            })
            .map(|entry| entry.path().to_path_buf())
    }

    pub fn image_annotations(&self, project_id: &str, image_id: &str) -> Vec<AnnotationObject> {
        let paths = project_fs::project_paths(project_id);
        let native_path = paths.annotations.join(format!("{image_id}.json"));
        if let Ok(data) = fs::read_to_string(native_path) {
            if let Ok(objects) = serde_json::from_str::<Vec<AnnotationObject>>(&data) {
                return objects;
            }
        }

        let Some(image_path) = self.image_path(project_id, image_id) else {
            return Vec::new();
        };
        let Some(label_path) = label_path_for_image(&paths.raw, &image_path) else {
            return Vec::new();
        };
        let Ok(label_data) = fs::read_to_string(label_path) else {
            return Vec::new();
        };

        let (width, height) = image::image_dimensions(&image_path).unwrap_or((0, 0));
        let labels = coco_labels();
        let prefer_polygon = project_fs::read_manifest(project_id)
            .map(|manifest| manifest.format == "yolo-seg")
            .unwrap_or(false);

        label_data
            .lines()
            .enumerate()
            .filter_map(|(index, line)| {
                yolo::line_to_annotation(line, width, height, &labels, index, prefer_polygon).ok()
            })
            .collect()
    }

    pub fn save_image_annotations(
        &self,
        project_id: &str,
        image_id: &str,
        objects: Vec<AnnotationObject>,
    ) -> Result<(), String> {
        let paths = project_fs::ensure_project_dirs(project_id)?;
        let data = serde_json::to_string_pretty(&objects).map_err(|err| err.to_string())?;
        fs::write(paths.annotations.join(format!("{image_id}.json")), data)
            .map_err(|err| err.to_string())
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

pub fn backend_design() -> BackendDesign {
    BackendDesign {
        layers: vec![
            BackendLayer {
                name: "Command API".to_string(),
                responsibility: "Tauri commands expose real dataset downloads, project indexing, annotation persistence, and independent annotation windows.".to_string(),
            },
            BackendLayer {
                name: "Project FS".to_string(),
                responsibility: "The local data/test_data workspace stores manifests, raw downloaded files, native annotations, thumbnails, exports, and SQLite databases.".to_string(),
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
        storage_plan: "Use data/test_data/projects/{projectId} for raw files, project.json manifests, project.sqlite metadata, annotations/native JSON, and export output.".to_string(),
        command_plan: vec![
            "list_builtin_datasets".to_string(),
            "download_test_dataset".to_string(),
            "list_dataset_projects".to_string(),
            "get_project_detail".to_string(),
            "list_project_images".to_string(),
            "get_image_annotations".to_string(),
            "save_image_annotations".to_string(),
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

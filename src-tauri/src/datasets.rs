use crate::{domain, project_fs, storage};
use serde::Serialize;
use std::{
    fs,
    io::{self, Cursor},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct BuiltinDatasetSource {
    pub key: String,
    pub name: String,
    pub description: String,
    pub task_type: String,
    pub format: String,
    pub download_url: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuiltinDataset {
    pub key: String,
    pub name: String,
    pub description: String,
    pub task_type: String,
    pub format: String,
    pub downloaded: bool,
    pub project_id: Option<String>,
}

pub fn builtin_dataset_sources() -> Vec<BuiltinDatasetSource> {
    vec![
        BuiltinDatasetSource {
            key: "coco128".to_string(),
            name: "COCO128".to_string(),
            description: "128 张 COCO 训练图片，适合目标检测流程验证。".to_string(),
            task_type: "目标检测".to_string(),
            format: "yolo-detect".to_string(),
            download_url:
                "https://github.com/ultralytics/assets/releases/download/v0.0.0/coco128.zip"
                    .to_string(),
        },
        BuiltinDatasetSource {
            key: "coco8".to_string(),
            name: "COCO8".to_string(),
            description: "8 张 COCO 示例图片，适合快速烟测。".to_string(),
            task_type: "目标检测".to_string(),
            format: "yolo-detect".to_string(),
            download_url:
                "https://github.com/ultralytics/assets/releases/download/v0.0.0/coco8.zip"
                    .to_string(),
        },
        BuiltinDatasetSource {
            key: "coco8-seg".to_string(),
            name: "COCO8-seg".to_string(),
            description: "8 张 COCO 分割示例图片，适合 polygon 标注验证。".to_string(),
            task_type: "实例分割".to_string(),
            format: "yolo-seg".to_string(),
            download_url:
                "https://github.com/ultralytics/assets/releases/download/v0.0.0/coco8-seg.zip"
                    .to_string(),
        },
        BuiltinDatasetSource {
            key: "coco128-seg".to_string(),
            name: "COCO128-seg".to_string(),
            description: "128 张 COCO 分割图片，适合分割数据生产预览。".to_string(),
            task_type: "实例分割".to_string(),
            format: "yolo-seg".to_string(),
            download_url:
                "https://github.com/ultralytics/assets/releases/download/v0.0.0/coco128-seg.zip"
                    .to_string(),
        },
    ]
}

pub fn builtin_datasets() -> Vec<BuiltinDataset> {
    builtin_dataset_sources()
        .into_iter()
        .map(|source| {
            let downloaded = project_is_imported(&source.key);
            BuiltinDataset {
                key: source.key.clone(),
                name: source.name,
                description: source.description,
                task_type: source.task_type,
                format: source.format,
                downloaded,
                project_id: downloaded.then_some(source.key),
            }
        })
        .collect()
}

pub fn source_by_key(dataset_key: &str) -> Option<BuiltinDatasetSource> {
    builtin_dataset_sources()
        .into_iter()
        .find(|dataset| dataset.key == dataset_key)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadJob {
    pub id: String,
    pub dataset_key: String,
    pub status: String,
    pub progress: u8,
    pub message: String,
    pub project_id: Option<String>,
}

pub fn download_test_dataset(dataset_key: &str) -> Result<DownloadJob, String> {
    project_fs::ensure_test_data_dirs()?;
    let source = source_by_key(dataset_key)
        .ok_or_else(|| format!("unknown builtin dataset: {dataset_key}"))?;
    let paths = project_fs::ensure_project_dirs(&source.key)?;
    if project_is_imported(&source.key) {
        rebuild_sqlite_index_if_needed(&source)?;
        let image_count = project_fs::read_manifest(&source.key)
            .map(|manifest| manifest.image_count)
            .unwrap_or_else(|| count_images(&paths.raw));
        return Ok(DownloadJob {
            id: format!("download-{}", source.key),
            dataset_key: source.key.clone(),
            status: "completed".to_string(),
            progress: 100,
            message: format!(
                "{} 已存在本地工程，包含 {} 张图片",
                source.name, image_count
            ),
            project_id: Some(source.key),
        });
    }
    let download_path = project_fs::downloads_dir().join(format!("{}.zip", source.key));

    if !download_path.exists() {
        let mut response = reqwest::blocking::Client::builder()
            .no_gzip()
            .no_brotli()
            .no_zstd()
            .no_deflate()
            .build()
            .map_err(|err| err.to_string())?
            .get(&source.download_url)
            .header(reqwest::header::ACCEPT_ENCODING, "identity")
            .send()
            .and_then(|response| response.error_for_status())
            .map_err(|err| err.to_string())?;
        let mut output = fs::File::create(&download_path).map_err(|err| err.to_string())?;
        response
            .copy_to(&mut output)
            .map_err(|err| err.to_string())?;
    }

    let archive_bytes = fs::read(&download_path).map_err(|err| err.to_string())?;
    import_dataset_archive(&source, archive_bytes)
}

pub fn import_dataset_archive(
    source: &BuiltinDatasetSource,
    archive_bytes: Vec<u8>,
) -> Result<DownloadJob, String> {
    project_fs::ensure_test_data_dirs()?;
    let paths = project_fs::ensure_project_dirs(&source.key)?;
    let mut archive =
        zip::ZipArchive::new(Cursor::new(archive_bytes)).map_err(|err| err.to_string())?;
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).map_err(|err| err.to_string())?;
        let Some(output_path) = project_fs::safe_extract_path(&paths.raw, file.name()) else {
            continue;
        };

        if file.is_dir() {
            fs::create_dir_all(&output_path).map_err(|err| err.to_string())?;
            continue;
        }

        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        let mut output = fs::File::create(&output_path).map_err(|err| err.to_string())?;
        io::copy(&mut file, &mut output).map_err(|err| err.to_string())?;
    }

    let images = indexed_images(&paths.raw);
    let labels = domain::coco_labels();
    let classes: Vec<_> = labels
        .iter()
        .enumerate()
        .map(|(index, label)| storage::StoredClass {
            id: index as u32,
            label: label.clone(),
            color: class_color(index),
        })
        .collect();
    let manifest = project_fs::ProjectManifest {
        id: source.key.clone(),
        name: source.name.clone(),
        source_dataset_key: source.key.clone(),
        format: source.format.clone(),
        root_path: paths.root.to_string_lossy().to_string(),
        created_at: now_unix_string(),
        class_count: labels.len() as u32,
        image_count: images.len() as u32,
    };

    project_fs::write_manifest(&manifest)?;
    storage::initialize_project_database(&paths.sqlite)?;
    storage::upsert_project_index(&paths.sqlite, &manifest, &images, &classes)?;

    Ok(DownloadJob {
        id: format!("download-{}", source.key),
        dataset_key: source.key.clone(),
        status: "completed".to_string(),
        progress: 100,
        message: format!("{} 已下载并导入 {} 张图片", source.name, images.len()),
        project_id: Some(source.key.clone()),
    })
}

pub fn completed_job(dataset_key: &str) -> Option<DownloadJob> {
    project_fs::read_manifest(dataset_key).map(|manifest| DownloadJob {
        id: format!("download-{dataset_key}"),
        dataset_key: dataset_key.to_string(),
        status: "completed".to_string(),
        progress: 100,
        message: format!("{} 已完成", manifest.name),
        project_id: Some(manifest.id),
    })
}

pub fn create_dataset_project(
    name: &str,
    dataset_type: &str,
    demo_template: &str,
) -> Result<domain::DatasetProject, String> {
    project_fs::ensure_test_data_dirs()?;
    let project_id = project_id_from_name(name, demo_template);
    let source = BuiltinDatasetSource {
        key: project_id.clone(),
        name: name.trim().to_string(),
        description: "本地新建数据集".to_string(),
        task_type: if dataset_type == "yolo-seg" {
            "实例分割".to_string()
        } else {
            "目标检测".to_string()
        },
        format: if dataset_type == "yolo-seg" {
            "yolo-seg".to_string()
        } else {
            "yolo-detect".to_string()
        },
        download_url: String::new(),
    };
    let paths = project_fs::ensure_project_dirs(&source.key)?;
    if demo_template != "empty" {
        create_demo_files(&paths.raw, &source.format, demo_template)?;
    }

    let images = indexed_images(&paths.raw);
    let labels = demo_class_labels();
    let classes: Vec<_> = labels
        .iter()
        .enumerate()
        .map(|(index, label)| storage::StoredClass {
            id: index as u32,
            label: label.clone(),
            color: class_color(index),
        })
        .collect();
    let manifest = project_fs::ProjectManifest {
        id: source.key.clone(),
        name: source.name.clone(),
        source_dataset_key: "local-demo".to_string(),
        format: source.format.clone(),
        root_path: paths.root.to_string_lossy().to_string(),
        created_at: now_unix_string(),
        class_count: classes.len() as u32,
        image_count: images.len() as u32,
    };

    project_fs::write_manifest(&manifest)?;
    storage::initialize_project_database(&paths.sqlite)?;
    storage::upsert_project_index(&paths.sqlite, &manifest, &images, &classes)?;

    Ok(domain::DatasetProject {
        id: manifest.id,
        name: manifest.name,
        description: "新建 Demo 数据集".to_string(),
        annotation_types: if source.format == "yolo-seg" {
            vec!["Polygon".to_string(), "BBox".to_string()]
        } else {
            vec!["BBox".to_string()]
        },
        image_count: images.len() as u32,
        annotated_percent: if images.is_empty() { 0 } else { 100 },
        review_count: 0,
        issue_count: 0,
        class_count: classes.len() as u16,
        tag_group_count: 3,
        status: "已导入".to_string(),
        tags: vec![
            "source: demo".to_string(),
            format!("format: {}", source.format),
            "split: train".to_string(),
        ],
    })
}

fn now_unix_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn create_demo_files(raw_root: &Path, format: &str, demo_template: &str) -> Result<(), String> {
    let image_dir = raw_root.join("images").join("train");
    let label_dir = raw_root.join("labels").join("train");
    fs::create_dir_all(&image_dir).map_err(|err| err.to_string())?;
    fs::create_dir_all(&label_dir).map_err(|err| err.to_string())?;

    for index in 1..=3 {
        let image_path = image_dir.join(format!("demo_{index:03}.png"));
        let label_path = label_dir.join(format!("demo_{index:03}.txt"));
        write_demo_image(&image_path, index)?;
        let label = if format == "yolo-seg" || demo_template == "demo-polygon" {
            "0 0.20 0.20 0.72 0.18 0.82 0.70 0.24 0.76\n"
        } else {
            "0 0.50 0.52 0.46 0.42\n1 0.28 0.30 0.18 0.16\n"
        };
        fs::write(label_path, label).map_err(|err| err.to_string())?;
    }

    Ok(())
}

fn write_demo_image(path: &Path, index: u32) -> Result<(), String> {
    let width = 640;
    let height = 420;
    let image = image::RgbaImage::from_fn(width, height, |x, y| {
        let lane = if (x / 80 + y / 60 + index) % 2 == 0 {
            32
        } else {
            18
        };
        let r = (28 + index * 28 + x / 16).min(255) as u8;
        let g = (72 + lane + y / 12).min(255) as u8;
        let b = (112 + index * 18).min(255) as u8;
        image::Rgba([r, g, b, 255])
    });
    image.save(path).map_err(|err| err.to_string())
}

fn demo_class_labels() -> Vec<String> {
    ["object", "region", "defect"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn project_id_from_name(name: &str, fallback: &str) -> String {
    let mut id = String::new();
    let mut last_dash = false;
    for character in name.trim().to_ascii_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            id.push(character);
            last_dash = false;
        } else if !last_dash && !id.is_empty() {
            id.push('-');
            last_dash = true;
        }
    }
    while id.ends_with('-') {
        id.pop();
    }
    if id.is_empty() {
        fallback.to_string()
    } else {
        id
    }
}

fn project_is_imported(project_id: &str) -> bool {
    let Some(manifest) = project_fs::read_manifest(project_id) else {
        return false;
    };
    manifest.image_count > 0 && count_images(&project_fs::project_paths(project_id).raw) > 0
}

fn rebuild_sqlite_index_if_needed(source: &BuiltinDatasetSource) -> Result<(), String> {
    let paths = project_fs::project_paths(&source.key);
    if !storage::read_images(&paths.sqlite, None)?.is_empty() {
        return Ok(());
    }
    let images = indexed_images(&paths.raw);
    let labels = domain::coco_labels();
    let classes: Vec<_> = labels
        .iter()
        .enumerate()
        .map(|(index, label)| storage::StoredClass {
            id: index as u32,
            label: label.clone(),
            color: class_color(index),
        })
        .collect();
    let manifest = project_fs::read_manifest(&source.key)
        .ok_or_else(|| format!("project manifest not found: {}", source.key))?;
    storage::initialize_project_database(&paths.sqlite)?;
    storage::upsert_project_index(&paths.sqlite, &manifest, &images, &classes)
}

fn count_images(raw_root: &Path) -> u32 {
    indexed_image_paths(raw_root).len() as u32
}

fn indexed_images(raw_root: &Path) -> Vec<storage::StoredImage> {
    let mut images: Vec<_> = indexed_image_paths(raw_root)
        .into_iter()
        .map(|path| {
            let (width, height) = image::image_dimensions(&path).unwrap_or((0, 0));
            let file_name = path
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_else(|| "image.jpg".to_string());
            let id = path
                .file_stem()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_else(|| file_name.clone());
            storage::StoredImage {
                id,
                file_name,
                width,
                height,
                split: split_for_path(&path),
                status: "已标注".to_string(),
            }
        })
        .collect();
    images.sort_by(|left, right| left.file_name.cmp(&right.file_name));
    images
}

fn indexed_image_paths(raw_root: &Path) -> Vec<PathBuf> {
    WalkDir::new(raw_root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_type().is_file() && domain::is_image_path(&entry.path().to_path_buf())
        })
        .map(|entry| entry.path().to_path_buf())
        .collect()
}

fn split_for_path(path: &Path) -> String {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    if lower.contains("val") {
        "val".to_string()
    } else if lower.contains("test") {
        "test".to_string()
    } else {
        "train".to_string()
    }
}

fn class_color(index: usize) -> String {
    const COLORS: [&str; 8] = [
        "#1fa7ff", "#cc54d8", "#f59e0b", "#22c55e", "#8b5cf6", "#ef4444", "#14b8a6", "#64748b",
    ];
    COLORS[index % COLORS.len()].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn imports_dataset_archive_into_manifest_and_sqlite_index() {
        let source = BuiltinDatasetSource {
            key: "unit-fixture".to_string(),
            name: "Unit Fixture".to_string(),
            description: "fixture".to_string(),
            task_type: "目标检测".to_string(),
            format: "yolo-detect".to_string(),
            download_url: "fixture://unit".to_string(),
        };
        let paths = project_fs::project_paths(&source.key);
        let _ = fs::remove_dir_all(&paths.root);
        let archive = fixture_zip();

        let job = import_dataset_archive(&source, archive).unwrap();

        assert_eq!(job.status, "completed");
        assert_eq!(
            project_fs::read_manifest(&source.key).unwrap().image_count,
            1
        );
        assert_eq!(storage::read_images(&paths.sqlite, None).unwrap().len(), 1);
        assert_eq!(storage::read_classes(&paths.sqlite).unwrap().len(), 80);
        let _ = fs::remove_dir_all(paths.root);
    }

    #[test]
    fn creates_demo_bbox_dataset_project_with_images_labels_and_index() {
        let project = create_dataset_project("Demo Unit", "yolo-detect", "demo-bbox").unwrap();
        let paths = project_fs::project_paths(&project.id);

        assert_eq!(project.name, "Demo Unit");
        assert_eq!(project.image_count, 3);
        assert!(paths
            .raw
            .join("images")
            .join("train")
            .join("demo_001.png")
            .exists());
        assert!(paths
            .raw
            .join("labels")
            .join("train")
            .join("demo_001.txt")
            .exists());
        assert_eq!(storage::read_images(&paths.sqlite, None).unwrap().len(), 3);

        let _ = fs::remove_dir_all(paths.root);
    }

    fn fixture_zip() -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            let options = zip::write::SimpleFileOptions::default();
            writer
                .start_file("fixture/images/train/0001.png", options)
                .unwrap();
            writer
                .write_all(&[
                    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0,
                    0, 1, 8, 6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156,
                    99, 248, 15, 4, 0, 9, 251, 3, 253, 167, 181, 93, 186, 0, 0, 0, 0, 73, 69, 78,
                    68, 174, 66, 96, 130,
                ])
                .unwrap();
            writer
                .start_file("fixture/labels/train/0001.txt", options)
                .unwrap();
            writer.write_all(b"0 0.5 0.5 1.0 1.0\n").unwrap();
            writer.finish().unwrap();
        }
        cursor.into_inner()
    }
}

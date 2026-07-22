use crate::{domain, importers::voc, project_fs, storage};
use serde::Serialize;
use std::{
    collections::{hash_map::DefaultHasher, BTreeSet},
    fs,
    hash::{Hash, Hasher},
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DataSourceTreeNode {
    pub name: String,
    pub path: String,
    pub kind: String,
    pub children: Vec<DataSourceTreeNode>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DataSourceAnalysis {
    pub source_paths: Vec<String>,
    pub root_path: String,
    pub source_kind: String,
    pub detected_format: String,
    pub recommended_action: String,
    pub image_count: u32,
    pub annotation_count: u32,
    pub class_count: u32,
    pub classes: Vec<String>,
    pub split_count: u32,
    pub warnings: Vec<String>,
    pub tree: Vec<DataSourceTreeNode>,
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

pub fn pick_data_source(selection_type: &str) -> Result<Option<Vec<String>>, String> {
    let picked = match selection_type {
        "files" => rfd::FileDialog::new()
            .add_filter(
                "Images and labels",
                &["jpg", "jpeg", "png", "bmp", "webp", "xml", "txt"],
            )
            .pick_files(),
        _ => rfd::FileDialog::new().pick_folder().map(|path| vec![path]),
    };
    Ok(picked.map(|paths| {
        paths
            .into_iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect()
    }))
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
    let paths = project_fs::ensure_test_project_dirs(&source.key)?;
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
    let paths = project_fs::ensure_test_project_dirs(&source.key)?;
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
    storage::record_import(
        &paths.sqlite,
        &source.download_url,
        "completed",
        &format!("已导入内置数据集 {} 张图片", images.len()),
    )?;

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
    let format = match dataset_type {
        "yolo-seg" => "yolo-seg",
        "image-classification" => "image-classification",
        _ => "yolo-detect",
    };
    let source = BuiltinDatasetSource {
        key: project_id.clone(),
        name: name.trim().to_string(),
        description: "本地新建数据集".to_string(),
        task_type: match format {
            "yolo-seg" => "实例分割".to_string(),
            "image-classification" => "图像分类".to_string(),
            _ => "目标检测".to_string(),
        },
        format: format.to_string(),
        download_url: String::new(),
    };
    let paths = project_fs::ensure_workspace_project_dirs(&source.key)?;
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
        annotation_types: annotation_types_for_format(&source.format),
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

pub fn import_images_into_project(
    project_id: &str,
    source_path: &str,
) -> Result<domain::DatasetProject, String> {
    let paths = project_fs::ensure_project_dirs(project_id)?;
    let source = PathBuf::from(source_path);
    if !source.exists() {
        return Err(format!("source path not found: {source_path}"));
    }
    let target_dir = paths.raw.join("images").join("imported");
    fs::create_dir_all(&target_dir).map_err(|err| err.to_string())?;
    for entry in WalkDir::new(&source).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() || !domain::is_image_path(&entry.path().to_path_buf()) {
            continue;
        }
        let file_name = entry
            .path()
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| "image.jpg".to_string());
        let target = unique_target_path(&target_dir, &file_name);
        fs::copy(entry.path(), target).map_err(|err| err.to_string())?;
    }
    let project = rescan_project_assets(project_id)?;
    storage::record_import(
        &paths.sqlite,
        &source.to_string_lossy(),
        "completed",
        &format!("已复制并索引 {} 张图片", project.image_count),
    )?;
    Ok(project)
}

pub fn import_yolo_dataset_into_project(
    project_id: &str,
    source_path: &str,
) -> Result<domain::DatasetProject, String> {
    let paths = project_fs::ensure_project_dirs(project_id)?;
    let source = PathBuf::from(source_path);
    if !source.exists() || !source.is_dir() {
        return Err(format!("YOLO dataset directory not found: {source_path}"));
    }
    copy_dir_contents(&source, &paths.raw)?;
    let project = rescan_project_assets(project_id)?;
    storage::record_import(
        &paths.sqlite,
        &source.to_string_lossy(),
        "completed",
        &format!("已复制 YOLO 数据集并索引 {} 张图片", project.image_count),
    )?;
    Ok(project)
}

pub fn import_files_into_project(
    project_id: &str,
    source_paths: &[String],
) -> Result<domain::DatasetProject, String> {
    let paths = project_fs::ensure_project_dirs(project_id)?;
    let target_dir = paths.raw.join("images").join("imported");
    fs::create_dir_all(&target_dir).map_err(|err| err.to_string())?;
    let mut copied = 0u32;
    for source_path in source_paths {
        let source = PathBuf::from(source_path);
        if !source.exists() || !source.is_file() || !domain::is_image_path(&source) {
            continue;
        }
        let file_name = source
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| "image.jpg".to_string());
        let target = unique_target_path(&target_dir, &file_name);
        fs::copy(&source, target).map_err(|err| err.to_string())?;
        copied += 1;
    }
    if copied == 0 {
        return Err("未选择可导入的图片文件".to_string());
    }
    let project = rescan_project_assets(project_id)?;
    storage::record_import(
        &paths.sqlite,
        &source_paths.join(";"),
        "completed",
        &format!("已复制并索引 {copied} 个选择文件"),
    )?;
    Ok(project)
}

pub fn analyze_data_source(source_paths: &[String]) -> Result<DataSourceAnalysis, String> {
    if source_paths.is_empty() {
        return Err("请选择文件夹或文件".to_string());
    }
    let paths = source_paths.iter().map(PathBuf::from).collect::<Vec<_>>();
    let source_kind = if paths.len() == 1 && paths[0].is_dir() {
        "folder"
    } else {
        "files"
    };
    let root = if source_kind == "folder" {
        fs::canonicalize(&paths[0]).unwrap_or_else(|_| paths[0].clone())
    } else {
        common_parent(&paths)
            .unwrap_or_else(|| paths[0].parent().unwrap_or(Path::new("")).to_path_buf())
    };
    let scan_files = collect_source_files(&paths);
    let image_count = scan_files
        .iter()
        .filter(|path| domain::is_image_path(path))
        .count() as u32;
    let xml_count = scan_files
        .iter()
        .filter(|path| has_extension(path, "xml"))
        .count() as u32;
    let yolo_label_count = scan_files
        .iter()
        .filter(|path| has_extension(path, "txt") && path_contains_segment(path, "labels"))
        .count() as u32;
    let mut classes = if xml_count > 0 {
        indexed_voc_labels_from_files(&scan_files)
    } else {
        indexed_yolo_labels(&root)
    };
    let detected_format = if xml_count > 0 {
        "voc-detect"
    } else if yolo_label_count > 0 || !classes.is_empty() {
        "yolo-detect"
    } else if image_count > 0 {
        "image-directory"
    } else {
        "unknown"
    }
    .to_string();
    if classes.is_empty() && detected_format == "yolo-detect" {
        classes = indexed_yolo_label_ids_from_files(&scan_files)
            .into_iter()
            .max()
            .map(|max_id| (0..=max_id).map(|id| format!("class_{id}")).collect())
            .unwrap_or_default();
    }
    let annotation_count = if detected_format == "voc-detect" {
        xml_count
    } else if detected_format == "yolo-detect" {
        yolo_label_count
    } else {
        0
    };
    let split_count = detected_splits(&scan_files).len() as u32;
    let recommended_action = if source_kind == "folder"
        && (detected_format == "voc-detect" || detected_format == "yolo-detect")
    {
        "open-local"
    } else {
        "copy-images"
    }
    .to_string();
    let mut warnings = Vec::new();
    if image_count == 0 {
        warnings.push("未发现可导入图片".to_string());
    }
    if detected_format == "voc-detect" && xml_count < image_count {
        warnings.push(format!(
            "发现 {image_count} 张图片，但只有 {xml_count} 个 XML 标注文件"
        ));
    }
    if detected_format == "image-directory" {
        warnings.push("未发现标注文件，将按未标注图片导入".to_string());
    }

    Ok(DataSourceAnalysis {
        source_paths: source_paths.to_vec(),
        root_path: root.to_string_lossy().to_string(),
        source_kind: source_kind.to_string(),
        detected_format,
        recommended_action,
        image_count,
        annotation_count,
        class_count: classes.len() as u32,
        classes,
        split_count,
        warnings,
        tree: build_source_tree(&paths, &root),
    })
}

pub fn rescan_project_assets(project_id: &str) -> Result<domain::DatasetProject, String> {
    let paths = project_fs::ensure_project_dirs(project_id)?;
    let mut manifest = project_fs::read_manifest(project_id)
        .ok_or_else(|| format!("project manifest not found: {project_id}"))?;
    let is_local_linked = manifest.source_dataset_key == "local-linked";
    let local_root = PathBuf::from(&manifest.root_path);
    let images = if is_local_linked {
        if !local_root.exists() {
            return Err(format!(
                "local dataset directory not found: {}",
                manifest.root_path
            ));
        }
        indexed_local_images(&local_root, &manifest.format)
    } else {
        indexed_images(&paths.raw)
    };
    let mut classes = if is_local_linked {
        classes_from_labels(local_labels_for_format(&local_root, &manifest.format))
    } else {
        Vec::new()
    };
    if classes.is_empty() {
        classes = storage::read_classes(&paths.sqlite).unwrap_or_default();
    }
    if classes.is_empty() {
        classes = classes_from_labels(demo_class_labels());
    }
    manifest.image_count = images.len() as u32;
    manifest.class_count = classes.len() as u32;
    project_fs::write_manifest_to_path(&manifest, &paths.manifest)?;
    storage::initialize_project_database(&paths.sqlite)?;
    storage::upsert_project_index(&paths.sqlite, &manifest, &images, &classes)?;
    domain::SampleRepository::new()
        .dataset_projects()
        .into_iter()
        .find(|project| project.id == project_id)
        .ok_or_else(|| format!("project not found after rescan: {project_id}"))
}

pub fn generate_project_thumbnails(project_id: &str) -> Result<u32, String> {
    let paths = project_fs::ensure_project_dirs(project_id)?;
    let manifest = project_fs::read_manifest(project_id);
    let root = manifest
        .as_ref()
        .filter(|manifest| manifest.source_dataset_key == "local-linked")
        .map(|manifest| PathBuf::from(&manifest.root_path))
        .unwrap_or_else(|| paths.raw.clone());
    if !root.exists() {
        return Err(format!(
            "dataset image directory not found: {}",
            root.display()
        ));
    }
    let is_local_linked = manifest
        .as_ref()
        .map(|manifest| manifest.source_dataset_key == "local-linked")
        .unwrap_or(false);
    let images = indexed_image_paths(&root);
    let mut count = 0;
    for image_path in images {
        let id = if is_local_linked {
            image_path
                .strip_prefix(&root)
                .map(|value| image_id_from_relative(&value.to_string_lossy().replace('\\', "/")))
                .unwrap_or_else(|_| format!("thumb-{count}"))
        } else {
            image_path
                .file_stem()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_else(|| format!("thumb-{count}"))
        };
        let target = paths.thumbnails.join(format!("{id}.png"));
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        let image = image::open(&image_path).map_err(|err| err.to_string())?;
        image
            .thumbnail(320, 240)
            .save(&target)
            .map_err(|err| err.to_string())?;
        count += 1;
    }
    Ok(count)
}

pub fn open_local_dataset(
    source_path: &str,
    dataset_type: &str,
) -> Result<domain::DatasetProject, String> {
    project_fs::ensure_workspace_dirs()?;
    let source = PathBuf::from(source_path);
    if !source.exists() || !source.is_dir() {
        return Err(format!("local dataset directory not found: {source_path}"));
    }
    let canonical = fs::canonicalize(&source).unwrap_or(source);
    let project_id = linked_project_id(&canonical);
    let paths = project_fs::ensure_workspace_project_dirs(&project_id)?;
    let format = if dataset_type == "voc-detect" {
        "voc-detect"
    } else {
        "yolo-detect"
    };
    let images = indexed_local_images(&canonical, format);
    let labels = local_labels_for_format(&canonical, format);
    let labels = if labels.is_empty() {
        demo_class_labels()
    } else {
        labels
    };
    let classes = classes_from_labels(labels);
    let name = canonical
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "本机数据集".to_string());
    let manifest = project_fs::ProjectManifest {
        id: project_id.clone(),
        name: format!("本机 {name}"),
        source_dataset_key: "local-linked".to_string(),
        format: format.to_string(),
        root_path: canonical.to_string_lossy().to_string(),
        created_at: now_unix_string(),
        class_count: classes.len() as u32,
        image_count: images.len() as u32,
    };

    project_fs::write_manifest_to_path(&manifest, &paths.manifest)?;
    storage::initialize_project_database(&paths.sqlite)?;
    storage::upsert_project_index(&paths.sqlite, &manifest, &images, &classes)?;
    storage::record_import(
        &paths.sqlite,
        &canonical.to_string_lossy(),
        "completed",
        &format!("已链接本机目录并索引 {} 张图片", images.len()),
    )?;
    domain::SampleRepository::new()
        .dataset_projects()
        .into_iter()
        .find(|project| project.id == project_id)
        .ok_or_else(|| format!("local project not found after indexing: {project_id}"))
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
    let is_classification =
        format == "image-classification" || demo_template == "demo-classification";
    fs::create_dir_all(&image_dir).map_err(|err| err.to_string())?;
    if !is_classification {
        fs::create_dir_all(&label_dir).map_err(|err| err.to_string())?;
    }

    for index in 1..=3 {
        let class_name = demo_class_labels()
            .get((index - 1) as usize)
            .cloned()
            .unwrap_or_else(|| "object".to_string());
        let image_path = if is_classification {
            let class_dir = image_dir.join(class_name);
            fs::create_dir_all(&class_dir).map_err(|err| err.to_string())?;
            class_dir.join(format!("demo_{index:03}.png"))
        } else {
            image_dir.join(format!("demo_{index:03}.png"))
        };
        write_demo_image(&image_path, index)?;
        if is_classification {
            continue;
        }
        let label_path = label_dir.join(format!("demo_{index:03}.txt"));
        let label = if format == "yolo-seg" || demo_template == "demo-polygon" {
            "0 0.20 0.20 0.72 0.18 0.82 0.70 0.24 0.76\n"
        } else {
            "0 0.50 0.52 0.46 0.42\n1 0.28 0.30 0.18 0.16\n"
        };
        fs::write(label_path, label).map_err(|err| err.to_string())?;
    }

    Ok(())
}

fn annotation_types_for_format(format: &str) -> Vec<String> {
    match format {
        "yolo-seg" => vec!["Polygon".to_string(), "BBox".to_string()],
        "image-classification" => vec!["Classification".to_string()],
        _ => vec!["BBox".to_string()],
    }
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

fn copy_dir_contents(source: &Path, target_root: &Path) -> Result<(), String> {
    for entry in WalkDir::new(source).into_iter().filter_map(Result::ok) {
        if entry.file_type().is_dir() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|err| err.to_string())?;
        let target = target_root.join(relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        fs::copy(entry.path(), target).map_err(|err| err.to_string())?;
    }
    Ok(())
}

fn unique_target_path(target_dir: &Path, file_name: &str) -> PathBuf {
    let original = target_dir.join(file_name);
    if !original.exists() {
        return original;
    }
    let path = Path::new(file_name);
    let stem = path
        .file_stem()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "image".to_string());
    let extension = path
        .extension()
        .map(|value| format!(".{}", value.to_string_lossy()))
        .unwrap_or_default();
    for index in 1.. {
        let candidate = target_dir.join(format!("{stem}-{index}{extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    original
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
                qa_status: String::new(),
                review_note: None,
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

fn indexed_local_images(root: &Path, format: &str) -> Vec<storage::StoredImage> {
    let mut images: Vec<_> = indexed_image_paths(root)
        .into_iter()
        .map(|path| {
            let (width, height) = image::image_dimensions(&path).unwrap_or((0, 0));
            let relative = path
                .strip_prefix(root)
                .map(|value| value.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| {
                    path.file_name()
                        .map(|value| value.to_string_lossy().to_string())
                        .unwrap_or_else(|| "image.jpg".to_string())
                });
            let id = image_id_from_relative(&relative);
            storage::StoredImage {
                id,
                file_name: relative,
                width,
                height,
                split: "local".to_string(),
                status: if local_image_has_annotation(root, &path, format) {
                    "已标注".to_string()
                } else {
                    "未标注".to_string()
                },
                qa_status: String::new(),
                review_note: None,
            }
        })
        .collect();
    images.sort_by(|left, right| left.file_name.cmp(&right.file_name));
    images
}

fn local_image_has_annotation(root: &Path, image_path: &Path, format: &str) -> bool {
    match format {
        "voc-detect" => image_path.with_extension("xml").exists(),
        "yolo-detect" => {
            yolo_label_path_for_local_image(root, image_path).is_some()
                || image_path.with_extension("txt").exists()
        }
        _ => false,
    }
}

fn yolo_label_path_for_local_image(root: &Path, image_path: &Path) -> Option<PathBuf> {
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
    label.exists().then_some(label)
}

fn local_labels_for_format(root: &Path, format: &str) -> Vec<String> {
    match format {
        "voc-detect" => indexed_voc_labels(root),
        "yolo-detect" => indexed_yolo_labels(root),
        _ => Vec::new(),
    }
}

fn classes_from_labels(labels: Vec<String>) -> Vec<storage::StoredClass> {
    labels
        .into_iter()
        .enumerate()
        .map(|(index, label)| storage::StoredClass {
            id: index as u32,
            label,
            color: class_color(index),
        })
        .collect()
}

fn indexed_voc_labels(root: &Path) -> Vec<String> {
    let mut labels = BTreeSet::new();
    for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file()
            || entry
                .path()
                .extension()
                .map(|extension| extension.to_string_lossy().to_ascii_lowercase() != "xml")
                .unwrap_or(true)
        {
            continue;
        }
        if let Ok(xml) = fs::read_to_string(entry.path()) {
            if let Ok(items) = voc::parse_voc_labels(&xml) {
                labels.extend(items);
            }
        }
    }
    labels.into_iter().collect()
}

fn indexed_voc_labels_from_files(paths: &[PathBuf]) -> Vec<String> {
    let mut labels = BTreeSet::new();
    for path in paths.iter().filter(|path| has_extension(path, "xml")) {
        if let Ok(xml) = fs::read_to_string(path) {
            if let Ok(items) = voc::parse_voc_labels(&xml) {
                labels.extend(items);
            }
        }
    }
    labels.into_iter().collect()
}

fn indexed_yolo_labels(root: &Path) -> Vec<String> {
    for candidate in ["classes.txt", "obj.names"] {
        let path = root.join(candidate);
        if let Ok(data) = fs::read_to_string(path) {
            let labels = label_lines(&data);
            if !labels.is_empty() {
                return labels;
            }
        }
    }

    let yaml_path = root.join("data.yaml");
    if let Ok(data) = fs::read_to_string(yaml_path) {
        let labels = parse_simple_yaml_names(&data);
        if !labels.is_empty() {
            return labels;
        }
    }

    let max_class_id = indexed_yolo_label_ids(root).into_iter().max();
    max_class_id
        .map(|max_id| (0..=max_id).map(|id| format!("class_{id}")).collect())
        .unwrap_or_default()
}

fn indexed_yolo_label_ids_from_files(paths: &[PathBuf]) -> Vec<u32> {
    paths
        .iter()
        .filter(|path| has_extension(path, "txt"))
        .filter_map(|path| fs::read_to_string(path).ok())
        .flat_map(|data| {
            data.lines()
                .filter_map(|line| line.split_whitespace().next()?.parse::<u32>().ok())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn label_lines(data: &str) -> Vec<String> {
    data.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn parse_simple_yaml_names(data: &str) -> Vec<String> {
    let Some((_, names)) = data.lines().find_map(|line| line.split_once("names:")) else {
        return Vec::new();
    };
    names
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|name| name.trim().trim_matches('"').trim_matches('\''))
        .filter(|name| !name.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn indexed_yolo_label_ids(root: &Path) -> Vec<u32> {
    WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_type().is_file()
                && entry
                    .path()
                    .extension()
                    .map(|extension| extension.to_string_lossy().eq_ignore_ascii_case("txt"))
                    .unwrap_or(false)
        })
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .flat_map(|data| {
            data.lines()
                .filter_map(|line| line.split_whitespace().next()?.parse::<u32>().ok())
                .collect::<Vec<_>>()
        })
        .collect()
}

pub fn image_id_from_relative(relative: &str) -> String {
    let without_extension = Path::new(relative)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/");
    without_extension
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

fn linked_project_id(source_path: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    source_path.to_string_lossy().hash(&mut hasher);
    let folder = source_path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "local".to_string());
    format!(
        "local-{}-{:x}",
        project_id_from_name(&folder, "dataset"),
        hasher.finish()
    )
}

fn collect_source_files(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for path in paths {
        if path.is_dir() {
            files.extend(
                WalkDir::new(path)
                    .into_iter()
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_type().is_file())
                    .map(|entry| entry.path().to_path_buf()),
            );
        } else if path.is_file() {
            files.push(path.clone());
        }
    }
    files
}

fn common_parent(paths: &[PathBuf]) -> Option<PathBuf> {
    let mut parents = paths
        .iter()
        .filter_map(|path| path.parent())
        .map(Path::to_path_buf);
    let mut common = parents.next()?;
    for parent in parents {
        while !parent.starts_with(&common) {
            if !common.pop() {
                return None;
            }
        }
    }
    Some(common)
}

fn detected_splits(paths: &[PathBuf]) -> BTreeSet<String> {
    paths
        .iter()
        .filter(|path| domain::is_image_path(path))
        .map(|path| split_for_path(path))
        .collect()
}

fn build_source_tree(paths: &[PathBuf], root: &Path) -> Vec<DataSourceTreeNode> {
    const MAX_CHILDREN: usize = 80;
    if paths.len() == 1 && paths[0].is_dir() {
        return vec![tree_node_for_path(&paths[0], root, 0, MAX_CHILDREN)];
    }
    paths
        .iter()
        .take(MAX_CHILDREN)
        .map(|path| tree_node_for_path(path, root, 0, MAX_CHILDREN))
        .collect()
}

fn tree_node_for_path(
    path: &Path,
    root: &Path,
    depth: usize,
    max_children: usize,
) -> DataSourceTreeNode {
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());
    let relative = path
        .strip_prefix(root)
        .map(|value| value.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().to_string());
    if !path.is_dir() || depth >= 3 {
        return DataSourceTreeNode {
            name,
            path: relative,
            kind: if path.is_dir() { "folder" } else { "file" }.to_string(),
            children: Vec::new(),
            truncated: path.is_dir() && depth >= 3,
        };
    }

    let mut children = fs::read_dir(path)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    children.sort_by(|left, right| {
        let left_is_dir = left.is_dir();
        let right_is_dir = right.is_dir();
        right_is_dir
            .cmp(&left_is_dir)
            .then_with(|| left.file_name().cmp(&right.file_name()))
    });
    let truncated = children.len() > max_children;
    let children = children
        .into_iter()
        .take(max_children)
        .map(|child| tree_node_for_path(&child, root, depth + 1, max_children))
        .collect();

    DataSourceTreeNode {
        name,
        path: relative,
        kind: "folder".to_string(),
        children,
        truncated,
    }
}

fn has_extension(path: &Path, extension: &str) -> bool {
    path.extension()
        .map(|value| value.to_string_lossy().eq_ignore_ascii_case(extension))
        .unwrap_or(false)
}

fn path_contains_segment(path: &Path, segment: &str) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(segment)
    })
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
        let paths = project_fs::test_project_paths(&source.key);
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

    #[test]
    fn creates_demo_classification_dataset_project_with_class_directories() {
        let project_id = project_id_from_name("Classification Unit", "demo-classification");
        let _ = fs::remove_dir_all(project_fs::project_paths(&project_id).root);
        let project = create_dataset_project(
            "Classification Unit",
            "image-classification",
            "demo-classification",
        )
        .unwrap();
        let paths = project_fs::project_paths(&project.id);

        assert_eq!(project.annotation_types, vec!["Classification".to_string()]);
        assert!(project
            .tags
            .contains(&"format: image-classification".to_string()));
        assert!(paths
            .raw
            .join("images")
            .join("train")
            .join("object")
            .join("demo_001.png")
            .exists());
        assert!(!paths
            .raw
            .join("labels")
            .join("train")
            .join("demo_001.txt")
            .exists());
        assert_eq!(storage::read_images(&paths.sqlite, None).unwrap().len(), 3);

        let _ = fs::remove_dir_all(paths.root);
    }

    #[test]
    fn opens_local_pascal_voc_folder_without_copying_images() {
        let source_root = std::env::temp_dir().join("image_annotation_voc_open_test");
        let _ = fs::remove_dir_all(&source_root);
        fs::create_dir_all(&source_root).unwrap();
        let image_path = source_root.join("sample.png");
        write_demo_image(&image_path, 1).unwrap();
        fs::write(
            source_root.join("sample.xml"),
            r#"
            <annotation>
              <filename>sample.png</filename>
              <path>sample.png</path>
              <size><width>640</width><height>420</height><depth>3</depth></size>
              <object>
                <name>毛刺</name>
                <bndbox><xmin>10</xmin><ymin>20</ymin><xmax>40</xmax><ymax>60</ymax></bndbox>
              </object>
            </annotation>
            "#,
        )
        .unwrap();
        let stale_project_id = linked_project_id(&fs::canonicalize(&source_root).unwrap());
        let _ = fs::remove_dir_all(project_fs::project_paths(&stale_project_id).root);

        let project = open_local_dataset(&source_root.to_string_lossy(), "voc-detect").unwrap();
        let paths = project_fs::project_paths(&project.id);
        let stored = storage::read_images(&paths.sqlite, None).unwrap();
        let imports = storage::list_import_records(&paths.sqlite).unwrap();

        assert_eq!(project.image_count, 1);
        assert_eq!(project.class_count, 1);
        assert_eq!(
            project.description,
            format!(
                "本机目录 {}",
                fs::canonicalize(&source_root).unwrap().to_string_lossy()
            )
        );
        assert!(project.tags.contains(&"source: local-linked".to_string()));
        assert_eq!(stored[0].file_name, "sample.png");
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].status, "completed");
        assert_eq!(
            imports[0].source_path,
            fs::canonicalize(&source_root).unwrap().to_string_lossy()
        );
        assert!(imports[0].message.contains("1 张图片"));
        assert_eq!(
            project_fs::read_manifest(&project.id).unwrap().root_path,
            fs::canonicalize(&source_root).unwrap().to_string_lossy()
        );
        assert!(paths.raw.read_dir().unwrap().next().is_none());

        let repository = domain::SampleRepository::new();
        let state = repository.image_annotation_state(&project.id, "sample");
        assert_eq!(state.objects[0].label, "毛刺");
        let mut edited = state.objects;
        edited[0].bbox.as_mut().unwrap().x = 25.0;
        repository
            .save_image_annotations_with_revision(&project.id, "sample", None, edited)
            .unwrap();
        let xml = fs::read_to_string(source_root.join("sample.xml")).unwrap();
        assert!(xml.contains("<xmin>25</xmin>"));

        let _ = fs::remove_dir_all(paths.root);
        let _ = fs::remove_dir_all(source_root);
    }

    #[test]
    fn opens_local_yolo_folder_with_existing_classes_and_labels() {
        let source_root = std::env::temp_dir().join("image_annotation_yolo_open_test");
        let _ = fs::remove_dir_all(&source_root);
        fs::create_dir_all(source_root.join("images").join("train")).unwrap();
        fs::create_dir_all(source_root.join("labels").join("train")).unwrap();
        let image_path = source_root.join("images").join("train").join("sample.png");
        write_demo_image(&image_path, 1).unwrap();
        fs::write(source_root.join("classes.txt"), "defect\nscratch\n").unwrap();
        fs::write(
            source_root.join("labels").join("train").join("sample.txt"),
            "1 0.500000 0.500000 0.250000 0.200000\n",
        )
        .unwrap();
        let stale_project_id = linked_project_id(&fs::canonicalize(&source_root).unwrap());
        let _ = fs::remove_dir_all(project_fs::project_paths(&stale_project_id).root);

        let project = open_local_dataset(&source_root.to_string_lossy(), "yolo-detect").unwrap();
        let paths = project_fs::project_paths(&project.id);
        let repository = domain::SampleRepository::new();
        let state = repository.image_annotation_state(&project.id, "images_train_sample");

        assert_eq!(project.class_count, 2);
        assert_eq!(state.objects[0].label, "scratch");
        assert_eq!(state.objects[0].class_id, 1);

        let _ = fs::remove_dir_all(paths.root);
        let _ = fs::remove_dir_all(source_root);
    }

    #[test]
    fn rescans_local_yolo_folder_against_source_directory() {
        let source_root = std::env::temp_dir().join("image_annotation_yolo_rescan_test");
        let _ = fs::remove_dir_all(&source_root);
        fs::create_dir_all(source_root.join("images").join("train")).unwrap();
        fs::create_dir_all(source_root.join("labels").join("train")).unwrap();
        let image_path = source_root.join("images").join("train").join("sample.png");
        write_demo_image(&image_path, 1).unwrap();
        fs::write(source_root.join("classes.txt"), "defect\nscratch\n").unwrap();
        fs::write(
            source_root.join("labels").join("train").join("sample.txt"),
            "1 0.500000 0.500000 0.250000 0.200000\n",
        )
        .unwrap();
        let stale_project_id = linked_project_id(&fs::canonicalize(&source_root).unwrap());
        let _ = fs::remove_dir_all(project_fs::project_paths(&stale_project_id).root);

        let project = open_local_dataset(&source_root.to_string_lossy(), "yolo-detect").unwrap();
        let paths = project_fs::project_paths(&project.id);
        let stored = storage::read_images(&paths.sqlite, None).unwrap();
        assert_eq!(stored[0].status, "已标注");

        write_demo_image(
            &source_root.join("images").join("train").join("extra.png"),
            2,
        )
        .unwrap();
        fs::write(
            source_root.join("labels").join("train").join("extra.txt"),
            "0 0.500000 0.500000 0.500000 0.500000\n",
        )
        .unwrap();

        let rescanned = rescan_project_assets(&project.id).unwrap();
        let stored = storage::read_images(&paths.sqlite, None).unwrap();
        let extra = stored
            .iter()
            .find(|image| image.id == "images_train_extra")
            .expect("new local image indexed after rescan");
        let repository = domain::SampleRepository::new();
        let extra_state = repository.image_annotation_state(&project.id, "images_train_extra");

        assert_eq!(rescanned.image_count, 2);
        assert_eq!(stored.len(), 2);
        assert_eq!(extra.file_name, "images/train/extra.png");
        assert_eq!(extra.status, "已标注");
        assert_eq!(extra_state.objects[0].label, "defect");
        assert!(paths.raw.read_dir().unwrap().next().is_none());

        let thumbnail_count = generate_project_thumbnails(&project.id).unwrap();
        assert_eq!(thumbnail_count, 2);
        assert!(paths.thumbnails.join("images_train_extra.png").exists());

        let _ = fs::remove_dir_all(paths.root);
        let _ = fs::remove_dir_all(source_root);
    }

    #[test]
    fn saves_local_yolo_annotations_back_to_source_labels_tree() {
        let source_root = std::env::temp_dir().join("image_annotation_yolo_save_test");
        let _ = fs::remove_dir_all(&source_root);
        fs::create_dir_all(source_root.join("images").join("train")).unwrap();
        fs::create_dir_all(source_root.join("labels").join("train")).unwrap();
        let image_path = source_root.join("images").join("train").join("sample.png");
        write_demo_image(&image_path, 1).unwrap();
        fs::write(source_root.join("classes.txt"), "defect\nscratch\n").unwrap();
        let stale_project_id = linked_project_id(&fs::canonicalize(&source_root).unwrap());
        let _ = fs::remove_dir_all(project_fs::project_paths(&stale_project_id).root);

        let project = open_local_dataset(&source_root.to_string_lossy(), "yolo-detect").unwrap();
        let paths = project_fs::project_paths(&project.id);
        let repository = domain::SampleRepository::new();
        repository
            .save_image_annotations_with_revision(
                &project.id,
                "images_train_sample",
                None,
                vec![domain::AnnotationObject::bbox(
                    "ann-1".to_string(),
                    1,
                    "scratch".to_string(),
                    domain::BBox {
                        x: 160.0,
                        y: 84.0,
                        width: 320.0,
                        height: 210.0,
                    },
                )],
            )
            .unwrap();

        let txt = fs::read_to_string(source_root.join("labels").join("train").join("sample.txt"))
            .unwrap();
        assert_eq!(txt, "1 0.500000 0.450000 0.500000 0.500000\n");

        let _ = fs::remove_dir_all(paths.root);
        let _ = fs::remove_dir_all(source_root);
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

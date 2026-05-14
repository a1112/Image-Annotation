use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectManifest {
    pub id: String,
    pub name: String,
    pub source_dataset_key: String,
    pub format: String,
    pub root_path: String,
    pub created_at: String,
    pub class_count: u32,
    pub image_count: u32,
}

#[derive(Debug, Clone)]
pub struct ProjectPaths {
    pub root: PathBuf,
    pub raw: PathBuf,
    pub annotations: PathBuf,
    pub exports: PathBuf,
    pub thumbnails: PathBuf,
    pub sqlite: PathBuf,
    pub manifest: PathBuf,
}

pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")))
        .to_path_buf()
}

pub fn test_data_root() -> PathBuf {
    workspace_root().join("data").join("test_data")
}

pub fn downloads_dir() -> PathBuf {
    test_data_root().join("cache").join("downloads")
}

pub fn projects_dir() -> PathBuf {
    test_data_root().join("projects")
}

pub fn project_paths(project_id: &str) -> ProjectPaths {
    let root = projects_dir().join(project_id);
    ProjectPaths {
        raw: root.join("raw"),
        annotations: root.join("annotations").join("native"),
        exports: root.join("exports"),
        thumbnails: root.join("thumbnails"),
        sqlite: root.join("project.sqlite"),
        manifest: root.join("project.json"),
        root,
    }
}

pub fn ensure_test_data_dirs() -> Result<(), String> {
    fs::create_dir_all(downloads_dir()).map_err(|err| err.to_string())?;
    fs::create_dir_all(projects_dir()).map_err(|err| err.to_string())?;
    let registry = test_data_root().join("registry.json");
    if !registry.exists() {
        fs::write(&registry, "[]\n").map_err(|err| err.to_string())?;
    }
    Ok(())
}

pub fn ensure_project_dirs(project_id: &str) -> Result<ProjectPaths, String> {
    let paths = project_paths(project_id);
    for path in [
        &paths.root,
        &paths.raw,
        &paths.annotations,
        &paths.exports,
        &paths.thumbnails,
    ] {
        fs::create_dir_all(path).map_err(|err| err.to_string())?;
    }
    Ok(paths)
}

pub fn read_manifest(project_id: &str) -> Option<ProjectManifest> {
    let path = project_paths(project_id).manifest;
    let data = fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn write_manifest(manifest: &ProjectManifest) -> Result<(), String> {
    let path = project_paths(&manifest.id).manifest;
    let data = serde_json::to_string_pretty(manifest).map_err(|err| err.to_string())?;
    fs::write(path, data).map_err(|err| err.to_string())
}

pub fn list_project_manifests() -> Vec<ProjectManifest> {
    let Ok(entries) = fs::read_dir(projects_dir()) else {
        return Vec::new();
    };

    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let project_id = entry.file_name().to_string_lossy().to_string();
            read_manifest(&project_id)
        })
        .collect()
}

pub fn safe_extract_path(root: &Path, entry_name: &str) -> Option<PathBuf> {
    let entry_path = Path::new(entry_name);
    if entry_path.is_absolute() {
        return None;
    }

    let mut out = root.to_path_buf();
    for component in entry_path.components() {
        match component {
            Component::Normal(value) => out.push(value),
            Component::CurDir => {}
            _ => return None,
        }
    }

    Some(out)
}

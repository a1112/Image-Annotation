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
    pub imports: PathBuf,
    pub snapshots: PathBuf,
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

pub fn workspace_data_root() -> PathBuf {
    workspace_root()
        .join("data")
        .join("workspaces")
        .join("default")
}

pub fn downloads_dir() -> PathBuf {
    test_data_root().join("cache").join("downloads")
}

pub fn projects_dir() -> PathBuf {
    test_data_root().join("projects")
}

pub fn workspace_projects_dir() -> PathBuf {
    workspace_data_root().join("projects")
}

pub fn project_paths(project_id: &str) -> ProjectPaths {
    let workspace = workspace_project_paths(project_id);
    if workspace.manifest.exists() || workspace.root.exists() {
        return workspace;
    }

    let test = test_project_paths(project_id);
    if test.manifest.exists() || test.root.exists() {
        return test;
    }

    workspace
}

pub fn test_project_paths(project_id: &str) -> ProjectPaths {
    let root = projects_dir().join(project_id);
    ProjectPaths {
        raw: root.join("raw"),
        annotations: root.join("annotations").join("native"),
        exports: root.join("exports"),
        imports: root.join("imports"),
        snapshots: root.join("snapshots"),
        thumbnails: root.join("thumbnails"),
        sqlite: root.join("project.sqlite"),
        manifest: root.join("project.json"),
        root,
    }
}

pub fn workspace_project_paths(project_id: &str) -> ProjectPaths {
    let root = workspace_projects_dir().join(project_id);
    ProjectPaths {
        raw: root.join("assets").join("original"),
        annotations: root.join("annotations").join("native"),
        exports: root.join("exports"),
        imports: root.join("imports"),
        snapshots: root.join("snapshots"),
        thumbnails: root.join("assets").join("thumbnails"),
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

pub fn ensure_workspace_dirs() -> Result<(), String> {
    fs::create_dir_all(workspace_projects_dir()).map_err(|err| err.to_string())?;
    let registry = workspace_data_root().join("registry.json");
    if !registry.exists() {
        fs::write(&registry, "[]\n").map_err(|err| err.to_string())?;
    }
    Ok(())
}

pub fn ensure_project_dirs(project_id: &str) -> Result<ProjectPaths, String> {
    ensure_workspace_dirs()?;
    ensure_dirs(project_paths(project_id))
}

pub fn ensure_workspace_project_dirs(project_id: &str) -> Result<ProjectPaths, String> {
    ensure_workspace_dirs()?;
    ensure_dirs(workspace_project_paths(project_id))
}

pub fn ensure_test_project_dirs(project_id: &str) -> Result<ProjectPaths, String> {
    ensure_test_data_dirs()?;
    ensure_dirs(test_project_paths(project_id))
}

fn ensure_dirs(paths: ProjectPaths) -> Result<ProjectPaths, String> {
    for path in [
        &paths.root,
        &paths.raw,
        &paths.annotations,
        &paths.exports,
        &paths.imports,
        &paths.snapshots,
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let data = serde_json::to_string_pretty(manifest).map_err(|err| err.to_string())?;
    fs::write(path, data).map_err(|err| err.to_string())
}

pub fn write_manifest_to_path(manifest: &ProjectManifest, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let data = serde_json::to_string_pretty(manifest).map_err(|err| err.to_string())?;
    fs::write(path, data).map_err(|err| err.to_string())
}

pub fn list_project_manifests() -> Vec<ProjectManifest> {
    let mut manifests = Vec::new();
    manifests.extend(list_project_manifests_from(workspace_projects_dir()));
    for manifest in list_project_manifests_from(projects_dir()) {
        if !manifests
            .iter()
            .any(|item: &ProjectManifest| item.id == manifest.id)
        {
            manifests.push(manifest);
        }
    }
    manifests
}

fn list_project_manifests_from(root: PathBuf) -> Vec<ProjectManifest> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };

    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let manifest_path = entry.path().join("project.json");
            let data = fs::read_to_string(manifest_path).ok()?;
            serde_json::from_str(&data).ok()
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

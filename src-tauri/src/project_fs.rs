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

// The embedded repository path is used only for debug development and as a
// read-only legacy-location hint. Packaged writes require startup selection.
pub fn development_workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")))
        .to_path_buf()
}

static STORAGE_ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub fn initialize_storage(root: PathBuf) -> Result<(), String> {
    STORAGE_ROOT
        .set(root)
        .map_err(|_| "数据目录已初始化，不能在活动任务期间切换。".to_string())
}

pub fn initialize_development_storage() -> Result<(), String> {
    #[cfg(all(dev, debug_assertions))]
    {
        initialize_storage(crate::runtime_storage::prepare_new(
            &development_workspace_root(),
        )?)
    }
    #[cfg(not(all(dev, debug_assertions)))]
    {
        Err("发行版独立后端需要明确的数据目录：使用 --data-root <工作目录>，或启动桌面应用选择数据目录。".to_string())
    }
}

#[cfg(not(test))]
pub fn workspace_root() -> PathBuf {
    // Debug callers (including cargo integration tests) preserve the established
    // development layout. Release callers must initialize before starting work.
    #[cfg(all(dev, debug_assertions))]
    {
        STORAGE_ROOT
            .get()
            .cloned()
            .unwrap_or_else(development_workspace_root)
    }
    #[cfg(not(all(dev, debug_assertions)))]
    {
        STORAGE_ROOT
            .get()
            .expect("storage must be selected before project access")
            .clone()
    }
}

#[cfg(test)]
pub fn workspace_root() -> PathBuf {
    use std::{
        collections::hash_map::DefaultHasher,
        hash::{Hash, Hasher},
    };

    let mut test_thread = DefaultHasher::new();
    std::thread::current()
        .name()
        .unwrap_or("unnamed-test-thread")
        .hash(&mut test_thread);
    std::env::temp_dir().join(format!(
        "image-annotation-tests-{}-{:016x}",
        std::process::id(),
        test_thread.finish()
    ))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_root_isolated_by_test_thread() {
        let first = std::thread::Builder::new()
            .name("workspace-root-test-a".to_string())
            .spawn(workspace_root)
            .unwrap()
            .join()
            .unwrap();
        let second = std::thread::Builder::new()
            .name("workspace-root-test-b".to_string())
            .spawn(workspace_root)
            .unwrap()
            .join()
            .unwrap();

        assert_ne!(first, second);
    }
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

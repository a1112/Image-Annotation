use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Component, Path, PathBuf},
    sync::{OnceLock, RwLock},
};

/// All filesystem access is rooted beneath this value.  Desktop builds keep
/// the historical repository-adjacent layout, while mobile builds resolve the
/// root from the application data directory (or the explicit override used by
/// integration tests).  Keeping the decision in one place prevents a mobile
/// bundle from accidentally writing beside its read-only executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageRoots {
    pub app_data: PathBuf,
    pub workspace: PathBuf,
    pub test_data: PathBuf,
    pub cache: PathBuf,
}

static STORAGE_ROOTS: OnceLock<RwLock<Option<StorageRoots>>> = OnceLock::new();

fn configured_storage_roots() -> &'static RwLock<Option<StorageRoots>> {
    STORAGE_ROOTS.get_or_init(|| RwLock::new(None))
}

/// Override the default roots before opening the repository.
///
/// This is intentionally process-scoped: a Tauri app owns one storage root,
/// and tests can inject an isolated temporary directory without changing the
/// public domain APIs that still accept project IDs.
pub fn configure_storage_roots(app_data: impl Into<PathBuf>) -> Result<(), String> {
    let app_data = std::env::var_os("IMAGE_ANNOTATION_DATA_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| app_data.into());
    if app_data.as_os_str().is_empty() {
        return Err("application data directory is empty".to_string());
    }
    let roots = StorageRoots {
        workspace: app_data.join("workspaces").join("default"),
        test_data: app_data.join("test_data"),
        cache: app_data.join("cache"),
        app_data,
    };
    let mut configured = configured_storage_roots()
        .write()
        .map_err(|_| "storage root lock is poisoned".to_string())?;
    *configured = Some(roots);
    Ok(())
}

pub fn storage_roots() -> StorageRoots {
    if let Ok(configured) = configured_storage_roots().read() {
        if let Some(roots) = configured.clone() {
            return roots;
        }
    }
    let app_data = default_app_data_root();
    StorageRoots {
        workspace: app_data.join("workspaces").join("default"),
        test_data: app_data.join("test_data"),
        cache: app_data.join("cache"),
        app_data,
    }
}

fn default_app_data_root() -> PathBuf {
    if let Some(path) = std::env::var_os("IMAGE_ANNOTATION_DATA_DIR") {
        return PathBuf::from(path);
    }

    #[cfg(mobile)]
    {
        // Tauri's mobile WebView exposes a writable HOME inside the app
        // sandbox.  Prefer it over current_dir, which is commonly read-only.
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join("Image Annotation");
        }
        return PathBuf::from("Image Annotation");
    }

    workspace_root()
}

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

#[cfg(not(test))]
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")))
        .to_path_buf()
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
    #[cfg(mobile)]
    {
        return storage_roots().test_data;
    }
    if std::env::var_os("IMAGE_ANNOTATION_DATA_DIR").is_some_and(|value| !value.is_empty()) {
        return storage_roots().test_data;
    }
    workspace_root().join("data").join("test_data")
}

pub fn workspace_data_root() -> PathBuf {
    #[cfg(mobile)]
    {
        return storage_roots().workspace;
    }
    if std::env::var_os("IMAGE_ANNOTATION_DATA_DIR").is_some_and(|value| !value.is_empty()) {
        return storage_roots().workspace;
    }
    workspace_root()
        .join("data")
        .join("workspaces")
        .join("default")
}

pub fn downloads_dir() -> PathBuf {
    #[cfg(mobile)]
    {
        return storage_roots().cache.join("downloads");
    }
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

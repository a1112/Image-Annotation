use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    sync::OnceLock,
};

static WORKSPACE_DATA_ROOT: OnceLock<PathBuf> = OnceLock::new();

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

pub fn resolve_workspace_data_root(
    explicit: Option<PathBuf>,
    environment: Option<PathBuf>,
    checkout_root: &Path,
) -> PathBuf {
    explicit
        .or_else(|| environment.filter(|path| !path.as_os_str().is_empty()))
        .unwrap_or_else(|| {
            checkout_root
                .join("data")
                .join("workspaces")
                .join("default")
        })
}

pub fn workspace_data_root_from(configured: Option<PathBuf>, checkout_root: &Path) -> PathBuf {
    resolve_workspace_data_root(configured, None, checkout_root)
}

pub fn configure_workspace_data_root(path: PathBuf) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("workspace data root must not be empty".to_string());
    }

    WORKSPACE_DATA_ROOT
        .set(path)
        .map_err(|_| "workspace data root is already configured".to_string())
}

pub fn workspace_data_root() -> PathBuf {
    resolve_workspace_data_root(
        WORKSPACE_DATA_ROOT.get().cloned(),
        std::env::var_os("IMAGE_ANNOTATION_DATA_DIR").map(PathBuf::from),
        &workspace_root(),
    )
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
    workspace_project_paths_from(&workspace_data_root(), project_id)
}

pub fn workspace_project_paths_from(data_root: &Path, project_id: &str) -> ProjectPaths {
    let root = data_root.join("projects").join(project_id);
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
    ensure_workspace_dirs_from(&workspace_data_root())
}

pub fn ensure_workspace_dirs_from(data_root: &Path) -> Result<(), String> {
    fs::create_dir_all(data_root.join("projects")).map_err(|err| err.to_string())?;
    let registry = data_root.join("registry.json");
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

pub fn ensure_workspace_project_dirs_from(
    data_root: &Path,
    project_id: &str,
) -> Result<ProjectPaths, String> {
    ensure_workspace_dirs_from(data_root)?;
    ensure_dirs(workspace_project_paths_from(data_root, project_id))
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
    write_manifest_to_path(manifest, &path)
}

pub fn write_manifest_to_path(manifest: &ProjectManifest, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let data = serde_json::to_string_pretty(manifest).map_err(|err| err.to_string())?;
    let temporary_path = manifest_temporary_path(path);
    let backup_path = manifest_backup_path(path);
    validate_manifest_artifacts(path, &temporary_path, &backup_path)?;
    remove_file_if_present(&temporary_path)?;
    let mut temporary = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary_path)
        .map_err(|err| err.to_string())?;
    temporary
        .write_all(data.as_bytes())
        .and_then(|()| temporary.sync_all())
        .map_err(|err| err.to_string())?;
    drop(temporary);

    remove_file_if_present(&backup_path)?;
    let had_target = path.exists();
    if had_target {
        fs::rename(path, &backup_path).map_err(|err| err.to_string())?;
    }
    if let Err(error) = fs::rename(&temporary_path, path) {
        if had_target {
            let _ = remove_file_if_present(path);
            if let Err(restore_error) = fs::rename(&backup_path, path) {
                return Err(format!(
                    "manifest replacement failed; backup restoration failed: {restore_error}"
                ));
            }
        }
        return Err(error.to_string());
    }
    remove_file_if_present(&backup_path)
}

pub fn recover_manifest_backup(path: &Path) -> Result<bool, String> {
    let temporary_path = manifest_temporary_path(path);
    let backup_path = manifest_backup_path(path);
    validate_manifest_artifacts(path, &temporary_path, &backup_path)?;
    if read_manifest_from_path(path).is_some() {
        remove_file_if_present(&temporary_path)?;
        remove_file_if_present(&backup_path)?;
        return Ok(false);
    }
    if read_manifest_from_path(&backup_path).is_none() {
        return Ok(false);
    }

    remove_file_if_present(path)?;
    fs::rename(&backup_path, path).map_err(|err| err.to_string())?;
    remove_file_if_present(&temporary_path)?;
    Ok(true)
}

fn read_manifest_from_path(path: &Path) -> Option<ProjectManifest> {
    let data = fs::read(path).ok()?;
    serde_json::from_slice(&data).ok()
}

fn manifest_temporary_path(path: &Path) -> PathBuf {
    path.with_extension("json.tmp")
}

fn manifest_backup_path(path: &Path) -> PathBuf {
    path.with_extension("json.bak")
}

fn validate_manifest_artifacts(
    manifest: &Path,
    temporary: &Path,
    backup: &Path,
) -> Result<(), String> {
    let parent = manifest
        .parent()
        .ok_or_else(|| "manifest path has no parent directory".to_string())?;
    let canonical_parent = fs::canonicalize(parent).map_err(|error| error.to_string())?;
    for path in [manifest, temporary, backup] {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        if is_symlink_or_reparse(&metadata) || !metadata.is_file() {
            return Err("manifest artifact is not a regular file".to_string());
        }
        let canonical = fs::canonicalize(path).map_err(|error| error.to_string())?;
        if canonical.parent() != Some(canonical_parent.as_path()) {
            return Err("manifest artifact is outside its project directory".to_string());
        }
    }
    Ok(())
}

fn is_symlink_or_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn remove_file_if_present(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

pub fn list_project_manifests() -> Vec<ProjectManifest> {
    let mut manifests = list_workspace_project_manifests();
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

pub fn list_workspace_project_manifests() -> Vec<ProjectManifest> {
    list_project_manifests_from(workspace_projects_dir())
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

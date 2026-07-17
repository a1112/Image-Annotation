use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::Serialize;

use crate::{
    datasets,
    domain::{DatasetProject, SampleRepository},
    project_fs,
};

use super::{
    config::ServerConfig,
    error::ServerBuildError,
    storage::{AuditEntry, ServerStorage},
    Role,
};

const MAX_PROJECT_NAME_CHARS: usize = 128;
const MAX_DESCRIPTION_CHARS: usize = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ServiceError {
    Validation,
    NotFound,
    Conflict,
    Storage,
}

impl ServiceError {
    pub(super) const fn storage() -> Self {
        Self::Storage
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct ProjectLifecycleResult {
    project_id: String,
    status: &'static str,
}

#[derive(Clone)]
pub(super) struct RemoteSampleService {
    data_dir: Arc<PathBuf>,
    projects_dir: Arc<PathBuf>,
    trash_projects_dir: Arc<PathBuf>,
    repository: Arc<SampleRepository>,
    storage: ServerStorage,
}

impl RemoteSampleService {
    pub(super) fn initialize(config: &ServerConfig) -> Result<Self, ServerBuildError> {
        fs::create_dir_all(&config.data_dir)
            .map_err(|_| ServerBuildError::initialization_failed())?;
        let data_dir = fs::canonicalize(&config.data_dir)
            .map_err(|_| ServerBuildError::initialization_failed())?;

        match project_fs::configure_workspace_data_root(config.data_dir.clone()) {
            Ok(()) => {}
            Err(_) => {
                let configured_root = canonical_existing(&project_fs::workspace_data_root())
                    .map_err(|_| ServerBuildError::initialization_failed())?;
                if configured_root != data_dir {
                    return Err(ServerBuildError::data_root_conflict());
                }
            }
        }

        let projects_dir = data_dir.join("projects");
        let trash_projects_dir = data_dir.join("trash").join("projects");
        fs::create_dir_all(&projects_dir).map_err(|_| ServerBuildError::initialization_failed())?;
        fs::create_dir_all(&trash_projects_dir)
            .map_err(|_| ServerBuildError::initialization_failed())?;
        let storage = ServerStorage::initialize(&data_dir)
            .map_err(|_| ServerBuildError::initialization_failed())?;

        Ok(Self {
            data_dir: Arc::new(data_dir),
            projects_dir: Arc::new(projects_dir),
            trash_projects_dir: Arc::new(trash_projects_dir),
            repository: Arc::new(SampleRepository::new()),
            storage,
        })
    }

    pub(super) fn list_projects(&self) -> Result<Vec<DatasetProject>, ServiceError> {
        self.ensure_configured_root()?;
        let active_ids = self.active_project_ids()?;
        let mut projects = self.repository.workspace_dataset_projects();
        projects.retain(|project| active_ids.contains(&project.id));
        for project in &mut projects {
            self.apply_description(project)?;
        }
        Ok(projects)
    }

    pub(super) fn get_project(&self, project_id: &str) -> Result<DatasetProject, ServiceError> {
        self.ensure_configured_root()?;
        validate_project_id(project_id)?;
        let active_dir = self.active_project_dir(project_id);
        if !active_dir.is_dir() {
            return Err(ServiceError::NotFound);
        }

        let mut project = self
            .repository
            .workspace_dataset_projects()
            .into_iter()
            .find(|project| project.id == project_id)
            .ok_or(ServiceError::Storage)?;
        self.apply_description(&mut project)?;
        Ok(project)
    }

    pub(super) fn create_project(
        &self,
        name: &str,
        dataset_type: &str,
        demo_template: &str,
        request_id: &str,
        role: Role,
    ) -> Result<DatasetProject, ServiceError> {
        self.ensure_configured_root()?;
        let name = validate_project_name(name)?;
        validate_dataset_type(dataset_type)?;
        validate_demo_template(demo_template)?;
        let project_id = datasets::project_id_from_name(name, demo_template);
        validate_project_id(&project_id)?;

        let active_dir = self.active_project_dir(&project_id);
        let trash_dir = self.trashed_project_dir(&project_id);
        if active_dir.exists()
            || trash_dir.exists()
            || self
                .storage
                .is_trashed(&project_id)
                .map_err(storage_failure)?
        {
            return Err(ServiceError::Conflict);
        }

        let project = datasets::create_dataset_project(name, dataset_type, demo_template)
            .map_err(storage_failure)?;
        self.validate_created_project_root(&project_id)?;
        self.storage
            .record_audit(audit(
                request_id,
                role,
                "create_project",
                Some(&project_id),
                "project created",
            ))
            .map_err(storage_failure)?;
        Ok(project)
    }

    pub(super) fn update_project(
        &self,
        project_id: &str,
        name: Option<&str>,
        description: Option<&str>,
        request_id: &str,
        role: Role,
    ) -> Result<DatasetProject, ServiceError> {
        self.ensure_configured_root()?;
        validate_project_id(project_id)?;
        if name.is_none() && description.is_none() {
            return Err(ServiceError::Validation);
        }
        let name = name.map(validate_project_name).transpose()?;
        let description = description.map(validate_description).transpose()?;
        let active_dir = self.active_project_dir(project_id);
        if !active_dir.is_dir() {
            return Err(ServiceError::NotFound);
        }

        if let Some(name) = name {
            self.persist_project_name(project_id, name)?;
        }
        self.storage
            .update_metadata_and_audit(
                project_id,
                description,
                audit(
                    request_id,
                    role,
                    "rename_project",
                    Some(project_id),
                    "project metadata updated",
                ),
            )
            .map_err(storage_failure)?;
        self.get_project(project_id)
    }

    pub(super) fn delete_project(
        &self,
        project_id: &str,
        request_id: &str,
        role: Role,
    ) -> Result<ProjectLifecycleResult, ServiceError> {
        self.ensure_configured_root()?;
        validate_project_id(project_id)?;
        let active_dir = self.active_project_dir(project_id);
        let trash_dir = self.trashed_project_dir(project_id);
        let tombstoned = self
            .storage
            .is_trashed(project_id)
            .map_err(storage_failure)?;

        if active_dir.exists() {
            if !active_dir.is_dir() || tombstoned || trash_dir.exists() {
                return Err(ServiceError::Conflict);
            }
            fs::rename(&active_dir, &trash_dir).map_err(storage_failure)?;
            if let Err(error) = self.storage.record_trash(
                project_id,
                audit(
                    request_id,
                    role,
                    "delete_project",
                    Some(project_id),
                    "project moved to trash",
                ),
            ) {
                if let Err(rollback_error) = fs::rename(&trash_dir, &active_dir) {
                    tracing::error!(%rollback_error, "failed to roll back project trash move");
                }
                return Err(storage_failure(error));
            }
            return Ok(trashed_result(project_id));
        }

        if tombstoned {
            if !trash_dir.is_dir() {
                return Err(ServiceError::Storage);
            }
            self.storage
                .record_audit(audit(
                    request_id,
                    role,
                    "delete_project",
                    Some(project_id),
                    "project already in trash",
                ))
                .map_err(storage_failure)?;
            return Ok(trashed_result(project_id));
        }

        if trash_dir.exists() {
            return Err(ServiceError::Storage);
        }
        Err(ServiceError::NotFound)
    }

    pub(super) fn restore_project(
        &self,
        project_id: &str,
        request_id: &str,
        role: Role,
    ) -> Result<DatasetProject, ServiceError> {
        self.ensure_configured_root()?;
        validate_project_id(project_id)?;
        let active_dir = self.active_project_dir(project_id);
        let trash_dir = self.trashed_project_dir(project_id);
        let tombstoned = self
            .storage
            .is_trashed(project_id)
            .map_err(storage_failure)?;

        if active_dir.exists() {
            if !active_dir.is_dir() || tombstoned || trash_dir.exists() {
                return Err(ServiceError::Conflict);
            }
            self.storage
                .record_audit(audit(
                    request_id,
                    role,
                    "restore_project",
                    Some(project_id),
                    "project already active",
                ))
                .map_err(storage_failure)?;
            return self.get_project(project_id);
        }

        if !tombstoned {
            return if trash_dir.exists() {
                Err(ServiceError::Storage)
            } else {
                Err(ServiceError::NotFound)
            };
        }
        if !trash_dir.is_dir() {
            return Err(ServiceError::Storage);
        }

        fs::rename(&trash_dir, &active_dir).map_err(storage_failure)?;
        if let Err(error) = self.storage.clear_trash(
            project_id,
            audit(
                request_id,
                role,
                "restore_project",
                Some(project_id),
                "project restored",
            ),
        ) {
            if let Err(rollback_error) = fs::rename(&active_dir, &trash_dir) {
                tracing::error!(%rollback_error, "failed to roll back project restore move");
            }
            return Err(storage_failure(error));
        }
        self.get_project(project_id)
    }

    fn ensure_configured_root(&self) -> Result<(), ServiceError> {
        let configured =
            canonical_existing(&project_fs::workspace_data_root()).map_err(storage_failure)?;
        if configured == *self.data_dir {
            Ok(())
        } else {
            Err(ServiceError::Storage)
        }
    }

    fn active_project_ids(&self) -> Result<HashSet<String>, ServiceError> {
        let entries = fs::read_dir(self.projects_dir.as_ref()).map_err(storage_failure)?;
        Ok(entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|project_id| validate_project_id(project_id).is_ok())
            .collect())
    }

    fn active_project_dir(&self, project_id: &str) -> PathBuf {
        self.projects_dir.join(project_id)
    }

    fn trashed_project_dir(&self, project_id: &str) -> PathBuf {
        self.trash_projects_dir.join(project_id)
    }

    fn apply_description(&self, project: &mut DatasetProject) -> Result<(), ServiceError> {
        if let Some(description) = self
            .storage
            .description(&project.id)
            .map_err(storage_failure)?
        {
            project.description = description;
        }
        Ok(())
    }

    fn validate_created_project_root(&self, project_id: &str) -> Result<(), ServiceError> {
        let active_dir = self.active_project_dir(project_id);
        let actual_dir = canonical_existing(&active_dir).map_err(storage_failure)?;
        if actual_dir.parent() != Some(self.projects_dir.as_ref()) {
            return Err(ServiceError::Storage);
        }
        let manifest_path = active_dir.join("project.json");
        let manifest_data = fs::read(&manifest_path).map_err(storage_failure)?;
        let manifest: project_fs::ProjectManifest =
            serde_json::from_slice(&manifest_data).map_err(storage_failure)?;
        let manifest_root =
            canonical_existing(Path::new(&manifest.root_path)).map_err(storage_failure)?;
        if manifest.id != project_id || manifest_root != actual_dir {
            return Err(ServiceError::Storage);
        }
        Ok(())
    }

    fn persist_project_name(&self, project_id: &str, name: &str) -> Result<(), ServiceError> {
        let active_dir = self.active_project_dir(project_id);
        let manifest_path = active_dir.join("project.json");
        let manifest_data = fs::read(&manifest_path).map_err(storage_failure)?;
        let old_manifest: project_fs::ProjectManifest =
            serde_json::from_slice(&manifest_data).map_err(storage_failure)?;
        if old_manifest.id != project_id {
            return Err(ServiceError::Storage);
        }
        let mut new_manifest = old_manifest.clone();
        new_manifest.name = name.to_string();
        project_fs::write_manifest_to_path(&new_manifest, &manifest_path)
            .map_err(storage_failure)?;

        let sqlite_path = active_dir.join("project.sqlite");
        if let Err(error) = crate::storage::update_project_name(&sqlite_path, project_id, name) {
            if let Err(rollback_error) =
                project_fs::write_manifest_to_path(&old_manifest, &manifest_path)
            {
                tracing::error!(%rollback_error, "failed to roll back project manifest rename");
            }
            return Err(storage_failure(error));
        }
        Ok(())
    }
}

fn canonical_existing(path: &Path) -> Result<PathBuf, String> {
    fs::canonicalize(path).map_err(|error| error.to_string())
}

fn validate_project_id(project_id: &str) -> Result<(), ServiceError> {
    let bytes = project_id.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= MAX_PROJECT_NAME_CHARS
        && bytes
            .first()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes
            .last()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(ServiceError::Validation)
    }
}

fn validate_project_name(name: &str) -> Result<&str, ServiceError> {
    let name = name.trim();
    if name.is_empty()
        || name.chars().count() > MAX_PROJECT_NAME_CHARS
        || name.chars().any(char::is_control)
    {
        Err(ServiceError::Validation)
    } else {
        Ok(name)
    }
}

fn validate_description(description: &str) -> Result<&str, ServiceError> {
    if description.chars().count() > MAX_DESCRIPTION_CHARS
        || description
            .chars()
            .any(|character| character.is_control() && character != '\n' && character != '\t')
    {
        Err(ServiceError::Validation)
    } else {
        Ok(description)
    }
}

fn validate_dataset_type(dataset_type: &str) -> Result<(), ServiceError> {
    if matches!(
        dataset_type,
        "yolo-detect" | "yolo-seg" | "image-classification"
    ) {
        Ok(())
    } else {
        Err(ServiceError::Validation)
    }
}

fn validate_demo_template(demo_template: &str) -> Result<(), ServiceError> {
    if matches!(
        demo_template,
        "empty" | "demo-bbox" | "demo-polygon" | "demo-classification"
    ) {
        Ok(())
    } else {
        Err(ServiceError::Validation)
    }
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Reader => "reader",
        Role::Editor => "editor",
        Role::Admin => "admin",
    }
}

fn audit<'a>(
    request_id: &'a str,
    role: Role,
    action: &'a str,
    project_id: Option<&'a str>,
    message: &'a str,
) -> AuditEntry<'a> {
    AuditEntry {
        request_id,
        role: role_name(role),
        action,
        project_id,
        image_id: None,
        message,
    }
}

fn trashed_result(project_id: &str) -> ProjectLifecycleResult {
    ProjectLifecycleResult {
        project_id: project_id.to_string(),
        status: "trashed",
    }
}

fn storage_failure(error: impl std::fmt::Display) -> ServiceError {
    tracing::error!(%error, "remote sample storage operation failed");
    ServiceError::Storage
}

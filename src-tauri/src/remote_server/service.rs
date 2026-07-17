use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
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
    storage::{AuditEntry, AuditOperation, ServerStorage, TrashState},
    Role,
};

const MAX_PROJECT_NAME_CHARS: usize = 128;
const MAX_DESCRIPTION_CHARS: usize = 2_000;
static PROJECT_MUTATION_LOCK: Mutex<()> = Mutex::new(());

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
        let projects_dir = canonical_existing(&projects_dir)
            .map_err(|_| ServerBuildError::initialization_failed())?;
        let trash_projects_dir = canonical_existing(&trash_projects_dir)
            .map_err(|_| ServerBuildError::initialization_failed())?;
        let storage = ServerStorage::initialize(&data_dir)
            .map_err(|_| ServerBuildError::initialization_failed())?;
        let service = Self {
            data_dir: Arc::new(data_dir),
            projects_dir: Arc::new(projects_dir),
            trash_projects_dir: Arc::new(trash_projects_dir),
            repository: Arc::new(SampleRepository::new()),
            storage,
        };

        let _mutation_guard = mutation_guard();
        match service.reconcile_lifecycle() {
            Ok(()) => {}
            Err(ServiceError::Conflict) => {
                return Err(ServerBuildError::project_state_conflict());
            }
            Err(_) => return Err(ServerBuildError::initialization_failed()),
        }
        service
            .repair_project_manifests()
            .map_err(|_| ServerBuildError::initialization_failed())?;
        Ok(service)
    }

    pub(super) fn list_projects(&self) -> Result<Vec<DatasetProject>, ServiceError> {
        self.ensure_configured_root()?;
        let active_ids = {
            let _mutation_guard = mutation_guard();
            self.repair_manifests_in_root(self.projects_dir.as_ref(), true)?;
            self.active_project_ids()?
        };
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
        let _mutation_guard = mutation_guard();
        self.get_project_locked(project_id)
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
        let _mutation_guard = mutation_guard();
        let operation = self
            .storage
            .begin_audit(audit(
                request_id,
                role,
                "create_project",
                Some(&project_id),
                "project creation requested",
            ))
            .map_err(storage_failure)?;

        let result = (|| {
            if self
                .existing_project_dir(self.projects_dir.as_ref(), &project_id)?
                .is_some()
                || self
                    .existing_project_dir(self.trash_projects_dir.as_ref(), &project_id)?
                    .is_some()
                || self
                    .storage
                    .trash_state(&project_id)
                    .map_err(storage_failure)?
                    .is_some()
            {
                return Err(ServiceError::Conflict);
            }

            let project = datasets::create_dataset_project(name, dataset_type, demo_template)
                .map_err(storage_failure)?;
            self.validate_created_project_root(&project_id)?;
            Ok(project)
        })();

        match result {
            Ok(project) => {
                self.complete_audit_best_effort(operation, "project created");
                Ok(project)
            }
            Err(error) => {
                self.fail_audit_best_effort(operation, failure_message(error));
                Err(error)
            }
        }
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
        let _mutation_guard = mutation_guard();
        let operation = self
            .storage
            .begin_audit(audit(
                request_id,
                role,
                "update_project",
                Some(project_id),
                "project update requested",
            ))
            .map_err(storage_failure)?;

        let result = (|| {
            let active_dir = self
                .existing_project_dir(self.projects_dir.as_ref(), project_id)?
                .ok_or(ServiceError::NotFound)?;
            self.ensure_project_manifest(&active_dir, true)?;
            if let Some(name) = name {
                self.persist_project_name(project_id, &active_dir, name)?;
            }
            self.storage
                .complete_metadata_and_audit(
                    project_id,
                    description,
                    operation,
                    "project metadata updated",
                )
                .map_err(storage_failure)?;
            self.get_project_locked(project_id)
        })();

        if let Err(error) = result {
            self.fail_audit_best_effort(operation, failure_message(error));
        }
        result
    }

    pub(super) fn delete_project(
        &self,
        project_id: &str,
        request_id: &str,
        role: Role,
    ) -> Result<ProjectLifecycleResult, ServiceError> {
        self.ensure_configured_root()?;
        validate_project_id(project_id)?;
        let _mutation_guard = mutation_guard();
        let active_dir = self.existing_project_dir(self.projects_dir.as_ref(), project_id)?;
        let trash_dir = self.existing_project_dir(self.trash_projects_dir.as_ref(), project_id)?;
        let trash_state = self
            .storage
            .trash_state(project_id)
            .map_err(storage_failure)?;

        if active_dir.is_some() && trash_dir.is_some() {
            self.record_failed_attempt(
                request_id,
                role,
                "delete_project",
                project_id,
                ServiceError::Conflict,
            )?;
            return Err(ServiceError::Conflict);
        }

        if let Some(active_dir) = active_dir {
            if trash_state.is_some() {
                self.record_failed_attempt(
                    request_id,
                    role,
                    "delete_project",
                    project_id,
                    ServiceError::Conflict,
                )?;
                return Err(ServiceError::Conflict);
            }
            let operation = self
                .storage
                .begin_trash(
                    project_id,
                    audit(
                        request_id,
                        role,
                        "delete_project",
                        Some(project_id),
                        "project deletion requested",
                    ),
                )
                .map_err(storage_failure)?;
            let trash_target = self.trash_projects_dir.join(project_id);
            if let Err(error) = fs::rename(&active_dir, &trash_target) {
                let service_error = storage_failure(error);
                if let Err(failure) =
                    self.storage
                        .fail_trash(project_id, operation, failure_message(service_error))
                {
                    tracing::error!(%failure, "failed to record project trash failure");
                }
                return Err(service_error);
            }
            self.storage
                .complete_trash(project_id, operation, "project moved to trash")
                .map_err(storage_failure)?;
            return Ok(trashed_result(project_id));
        }

        if trash_state == Some(TrashState::Trashed) {
            if trash_dir.is_none() {
                self.record_failed_attempt(
                    request_id,
                    role,
                    "delete_project",
                    project_id,
                    ServiceError::Storage,
                )?;
                return Err(ServiceError::Storage);
            }
            let operation = self.begin_generic_operation(
                request_id,
                role,
                "delete_project",
                project_id,
                "project deletion requested",
            )?;
            self.complete_audit_best_effort(operation, "project already in trash");
            return Ok(trashed_result(project_id));
        }

        let error = if trash_dir.is_some() || trash_state.is_some() {
            ServiceError::Storage
        } else {
            ServiceError::NotFound
        };
        self.record_failed_attempt(request_id, role, "delete_project", project_id, error)?;
        Err(error)
    }

    pub(super) fn restore_project(
        &self,
        project_id: &str,
        request_id: &str,
        role: Role,
    ) -> Result<DatasetProject, ServiceError> {
        self.ensure_configured_root()?;
        validate_project_id(project_id)?;
        let _mutation_guard = mutation_guard();
        let active_dir = self.existing_project_dir(self.projects_dir.as_ref(), project_id)?;
        let trash_dir = self.existing_project_dir(self.trash_projects_dir.as_ref(), project_id)?;
        let trash_state = self
            .storage
            .trash_state(project_id)
            .map_err(storage_failure)?;

        if active_dir.is_some() && trash_dir.is_some() {
            self.record_failed_attempt(
                request_id,
                role,
                "restore_project",
                project_id,
                ServiceError::Conflict,
            )?;
            return Err(ServiceError::Conflict);
        }

        if active_dir.is_some() {
            if trash_state.is_some() {
                self.record_failed_attempt(
                    request_id,
                    role,
                    "restore_project",
                    project_id,
                    ServiceError::Conflict,
                )?;
                return Err(ServiceError::Conflict);
            }
            let operation = self.begin_generic_operation(
                request_id,
                role,
                "restore_project",
                project_id,
                "project restoration requested",
            )?;
            self.complete_audit_best_effort(operation, "project already active");
            return self.get_project_locked(project_id);
        }

        if trash_state != Some(TrashState::Trashed) {
            let error = if trash_dir.is_some() || trash_state.is_some() {
                ServiceError::Storage
            } else {
                ServiceError::NotFound
            };
            self.record_failed_attempt(request_id, role, "restore_project", project_id, error)?;
            return Err(error);
        }
        let trash_dir = match trash_dir {
            Some(trash_dir) => trash_dir,
            None => {
                self.record_failed_attempt(
                    request_id,
                    role,
                    "restore_project",
                    project_id,
                    ServiceError::Storage,
                )?;
                return Err(ServiceError::Storage);
            }
        };
        let operation = self
            .storage
            .begin_restore(
                project_id,
                audit(
                    request_id,
                    role,
                    "restore_project",
                    Some(project_id),
                    "project restoration requested",
                ),
            )
            .map_err(storage_failure)?;
        let active_target = self.projects_dir.join(project_id);
        if let Err(error) = fs::rename(&trash_dir, &active_target) {
            let service_error = storage_failure(error);
            if let Err(failure) =
                self.storage
                    .fail_restore(project_id, operation, failure_message(service_error))
            {
                tracing::error!(%failure, "failed to record project restore failure");
            }
            return Err(service_error);
        }
        self.storage
            .complete_restore(project_id, operation, "project restored")
            .map_err(storage_failure)?;
        self.get_project_locked(project_id)
    }

    fn reconcile_lifecycle(&self) -> Result<(), ServiceError> {
        self.reject_active_trash_collisions()?;
        for record in self.storage.trash_records().map_err(storage_failure)? {
            validate_project_id(&record.project_id).map_err(|_| ServiceError::Storage)?;
            let active_dir =
                self.existing_project_dir(self.projects_dir.as_ref(), &record.project_id)?;
            let trash_dir =
                self.existing_project_dir(self.trash_projects_dir.as_ref(), &record.project_id)?;
            if active_dir.is_some() && trash_dir.is_some() {
                return Err(ServiceError::Conflict);
            }

            match (record.state, active_dir, trash_dir) {
                (TrashState::Trashing, Some(active_dir), None) => {
                    fs::rename(active_dir, self.trash_projects_dir.join(&record.project_id))
                        .map_err(storage_failure)?;
                    self.storage
                        .reconcile_trashed(&record.project_id)
                        .map_err(storage_failure)?;
                }
                (TrashState::Trashing, None, Some(_)) => {
                    self.storage
                        .reconcile_trashed(&record.project_id)
                        .map_err(storage_failure)?;
                }
                (TrashState::Restoring, None, Some(trash_dir)) => {
                    fs::rename(trash_dir, self.projects_dir.join(&record.project_id))
                        .map_err(storage_failure)?;
                    self.storage
                        .reconcile_restored(&record.project_id)
                        .map_err(storage_failure)?;
                }
                (TrashState::Restoring, Some(_), None) => {
                    self.storage
                        .reconcile_restored(&record.project_id)
                        .map_err(storage_failure)?;
                }
                (TrashState::Trashed, None, Some(_)) => {}
                _ => return Err(ServiceError::Storage),
            }
        }
        Ok(())
    }

    fn reject_active_trash_collisions(&self) -> Result<(), ServiceError> {
        for entry in fs::read_dir(self.trash_projects_dir.as_ref()).map_err(storage_failure)? {
            let entry = entry.map_err(storage_failure)?;
            let Some(project_id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if validate_project_id(&project_id).is_err() {
                continue;
            }
            let trash_dir =
                self.existing_project_dir(self.trash_projects_dir.as_ref(), &project_id)?;
            let active_dir = self.existing_project_dir(self.projects_dir.as_ref(), &project_id)?;
            if trash_dir.is_some() && active_dir.is_some() {
                return Err(ServiceError::Conflict);
            }
        }
        Ok(())
    }

    fn repair_project_manifests(&self) -> Result<(), ServiceError> {
        self.repair_manifests_in_root(self.projects_dir.as_ref(), true)?;
        self.repair_manifests_in_root(self.trash_projects_dir.as_ref(), false)
    }

    fn repair_manifests_in_root(
        &self,
        root: &Path,
        require_active_root: bool,
    ) -> Result<(), ServiceError> {
        let entries = fs::read_dir(root).map_err(storage_failure)?;
        for entry in entries {
            let entry = entry.map_err(storage_failure)?;
            let Some(project_id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if validate_project_id(&project_id).is_err() {
                continue;
            }
            let project_dir = match self.existing_project_dir(root, &project_id) {
                Ok(Some(project_dir)) => project_dir,
                Ok(None) => continue,
                Err(error) => {
                    tracing::error!(
                        project_id,
                        ?error,
                        "project directory is outside the configured root"
                    );
                    continue;
                }
            };
            if let Err(error) = self.ensure_project_manifest(&project_dir, require_active_root) {
                tracing::error!(project_id, ?error, "project manifest could not be repaired");
            }
        }
        Ok(())
    }

    fn ensure_project_manifest(
        &self,
        project_dir: &Path,
        require_active_root: bool,
    ) -> Result<(), ServiceError> {
        let manifest_path = project_dir.join("project.json");
        project_fs::recover_manifest_backup(&manifest_path).map_err(storage_failure)?;
        let manifest = fs::read(&manifest_path)
            .ok()
            .and_then(|data| serde_json::from_slice::<project_fs::ProjectManifest>(&data).ok());
        let manifest = match manifest {
            Some(manifest) => manifest,
            None => {
                let manifest =
                    crate::storage::read_project_manifest(&project_dir.join("project.sqlite"))
                        .map_err(storage_failure)?
                        .ok_or(ServiceError::Storage)?;
                project_fs::write_manifest_to_path(&manifest, &manifest_path)
                    .map_err(storage_failure)?;
                manifest
            }
        };
        let project_id = project_dir
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(ServiceError::Storage)?;
        if manifest.id != project_id {
            return Err(ServiceError::Storage);
        }
        if let Some(indexed_manifest) =
            crate::storage::read_project_manifest(&project_dir.join("project.sqlite"))
                .map_err(storage_failure)?
        {
            if indexed_manifest.id != manifest.id {
                return Err(ServiceError::Storage);
            }
            if indexed_manifest.name != manifest.name {
                crate::storage::update_project_name(
                    &project_dir.join("project.sqlite"),
                    project_id,
                    &manifest.name,
                )
                .map_err(storage_failure)?;
            }
        }
        if require_active_root {
            let manifest_root =
                canonical_existing(Path::new(&manifest.root_path)).map_err(storage_failure)?;
            if manifest_root != project_dir {
                return Err(ServiceError::Storage);
            }
        }
        Ok(())
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
        let mut project_ids = HashSet::new();
        for entry in fs::read_dir(self.projects_dir.as_ref()).map_err(storage_failure)? {
            let entry = entry.map_err(storage_failure)?;
            let Some(project_id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if validate_project_id(&project_id).is_err() {
                continue;
            }
            match self.existing_project_dir(self.projects_dir.as_ref(), &project_id) {
                Ok(Some(_)) => {
                    project_ids.insert(project_id);
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::error!(
                        project_id,
                        ?error,
                        "project directory is outside the configured root"
                    );
                }
            }
        }
        Ok(project_ids)
    }

    fn existing_project_dir(
        &self,
        root: &Path,
        project_id: &str,
    ) -> Result<Option<PathBuf>, ServiceError> {
        let candidate = root.join(project_id);
        let metadata = match fs::symlink_metadata(&candidate) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(storage_failure(error)),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(ServiceError::Storage);
        }
        let canonical_candidate = canonical_existing(&candidate).map_err(storage_failure)?;
        if !canonical_project_path_is_direct_child(root, &canonical_candidate) {
            return Err(ServiceError::Storage);
        }
        Ok(Some(canonical_candidate))
    }

    fn get_project_locked(&self, project_id: &str) -> Result<DatasetProject, ServiceError> {
        let active_dir = self
            .existing_project_dir(self.projects_dir.as_ref(), project_id)?
            .ok_or(ServiceError::NotFound)?;
        self.ensure_project_manifest(&active_dir, true)?;
        let mut project = self
            .repository
            .workspace_dataset_projects()
            .into_iter()
            .find(|project| project.id == project_id)
            .ok_or(ServiceError::Storage)?;
        self.apply_description(&mut project)?;
        Ok(project)
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
        let actual_dir = self
            .existing_project_dir(self.projects_dir.as_ref(), project_id)?
            .ok_or(ServiceError::Storage)?;
        self.ensure_project_manifest(&actual_dir, true)
    }

    fn persist_project_name(
        &self,
        project_id: &str,
        active_dir: &Path,
        name: &str,
    ) -> Result<(), ServiceError> {
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
                tracing::error!(%rollback_error, "failed to roll back project manifest update");
            }
            return Err(storage_failure(error));
        }
        Ok(())
    }

    fn begin_generic_operation(
        &self,
        request_id: &str,
        role: Role,
        action: &str,
        project_id: &str,
        message: &str,
    ) -> Result<AuditOperation, ServiceError> {
        self.storage
            .begin_audit(audit(request_id, role, action, Some(project_id), message))
            .map_err(storage_failure)
    }

    fn record_failed_attempt(
        &self,
        request_id: &str,
        role: Role,
        action: &str,
        project_id: &str,
        error: ServiceError,
    ) -> Result<(), ServiceError> {
        let operation = self.begin_generic_operation(
            request_id,
            role,
            action,
            project_id,
            "project mutation requested",
        )?;
        self.fail_audit_best_effort(operation, failure_message(error));
        Ok(())
    }

    fn complete_audit_best_effort(&self, operation: AuditOperation, message: &str) {
        if let Err(error) = self.storage.complete_audit(operation, message) {
            tracing::error!(%error, "failed to complete project audit operation");
        }
    }

    fn fail_audit_best_effort(&self, operation: AuditOperation, message: &str) {
        if let Err(error) = self.storage.fail_audit(operation, message) {
            tracing::error!(%error, "failed to record project audit failure");
        }
    }
}

fn mutation_guard() -> std::sync::MutexGuard<'static, ()> {
    PROJECT_MUTATION_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn canonical_project_path_is_direct_child(root: &Path, candidate: &Path) -> bool {
    candidate.starts_with(root) && candidate.parent() == Some(root)
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

fn failure_message(error: ServiceError) -> &'static str {
    match error {
        ServiceError::Validation => "project mutation validation failed",
        ServiceError::NotFound => "project was not found",
        ServiceError::Conflict => "project mutation conflicted with existing state",
        ServiceError::Storage => "project storage operation failed",
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

#[cfg(test)]
mod tests {
    use super::canonical_project_path_is_direct_child;
    use std::path::Path;

    #[test]
    fn canonical_project_path_must_be_a_direct_child_of_root() {
        let root = Path::new("configured").join("projects");
        assert!(canonical_project_path_is_direct_child(
            &root,
            &root.join("valid-project")
        ));
        assert!(!canonical_project_path_is_direct_child(
            &root,
            &root.join("nested").join("project")
        ));
        assert!(!canonical_project_path_is_direct_child(
            &root,
            Path::new("outside").join("project").as_path()
        ));
    }
}

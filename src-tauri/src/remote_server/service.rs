use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock, Weak,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::{
    datasets,
    domain::{DatasetProject, SampleRepository},
    project_fs,
};

use super::{
    config::ServerConfig,
    error::ServerBuildError,
    storage::{
        AuditEntry, AuditOperation, OperationRecord, ServerStorage, TrashRecord, TrashState,
    },
    Role,
};

const MAX_PROJECT_NAME_CHARS: usize = 128;
const MAX_DESCRIPTION_CHARS: usize = 2_000;
const CREATE_OWNERSHIP_FILE: &str = ".remote-create-owner";
static PROJECT_MUTATION_LOCK: Mutex<()> = Mutex::new(());
static DATA_ROOT_LEASES: OnceLock<Mutex<HashMap<PathBuf, Weak<DataRootLease>>>> = OnceLock::new();
static CREATE_OWNERSHIP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
struct DataRootLease {
    file: File,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateOperationPayload {
    project_id: String,
    name: String,
    dataset_type: String,
    demo_template: String,
    ownership_marker: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ProjectSnapshot {
    name: String,
    description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateOperationPayload {
    project_id: String,
    old: ProjectSnapshot,
    new: ProjectSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActualProjectState {
    manifest_name: String,
    indexed_name: String,
    description: Option<String>,
}

impl ActualProjectState {
    fn matches(&self, snapshot: &ProjectSnapshot) -> bool {
        self.manifest_name == snapshot.name
            && self.indexed_name == snapshot.name
            && self.description == snapshot.description
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LifecycleOperationPayload {
    project_id: String,
}

impl Drop for DataRootLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

enum DataRootLeaseError {
    InUse,
    Storage,
}

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
    _data_root_lease: Arc<DataRootLease>,
}

impl RemoteSampleService {
    pub(super) fn initialize(config: &ServerConfig) -> Result<Self, ServerBuildError> {
        ensure_data_root_directory(&config.data_dir)
            .map_err(|_| ServerBuildError::initialization_failed())?;
        let data_dir = fs::canonicalize(&config.data_dir)
            .map_err(|_| ServerBuildError::initialization_failed())?;
        let data_root_lease = match acquire_data_root_lease(&data_dir) {
            Ok(lease) => lease,
            Err(DataRootLeaseError::InUse) => return Err(ServerBuildError::data_root_in_use()),
            Err(DataRootLeaseError::Storage) => {
                return Err(ServerBuildError::initialization_failed());
            }
        };

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

        let projects_dir = ensure_managed_subdirectory(&data_dir, &["projects"])
            .map_err(|_| ServerBuildError::initialization_failed())?;
        let trash_projects_dir = ensure_managed_subdirectory(&data_dir, &["trash", "projects"])
            .map_err(|_| ServerBuildError::initialization_failed())?;
        if !canonical_path_is_within(&data_dir, &projects_dir)
            || !canonical_path_is_within(&data_dir, &trash_projects_dir)
        {
            return Err(ServerBuildError::initialization_failed());
        }
        let storage = ServerStorage::initialize(&data_dir)
            .map_err(|_| ServerBuildError::initialization_failed())?;
        let service = Self {
            data_dir: Arc::new(data_dir),
            projects_dir: Arc::new(projects_dir),
            trash_projects_dir: Arc::new(trash_projects_dir),
            repository: Arc::new(SampleRepository::new()),
            storage,
            _data_root_lease: data_root_lease,
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
            .reconcile_pending_operations()
            .map_err(|_| ServerBuildError::initialization_failed())?;
        service
            .repair_project_manifests()
            .map_err(|_| ServerBuildError::initialization_failed())?;
        Ok(service)
    }

    pub(super) fn list_projects(&self) -> Result<Vec<DatasetProject>, ServiceError> {
        self.ensure_configured_root()?;
        let _mutation_guard = mutation_guard();
        let manifests = self.validated_active_manifests()?;
        let mut projects = self
            .repository
            .workspace_dataset_projects_from_manifests(manifests);
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
        let ownership_marker = next_create_ownership_marker();
        let operation_payload = serde_json::to_string(&CreateOperationPayload {
            project_id: project_id.clone(),
            name: name.to_string(),
            dataset_type: dataset_type.to_string(),
            demo_template: demo_template.to_string(),
            ownership_marker: ownership_marker.clone(),
        })
        .map_err(storage_failure)?;
        let operation = self
            .storage
            .begin_audit(audit(
                request_id,
                role,
                "create_project",
                Some(&project_id),
                "project creation requested",
                &operation_payload,
            ))
            .map_err(storage_failure)?;

        let conflict = self
            .existing_project_dir(self.projects_dir.as_ref(), &project_id)?
            .is_some()
            || self
                .existing_project_dir(self.trash_projects_dir.as_ref(), &project_id)?
                .is_some()
            || self
                .storage
                .trash_state(&project_id)
                .map_err(storage_failure)?
                .is_some();
        if conflict {
            self.fail_audit_best_effort(&operation, failure_message(ServiceError::Conflict));
            return Err(ServiceError::Conflict);
        }

        if let Err(error) = self.prepare_owned_project_directory(&project_id, &ownership_marker) {
            if error == ServiceError::Conflict
                || self.cleanup_partial_created_project(&project_id).is_ok()
            {
                self.fail_audit_best_effort(&operation, failure_message(error));
            }
            return Err(error);
        }
        let result = datasets::create_dataset_project(name, dataset_type, demo_template)
            .map_err(storage_failure)
            .and_then(|project| {
                self.validate_created_project_root(&project_id)?;
                Ok(project)
            });
        match result {
            Ok(project) => {
                self.complete_audit_best_effort(&operation, "project created");
                Ok(project)
            }
            Err(error) => {
                if self.cleanup_partial_created_project(&project_id).is_ok() {
                    self.fail_audit_best_effort(&operation, failure_message(error));
                }
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
        let active_dir = self
            .existing_project_dir(self.projects_dir.as_ref(), project_id)?
            .ok_or(ServiceError::NotFound)?;
        let old_manifest = self.ensure_project_manifest(&active_dir, true)?;
        let old_snapshot = ProjectSnapshot {
            name: old_manifest.name,
            description: self
                .storage
                .description(project_id)
                .map_err(storage_failure)?,
        };
        let new_snapshot = ProjectSnapshot {
            name: name.unwrap_or(&old_snapshot.name).to_string(),
            description: description
                .map(str::to_string)
                .or_else(|| old_snapshot.description.clone()),
        };
        let operation_payload = serde_json::to_string(&UpdateOperationPayload {
            project_id: project_id.to_string(),
            old: old_snapshot.clone(),
            new: new_snapshot.clone(),
        })
        .map_err(storage_failure)?;
        let operation = self
            .storage
            .begin_audit(audit(
                request_id,
                role,
                "update_project",
                Some(project_id),
                "project update requested",
                &operation_payload,
            ))
            .map_err(storage_failure)?;

        if new_snapshot.name != old_snapshot.name {
            if let Err(error) =
                self.persist_project_name(project_id, &active_dir, &new_snapshot.name)
            {
                self.compensate_failed_update(
                    project_id,
                    &active_dir,
                    &old_snapshot,
                    &operation,
                    error,
                );
                return Err(error);
            }
        }
        match self.storage.complete_metadata_and_audit(
            project_id,
            new_snapshot.description.as_deref(),
            &operation,
            "project metadata updated",
        ) {
            Ok(()) => self.get_project_locked(project_id),
            Err(error) if description.is_none() => {
                tracing::error!(%error, "project update audit completion remains pending");
                self.get_project_locked(project_id)
            }
            Err(error) => {
                let service_error = storage_failure(error);
                self.compensate_failed_update(
                    project_id,
                    &active_dir,
                    &old_snapshot,
                    &operation,
                    service_error,
                );
                Err(service_error)
            }
        }
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
            let operation_payload = lifecycle_payload(project_id)?;
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
                        &operation_payload,
                    ),
                )
                .map_err(storage_failure)?;
            let trash_target = self.trash_projects_dir.join(project_id);
            if let Err(error) = fs::rename(&active_dir, &trash_target) {
                let service_error = storage_failure(error);
                if let Err(failure) =
                    self.storage
                        .fail_trash(project_id, &operation, failure_message(service_error))
                {
                    tracing::error!(%failure, "failed to record project trash failure");
                }
                return Err(service_error);
            }
            if let Err(error) =
                self.storage
                    .complete_trash(project_id, &operation, "project moved to trash")
            {
                tracing::error!(%error, "project trash completion remains pending");
            }
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
            self.validate_trash_record(project_id, TrashState::Trashed)?;
            let operation = self.begin_generic_operation(
                request_id,
                role,
                "delete_project",
                project_id,
                "project deletion requested",
            )?;
            self.complete_audit_best_effort(&operation, "project already in trash");
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
            self.complete_audit_best_effort(&operation, "project already active");
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
        self.validate_trash_record(project_id, TrashState::Trashed)?;
        let operation_payload = lifecycle_payload(project_id)?;
        let restore = self
            .storage
            .begin_restore(
                project_id,
                audit(
                    request_id,
                    role,
                    "restore_project",
                    Some(project_id),
                    "project restoration requested",
                    &operation_payload,
                ),
            )
            .map_err(storage_failure)?;
        let active_target = self.projects_dir.join(project_id);
        if let Err(error) = fs::rename(&trash_dir, &active_target) {
            let service_error = storage_failure(error);
            if let Err(failure) =
                self.storage
                    .fail_restore(project_id, &restore, failure_message(service_error))
            {
                tracing::error!(%failure, "failed to record project restore failure");
            }
            return Err(service_error);
        }
        if let Err(error) = self
            .storage
            .complete_restore(project_id, &restore, "project restored")
        {
            tracing::error!(%error, "project restore completion remains pending");
        }
        self.get_project_locked(project_id)
    }

    fn reconcile_lifecycle(&self) -> Result<(), ServiceError> {
        self.reject_active_trash_collisions()?;
        for record in self.storage.trash_records().map_err(storage_failure)? {
            validate_project_id(&record.project_id).map_err(|_| ServiceError::Storage)?;
            let operation_id = record
                .operation_id
                .as_deref()
                .ok_or(ServiceError::Storage)?;
            self.validate_lifecycle_operation(
                &record.project_id,
                operation_id,
                lifecycle_operation_expectation(record.state),
            )?;
            let operation = AuditOperation {
                operation_id: operation_id.to_string(),
            };
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
                        .reconcile_trashed(&record.project_id, &operation)
                        .map_err(storage_failure)?;
                }
                (TrashState::Trashing, None, Some(_)) => {
                    self.storage
                        .reconcile_trashed(&record.project_id, &operation)
                        .map_err(storage_failure)?;
                }
                (TrashState::Restoring, None, Some(trash_dir)) => {
                    fs::rename(trash_dir, self.projects_dir.join(&record.project_id))
                        .map_err(storage_failure)?;
                    self.storage
                        .reconcile_restored(&record.project_id, &operation)
                        .map_err(storage_failure)?;
                }
                (TrashState::Restoring, Some(_), None) => {
                    self.storage
                        .reconcile_restored(&record.project_id, &operation)
                        .map_err(storage_failure)?;
                }
                (TrashState::Trashed, None, Some(_)) => {}
                _ => return Err(ServiceError::Storage),
            }
        }
        Ok(())
    }

    fn validate_trash_record(
        &self,
        project_id: &str,
        expected_state: TrashState,
    ) -> Result<TrashRecord, ServiceError> {
        let record = self
            .storage
            .trash_records()
            .map_err(storage_failure)?
            .into_iter()
            .find(|record| record.project_id == project_id)
            .ok_or(ServiceError::Storage)?;
        if record.state != expected_state {
            return Err(ServiceError::Storage);
        }
        let operation_id = record
            .operation_id
            .as_deref()
            .ok_or(ServiceError::Storage)?;
        self.validate_lifecycle_operation(
            project_id,
            operation_id,
            lifecycle_operation_expectation(expected_state),
        )?;
        Ok(record)
    }

    fn validate_lifecycle_operation(
        &self,
        project_id: &str,
        operation_id: &str,
        (expected_action, expected_state): (&str, &str),
    ) -> Result<(), ServiceError> {
        let operation = self
            .storage
            .operation(operation_id)
            .map_err(storage_failure)?
            .ok_or(ServiceError::Storage)?;
        let payload: LifecycleOperationPayload =
            serde_json::from_str(&operation.payload).map_err(|_| ServiceError::Storage)?;
        if operation.project_id.as_deref() != Some(project_id)
            || payload.project_id != project_id
            || operation.action != expected_action
            || operation.state != expected_state
        {
            return Err(ServiceError::Storage);
        }
        Ok(())
    }

    fn reconcile_pending_operations(&self) -> Result<(), ServiceError> {
        for record in self.storage.pending_operations().map_err(storage_failure)? {
            match record.action.as_str() {
                "create_project" => self.reconcile_pending_create(&record)?,
                "update_project" => self.reconcile_pending_update(&record)?,
                "delete_project" => {
                    let project_id = record.project_id.as_deref().ok_or(ServiceError::Storage)?;
                    validate_lifecycle_record_payload(&record, project_id)?;
                    if self
                        .storage
                        .trash_state(project_id)
                        .map_err(storage_failure)?
                        == Some(TrashState::Trashed)
                    {
                        self.storage
                            .complete_audit(
                                &AuditOperation {
                                    operation_id: record.operation_id,
                                },
                                "project deletion reconciled",
                            )
                            .map_err(storage_failure)?;
                    } else {
                        return Err(ServiceError::Storage);
                    }
                }
                "restore_project" => {
                    let project_id = record.project_id.as_deref().ok_or(ServiceError::Storage)?;
                    validate_lifecycle_record_payload(&record, project_id)?;
                    if self
                        .existing_project_dir(self.projects_dir.as_ref(), project_id)?
                        .is_some()
                        && self
                            .storage
                            .trash_state(project_id)
                            .map_err(storage_failure)?
                            .is_none()
                    {
                        self.storage
                            .complete_audit(
                                &AuditOperation {
                                    operation_id: record.operation_id,
                                },
                                "project restoration reconciled",
                            )
                            .map_err(storage_failure)?;
                    } else {
                        return Err(ServiceError::Storage);
                    }
                }
                _ => return Err(ServiceError::Storage),
            }
        }
        Ok(())
    }

    fn reconcile_pending_create(&self, record: &OperationRecord) -> Result<(), ServiceError> {
        let payload: CreateOperationPayload =
            serde_json::from_str(&record.payload).map_err(storage_failure)?;
        if record.project_id.as_deref() != Some(payload.project_id.as_str()) {
            return Err(ServiceError::Storage);
        }
        validate_project_id(&payload.project_id).map_err(|_| ServiceError::Storage)?;
        validate_project_name(&payload.name).map_err(|_| ServiceError::Storage)?;
        validate_dataset_type(&payload.dataset_type).map_err(|_| ServiceError::Storage)?;
        validate_demo_template(&payload.demo_template).map_err(|_| ServiceError::Storage)?;
        if datasets::project_id_from_name(&payload.name, &payload.demo_template)
            != payload.project_id
        {
            return Err(ServiceError::Storage);
        }
        let active_dir =
            self.existing_project_dir(self.projects_dir.as_ref(), &payload.project_id)?;
        let trash_dir =
            self.existing_project_dir(self.trash_projects_dir.as_ref(), &payload.project_id)?;
        if active_dir.is_some() && trash_dir.is_some() {
            return Err(ServiceError::Conflict);
        }
        let operation = AuditOperation {
            operation_id: record.operation_id.clone(),
        };
        match (active_dir, trash_dir) {
            (Some(active_dir), None) => {
                if self.read_create_ownership_marker(&active_dir)?.as_deref()
                    != Some(payload.ownership_marker.as_str())
                {
                    return self
                        .storage
                        .fail_audit(
                            &operation,
                            "create operation did not own the existing project",
                        )
                        .map_err(storage_failure);
                }
                match self.ensure_project_manifest(&active_dir, true) {
                    Ok(manifest)
                        if manifest.name == payload.name
                            && manifest.format == payload.dataset_type =>
                    {
                        self.storage
                            .complete_audit(&operation, "project creation reconciled")
                            .map_err(storage_failure)
                    }
                    Ok(_) => Err(ServiceError::Storage),
                    Err(_) => {
                        fs::remove_dir_all(&active_dir).map_err(storage_failure)?;
                        self.storage
                            .fail_audit(&operation, "partial project creation removed")
                            .map_err(storage_failure)
                    }
                }
            }
            (None, None) => self
                .storage
                .fail_audit(&operation, "project creation did not persist")
                .map_err(storage_failure),
            _ => Err(ServiceError::Storage),
        }
    }

    fn reconcile_pending_update(&self, record: &OperationRecord) -> Result<(), ServiceError> {
        let payload: UpdateOperationPayload =
            serde_json::from_str(&record.payload).map_err(storage_failure)?;
        if record.project_id.as_deref() != Some(payload.project_id.as_str()) {
            return Err(ServiceError::Storage);
        }
        let active_dir = self
            .existing_project_dir(self.projects_dir.as_ref(), &payload.project_id)?
            .ok_or(ServiceError::Storage)?;
        let actual = self.read_actual_project_state(&active_dir, &payload.project_id)?;
        let operation = AuditOperation {
            operation_id: record.operation_id.clone(),
        };
        if actual.matches(&payload.new) {
            return self
                .storage
                .complete_audit(&operation, "project update reconciled")
                .map_err(storage_failure);
        }
        if actual.matches(&payload.old) {
            return self
                .storage
                .fail_update_and_restore_metadata(
                    &payload.project_id,
                    payload.old.description.as_deref(),
                    &operation,
                    "project update rolled back before restart",
                )
                .map_err(storage_failure);
        }

        self.persist_project_name(&payload.project_id, &active_dir, &payload.old.name)?;
        self.storage
            .fail_update_and_restore_metadata(
                &payload.project_id,
                payload.old.description.as_deref(),
                &operation,
                "partial project update rolled back during startup",
            )
            .map_err(storage_failure)?;
        let rolled_back = self.read_actual_project_state(&active_dir, &payload.project_id)?;
        if rolled_back.matches(&payload.old) {
            Ok(())
        } else {
            Err(ServiceError::Storage)
        }
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
    ) -> Result<project_fs::ProjectManifest, ServiceError> {
        let manifest_path = project_dir.join("project.json");
        for artifact in [
            manifest_path.clone(),
            manifest_path.with_extension("json.bak"),
            manifest_path.with_extension("json.tmp"),
        ] {
            self.validate_optional_project_file(project_dir, &artifact)?;
        }
        project_fs::recover_manifest_backup(&manifest_path).map_err(storage_failure)?;
        self.validate_required_project_file(project_dir, &manifest_path)?;
        let sqlite_path = project_dir.join("project.sqlite");
        self.validate_project_database(project_dir, &sqlite_path)?;
        let manifest = fs::read(&manifest_path)
            .ok()
            .and_then(|data| serde_json::from_slice::<project_fs::ProjectManifest>(&data).ok());
        let manifest = match manifest {
            Some(manifest) => manifest,
            None => {
                let manifest = crate::storage::read_project_manifest(&sqlite_path)
                    .map_err(storage_failure)?
                    .ok_or(ServiceError::Storage)?;
                self.validate_managed_manifest(&manifest, project_dir, require_active_root)?;
                project_fs::write_manifest_to_path(&manifest, &manifest_path)
                    .map_err(storage_failure)?;
                manifest
            }
        };
        self.validate_managed_manifest(&manifest, project_dir, require_active_root)?;
        let indexed_manifest = crate::storage::read_project_manifest(&sqlite_path)
            .map_err(storage_failure)?
            .ok_or(ServiceError::Storage)?;
        self.validate_managed_manifest(&indexed_manifest, project_dir, require_active_root)?;
        if indexed_manifest.id != manifest.id
            || indexed_manifest.source_dataset_key != manifest.source_dataset_key
            || indexed_manifest.format != manifest.format
            || indexed_manifest.root_path != manifest.root_path
        {
            return Err(ServiceError::Storage);
        }
        if indexed_manifest.name != manifest.name {
            crate::storage::update_project_name(&sqlite_path, &manifest.id, &manifest.name)
                .map_err(storage_failure)?;
        }
        Ok(manifest)
    }

    fn read_actual_project_state(
        &self,
        project_dir: &Path,
        project_id: &str,
    ) -> Result<ActualProjectState, ServiceError> {
        let manifest_path = project_dir.join("project.json");
        for artifact in [
            manifest_path.clone(),
            manifest_path.with_extension("json.bak"),
            manifest_path.with_extension("json.tmp"),
        ] {
            self.validate_optional_project_file(project_dir, &artifact)?;
        }
        project_fs::recover_manifest_backup(&manifest_path).map_err(storage_failure)?;
        self.validate_required_project_file(project_dir, &manifest_path)?;
        let sqlite_path = project_dir.join("project.sqlite");
        self.validate_project_database(project_dir, &sqlite_path)?;
        let manifest: project_fs::ProjectManifest =
            serde_json::from_slice(&fs::read(&manifest_path).map_err(storage_failure)?)
                .map_err(storage_failure)?;
        let indexed_manifest = crate::storage::read_project_manifest(&sqlite_path)
            .map_err(storage_failure)?
            .ok_or(ServiceError::Storage)?;
        self.validate_managed_manifest(&manifest, project_dir, true)?;
        self.validate_managed_manifest(&indexed_manifest, project_dir, true)?;
        if manifest.id != project_id
            || indexed_manifest.id != project_id
            || indexed_manifest.source_dataset_key != manifest.source_dataset_key
            || indexed_manifest.format != manifest.format
            || indexed_manifest.root_path != manifest.root_path
        {
            return Err(ServiceError::Storage);
        }
        Ok(ActualProjectState {
            manifest_name: manifest.name,
            indexed_name: indexed_manifest.name,
            description: self
                .storage
                .description(project_id)
                .map_err(storage_failure)?,
        })
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

    fn validated_active_manifests(&self) -> Result<Vec<project_fs::ProjectManifest>, ServiceError> {
        let mut manifests = Vec::new();
        for entry in fs::read_dir(self.projects_dir.as_ref()).map_err(storage_failure)? {
            let entry = entry.map_err(storage_failure)?;
            let Some(project_id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if validate_project_id(&project_id).is_err() {
                continue;
            }
            match self.existing_project_dir(self.projects_dir.as_ref(), &project_id) {
                Ok(Some(project_dir)) => match self.ensure_project_manifest(&project_dir, true) {
                    Ok(manifest) => manifests.push(manifest),
                    Err(error) => {
                        tracing::error!(
                            project_id,
                            ?error,
                            "invalid managed project was excluded from listing"
                        );
                    }
                },
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
        Ok(manifests)
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
        if is_symlink_or_reparse(&metadata) || !metadata.is_dir() {
            return Err(ServiceError::Storage);
        }
        let canonical_candidate = canonical_existing(&candidate).map_err(storage_failure)?;
        if !canonical_project_path_is_direct_child(root, &canonical_candidate) {
            return Err(ServiceError::Storage);
        }
        Ok(Some(canonical_candidate))
    }

    fn validate_optional_project_file(
        &self,
        project_dir: &Path,
        path: &Path,
    ) -> Result<Option<PathBuf>, ServiceError> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(storage_failure(error)),
        };
        if is_symlink_or_reparse(&metadata) || !metadata.is_file() {
            return Err(ServiceError::Storage);
        }
        let canonical = canonical_existing(path).map_err(storage_failure)?;
        if canonical.parent() != Some(project_dir) {
            return Err(ServiceError::Storage);
        }
        Ok(Some(canonical))
    }

    fn validate_required_project_file(
        &self,
        project_dir: &Path,
        path: &Path,
    ) -> Result<PathBuf, ServiceError> {
        self.validate_optional_project_file(project_dir, path)?
            .ok_or(ServiceError::Storage)
    }

    fn validate_project_database(
        &self,
        project_dir: &Path,
        path: &Path,
    ) -> Result<(), ServiceError> {
        self.validate_required_project_file(project_dir, path)?;
        for suffix in ["-journal", "-wal", "-shm"] {
            let mut sidecar = path.as_os_str().to_os_string();
            sidecar.push(suffix);
            self.validate_optional_project_file(project_dir, Path::new(&sidecar))?;
        }
        crate::storage::validate_project_database_artifacts(path).map_err(storage_failure)
    }

    fn validate_managed_manifest(
        &self,
        manifest: &project_fs::ProjectManifest,
        project_dir: &Path,
        require_active_root: bool,
    ) -> Result<(), ServiceError> {
        let project_id = project_dir
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(ServiceError::Storage)?;
        if manifest.id != project_id
            || manifest.source_dataset_key != "local-demo"
            || !matches!(
                manifest.format.as_str(),
                "yolo-detect" | "yolo-seg" | "image-classification"
            )
        {
            return Err(ServiceError::Storage);
        }
        if require_active_root {
            let manifest_root =
                canonical_existing(Path::new(&manifest.root_path)).map_err(storage_failure)?;
            if manifest_root != project_dir {
                return Err(ServiceError::Storage);
            }
        } else {
            let manifest_root = Path::new(&manifest.root_path);
            let manifest_parent = manifest_root.parent().ok_or(ServiceError::Storage)?;
            let canonical_parent =
                canonical_existing(manifest_parent).map_err(|_| ServiceError::Storage)?;
            if canonical_parent.as_path() != self.projects_dir.as_path()
                || manifest_root.file_name().and_then(|name| name.to_str()) != Some(project_id)
            {
                return Err(ServiceError::Storage);
            }
        }
        Ok(())
    }

    fn get_project_locked(&self, project_id: &str) -> Result<DatasetProject, ServiceError> {
        let active_dir = self
            .existing_project_dir(self.projects_dir.as_ref(), project_id)?
            .ok_or(ServiceError::NotFound)?;
        let manifest = self.ensure_project_manifest(&active_dir, true)?;
        let mut project = self
            .repository
            .workspace_dataset_projects_from_manifests(vec![manifest])
            .into_iter()
            .next()
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
        self.ensure_project_manifest(&actual_dir, true).map(|_| ())
    }

    fn prepare_owned_project_directory(
        &self,
        project_id: &str,
        ownership_marker: &str,
    ) -> Result<(), ServiceError> {
        let project_dir = self.projects_dir.join(project_id);
        match fs::create_dir(&project_dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(ServiceError::Conflict);
            }
            Err(error) => return Err(storage_failure(error)),
        }
        let project_dir = self
            .existing_project_dir(self.projects_dir.as_ref(), project_id)?
            .ok_or(ServiceError::Storage)?;
        let marker_path = project_dir.join(CREATE_OWNERSHIP_FILE);
        let mut marker = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&marker_path)
            .map_err(storage_failure)?;
        marker
            .write_all(ownership_marker.as_bytes())
            .and_then(|()| marker.sync_all())
            .map_err(storage_failure)?;
        self.validate_required_project_file(&project_dir, &marker_path)?;
        Ok(())
    }

    fn read_create_ownership_marker(
        &self,
        project_dir: &Path,
    ) -> Result<Option<String>, ServiceError> {
        let marker_path = project_dir.join(CREATE_OWNERSHIP_FILE);
        let Some(marker_path) = self.validate_optional_project_file(project_dir, &marker_path)?
        else {
            return Ok(None);
        };
        let marker = fs::read_to_string(marker_path).map_err(storage_failure)?;
        if marker.is_empty() || marker.len() > 256 || marker.chars().any(char::is_control) {
            return Err(ServiceError::Storage);
        }
        Ok(Some(marker))
    }

    fn cleanup_partial_created_project(&self, project_id: &str) -> Result<(), ServiceError> {
        let Some(project_dir) =
            self.existing_project_dir(self.projects_dir.as_ref(), project_id)?
        else {
            return Ok(());
        };
        fs::remove_dir_all(project_dir).map_err(storage_failure)
    }

    fn compensate_failed_update(
        &self,
        project_id: &str,
        active_dir: &Path,
        old: &ProjectSnapshot,
        operation: &AuditOperation,
        error: ServiceError,
    ) {
        if let Err(rollback_error) = self.persist_project_name(project_id, active_dir, &old.name) {
            tracing::error!(
                ?rollback_error,
                "project update rollback could not restore project name"
            );
            return;
        }
        if let Err(rollback_error) = self.storage.fail_update_and_restore_metadata(
            project_id,
            old.description.as_deref(),
            operation,
            failure_message(error),
        ) {
            tracing::error!(%rollback_error, "project update rollback remains pending");
            return;
        }
        match self.read_actual_project_state(active_dir, project_id) {
            Ok(actual) if actual.matches(old) => {}
            Ok(_) => tracing::error!("project update rollback verification failed"),
            Err(verification_error) => {
                tracing::error!(
                    ?verification_error,
                    "project update rollback could not be verified"
                );
            }
        }
    }

    fn persist_project_name(
        &self,
        project_id: &str,
        active_dir: &Path,
        name: &str,
    ) -> Result<(), ServiceError> {
        let manifest_path = active_dir.join("project.json");
        self.validate_required_project_file(active_dir, &manifest_path)?;
        let sqlite_path = active_dir.join("project.sqlite");
        self.validate_project_database(active_dir, &sqlite_path)?;
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
        let payload = lifecycle_payload(project_id)?;
        self.storage
            .begin_audit(audit(
                request_id,
                role,
                action,
                Some(project_id),
                message,
                &payload,
            ))
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
        self.fail_audit_best_effort(&operation, failure_message(error));
        Ok(())
    }

    fn complete_audit_best_effort(&self, operation: &AuditOperation, message: &str) {
        if let Err(error) = self.storage.complete_audit(operation, message) {
            tracing::error!(%error, "failed to complete project audit operation");
        }
    }

    fn fail_audit_best_effort(&self, operation: &AuditOperation, message: &str) {
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

fn acquire_data_root_lease(data_dir: &Path) -> Result<Arc<DataRootLease>, DataRootLeaseError> {
    let registry = DATA_ROOT_LEASES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut leases = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let lock_path = data_dir.join(".image-annotation-server.lock");
    if let Some(lease) = leases.get(data_dir).and_then(Weak::upgrade) {
        validate_regular_file_within(data_dir, &lock_path)
            .map_err(|_| DataRootLeaseError::Storage)?;
        return Ok(lease);
    }
    match fs::symlink_metadata(&lock_path) {
        Ok(metadata) if is_symlink_or_reparse(&metadata) || !metadata.is_file() => {
            return Err(DataRootLeaseError::Storage);
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(DataRootLeaseError::Storage),
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|_| DataRootLeaseError::Storage)?;
    lock_data_root_file(&file)?;
    validate_regular_file_within(data_dir, &lock_path).map_err(|_| DataRootLeaseError::Storage)?;
    let lease = Arc::new(DataRootLease { file });
    leases.insert(data_dir.to_path_buf(), Arc::downgrade(&lease));
    Ok(lease)
}

fn lock_data_root_file(file: &File) -> Result<(), DataRootLeaseError> {
    const RELEASE_RETRIES: usize = 50;
    const RELEASE_RETRY_DELAY: Duration = Duration::from_millis(2);

    for attempt in 0..=RELEASE_RETRIES {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(()),
            Err(error) if lock_is_contended(&error) && attempt < RELEASE_RETRIES => {
                thread::sleep(RELEASE_RETRY_DELAY);
            }
            Err(error) if lock_is_contended(&error) => return Err(DataRootLeaseError::InUse),
            Err(_) => return Err(DataRootLeaseError::Storage),
        }
    }
    Err(DataRootLeaseError::InUse)
}

fn lock_is_contended(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    {
        matches!(error.raw_os_error(), Some(32 | 33))
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn validate_regular_file_within(root: &Path, path: &Path) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if is_symlink_or_reparse(&metadata) || !metadata.is_file() {
        return Err("path is not a regular managed file".to_string());
    }
    let canonical = canonical_existing(path)?;
    if canonical_path_is_within(root, &canonical) {
        Ok(canonical)
    } else {
        Err("managed file is outside its configured root".to_string())
    }
}

fn canonical_path_is_within(root: &Path, candidate: &Path) -> bool {
    candidate != root && candidate.starts_with(root)
}

fn canonical_project_path_is_direct_child(root: &Path, candidate: &Path) -> bool {
    candidate.starts_with(root) && candidate.parent() == Some(root)
}

fn canonical_existing(path: &Path) -> Result<PathBuf, String> {
    fs::canonicalize(path).map_err(|error| error.to_string())
}

fn ensure_data_root_directory(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if is_symlink_or_reparse(&metadata) || !metadata.is_dir() => {
            return Err("configured data root is not a regular directory".to_string());
        }
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    fs::create_dir_all(path).map_err(|error| error.to_string())?;
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if is_symlink_or_reparse(&metadata) || !metadata.is_dir() {
        Err("configured data root is not a regular directory".to_string())
    } else {
        Ok(())
    }
}

fn ensure_managed_subdirectory(root: &Path, components: &[&str]) -> Result<PathBuf, String> {
    let mut current = root.to_path_buf();
    for component in components {
        let candidate = current.join(component);
        match fs::symlink_metadata(&candidate) {
            Ok(metadata) if is_symlink_or_reparse(&metadata) || !metadata.is_dir() => {
                return Err("managed path component is not a regular directory".to_string());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::create_dir(&candidate) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
            Err(error) => return Err(error.to_string()),
        }
        let metadata = fs::symlink_metadata(&candidate).map_err(|error| error.to_string())?;
        if is_symlink_or_reparse(&metadata) || !metadata.is_dir() {
            return Err("managed path component is not a regular directory".to_string());
        }
        let canonical = canonical_existing(&candidate)?;
        if !canonical_path_is_within(root, &canonical) || canonical.parent() != Some(&current) {
            return Err("managed directory is outside its configured root".to_string());
        }
        current = canonical;
    }
    Ok(current)
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
    payload: &'a str,
) -> AuditEntry<'a> {
    AuditEntry {
        request_id,
        role: role_name(role),
        action,
        project_id,
        image_id: None,
        message,
        payload,
    }
}

fn lifecycle_payload(project_id: &str) -> Result<String, ServiceError> {
    serde_json::to_string(&LifecycleOperationPayload {
        project_id: project_id.to_string(),
    })
    .map_err(storage_failure)
}

fn next_create_ownership_marker() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "create-owner-{timestamp}-{}-{}",
        std::process::id(),
        CREATE_OWNERSHIP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn lifecycle_operation_expectation(state: TrashState) -> (&'static str, &'static str) {
    match state {
        TrashState::Trashing => ("delete_project", "pending"),
        TrashState::Trashed => ("delete_project", "completed"),
        TrashState::Restoring => ("restore_project", "pending"),
    }
}

fn validate_lifecycle_record_payload(
    record: &OperationRecord,
    project_id: &str,
) -> Result<(), ServiceError> {
    let payload: LifecycleOperationPayload =
        serde_json::from_str(&record.payload).map_err(|_| ServiceError::Storage)?;
    if payload.project_id == project_id {
        Ok(())
    } else {
        Err(ServiceError::Storage)
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

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Cursor, Write},
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock, Weak,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use image::codecs::jpeg::JpegEncoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;

use crate::{
    datasets,
    domain::{
        AnnotationObject, AnnotationState, AnnotationVersion, BBox, DatasetProject, Point,
        SampleRepository,
    },
    importers::{
        adapter::{source_version_matches, PrepareSourceSyncError, PreparedSourceSync},
        labelme, voc_adapter, yolo_adapter,
    },
    project_fs,
    storage::{
        self as project_storage, AnnotationRevisionExpectation, RemoteMutationError,
        RemoteWorkflowState, SampleMutationEvidence, StoredImage, StoredImageSource, StoredSample,
        StoredSampleClass, StoredSampleFilter,
    },
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
const MAX_SAMPLE_TEXT_CHARS: usize = 2_000;
const MAX_SAMPLE_QUERY_CHARS: usize = 256;
const MAX_ANNOTATION_OBJECTS: usize = 1_000;
const MAX_ANNOTATION_ID_CHARS: usize = 128;
const MAX_ANNOTATION_LABEL_CHARS: usize = 256;
const MAX_ANNOTATION_ATTRIBUTES_BYTES: usize = 16 * 1024;
const MAX_REVIEW_NOTE_CHARS: usize = 2_000;
const THUMBNAIL_EDGE: u32 = 320;
const CREATE_OWNERSHIP_FILE: &str = ".remote-create-owner";
const ANNOTATION_TRANSACTION_VERSION: u8 = 2;
static PROJECT_MUTATION_LOCK: Mutex<()> = Mutex::new(());
static DATA_ROOT_LEASES: OnceLock<Mutex<HashMap<PathBuf, Weak<DataRootLease>>>> = OnceLock::new();
static CREATE_OWNERSHIP_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static ANNOTATION_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

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
    #[serde(default)]
    ownership_marker: Option<String>,
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
    AnnotationValidation,
    NotFound,
    Conflict,
    RevisionConflict,
    UnsupportedMedia,
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

#[derive(Debug, Clone, Default)]
pub(super) struct SampleQueryOptions {
    pub offset: u32,
    pub limit: u32,
    pub split: Option<String>,
    pub status: Option<String>,
    pub qa_status: Option<String>,
    pub class_id: Option<u32>,
    pub label: Option<String>,
    pub query: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SamplePatch {
    pub split: Option<String>,
    pub status: Option<String>,
    pub qa_status: Option<String>,
    pub review_note: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct SampleMetadataSnapshot {
    split: String,
    status: String,
    qa_status: String,
    review_note: Option<String>,
}

impl SampleMetadataSnapshot {
    fn from_stored_image(image: &StoredImage) -> Self {
        Self {
            split: image.split.clone(),
            status: image.status.clone(),
            qa_status: image.qa_status.clone(),
            review_note: image.review_note.clone(),
        }
    }

    fn apply_patch(&self, patch: &SamplePatch) -> Self {
        Self {
            split: patch.split.clone().unwrap_or_else(|| self.split.clone()),
            status: patch.status.clone().unwrap_or_else(|| self.status.clone()),
            qa_status: patch
                .qa_status
                .clone()
                .unwrap_or_else(|| self.qa_status.clone()),
            review_note: patch
                .review_note
                .clone()
                .or_else(|| self.review_note.clone()),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SampleMutationPayload<'a> {
    project_id: &'a str,
    image_id: &'a str,
    before: &'a SampleMetadataSnapshot,
    patch: &'a SamplePatch,
    after: &'a SampleMetadataSnapshot,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingSampleMutationPayload {
    project_id: String,
    image_id: String,
    #[serde(default)]
    before: Option<SampleMetadataSnapshot>,
    patch: SamplePatch,
    #[serde(default)]
    after: Option<SampleMetadataSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnnotationOperationPayload {
    project_id: String,
    image_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    object_count: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    decision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

struct AnnotationWorkflowRequest<'a> {
    project_id: &'a str,
    sample_id: &'a str,
    request_id: &'a str,
    role: Role,
    action: &'static str,
    payload: AnnotationOperationPayload,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AnnotationHistory {
    items: Vec<AnnotationVersion>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct AnnotationWorkflowView {
    image_id: String,
    status: String,
    qa_status: String,
    review_note: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct SampleClassView {
    id: u32,
    label: String,
    object_count: u32,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(super) struct SampleView {
    id: String,
    file_name: String,
    width: u32,
    height: u32,
    split: String,
    status: String,
    qa_status: String,
    review_note: Option<String>,
    annotation_revision: Option<String>,
    annotation_updated_at: Option<String>,
    annotation_count: u32,
    classes: Vec<SampleClassView>,
    tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(super) struct SamplePage {
    offset: u32,
    limit: u32,
    total: u64,
    items: Vec<SampleView>,
}

#[derive(Debug)]
pub(super) enum AssetSource {
    Bytes(Vec<u8>),
    File(File),
}

#[derive(Debug)]
pub(super) struct AssetPayload {
    pub source: AssetSource,
    pub size: u64,
    pub content_type: &'static str,
    pub etag: String,
    pub download_name: String,
}

struct SampleProjectContext {
    manifest: project_fs::ProjectManifest,
    sqlite: PathBuf,
    original_dir: PathBuf,
    thumbnail_dir: PathBuf,
    managed_annotations_dir: PathBuf,
    annotation_transactions_dir: PathBuf,
    annotation_transaction_quarantine_dir: PathBuf,
}

struct NativeAnnotationTarget {
    image_path: PathBuf,
    annotation_path: PathBuf,
    relative_path: String,
    relative_annotation_path: String,
    expected_source_version: String,
    external_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnnotationFileJournal {
    version: u8,
    operation_id: String,
    project_id: String,
    image_id: String,
    image_relative_path: String,
    sidecar_relative_path: String,
    expected_source_version: String,
    managed_had_original: bool,
    sidecar_had_original: bool,
    #[serde(default)]
    managed_old_sha256: Option<String>,
    #[serde(default)]
    managed_new_sha256: Option<String>,
    #[serde(default)]
    sidecar_old_sha256: Option<String>,
    #[serde(default)]
    sidecar_new_sha256: String,
}

struct AnnotationFileTransaction {
    directory: PathBuf,
    journal: AnnotationFileJournal,
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
            .reconcile_orphan_annotation_file_transactions()
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

    pub(super) fn list_samples(
        &self,
        project_id: &str,
        query: SampleQueryOptions,
    ) -> Result<SamplePage, ServiceError> {
        self.ensure_configured_root()?;
        validate_project_id(project_id)?;
        validate_sample_query(&query)?;
        let _mutation_guard = mutation_guard();
        self.query_samples_locked(project_id, query)
    }

    pub(super) fn get_sample(
        &self,
        project_id: &str,
        sample_id: &str,
    ) -> Result<SampleView, ServiceError> {
        self.ensure_configured_root()?;
        validate_project_id(project_id)?;
        validate_sample_id(sample_id)?;
        let _mutation_guard = mutation_guard();
        self.get_sample_locked(project_id, sample_id)
    }

    pub(super) fn update_sample(
        &self,
        project_id: &str,
        sample_id: &str,
        request_id: &str,
        role: Role,
        patch: SamplePatch,
    ) -> Result<SampleView, ServiceError> {
        self.ensure_configured_root()?;
        validate_project_id(project_id)?;
        validate_sample_id(sample_id)?;
        validate_sample_patch(&patch)?;
        let _mutation_guard = mutation_guard();
        let context = self.sample_project_context(project_id)?;
        let previous = self.stored_sample(&context, sample_id)?;
        let before = SampleMetadataSnapshot::from_stored_image(&previous.image);
        let after = before.apply_patch(&patch);
        let operation_payload = serde_json::to_string(&SampleMutationPayload {
            project_id,
            image_id: sample_id,
            before: &before,
            patch: &patch,
            after: &after,
        })
        .map_err(storage_failure)?;
        let operation = self
            .storage
            .begin_audit(AuditEntry {
                request_id,
                role: role_name(role),
                action: "update_sample_metadata",
                project_id: Some(project_id),
                image_id: Some(sample_id),
                message: "sample metadata update requested",
                payload: &operation_payload,
            })
            .map_err(storage_failure)?;
        let updated = match project_storage::update_sample_metadata(
            &context.sqlite,
            sample_id,
            &operation.operation_id,
            patch.split.as_deref(),
            patch.status.as_deref(),
            patch.qa_status.as_deref(),
            patch.review_note.as_deref(),
        )
        .map_err(storage_failure)
        {
            Ok(updated) => updated,
            Err(error) => {
                self.fail_audit_best_effort(&operation, failure_message(error));
                return Err(error);
            }
        };
        if !updated {
            self.fail_audit_best_effort(&operation, failure_message(ServiceError::NotFound));
            return Err(ServiceError::NotFound);
        }
        let updated_sample = self
            .refresh_sample_classes(&context)
            .and_then(|()| self.stored_sample(&context, sample_id))
            .and_then(sample_view);
        let updated_sample = match updated_sample {
            Ok(updated_sample) => updated_sample,
            Err(error) => {
                return match self.restore_sample_metadata(&context, sample_id, &before, &operation)
                {
                    Ok(()) => {
                        self.fail_audit_best_effort(&operation, failure_message(error));
                        Err(error)
                    }
                    Err(rollback_error) => {
                        tracing::error!(
                            ?rollback_error,
                            "failed to compensate sample metadata update"
                        );
                        self.note_pending_audit_best_effort(
                            &operation,
                            "sample metadata rollback failed after response validation",
                        );
                        Err(ServiceError::Storage)
                    }
                };
            }
        };
        if let Err(error) = self
            .storage
            .complete_audit(&operation, "sample metadata updated")
        {
            tracing::error!(%error, "failed to complete sample metadata audit operation");
            self.note_pending_audit_best_effort(
                &operation,
                "sample metadata applied; audit completion deferred",
            );
        }
        Ok(updated_sample)
    }

    pub(super) fn annotation_state(
        &self,
        project_id: &str,
        sample_id: &str,
    ) -> Result<AnnotationState, ServiceError> {
        self.ensure_configured_root()?;
        validate_project_id(project_id)?;
        validate_sample_id(sample_id)?;
        let _mutation_guard = mutation_guard();
        let context = self.sample_project_context(project_id)?;
        self.annotation_state_in_context(&context, sample_id)
    }

    pub(super) fn annotation_history(
        &self,
        project_id: &str,
        sample_id: &str,
    ) -> Result<AnnotationHistory, ServiceError> {
        self.ensure_configured_root()?;
        validate_project_id(project_id)?;
        validate_sample_id(sample_id)?;
        let _mutation_guard = mutation_guard();
        let context = self.sample_project_context(project_id)?;
        self.stored_sample(&context, sample_id)?;
        let items = project_storage::read_annotation_versions(&context.sqlite, sample_id)
            .map_err(storage_failure)?
            .into_iter()
            .map(|record| {
                Ok(AnnotationVersion {
                    id: record.id,
                    image_id: record.image_id,
                    revision: record.revision,
                    objects: serde_json::from_str(&record.object_json)
                        .map_err(|_| ServiceError::Storage)?,
                    created_at: record.created_at,
                })
            })
            .collect::<Result<Vec<_>, ServiceError>>()?;
        Ok(AnnotationHistory { items })
    }

    pub(super) fn save_annotations(
        &self,
        project_id: &str,
        sample_id: &str,
        request_id: &str,
        role: Role,
        expectation: AnnotationRevisionExpectation,
        raw_objects: Vec<serde_json::Value>,
    ) -> Result<AnnotationState, ServiceError> {
        self.ensure_configured_root()?;
        validate_project_id(project_id)?;
        validate_sample_id(sample_id)?;
        let _mutation_guard = mutation_guard();
        let context = self.sample_project_context(project_id)?;
        let sample = self.stored_sample(&context, sample_id)?;
        if context.manifest.format == "image-classification" {
            return Err(ServiceError::AnnotationValidation);
        }
        let objects = self.validate_annotation_objects(&context, &sample.image, raw_objects)?;
        let native_target = self.native_annotation_target(&context, &sample.image)?;
        let prepared_native = self.prepare_native_annotation(&context, &objects, &native_target)?;
        let object_json = serde_json::to_string(&objects).map_err(storage_failure)?;
        let operation_payload = serde_json::to_string(&AnnotationOperationPayload {
            project_id: project_id.to_string(),
            image_id: sample_id.to_string(),
            object_count: Some(objects.len()),
            content_sha256: Some(sha256_hex(object_json.as_bytes())),
            decision: None,
            note: None,
        })
        .map_err(storage_failure)?;
        let operation = self
            .storage
            .begin_audit(AuditEntry {
                request_id,
                role: role_name(role),
                action: "save_annotations",
                project_id: Some(project_id),
                image_id: Some(sample_id),
                message: "annotation save requested",
                payload: &operation_payload,
            })
            .map_err(storage_failure)?;
        let mut file_transaction = match self.prepare_annotation_file_transaction(
            &context,
            project_id,
            sample_id,
            &operation.operation_id,
            &native_target,
            &prepared_native,
        ) {
            Ok(transaction) => transaction,
            Err(error) => {
                self.fail_audit_best_effort(&operation, failure_message(error));
                return Err(error);
            }
        };
        if let Err(error) = self.apply_native_annotation_transaction(&context, &file_transaction) {
            if self
                .rollback_annotation_file_transaction(&context, &file_transaction)
                .and_then(|()| self.cleanup_annotation_file_transaction(&file_transaction))
                .is_ok()
            {
                self.fail_audit_best_effort(&operation, failure_message(error));
            } else {
                self.note_pending_audit_best_effort(
                    &operation,
                    "annotation file replacement requires startup rollback",
                );
            }
            return Err(error);
        }
        let source_version = native_source_version(
            &context.manifest.format,
            &context.original_dir,
            &native_target.image_path,
        )?;
        let source = StoredImageSource {
            image_id: sample_id.to_string(),
            relative_path: native_target.relative_path.clone(),
            external_id: native_target.external_id.clone(),
            annotation_path: Some(native_target.relative_annotation_path.clone()),
            source_version,
        };
        let saved = match project_storage::save_remote_annotation_payload(
            &context.sqlite,
            sample_id,
            &expectation,
            &object_json,
            &operation.operation_id,
            &source,
        ) {
            Ok(saved) => saved,
            Err(error) => {
                let service_error = remote_mutation_error(error);
                if self
                    .rollback_annotation_file_transaction(&context, &file_transaction)
                    .and_then(|()| self.cleanup_annotation_file_transaction(&file_transaction))
                    .is_ok()
                {
                    self.fail_audit_best_effort(&operation, failure_message(service_error));
                } else {
                    self.note_pending_audit_best_effort(
                        &operation,
                        "annotation database rejection requires startup file rollback",
                    );
                }
                return Err(service_error);
            }
        };
        let state = AnnotationState {
            image_id: sample_id.to_string(),
            revision: Some(saved.revision.clone()),
            objects,
            status: "草稿".to_string(),
            updated_at: Some(saved.saved_at.clone()),
        };
        if let Err(error) = self
            .stage_managed_annotation(&context, &mut file_transaction, &state)
            .and_then(|()| self.apply_managed_annotation_transaction(&context, &file_transaction))
        {
            let database_rolled_back = project_storage::compensate_remote_annotation_save(
                &context.sqlite,
                sample_id,
                &operation.operation_id,
                &saved,
            )
            .unwrap_or(false);
            let files_rolled_back = self
                .rollback_annotation_file_transaction(&context, &file_transaction)
                .is_ok();
            if database_rolled_back
                && files_rolled_back
                && self
                    .cleanup_annotation_file_transaction(&file_transaction)
                    .is_ok()
            {
                self.fail_audit_best_effort(
                    &operation,
                    "annotation managed persistence failed and was compensated",
                );
            } else {
                self.note_pending_audit_best_effort(
                    &operation,
                    "annotation persistence compensation requires startup reconciliation",
                );
            }
            return Err(error);
        }
        match self.storage.complete_audit(&operation, "annotation saved") {
            Ok(()) => {
                if let Err(error) = self.cleanup_annotation_file_transaction(&file_transaction) {
                    tracing::warn!(
                        ?error,
                        operation_id = %operation.operation_id,
                        "annotation transaction cleanup is deferred"
                    );
                }
            }
            Err(error) => {
                tracing::error!(
                    %error,
                    operation_id = %operation.operation_id,
                    "failed to complete annotation audit operation"
                );
            }
        }
        Ok(state)
    }

    pub(super) fn submit_annotations(
        &self,
        project_id: &str,
        sample_id: &str,
        request_id: &str,
        role: Role,
    ) -> Result<AnnotationWorkflowView, ServiceError> {
        self.annotation_workflow_mutation(
            AnnotationWorkflowRequest {
                project_id,
                sample_id,
                request_id,
                role,
                action: "submit_annotations",
                payload: AnnotationOperationPayload {
                    project_id: project_id.to_string(),
                    image_id: sample_id.to_string(),
                    object_count: None,
                    content_sha256: None,
                    decision: None,
                    note: None,
                },
            },
            |sqlite, image_id, operation_id| {
                project_storage::submit_remote_annotation(sqlite, image_id, operation_id)
            },
        )
    }

    pub(super) fn review_annotations(
        &self,
        project_id: &str,
        sample_id: &str,
        request_id: &str,
        role: Role,
        decision: &str,
        note: &str,
    ) -> Result<AnnotationWorkflowView, ServiceError> {
        if !matches!(decision, "approved" | "rejected")
            || note.chars().count() > MAX_REVIEW_NOTE_CHARS
            || note
                .chars()
                .any(|character| character.is_control() && character != '\n' && character != '\t')
        {
            return Err(ServiceError::AnnotationValidation);
        }
        self.annotation_workflow_mutation(
            AnnotationWorkflowRequest {
                project_id,
                sample_id,
                request_id,
                role,
                action: "review_annotations",
                payload: AnnotationOperationPayload {
                    project_id: project_id.to_string(),
                    image_id: sample_id.to_string(),
                    object_count: None,
                    content_sha256: None,
                    decision: Some(decision.to_string()),
                    note: Some(note.to_string()),
                },
            },
            |sqlite, image_id, operation_id| {
                project_storage::review_remote_annotation(
                    sqlite,
                    image_id,
                    operation_id,
                    decision,
                    note,
                )
            },
        )
    }

    pub(super) fn sample_content(
        &self,
        project_id: &str,
        sample_id: &str,
    ) -> Result<AssetPayload, ServiceError> {
        self.ensure_configured_root()?;
        validate_project_id(project_id)?;
        validate_sample_id(sample_id)?;
        let _mutation_guard = mutation_guard();
        let context = self.sample_project_context(project_id)?;
        let sample = self.stored_sample(&context, sample_id)?;
        let path = self.resolve_sample_asset(&context, &sample.image.file_name)?;
        let file = OpenOptions::new()
            .read(true)
            .open(&path)
            .map_err(storage_failure)?;
        let metadata = file.metadata().map_err(storage_failure)?;
        Ok(AssetPayload {
            content_type: image_content_type(&path)?,
            etag: metadata_etag(&metadata),
            download_name: indexed_download_name(&sample.image.file_name, sample_id)?,
            size: metadata.len(),
            source: AssetSource::File(file),
        })
    }

    pub(super) fn sample_thumbnail(
        &self,
        project_id: &str,
        sample_id: &str,
    ) -> Result<AssetPayload, ServiceError> {
        self.ensure_configured_root()?;
        validate_project_id(project_id)?;
        validate_sample_id(sample_id)?;
        let _mutation_guard = mutation_guard();
        let context = self.sample_project_context(project_id)?;
        let sample = self.stored_sample(&context, sample_id)?;
        let original_path = self.resolve_sample_asset(&context, &sample.image.file_name)?;
        let source = fs::read(&original_path).map_err(storage_failure)?;
        let sample_cache_prefix = sha256_hex(sample_id.as_bytes());
        let source_fingerprint = sha256_hex(&source);
        let cache_name = format!("{sample_cache_prefix}-{source_fingerprint}.jpg");
        let cache_path = context.thumbnail_dir.join(cache_name);
        self.cleanup_stale_sample_thumbnails(
            &context.thumbnail_dir,
            &sample_cache_prefix,
            &cache_path,
        )?;
        let bytes = match fs::symlink_metadata(&cache_path) {
            Ok(_) => {
                self.validate_managed_asset_file(&context.thumbnail_dir, &cache_path)?;
                fs::read(&cache_path).map_err(storage_failure)?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let decoded =
                    image::load_from_memory(&source).map_err(|_| ServiceError::UnsupportedMedia)?;
                let thumbnail = decoded.thumbnail(THUMBNAIL_EDGE, THUMBNAIL_EDGE);
                let mut bytes = Vec::new();
                JpegEncoder::new_with_quality(Cursor::new(&mut bytes), 85)
                    .encode_image(&thumbnail)
                    .map_err(|_| ServiceError::UnsupportedMedia)?;
                self.persist_thumbnail(&context.thumbnail_dir, &cache_path, &bytes)?;
                bytes
            }
            Err(error) => return Err(storage_failure(error)),
        };
        Ok(AssetPayload {
            etag: format!("\"sha256-{}\"", sha256_hex(&bytes)),
            content_type: "image/jpeg",
            download_name: thumbnail_download_name(&sample.image.file_name, sample_id)?,
            size: bytes.len() as u64,
            source: AssetSource::Bytes(bytes),
        })
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
            ownership_marker: Some(ownership_marker.clone()),
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
        let result =
            datasets::create_dataset_project_in(&self.data_dir, name, dataset_type, demo_template)
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
                "update_sample_metadata" => self.reconcile_pending_sample_update(&record),
                "save_annotations" => self.reconcile_pending_annotation_mutation(
                    &record,
                    "annotation.save",
                    Some("annotation.save.rollback"),
                    true,
                ),
                "submit_annotations" => self.reconcile_pending_annotation_mutation(
                    &record,
                    "annotation.submit",
                    None,
                    false,
                ),
                "review_annotations" => {
                    self.reconcile_pending_annotation_mutation(&record, "qa.review", None, false)
                }
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

    fn reconcile_orphan_annotation_file_transactions(&self) -> Result<(), ServiceError> {
        for entry in fs::read_dir(self.projects_dir.as_ref()).map_err(storage_failure)? {
            let entry = entry.map_err(storage_failure)?;
            let Some(project_id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if validate_project_id(&project_id).is_err() {
                continue;
            }
            let context = match self.sample_project_context(&project_id) {
                Ok(context) => context,
                Err(error) => {
                    tracing::warn!(
                        ?error,
                        project_id,
                        "annotation transaction project is unavailable during startup"
                    );
                    continue;
                }
            };
            for transaction_entry in
                fs::read_dir(&context.annotation_transactions_dir).map_err(storage_failure)?
            {
                let transaction_entry = transaction_entry.map_err(storage_failure)?;
                let metadata =
                    fs::symlink_metadata(transaction_entry.path()).map_err(storage_failure)?;
                if is_symlink_or_reparse(&metadata) || !metadata.is_dir() {
                    return Err(ServiceError::Storage);
                }
                let journal_path = transaction_entry.path().join("journal.json");
                if fs::symlink_metadata(&journal_path)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                {
                    self.mark_unreadable_annotation_transaction_indeterminate(
                        &transaction_entry.path(),
                    );
                    self.quarantine_annotation_file_transaction(
                        &context,
                        &transaction_entry.path(),
                    )?;
                    continue;
                }
                self.validate_required_project_file(&transaction_entry.path(), &journal_path)?;
                let journal_bytes = fs::read(journal_path).map_err(storage_failure)?;
                let journal: AnnotationFileJournal = match serde_json::from_slice(&journal_bytes) {
                    Ok(journal) => journal,
                    Err(error) => {
                        tracing::error!(
                            %error,
                            transaction = %transaction_entry.path().display(),
                            "annotation transaction journal is corrupt"
                        );
                        self.mark_unreadable_annotation_transaction_indeterminate(
                            &transaction_entry.path(),
                        );
                        self.quarantine_annotation_file_transaction(
                            &context,
                            &transaction_entry.path(),
                        )?;
                        continue;
                    }
                };
                let operation = self
                    .storage
                    .operation(&journal.operation_id)
                    .map_err(storage_failure)?;
                if journal.version != ANNOTATION_TRANSACTION_VERSION {
                    if let Some(operation) = operation.as_ref() {
                        if operation.state == "pending" {
                            let audit_operation = AuditOperation {
                                operation_id: operation.operation_id.clone(),
                            };
                            self.mark_sample_reconciliation_indeterminate(
                                &audit_operation,
                                "annotation transaction journal version is unsupported",
                            );
                        } else {
                            self.mark_orphan_annotation_indeterminate(
                                &operation.operation_id,
                                "annotation transaction journal version is unsupported",
                            );
                        }
                    }
                    self.quarantine_annotation_file_transaction(
                        &context,
                        &transaction_entry.path(),
                    )?;
                    continue;
                }
                let Some(operation) = operation else {
                    self.quarantine_annotation_file_transaction(
                        &context,
                        &transaction_entry.path(),
                    )?;
                    continue;
                };
                if operation.state == "pending" {
                    continue;
                }
                if operation.action != "save_annotations"
                    || operation.project_id.as_deref() != Some(journal.project_id.as_str())
                    || operation.image_id.as_deref() != Some(journal.image_id.as_str())
                {
                    self.mark_orphan_annotation_indeterminate(
                        &operation.operation_id,
                        "annotation transaction identity is inconsistent",
                    );
                    continue;
                }
                let transaction =
                    match self.load_annotation_file_transaction(&context, &journal.operation_id) {
                        Ok(Some(transaction)) => transaction,
                        Ok(None) => continue,
                        Err(ServiceError::Conflict | ServiceError::NotFound) => {
                            self.mark_orphan_annotation_indeterminate(
                                &operation.operation_id,
                                "annotation transaction identity or content is inconsistent",
                            );
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                let evidence = project_storage::remote_mutation_evidence(
                    &context.sqlite,
                    &journal.operation_id,
                    &journal.image_id,
                    "annotation.save",
                    Some("annotation.save.rollback"),
                )
                .map_err(storage_failure)?;
                let result = match evidence {
                    SampleMutationEvidence::Committed => {
                        self.recover_committed_annotation_file_transaction(&context, &transaction)
                    }
                    SampleMutationEvidence::Compensated | SampleMutationEvidence::None => {
                        self.recover_rolled_back_annotation_file_transaction(&context, &transaction)
                    }
                    SampleMutationEvidence::Indeterminate => continue,
                };
                if let Err(error) = result {
                    tracing::error!(
                        ?error,
                        operation_id = %journal.operation_id,
                        "orphan annotation file transaction recovery failed"
                    );
                    if error == ServiceError::Conflict {
                        let audit_operation = AuditOperation {
                            operation_id: journal.operation_id.clone(),
                        };
                        if let Err(note_error) = self.storage.mark_orphan_audit_indeterminate(
                            &audit_operation,
                            "annotation file recovery conflicted with external content",
                        ) {
                            tracing::warn!(
                                %note_error,
                                operation_id = %journal.operation_id,
                                "failed to record annotation recovery conflict"
                            );
                        }
                        continue;
                    }
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn mark_unreadable_annotation_transaction_indeterminate(&self, directory: &Path) {
        let Some(directory_name) = directory.file_name().and_then(|name| name.to_str()) else {
            return;
        };
        let operations = match self.storage.pending_operations() {
            Ok(operations) => operations,
            Err(error) => {
                tracing::warn!(%error, "failed to inspect pending annotation operations");
                return;
            }
        };
        for operation in operations {
            if operation.action == "save_annotations"
                && sha256_hex(operation.operation_id.as_bytes()) == directory_name
            {
                let audit_operation = AuditOperation {
                    operation_id: operation.operation_id,
                };
                self.mark_sample_reconciliation_indeterminate(
                    &audit_operation,
                    "annotation transaction journal is unreadable",
                );
            }
        }
    }

    fn mark_orphan_annotation_indeterminate(&self, operation_id: &str, message: &str) {
        let operation = AuditOperation {
            operation_id: operation_id.to_string(),
        };
        if let Err(error) = self
            .storage
            .mark_orphan_audit_indeterminate(&operation, message)
        {
            tracing::warn!(
                %error,
                operation_id,
                "failed to mark orphan annotation transaction indeterminate"
            );
        }
    }

    fn quarantine_annotation_file_transaction(
        &self,
        context: &SampleProjectContext,
        directory: &Path,
    ) -> Result<(), ServiceError> {
        if directory.parent() != Some(&context.annotation_transactions_dir) {
            return Err(ServiceError::Storage);
        }
        let metadata = fs::symlink_metadata(directory).map_err(storage_failure)?;
        if is_symlink_or_reparse(&metadata) || !metadata.is_dir() {
            return Err(ServiceError::Storage);
        }
        let name = directory
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(ServiceError::Storage)?;
        let destination = context.annotation_transaction_quarantine_dir.join(format!(
            "{name}-{}-{}",
            std::process::id(),
            ANNOTATION_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        if fs::symlink_metadata(&destination).is_ok() {
            return Err(ServiceError::Storage);
        }
        fs::rename(directory, &destination).map_err(storage_failure)?;
        sync_directory(&context.annotation_transactions_dir)?;
        sync_directory(&context.annotation_transaction_quarantine_dir)
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
        let operation = AuditOperation {
            operation_id: record.operation_id.clone(),
        };
        let Some(ownership_marker) = payload.ownership_marker.as_deref() else {
            return self
                .storage
                .fail_audit(
                    &operation,
                    "legacy create ownership is unknown; operation was not applied",
                )
                .map_err(storage_failure);
        };
        let active_dir =
            self.existing_project_dir(self.projects_dir.as_ref(), &payload.project_id)?;
        let trash_dir =
            self.existing_project_dir(self.trash_projects_dir.as_ref(), &payload.project_id)?;
        if active_dir.is_some() && trash_dir.is_some() {
            return Err(ServiceError::Conflict);
        }
        match (active_dir, trash_dir) {
            (Some(active_dir), None) => {
                if self.read_create_ownership_marker(&active_dir)?.as_deref()
                    != Some(ownership_marker)
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

    fn reconcile_pending_sample_update(&self, record: &OperationRecord) {
        let operation = AuditOperation {
            operation_id: record.operation_id.clone(),
        };
        let payload = match serde_json::from_str::<PendingSampleMutationPayload>(&record.payload) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!(
                    %error,
                    operation_id = %record.operation_id,
                    "failed to parse pending sample mutation payload"
                );
                self.mark_sample_reconciliation_indeterminate(
                    &operation,
                    "sample mutation evidence is indeterminate because its payload is invalid",
                );
                return;
            }
        };
        if record.project_id.as_deref() != Some(payload.project_id.as_str())
            || record.image_id.as_deref() != Some(payload.image_id.as_str())
            || validate_project_id(&payload.project_id).is_err()
            || validate_sample_id(&payload.image_id).is_err()
        {
            self.mark_sample_reconciliation_indeterminate(
                &operation,
                "sample mutation evidence is indeterminate because its identity is inconsistent",
            );
            return;
        }
        let trusted_payload = match (&payload.before, &payload.after) {
            (Some(before), Some(after)) => {
                validate_sample_patch(&payload.patch).is_ok()
                    && validate_sample_metadata_snapshot(before).is_ok()
                    && validate_sample_metadata_snapshot(after).is_ok()
                    && before.apply_patch(&payload.patch) == *after
            }
            _ => false,
        };
        let context = match self.sample_project_context(&payload.project_id) {
            Ok(context) => context,
            Err(ServiceError::NotFound) => {
                if trusted_payload {
                    self.fail_sample_reconciliation(
                        &operation,
                        "sample metadata update was not committed to project storage",
                    );
                } else {
                    self.mark_sample_reconciliation_indeterminate(
                        &operation,
                        "legacy sample mutation evidence is indeterminate",
                    );
                }
                return;
            }
            Err(error) => {
                tracing::error!(
                    ?error,
                    operation_id = %record.operation_id,
                    "could not inspect pending sample mutation project"
                );
                self.mark_sample_reconciliation_indeterminate(
                    &operation,
                    "sample mutation project evidence is indeterminate",
                );
                return;
            }
        };
        let evidence = match project_storage::sample_mutation_evidence(
            &context.sqlite,
            &record.operation_id,
            &payload.image_id,
        ) {
            Ok(evidence) => evidence,
            Err(error) => {
                tracing::error!(
                    %error,
                    operation_id = %record.operation_id,
                    "could not read pending sample mutation evidence"
                );
                self.mark_sample_reconciliation_indeterminate(
                    &operation,
                    "sample mutation project evidence is indeterminate",
                );
                return;
            }
        };
        match evidence {
            SampleMutationEvidence::Committed => {
                self.complete_sample_reconciliation(&operation, "sample metadata update reconciled")
            }
            SampleMutationEvidence::Compensated => self.fail_sample_reconciliation(
                &operation,
                "sample metadata update was compensated in project storage",
            ),
            SampleMutationEvidence::None if trusted_payload => self.fail_sample_reconciliation(
                &operation,
                "sample metadata update was not committed to project storage",
            ),
            SampleMutationEvidence::None => self.mark_sample_reconciliation_indeterminate(
                &operation,
                "legacy sample mutation evidence is indeterminate",
            ),
            SampleMutationEvidence::Indeterminate => self.mark_sample_reconciliation_indeterminate(
                &operation,
                "sample mutation project evidence is indeterminate",
            ),
        }
    }

    fn reconcile_pending_annotation_mutation(
        &self,
        record: &OperationRecord,
        committed_action: &str,
        compensated_action: Option<&str>,
        repair_native: bool,
    ) {
        let operation = AuditOperation {
            operation_id: record.operation_id.clone(),
        };
        let payload = match serde_json::from_str::<AnnotationOperationPayload>(&record.payload) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!(
                    %error,
                    operation_id = %record.operation_id,
                    "failed to parse pending annotation operation"
                );
                self.mark_sample_reconciliation_indeterminate(
                    &operation,
                    "annotation mutation payload is indeterminate",
                );
                return;
            }
        };
        if record.project_id.as_deref() != Some(payload.project_id.as_str())
            || record.image_id.as_deref() != Some(payload.image_id.as_str())
            || validate_project_id(&payload.project_id).is_err()
            || validate_sample_id(&payload.image_id).is_err()
        {
            self.mark_sample_reconciliation_indeterminate(
                &operation,
                "annotation mutation identity is inconsistent",
            );
            return;
        }
        let context = match self.sample_project_context(&payload.project_id) {
            Ok(context) => context,
            Err(error) => {
                tracing::warn!(
                    ?error,
                    operation_id = %record.operation_id,
                    "pending annotation project evidence is unavailable"
                );
                self.mark_sample_reconciliation_indeterminate(
                    &operation,
                    "annotation project evidence is indeterminate",
                );
                return;
            }
        };
        let evidence = match project_storage::remote_mutation_evidence(
            &context.sqlite,
            &record.operation_id,
            &payload.image_id,
            committed_action,
            compensated_action,
        ) {
            Ok(evidence) => evidence,
            Err(error) => {
                tracing::error!(
                    %error,
                    operation_id = %record.operation_id,
                    "pending annotation project evidence could not be read"
                );
                self.mark_sample_reconciliation_indeterminate(
                    &operation,
                    "annotation project evidence is indeterminate",
                );
                return;
            }
        };
        let file_transaction = if repair_native {
            match self.load_annotation_file_transaction(&context, &record.operation_id) {
                Ok(transaction) => transaction,
                Err(error) => {
                    tracing::error!(
                        ?error,
                        operation_id = %record.operation_id,
                        "annotation file transaction could not be loaded"
                    );
                    self.note_pending_audit_best_effort(
                        &operation,
                        "annotation file recovery remains pending",
                    );
                    return;
                }
            }
        } else {
            None
        };
        if file_transaction.as_ref().is_some_and(|transaction| {
            transaction.journal.operation_id != record.operation_id
                || transaction.journal.project_id != payload.project_id
                || transaction.journal.image_id != payload.image_id
        }) {
            self.note_pending_audit_best_effort(
                &operation,
                "annotation file transaction identity is inconsistent",
            );
            return;
        }
        match evidence {
            SampleMutationEvidence::Committed => {
                if repair_native {
                    match file_transaction.as_ref() {
                        Some(transaction) => {
                            if let Err(error) = self.recover_committed_annotation_file_transaction(
                                &context,
                                transaction,
                            ) {
                                tracing::error!(
                                    ?error,
                                    operation_id = %record.operation_id,
                                    "annotation committed file recovery failed"
                                );
                                self.note_pending_audit_best_effort(
                                    &operation,
                                    "annotation project commit is valid; file recovery remains pending",
                                );
                                return;
                            }
                        }
                        None => {
                            let state = match self
                                .annotation_state_in_context(&context, &payload.image_id)
                            {
                                Ok(state) if state.revision.is_some() => state,
                                _ => {
                                    self.mark_sample_reconciliation_indeterminate(
                                        &operation,
                                        "annotation native state cannot be repaired from project evidence",
                                    );
                                    return;
                                }
                            };
                            if let Err(error) =
                                self.persist_managed_annotation(&context, &payload.image_id, &state)
                            {
                                tracing::error!(
                                    ?error,
                                    operation_id = %record.operation_id,
                                    "legacy annotation managed state repair failed"
                                );
                                self.note_pending_audit_best_effort(
                                    &operation,
                                    "annotation project commit is valid; managed repair remains pending",
                                );
                                return;
                            }
                        }
                    }
                }
                self.complete_sample_reconciliation(
                    &operation,
                    "annotation mutation reconciled from project evidence",
                );
            }
            SampleMutationEvidence::Compensated => {
                if let Some(transaction) = file_transaction.as_ref() {
                    if let Err(error) =
                        self.recover_rolled_back_annotation_file_transaction(&context, transaction)
                    {
                        tracing::error!(
                            ?error,
                            operation_id = %record.operation_id,
                            "annotation compensated file rollback failed"
                        );
                        self.note_pending_audit_best_effort(
                            &operation,
                            "annotation compensation file rollback remains pending",
                        );
                        return;
                    }
                }
                self.fail_sample_reconciliation(
                    &operation,
                    "annotation mutation was compensated in project storage",
                );
            }
            SampleMutationEvidence::None => {
                if let Some(transaction) = file_transaction.as_ref() {
                    if let Err(error) =
                        self.recover_rolled_back_annotation_file_transaction(&context, transaction)
                    {
                        tracing::error!(
                            ?error,
                            operation_id = %record.operation_id,
                            "uncommitted annotation file rollback failed"
                        );
                        self.note_pending_audit_best_effort(
                            &operation,
                            "uncommitted annotation file rollback remains pending",
                        );
                        return;
                    }
                }
                self.mark_sample_reconciliation_indeterminate(
                    &operation,
                    "annotation mutation has no conclusive project evidence",
                );
            }
            SampleMutationEvidence::Indeterminate => self.mark_sample_reconciliation_indeterminate(
                &operation,
                "annotation mutation project evidence is inconsistent",
            ),
        }
    }

    fn complete_sample_reconciliation(&self, operation: &AuditOperation, message: &str) {
        if let Err(error) = self.storage.complete_audit(operation, message) {
            tracing::error!(
                %error,
                operation_id = %operation.operation_id,
                "failed to complete reconciled sample audit"
            );
            self.note_pending_audit_best_effort(
                operation,
                "sample mutation is applied; startup audit completion remains pending",
            );
        }
    }

    fn fail_sample_reconciliation(&self, operation: &AuditOperation, message: &str) {
        if let Err(error) = self.storage.fail_audit(operation, message) {
            tracing::error!(
                %error,
                operation_id = %operation.operation_id,
                "failed to fail reconciled sample audit"
            );
            self.note_pending_audit_best_effort(
                operation,
                "sample mutation reconciliation could not update the audit state",
            );
        }
    }

    fn mark_sample_reconciliation_indeterminate(&self, operation: &AuditOperation, message: &str) {
        if let Err(error) = self.storage.mark_audit_indeterminate(operation, message) {
            tracing::error!(
                %error,
                operation_id = %operation.operation_id,
                "failed to mark sample audit indeterminate"
            );
            self.note_pending_audit_best_effort(
                operation,
                "sample mutation evidence could not be determined during startup",
            );
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
            match self.ensure_project_manifest(&project_dir, require_active_root) {
                Ok(_) if require_active_root => {
                    match validate_managed_directory_chain(&project_dir, &["annotations", "native"])
                    {
                        Ok(annotations_dir) => {
                            if let Err(error) =
                                self.cleanup_annotation_temporary_files(&annotations_dir)
                            {
                                tracing::error!(
                                    project_id,
                                    ?error,
                                    "annotation temporary files could not be cleaned"
                                );
                            }
                        }
                        Err(error) => tracing::error!(
                            project_id,
                            ?error,
                            "annotation directory is not managed"
                        ),
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::error!(project_id, ?error, "project manifest could not be repaired");
                }
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
        let data_dir = canonical_existing(self.data_dir.as_ref()).map_err(storage_failure)?;
        let projects_dir =
            canonical_existing(self.projects_dir.as_ref()).map_err(storage_failure)?;
        let trash_projects_dir =
            canonical_existing(self.trash_projects_dir.as_ref()).map_err(storage_failure)?;
        if data_dir != *self.data_dir
            || projects_dir != *self.projects_dir
            || trash_projects_dir != *self.trash_projects_dir
            || !canonical_path_is_within(&data_dir, &projects_dir)
            || !canonical_path_is_within(&data_dir, &trash_projects_dir)
        {
            Err(ServiceError::Storage)
        } else {
            Ok(())
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
                "yolo-detect" | "yolo-seg" | "voc-detect" | "labelme" | "image-classification"
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

    fn query_samples_locked(
        &self,
        project_id: &str,
        query: SampleQueryOptions,
    ) -> Result<SamplePage, ServiceError> {
        let context = self.sample_project_context(project_id)?;
        self.refresh_sample_classes(&context)?;
        let filter = StoredSampleFilter {
            sample_id: None,
            split: query.split,
            status: query.status,
            qa_status: query.qa_status,
            class_id: query.class_id,
            label: query.label,
            query: query.query,
        };
        let page =
            project_storage::query_samples(&context.sqlite, &filter, query.offset, query.limit)
                .map_err(storage_failure)?;
        Ok(SamplePage {
            offset: query.offset,
            limit: query.limit,
            total: page.total,
            items: page
                .items
                .into_iter()
                .map(sample_view)
                .collect::<Result<Vec<_>, _>>()?,
        })
    }

    fn get_sample_locked(
        &self,
        project_id: &str,
        sample_id: &str,
    ) -> Result<SampleView, ServiceError> {
        let context = self.sample_project_context(project_id)?;
        self.refresh_sample_classes(&context)?;
        sample_view(self.stored_sample(&context, sample_id)?)
    }

    fn stored_sample(
        &self,
        context: &SampleProjectContext,
        sample_id: &str,
    ) -> Result<StoredSample, ServiceError> {
        let page = project_storage::query_samples(
            &context.sqlite,
            &StoredSampleFilter {
                sample_id: Some(sample_id.to_string()),
                ..StoredSampleFilter::default()
            },
            0,
            1,
        )
        .map_err(storage_failure)?;
        page.items.into_iter().next().ok_or(ServiceError::NotFound)
    }

    fn annotation_state_in_context(
        &self,
        context: &SampleProjectContext,
        sample_id: &str,
    ) -> Result<AnnotationState, ServiceError> {
        let sample = self.stored_sample(context, sample_id)?;
        let payload = project_storage::read_annotation_payload(&context.sqlite, sample_id)
            .map_err(storage_failure)?;
        match payload {
            Some(payload) => Ok(AnnotationState {
                image_id: sample_id.to_string(),
                revision: Some(payload.revision),
                objects: serde_json::from_str(&payload.object_json)
                    .map_err(|_| ServiceError::Storage)?,
                status: sample.image.status,
                updated_at: Some(payload.updated_at),
            }),
            None => {
                let objects = self.load_native_annotation_objects(context, &sample.image)?;
                Ok(AnnotationState {
                    image_id: sample_id.to_string(),
                    revision: None,
                    objects,
                    status: sample.image.status,
                    updated_at: None,
                })
            }
        }
    }

    fn load_native_annotation_objects(
        &self,
        context: &SampleProjectContext,
        image: &StoredImage,
    ) -> Result<Vec<AnnotationObject>, ServiceError> {
        let image_path = self.resolve_sample_asset(context, &image.file_name)?;
        let annotation_path = match context.manifest.format.as_str() {
            "yolo-detect" | "yolo-seg" => {
                yolo_adapter::annotation_path(&context.original_dir, &image_path)
            }
            "voc-detect" => voc_adapter::annotation_path(&context.original_dir, &image_path),
            "labelme" => labelme::annotation_path(&context.original_dir, &image_path),
            _ => return Ok(Vec::new()),
        };
        if self
            .read_optional_managed_file(&context.original_dir, &annotation_path)?
            .is_none()
        {
            return Ok(Vec::new());
        }
        let labels = project_storage::read_enabled_classes(&context.sqlite)
            .map_err(storage_failure)?
            .into_iter()
            .map(|class| class.label)
            .collect::<Vec<_>>();
        match context.manifest.format.as_str() {
            "yolo-detect" | "yolo-seg" => yolo_adapter::load_annotations(
                &context.original_dir,
                &image_path,
                &context.manifest.format,
                &labels,
            ),
            "voc-detect" => {
                voc_adapter::load_annotations(&context.original_dir, &image_path, &labels)
            }
            "labelme" => labelme::load_annotations(&context.original_dir, &image_path, &labels)
                .map(|loaded| loaded.objects),
            _ => Ok(Vec::new()),
        }
        .map_err(storage_failure)
    }

    fn annotation_workflow_mutation<F>(
        &self,
        request: AnnotationWorkflowRequest<'_>,
        mutation: F,
    ) -> Result<AnnotationWorkflowView, ServiceError>
    where
        F: FnOnce(
            &Path,
            &str,
            &str,
        ) -> Result<RemoteWorkflowState, project_storage::RemoteMutationError>,
    {
        self.ensure_configured_root()?;
        validate_project_id(request.project_id)?;
        validate_sample_id(request.sample_id)?;
        let _mutation_guard = mutation_guard();
        let context = self.sample_project_context(request.project_id)?;
        self.stored_sample(&context, request.sample_id)?;
        let operation_payload = serde_json::to_string(&request.payload).map_err(storage_failure)?;
        let operation = self
            .storage
            .begin_audit(AuditEntry {
                request_id: request.request_id,
                role: role_name(request.role),
                action: request.action,
                project_id: Some(request.project_id),
                image_id: Some(request.sample_id),
                message: "annotation workflow mutation requested",
                payload: &operation_payload,
            })
            .map_err(storage_failure)?;
        let state = match mutation(&context.sqlite, request.sample_id, &operation.operation_id) {
            Ok(state) => state,
            Err(error) => {
                let service_error = remote_mutation_error(error);
                self.fail_audit_best_effort(&operation, failure_message(service_error));
                return Err(service_error);
            }
        };
        self.complete_audit_best_effort(&operation, "annotation workflow updated");
        Ok(annotation_workflow_view(state))
    }

    fn validate_annotation_objects(
        &self,
        context: &SampleProjectContext,
        image: &StoredImage,
        values: Vec<serde_json::Value>,
    ) -> Result<Vec<AnnotationObject>, ServiceError> {
        if values.len() > MAX_ANNOTATION_OBJECTS {
            return Err(ServiceError::AnnotationValidation);
        }
        let classes = project_storage::read_enabled_classes(&context.sqlite)
            .map_err(storage_failure)?
            .into_iter()
            .map(|class| (class.id, class.label))
            .collect::<HashMap<_, _>>();
        let mut ids = HashSet::new();
        let mut objects = Vec::with_capacity(values.len());
        for value in values {
            let object = parse_annotation_object(value, image, &classes)?;
            if !ids.insert(object.id.clone()) {
                return Err(ServiceError::AnnotationValidation);
            }
            let supported = match context.manifest.format.as_str() {
                "yolo-detect" | "voc-detect" => object.bbox.is_some(),
                "yolo-seg" => object.polygon.is_some(),
                "labelme" => object.bbox.is_some() || object.polygon.is_some(),
                _ => false,
            };
            if !supported {
                return Err(ServiceError::AnnotationValidation);
            }
            objects.push(object);
        }
        Ok(objects)
    }

    fn native_annotation_target(
        &self,
        context: &SampleProjectContext,
        image: &StoredImage,
    ) -> Result<NativeAnnotationTarget, ServiceError> {
        let (image_path, annotation_path, relative_path, relative_annotation_path) =
            self.native_annotation_paths(context, image)?;
        self.prepare_native_sidecar_path(&context.original_dir, &annotation_path)?;
        let source = project_storage::read_image_source(&context.sqlite, &image.id)
            .map_err(storage_failure)?;
        if let Some(source) = &source {
            if normalize_separator(&source.relative_path) != relative_path
                || source
                    .annotation_path
                    .as_deref()
                    .is_some_and(|path| normalize_separator(path) != relative_annotation_path)
            {
                return Err(ServiceError::Conflict);
            }
        }
        let current_source_version =
            native_source_version(&context.manifest.format, &context.original_dir, &image_path)?;
        if source
            .as_ref()
            .map(|source| source.source_version.as_str())
            .is_some_and(|expected| {
                !expected.is_empty() && !source_version_matches(&annotation_path, expected)
            })
        {
            return Err(ServiceError::Conflict);
        }
        Ok(NativeAnnotationTarget {
            image_path,
            annotation_path,
            relative_path,
            relative_annotation_path,
            expected_source_version: current_source_version,
            external_id: source.and_then(|source| source.external_id),
        })
    }

    fn native_annotation_paths(
        &self,
        context: &SampleProjectContext,
        image: &StoredImage,
    ) -> Result<(PathBuf, PathBuf, String, String), ServiceError> {
        let image_path = self.resolve_sample_asset(context, &image.file_name)?;
        let annotation_path = match context.manifest.format.as_str() {
            "yolo-detect" | "yolo-seg" => {
                yolo_adapter::annotation_path(&context.original_dir, &image_path)
            }
            "voc-detect" => voc_adapter::annotation_path(&context.original_dir, &image_path),
            "labelme" => labelme::annotation_path(&context.original_dir, &image_path),
            _ => return Err(ServiceError::AnnotationValidation),
        };
        let relative_path = relative_managed_path(&context.original_dir, &image_path)?;
        let relative_annotation_path =
            relative_managed_path(&context.original_dir, &annotation_path)?;
        Ok((
            image_path,
            annotation_path,
            relative_path,
            relative_annotation_path,
        ))
    }

    fn prepare_native_sidecar_path(&self, root: &Path, path: &Path) -> Result<(), ServiceError> {
        let relative = path.strip_prefix(root).map_err(|_| ServiceError::Storage)?;
        if relative.as_os_str().is_empty()
            || !relative
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
        {
            return Err(ServiceError::Storage);
        }
        let parent = relative.parent().ok_or(ServiceError::Storage)?;
        let components = parent
            .components()
            .map(|component| {
                let Component::Normal(component) = component else {
                    return Err(ServiceError::Storage);
                };
                component
                    .to_str()
                    .map(str::to_string)
                    .ok_or(ServiceError::Storage)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let component_refs = components.iter().map(String::as_str).collect::<Vec<_>>();
        ensure_managed_subdirectory(root, &component_refs).map_err(storage_failure)?;
        for candidate in [path.to_path_buf(), adapter_temporary_path(path)] {
            match fs::symlink_metadata(&candidate) {
                Ok(_) => {
                    self.validate_managed_asset_file(root, &candidate)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(storage_failure(error)),
            }
        }
        Ok(())
    }

    fn prepare_native_annotation(
        &self,
        context: &SampleProjectContext,
        objects: &[AnnotationObject],
        target: &NativeAnnotationTarget,
    ) -> Result<PreparedSourceSync, ServiceError> {
        let expected_version = Some(target.expected_source_version.as_str());
        let prepared = match context.manifest.format.as_str() {
            "yolo-detect" | "yolo-seg" => yolo_adapter::prepare_annotations(
                &context.original_dir,
                &target.image_path,
                &context.manifest.format,
                objects,
                expected_version,
            ),
            "voc-detect" => voc_adapter::prepare_annotations(
                &context.original_dir,
                &target.image_path,
                objects,
                expected_version,
            ),
            "labelme" => labelme::prepare_annotations(
                &context.original_dir,
                &target.image_path,
                objects,
                expected_version,
            ),
            _ => return Err(ServiceError::AnnotationValidation),
        }
        .map_err(|error| match error {
            PrepareSourceSyncError::Conflict => ServiceError::Conflict,
            PrepareSourceSyncError::Storage(message) => storage_failure(message),
        })?;
        if prepared.path != target.annotation_path {
            return Err(ServiceError::Storage);
        }
        Ok(prepared)
    }

    fn prepare_annotation_file_transaction(
        &self,
        context: &SampleProjectContext,
        project_id: &str,
        sample_id: &str,
        operation_id: &str,
        target: &NativeAnnotationTarget,
        prepared: &PreparedSourceSync,
    ) -> Result<AnnotationFileTransaction, ServiceError> {
        if native_source_version(
            &context.manifest.format,
            &context.original_dir,
            &target.image_path,
        )? != target.expected_source_version
        {
            return Err(ServiceError::Conflict);
        }
        let directory_name = sha256_hex(operation_id.as_bytes());
        let directory = context.annotation_transactions_dir.join(&directory_name);
        match fs::create_dir(&directory) {
            Ok(()) => sync_directory(&context.annotation_transactions_dir)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(ServiceError::Storage);
            }
            Err(error) => return Err(storage_failure(error)),
        }
        let result = (|| {
            validate_managed_directory_chain(
                &context.annotation_transactions_dir,
                &[&directory_name],
            )?;
            let managed_target = context
                .managed_annotations_dir
                .join(format!("{sample_id}.json"));
            let managed_before =
                self.read_optional_managed_file(&context.managed_annotations_dir, &managed_target)?;
            let sidecar_before =
                self.read_optional_managed_file(&context.original_dir, &target.annotation_path)?;
            if native_source_version(
                &context.manifest.format,
                &context.original_dir,
                &target.image_path,
            )? != target.expected_source_version
            {
                return Err(ServiceError::Conflict);
            }
            if let Some(bytes) = managed_before.as_deref() {
                write_new_synced_file(&directory.join("managed.old"), bytes)?;
            }
            if let Some(bytes) = sidecar_before.as_deref() {
                write_new_synced_file(&directory.join("sidecar.old"), bytes)?;
            }
            write_new_synced_file(&directory.join("sidecar.new"), &prepared.data)?;
            let journal = AnnotationFileJournal {
                version: ANNOTATION_TRANSACTION_VERSION,
                operation_id: operation_id.to_string(),
                project_id: project_id.to_string(),
                image_id: sample_id.to_string(),
                image_relative_path: target.relative_path.clone(),
                sidecar_relative_path: target.relative_annotation_path.clone(),
                expected_source_version: target.expected_source_version.clone(),
                managed_had_original: managed_before.is_some(),
                sidecar_had_original: sidecar_before.is_some(),
                managed_old_sha256: managed_before.as_deref().map(sha256_hex),
                managed_new_sha256: None,
                sidecar_old_sha256: sidecar_before.as_deref().map(sha256_hex),
                sidecar_new_sha256: sha256_hex(&prepared.data),
            };
            write_annotation_journal(&directory, &journal, true)?;
            Ok(AnnotationFileTransaction {
                directory: directory.clone(),
                journal,
            })
        })();
        if result.is_err() {
            let _ = cleanup_controlled_transaction_directory(
                &context.annotation_transactions_dir,
                &directory,
            );
        }
        result
    }

    fn stage_managed_annotation(
        &self,
        context: &SampleProjectContext,
        transaction: &mut AnnotationFileTransaction,
        state: &AnnotationState,
    ) -> Result<(), ServiceError> {
        self.validate_annotation_file_transaction(context, transaction)?;
        let bytes = serde_json::to_vec_pretty(state).map_err(storage_failure)?;
        write_new_synced_file(&transaction.directory.join("managed.new"), &bytes)?;
        transaction.journal.managed_new_sha256 = Some(sha256_hex(&bytes));
        write_annotation_journal(&transaction.directory, &transaction.journal, false)
    }

    fn apply_native_annotation_transaction(
        &self,
        context: &SampleProjectContext,
        transaction: &AnnotationFileTransaction,
    ) -> Result<(), ServiceError> {
        self.validate_annotation_file_transaction(context, transaction)?;
        let target = context.original_dir.join(validated_relative_asset_path(
            &transaction.journal.sidecar_relative_path,
        )?);
        let current_version = crate::importers::adapter::source_version(&target);
        if current_version != transaction.journal.expected_source_version {
            return Err(ServiceError::Conflict);
        }
        replace_file_from_stage(
            &context.original_dir,
            &target,
            &transaction.directory.join("sidecar.new"),
            &transaction.journal.operation_id,
            "sidecar",
        )
    }

    fn apply_managed_annotation_transaction(
        &self,
        context: &SampleProjectContext,
        transaction: &AnnotationFileTransaction,
    ) -> Result<(), ServiceError> {
        self.validate_annotation_file_transaction(context, transaction)?;
        let target = context
            .managed_annotations_dir
            .join(format!("{}.json", transaction.journal.image_id));
        replace_file_from_stage(
            &context.managed_annotations_dir,
            &target,
            &transaction.directory.join("managed.new"),
            &transaction.journal.operation_id,
            "managed",
        )
    }

    fn rollback_annotation_file_transaction(
        &self,
        context: &SampleProjectContext,
        transaction: &AnnotationFileTransaction,
    ) -> Result<(), ServiceError> {
        self.validate_annotation_file_transaction(context, transaction)?;
        let managed_target = context
            .managed_annotations_dir
            .join(format!("{}.json", transaction.journal.image_id));
        restore_transaction_target(
            &context.managed_annotations_dir,
            &managed_target,
            transaction
                .journal
                .managed_had_original
                .then(|| transaction.directory.join("managed.old"))
                .as_deref(),
            optional_regular_file(&transaction.directory.join("managed.new"))?.as_deref(),
            &transaction.journal.operation_id,
            "managed",
        )?;
        let sidecar_target = context.original_dir.join(validated_relative_asset_path(
            &transaction.journal.sidecar_relative_path,
        )?);
        restore_transaction_target(
            &context.original_dir,
            &sidecar_target,
            transaction
                .journal
                .sidecar_had_original
                .then(|| transaction.directory.join("sidecar.old"))
                .as_deref(),
            Some(transaction.directory.join("sidecar.new").as_path()),
            &transaction.journal.operation_id,
            "sidecar",
        )
    }

    fn cleanup_annotation_file_transaction(
        &self,
        transaction: &AnnotationFileTransaction,
    ) -> Result<(), ServiceError> {
        let parent = transaction
            .directory
            .parent()
            .ok_or(ServiceError::Storage)?;
        cleanup_controlled_transaction_directory(parent, &transaction.directory)
    }

    fn validate_annotation_file_transaction(
        &self,
        context: &SampleProjectContext,
        transaction: &AnnotationFileTransaction,
    ) -> Result<(), ServiceError> {
        if transaction.journal.version != ANNOTATION_TRANSACTION_VERSION
            || transaction.journal.project_id != context.manifest.id
            || validate_sample_id(&transaction.journal.image_id).is_err()
            || transaction.directory.parent() != Some(&context.annotation_transactions_dir)
        {
            return Err(ServiceError::Storage);
        }
        let directory_name = transaction
            .directory
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or(ServiceError::Storage)?;
        if directory_name != sha256_hex(transaction.journal.operation_id.as_bytes()) {
            return Err(ServiceError::Storage);
        }
        validate_managed_directory_chain(&context.annotation_transactions_dir, &[directory_name])?;
        let sample = self.stored_sample(context, &transaction.journal.image_id)?;
        let (_, _, image_relative_path, sidecar_relative_path) =
            self.native_annotation_paths(context, &sample.image)?;
        if transaction.journal.image_relative_path != image_relative_path
            || transaction.journal.sidecar_relative_path != sidecar_relative_path
        {
            return Err(ServiceError::Conflict);
        }
        validate_staged_artifact(
            &transaction.directory,
            "managed.old",
            transaction.journal.managed_old_sha256.as_deref(),
            transaction.journal.managed_had_original,
        )?;
        validate_staged_artifact(
            &transaction.directory,
            "managed.new",
            transaction.journal.managed_new_sha256.as_deref(),
            transaction.journal.managed_new_sha256.is_some(),
        )?;
        validate_staged_artifact(
            &transaction.directory,
            "sidecar.old",
            transaction.journal.sidecar_old_sha256.as_deref(),
            transaction.journal.sidecar_had_original,
        )?;
        validate_staged_artifact(
            &transaction.directory,
            "sidecar.new",
            Some(&transaction.journal.sidecar_new_sha256),
            true,
        )?;
        Ok(())
    }

    fn read_optional_managed_file(
        &self,
        root: &Path,
        path: &Path,
    ) -> Result<Option<Vec<u8>>, ServiceError> {
        match fs::symlink_metadata(path) {
            Ok(_) => {
                self.validate_managed_asset_file(root, path)?;
                fs::read(path).map(Some).map_err(storage_failure)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(storage_failure(error)),
        }
    }

    fn load_annotation_file_transaction(
        &self,
        context: &SampleProjectContext,
        operation_id: &str,
    ) -> Result<Option<AnnotationFileTransaction>, ServiceError> {
        let directory = context
            .annotation_transactions_dir
            .join(sha256_hex(operation_id.as_bytes()));
        let metadata = match fs::symlink_metadata(&directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(storage_failure(error)),
        };
        if is_symlink_or_reparse(&metadata) || !metadata.is_dir() {
            return Err(ServiceError::Storage);
        }
        let journal_path = directory.join("journal.json");
        self.validate_required_project_file(&directory, &journal_path)?;
        let journal: AnnotationFileJournal =
            serde_json::from_slice(&fs::read(journal_path).map_err(storage_failure)?)
                .map_err(storage_failure)?;
        if journal.operation_id != operation_id {
            return Err(ServiceError::Storage);
        }
        let transaction = AnnotationFileTransaction { directory, journal };
        self.validate_annotation_file_transaction(context, &transaction)?;
        Ok(Some(transaction))
    }

    fn recover_committed_annotation_file_transaction(
        &self,
        context: &SampleProjectContext,
        transaction: &AnnotationFileTransaction,
    ) -> Result<(), ServiceError> {
        self.validate_annotation_file_transaction(context, transaction)?;
        let mut source =
            project_storage::read_image_source(&context.sqlite, &transaction.journal.image_id)
                .map_err(storage_failure)?
                .ok_or(ServiceError::Storage)?;
        if normalize_separator(&source.relative_path) != transaction.journal.image_relative_path
            || source.annotation_path.as_deref().map(normalize_separator)
                != Some(transaction.journal.sidecar_relative_path.clone())
        {
            return Err(ServiceError::Conflict);
        }
        let sidecar_target = context.original_dir.join(validated_relative_asset_path(
            &transaction.journal.sidecar_relative_path,
        )?);
        roll_forward_transaction_target(
            &context.original_dir,
            &sidecar_target,
            transaction
                .journal
                .sidecar_had_original
                .then(|| transaction.directory.join("sidecar.old"))
                .as_deref(),
            &transaction.directory.join("sidecar.new"),
            &transaction.journal.operation_id,
            "sidecar",
        )?;

        let state = self.annotation_state_in_context(context, &transaction.journal.image_id)?;
        if state.revision.is_none() {
            return Err(ServiceError::Storage);
        }
        let managed_target = context
            .managed_annotations_dir
            .join(format!("{}.json", transaction.journal.image_id));
        let managed_new = optional_regular_file(&transaction.directory.join("managed.new"))?;
        validate_roll_forward_transaction_target(
            &context.managed_annotations_dir,
            &managed_target,
            transaction
                .journal
                .managed_had_original
                .then(|| transaction.directory.join("managed.old"))
                .as_deref(),
            managed_new.as_deref(),
        )?;
        let recovery_stage = transaction.directory.join("managed.recovery");
        if optional_regular_file(&recovery_stage)?.is_some() {
            fs::remove_file(&recovery_stage).map_err(storage_failure)?;
            sync_directory(&transaction.directory)?;
        }
        let managed_bytes = serde_json::to_vec_pretty(&state).map_err(storage_failure)?;
        write_new_synced_file(&recovery_stage, &managed_bytes)?;
        sync_directory(&transaction.directory)?;
        replace_file_from_stage(
            &context.managed_annotations_dir,
            &managed_target,
            &recovery_stage,
            &transaction.journal.operation_id,
            "managed",
        )?;

        source.source_version = crate::importers::adapter::source_version(&sidecar_target);
        project_storage::write_image_source(&context.sqlite, &source).map_err(storage_failure)?;
        self.cleanup_annotation_file_transaction(transaction)
    }

    fn recover_rolled_back_annotation_file_transaction(
        &self,
        context: &SampleProjectContext,
        transaction: &AnnotationFileTransaction,
    ) -> Result<(), ServiceError> {
        self.rollback_annotation_file_transaction(context, transaction)?;
        self.cleanup_annotation_file_transaction(transaction)
    }

    fn managed_annotation_paths(
        &self,
        context: &SampleProjectContext,
        sample_id: &str,
    ) -> (PathBuf, PathBuf) {
        let target = context
            .managed_annotations_dir
            .join(format!("{sample_id}.json"));
        let backup = context.managed_annotations_dir.join(format!(
            ".annotation-{}.bak",
            sha256_hex(sample_id.as_bytes())
        ));
        (target, backup)
    }

    fn prepare_managed_annotation_artifacts(
        &self,
        context: &SampleProjectContext,
        sample_id: &str,
    ) -> Result<(), ServiceError> {
        let (target, backup) = self.managed_annotation_paths(context, sample_id);
        let target_exists = self
            .validate_optional_project_file(&context.managed_annotations_dir, &target)?
            .is_some();
        let backup_exists = self
            .validate_optional_project_file(&context.managed_annotations_dir, &backup)?
            .is_some();
        match (target_exists, backup_exists) {
            (true, true) => {
                fs::remove_file(backup).map_err(storage_failure)?;
                sync_directory(&context.managed_annotations_dir)
            }
            (false, true) => {
                fs::rename(&backup, &target).map_err(storage_failure)?;
                sync_directory(&context.managed_annotations_dir)?;
                self.validate_required_project_file(&context.managed_annotations_dir, &target)?;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn persist_managed_annotation(
        &self,
        context: &SampleProjectContext,
        sample_id: &str,
        state: &AnnotationState,
    ) -> Result<(), ServiceError> {
        self.prepare_managed_annotation_artifacts(context, sample_id)?;
        let (target, backup) = self.managed_annotation_paths(context, sample_id);
        let temporary = context.managed_annotations_dir.join(format!(
            ".annotation-{}-{}-{}.tmp",
            sha256_hex(sample_id.as_bytes()),
            std::process::id(),
            ANNOTATION_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let bytes = serde_json::to_vec_pretty(state).map_err(storage_failure)?;
        let result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .map_err(storage_failure)?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(storage_failure)?;
            drop(file);
            sync_directory(&context.managed_annotations_dir)?;
            self.validate_required_project_file(&context.managed_annotations_dir, &temporary)?;
            let had_target = self
                .validate_optional_project_file(&context.managed_annotations_dir, &target)?
                .is_some();
            self.validate_optional_project_file(&context.managed_annotations_dir, &backup)?;
            if had_target {
                fs::rename(&target, &backup).map_err(storage_failure)?;
                sync_directory(&context.managed_annotations_dir)?;
            }
            if let Err(error) = fs::rename(&temporary, &target).map_err(storage_failure) {
                if had_target {
                    let _ = fs::remove_file(&target);
                    if fs::rename(&backup, &target).is_err() {
                        return Err(ServiceError::Storage);
                    }
                    sync_directory(&context.managed_annotations_dir)?;
                }
                return Err(error);
            }
            sync_directory(&context.managed_annotations_dir)?;
            if let Err(error) =
                self.validate_required_project_file(&context.managed_annotations_dir, &target)
            {
                let _ = fs::remove_file(&target);
                if had_target && fs::rename(&backup, &target).is_err() {
                    return Err(ServiceError::Storage);
                }
                sync_directory(&context.managed_annotations_dir)?;
                return Err(error);
            }
            if had_target {
                fs::remove_file(&backup).map_err(storage_failure)?;
                sync_directory(&context.managed_annotations_dir)?;
            }
            Ok(())
        })();
        if result.is_err()
            && fs::symlink_metadata(&temporary)
                .ok()
                .is_some_and(|metadata| metadata.is_file() && !is_symlink_or_reparse(&metadata))
            && fs::remove_file(&temporary).is_ok()
        {
            let _ = sync_directory(&context.managed_annotations_dir);
        }
        result
    }

    fn sample_project_context(
        &self,
        project_id: &str,
    ) -> Result<SampleProjectContext, ServiceError> {
        let project_dir = self
            .existing_project_dir(self.projects_dir.as_ref(), project_id)?
            .ok_or(ServiceError::NotFound)?;
        let manifest = self.ensure_project_manifest(&project_dir, true)?;
        let sqlite = project_dir.join("project.sqlite");
        self.validate_project_database(&project_dir, &sqlite)?;
        project_storage::initialize_project_database(&sqlite).map_err(storage_failure)?;
        self.validate_project_database(&project_dir, &sqlite)?;
        let original_dir = validate_managed_directory_chain(&project_dir, &["assets", "original"])?;
        let thumbnail_dir =
            validate_managed_directory_chain(&project_dir, &["assets", "thumbnails"])?;
        let managed_annotations_dir =
            validate_managed_directory_chain(&project_dir, &["annotations", "native"])?;
        let annotation_transactions_dir =
            ensure_managed_subdirectory(&project_dir, &["annotations", "transactions"])
                .map_err(storage_failure)?;
        let annotation_transaction_quarantine_dir =
            ensure_managed_subdirectory(&project_dir, &["annotations", "transaction-quarantine"])
                .map_err(storage_failure)?;
        self.cleanup_thumbnail_temporary_files(&thumbnail_dir)?;
        self.cleanup_annotation_temporary_files(&managed_annotations_dir)?;
        Ok(SampleProjectContext {
            manifest,
            sqlite,
            original_dir,
            thumbnail_dir,
            managed_annotations_dir,
            annotation_transactions_dir,
            annotation_transaction_quarantine_dir,
        })
    }

    fn refresh_sample_classes(&self, context: &SampleProjectContext) -> Result<(), ServiceError> {
        if context.manifest.format != "image-classification"
            || project_storage::has_sample_class_links(&context.sqlite).map_err(storage_failure)?
        {
            return Ok(());
        }
        let classification_links = self.classification_links(context)?;
        project_storage::refresh_sample_class_links(&context.sqlite, &classification_links)
            .map_err(storage_failure)
    }

    fn classification_links(
        &self,
        context: &SampleProjectContext,
    ) -> Result<Vec<(String, u32)>, ServiceError> {
        let images =
            project_storage::read_images(&context.sqlite, None).map_err(storage_failure)?;
        let classes = project_storage::read_classes(&context.sqlite).map_err(storage_failure)?;
        let exact_images = images
            .iter()
            .map(|image| (normalize_separator(&image.file_name), image.id.clone()))
            .collect::<HashMap<_, _>>();
        let mut basename_images = HashMap::<String, Option<String>>::new();
        for image in &images {
            let Some(name) = Path::new(&image.file_name)
                .file_name()
                .and_then(|name| name.to_str())
            else {
                continue;
            };
            basename_images
                .entry(name.to_string())
                .and_modify(|entry| *entry = None)
                .or_insert_with(|| Some(image.id.clone()));
        }
        let classes_by_label = classes
            .into_iter()
            .map(|class| (class.label, class.id))
            .collect::<HashMap<_, _>>();
        let mut links = Vec::new();
        for entry in WalkDir::new(&context.original_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
        {
            let path = entry.path();
            self.validate_managed_asset_file(&context.original_dir, path)?;
            let relative = path
                .strip_prefix(&context.original_dir)
                .map_err(|_| ServiceError::Storage)?;
            let relative_text = normalize_separator(&relative.to_string_lossy());
            let image_id = exact_images.get(&relative_text).cloned().or_else(|| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| basename_images.get(name))
                    .and_then(Clone::clone)
            });
            let Some(image_id) = image_id else {
                continue;
            };
            let class_id = relative
                .components()
                .filter_map(|component| match component {
                    Component::Normal(value) => value.to_str(),
                    _ => None,
                })
                .find_map(|component| classes_by_label.get(component).copied());
            if let Some(class_id) = class_id {
                links.push((image_id, class_id));
            }
        }
        links.sort();
        links.dedup();
        Ok(links)
    }

    fn resolve_sample_asset(
        &self,
        context: &SampleProjectContext,
        file_name: &str,
    ) -> Result<PathBuf, ServiceError> {
        let relative = validated_relative_asset_path(file_name)?;
        let candidate = context.original_dir.join(&relative);
        if fs::symlink_metadata(&candidate).is_ok() {
            return self.validate_managed_asset_file(&context.original_dir, &candidate);
        }
        if relative.components().count() != 1 {
            return Err(ServiceError::Storage);
        }
        let mut matches = WalkDir::new(&context.original_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .filter(|entry| entry.file_name() == relative.as_os_str())
            .map(|entry| self.validate_managed_asset_file(&context.original_dir, entry.path()))
            .collect::<Result<Vec<_>, _>>()?;
        if matches.len() == 1 {
            Ok(matches.remove(0))
        } else {
            Err(ServiceError::Storage)
        }
    }

    fn validate_managed_asset_file(
        &self,
        root: &Path,
        path: &Path,
    ) -> Result<PathBuf, ServiceError> {
        let relative = path.strip_prefix(root).map_err(|_| ServiceError::Storage)?;
        let mut current = root.to_path_buf();
        let components = relative.components().collect::<Vec<_>>();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            let Component::Normal(component) = component else {
                return Err(ServiceError::Storage);
            };
            current.push(component);
            let metadata = fs::symlink_metadata(&current).map_err(storage_failure)?;
            if is_symlink_or_reparse(&metadata) || !metadata.is_dir() {
                return Err(ServiceError::Storage);
            }
            let canonical = canonical_existing(&current).map_err(storage_failure)?;
            if canonical.parent() != current.parent() || !canonical_path_is_within(root, &canonical)
            {
                return Err(ServiceError::Storage);
            }
            current = canonical;
        }
        validate_regular_file_within(root, path).map_err(storage_failure)
    }

    fn cleanup_thumbnail_temporary_files(&self, thumbnail_dir: &Path) -> Result<(), ServiceError> {
        for entry in fs::read_dir(thumbnail_dir).map_err(storage_failure)? {
            let entry = entry.map_err(storage_failure)?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !name.starts_with(".thumbnail-") || !name.ends_with(".tmp") {
                continue;
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(storage_failure)?;
            if is_symlink_or_reparse(&metadata) || !metadata.is_file() {
                continue;
            }
            self.validate_managed_asset_file(thumbnail_dir, &path)?;
            fs::remove_file(path).map_err(storage_failure)?;
        }
        Ok(())
    }

    fn cleanup_annotation_temporary_files(
        &self,
        annotations_dir: &Path,
    ) -> Result<(), ServiceError> {
        for entry in fs::read_dir(annotations_dir).map_err(storage_failure)? {
            let entry = entry.map_err(storage_failure)?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !name.starts_with(".annotation-") || !name.ends_with(".tmp") {
                continue;
            }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(storage_failure)?;
            if is_symlink_or_reparse(&metadata) || !metadata.is_file() {
                continue;
            }
            self.validate_required_project_file(annotations_dir, &path)?;
            fs::remove_file(path).map_err(storage_failure)?;
        }
        Ok(())
    }

    fn cleanup_stale_sample_thumbnails(
        &self,
        thumbnail_dir: &Path,
        sample_cache_prefix: &str,
        current_cache_path: &Path,
    ) -> Result<(), ServiceError> {
        let legacy_name = format!("{sample_cache_prefix}.jpg");
        let fingerprinted_prefix = format!("{sample_cache_prefix}-");
        for entry in fs::read_dir(thumbnail_dir).map_err(storage_failure)? {
            let entry = entry.map_err(storage_failure)?;
            let path = entry.path();
            if path == current_cache_path {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name != legacy_name
                && !(name.starts_with(&fingerprinted_prefix) && name.ends_with(".jpg"))
            {
                continue;
            }
            let metadata = fs::symlink_metadata(&path).map_err(storage_failure)?;
            if is_symlink_or_reparse(&metadata) || !metadata.is_file() {
                continue;
            }
            self.validate_managed_asset_file(thumbnail_dir, &path)?;
            fs::remove_file(path).map_err(storage_failure)?;
        }
        Ok(())
    }

    fn persist_thumbnail(
        &self,
        thumbnail_dir: &Path,
        cache_path: &Path,
        bytes: &[u8],
    ) -> Result<(), ServiceError> {
        let temporary = thumbnail_dir.join(format!(
            ".thumbnail-{}-{}.tmp",
            std::process::id(),
            CREATE_OWNERSHIP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .map_err(storage_failure)?;
            file.write_all(bytes)
                .and_then(|()| file.sync_all())
                .map_err(storage_failure)?;
            self.validate_managed_asset_file(thumbnail_dir, &temporary)?;
            fs::rename(&temporary, cache_path).map_err(storage_failure)?;
            self.validate_managed_asset_file(thumbnail_dir, cache_path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
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

    fn restore_sample_metadata(
        &self,
        context: &SampleProjectContext,
        sample_id: &str,
        before: &SampleMetadataSnapshot,
        operation: &AuditOperation,
    ) -> Result<(), ServiceError> {
        project_storage::restore_sample_metadata(
            &context.sqlite,
            sample_id,
            &operation.operation_id,
            &before.split,
            &before.status,
            &before.qa_status,
            before.review_note.as_deref(),
        )
        .map_err(storage_failure)
    }

    fn note_pending_audit_best_effort(&self, operation: &AuditOperation, message: &str) {
        if let Err(error) = self.storage.note_pending_audit(operation, message) {
            tracing::error!(%error, "failed to update pending sample audit message");
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
        .truncate(false)
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

fn validate_managed_directory_chain(
    root: &Path,
    components: &[&str],
) -> Result<PathBuf, ServiceError> {
    let mut current = root.to_path_buf();
    for component in components {
        let candidate = current.join(component);
        let metadata = fs::symlink_metadata(&candidate).map_err(storage_failure)?;
        if is_symlink_or_reparse(&metadata) || !metadata.is_dir() {
            return Err(ServiceError::Storage);
        }
        let canonical = canonical_existing(&candidate).map_err(storage_failure)?;
        if canonical.parent() != Some(current.as_path())
            || !canonical_path_is_within(root, &canonical)
        {
            return Err(ServiceError::Storage);
        }
        current = canonical;
    }
    Ok(current)
}

fn validated_relative_asset_path(value: &str) -> Result<PathBuf, ServiceError> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(ServiceError::Storage);
    }
    let normalized = value.replace('\\', "/");
    let path = PathBuf::from(normalized);
    let valid = !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    if valid {
        Ok(path)
    } else {
        Err(ServiceError::Storage)
    }
}

fn validate_sample_id(sample_id: &str) -> Result<(), ServiceError> {
    let valid = !sample_id.is_empty()
        && sample_id.len() <= 256
        && !sample_id.chars().any(char::is_control)
        && !sample_id.contains('/')
        && !sample_id.contains('\\');
    if valid {
        Ok(())
    } else {
        Err(ServiceError::Validation)
    }
}

fn validate_sample_query(query: &SampleQueryOptions) -> Result<(), ServiceError> {
    if query.limit == 0 || query.limit > 500 {
        return Err(ServiceError::Validation);
    }
    if query
        .split
        .as_deref()
        .is_some_and(|value| !valid_split(value))
        || query
            .status
            .as_deref()
            .is_some_and(|value| !valid_status(value))
        || query
            .qa_status
            .as_deref()
            .is_some_and(|value| !valid_qa_status(value))
        || query.label.as_deref().is_some_and(|value| {
            value.trim().is_empty()
                || value.chars().count() > 128
                || value.chars().any(char::is_control)
        })
        || query.query.as_deref().is_some_and(|value| {
            value.trim().is_empty()
                || value.chars().count() > MAX_SAMPLE_QUERY_CHARS
                || value.chars().any(char::is_control)
        })
    {
        return Err(ServiceError::Validation);
    }
    Ok(())
}

fn validate_sample_patch(patch: &SamplePatch) -> Result<(), ServiceError> {
    if patch.split.is_none()
        && patch.status.is_none()
        && patch.qa_status.is_none()
        && patch.review_note.is_none()
    {
        return Err(ServiceError::Validation);
    }
    if patch
        .split
        .as_deref()
        .is_some_and(|value| !valid_split(value))
        || patch
            .status
            .as_deref()
            .is_some_and(|value| !valid_status(value))
        || patch
            .qa_status
            .as_deref()
            .is_some_and(|value| !valid_qa_status(value))
        || patch.review_note.as_deref().is_some_and(|value| {
            value.chars().count() > MAX_SAMPLE_TEXT_CHARS
                || value.chars().any(|character| {
                    character.is_control() && character != '\n' && character != '\t'
                })
        })
    {
        return Err(ServiceError::Validation);
    }
    Ok(())
}

fn validate_sample_metadata_snapshot(
    snapshot: &SampleMetadataSnapshot,
) -> Result<(), ServiceError> {
    if !valid_split(&snapshot.split)
        || !valid_status(&snapshot.status)
        || !valid_qa_status(&snapshot.qa_status)
        || snapshot.review_note.as_deref().is_some_and(|value| {
            value.chars().count() > MAX_SAMPLE_TEXT_CHARS
                || value.chars().any(|character| {
                    character.is_control() && character != '\n' && character != '\t'
                })
        })
    {
        Err(ServiceError::Validation)
    } else {
        Ok(())
    }
}

fn valid_split(value: &str) -> bool {
    matches!(value, "train" | "val" | "test" | "local")
}

fn valid_status(value: &str) -> bool {
    matches!(value, "未标注" | "草稿" | "已标注" | "待质检" | "通过")
}

fn valid_qa_status(value: &str) -> bool {
    matches!(value, "" | "待质检" | "通过" | "驳回")
}

fn parse_annotation_object(
    value: serde_json::Value,
    image: &StoredImage,
    classes: &HashMap<u32, String>,
) -> Result<AnnotationObject, ServiceError> {
    let object = value
        .as_object()
        .ok_or(ServiceError::AnnotationValidation)?;
    let allowed = [
        "id",
        "classId",
        "label",
        "type",
        "bbox",
        "polygon",
        "attributes",
    ];
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(ServiceError::AnnotationValidation);
    }
    let id = annotation_text(object.get("id"), MAX_ANNOTATION_ID_CHARS)?;
    let class_id = object
        .get("classId")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(ServiceError::AnnotationValidation)?;
    let label = annotation_text(object.get("label"), MAX_ANNOTATION_LABEL_CHARS)?;
    if classes.get(&class_id).map(String::as_str) != Some(label.as_str()) {
        return Err(ServiceError::AnnotationValidation);
    }
    let object_type = annotation_text(object.get("type"), 32)?;
    let attributes_object = object
        .get("attributes")
        .and_then(serde_json::Value::as_object)
        .ok_or(ServiceError::AnnotationValidation)?;
    if serde_json::to_vec(attributes_object)
        .map_err(|_| ServiceError::AnnotationValidation)?
        .len()
        > MAX_ANNOTATION_ATTRIBUTES_BYTES
    {
        return Err(ServiceError::AnnotationValidation);
    }
    let attributes = attributes_object
        .clone()
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    match object_type.as_str() {
        "bbox" => {
            if object.contains_key("polygon") {
                return Err(ServiceError::AnnotationValidation);
            }
            let bbox = object
                .get("bbox")
                .and_then(serde_json::Value::as_object)
                .ok_or(ServiceError::AnnotationValidation)?;
            if bbox.len() != 4
                || bbox
                    .keys()
                    .any(|key| !matches!(key.as_str(), "x" | "y" | "width" | "height"))
            {
                return Err(ServiceError::AnnotationValidation);
            }
            let x = annotation_number(bbox.get("x"))?;
            let y = annotation_number(bbox.get("y"))?;
            let width = annotation_number(bbox.get("width"))?;
            let height = annotation_number(bbox.get("height"))?;
            if x < 0.0
                || y < 0.0
                || width <= 0.0
                || height <= 0.0
                || x + width > f64::from(image.width)
                || y + height > f64::from(image.height)
            {
                return Err(ServiceError::AnnotationValidation);
            }
            Ok(AnnotationObject {
                id,
                class_id,
                label,
                object_type,
                bbox: Some(BBox {
                    x,
                    y,
                    width,
                    height,
                }),
                polygon: None,
                attributes,
            })
        }
        "polygon" => {
            if object.contains_key("bbox") {
                return Err(ServiceError::AnnotationValidation);
            }
            let polygon = object
                .get("polygon")
                .and_then(serde_json::Value::as_array)
                .ok_or(ServiceError::AnnotationValidation)?;
            if polygon.len() < 3 || polygon.len() > 10_000 {
                return Err(ServiceError::AnnotationValidation);
            }
            let mut points = Vec::with_capacity(polygon.len());
            for point in polygon {
                let point = point
                    .as_object()
                    .ok_or(ServiceError::AnnotationValidation)?;
                if point.len() != 2 || point.keys().any(|key| !matches!(key.as_str(), "x" | "y")) {
                    return Err(ServiceError::AnnotationValidation);
                }
                let x = annotation_number(point.get("x"))?;
                let y = annotation_number(point.get("y"))?;
                if x < 0.0 || y < 0.0 || x > f64::from(image.width) || y > f64::from(image.height) {
                    return Err(ServiceError::AnnotationValidation);
                }
                points.push(Point { x, y });
            }
            let distinct = points
                .iter()
                .enumerate()
                .filter(|(index, point)| {
                    points[..*index]
                        .iter()
                        .all(|existing| existing.x != point.x || existing.y != point.y)
                })
                .count();
            if distinct < 3 {
                return Err(ServiceError::AnnotationValidation);
            }
            Ok(AnnotationObject {
                id,
                class_id,
                label,
                object_type,
                bbox: None,
                polygon: Some(points),
                attributes,
            })
        }
        _ => Err(ServiceError::AnnotationValidation),
    }
}

fn annotation_text(
    value: Option<&serde_json::Value>,
    max_chars: usize,
) -> Result<String, ServiceError> {
    let value = value
        .and_then(serde_json::Value::as_str)
        .ok_or(ServiceError::AnnotationValidation)?;
    if value.is_empty() || value.chars().count() > max_chars || value.chars().any(char::is_control)
    {
        Err(ServiceError::AnnotationValidation)
    } else {
        Ok(value.to_string())
    }
}

fn annotation_number(value: Option<&serde_json::Value>) -> Result<f64, ServiceError> {
    let value = value
        .and_then(serde_json::Value::as_f64)
        .ok_or(ServiceError::AnnotationValidation)?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(ServiceError::AnnotationValidation)
    }
}

fn sample_view(sample: StoredSample) -> Result<SampleView, ServiceError> {
    let file_name = validated_relative_asset_path(&sample.image.file_name)?
        .to_string_lossy()
        .replace('\\', "/");
    Ok(SampleView {
        id: sample.image.id,
        file_name,
        width: sample.image.width,
        height: sample.image.height,
        split: sample.image.split.clone(),
        status: sample.image.status,
        qa_status: sample.image.qa_status,
        review_note: sample.image.review_note,
        annotation_revision: sample.annotation_revision,
        annotation_updated_at: sample.annotation_updated_at,
        annotation_count: sample.annotation_count,
        classes: sample.classes.into_iter().map(sample_class_view).collect(),
        tags: vec![format!("split={}", sample.image.split)],
    })
}

fn sample_class_view(class: StoredSampleClass) -> SampleClassView {
    SampleClassView {
        id: class.id,
        label: class.label,
        object_count: class.object_count,
    }
}

fn annotation_workflow_view(state: RemoteWorkflowState) -> AnnotationWorkflowView {
    AnnotationWorkflowView {
        image_id: state.image_id,
        status: state.status,
        qa_status: state.qa_status,
        review_note: state.review_note,
    }
}

fn normalize_separator(value: &str) -> String {
    value.replace('\\', "/")
}

fn write_new_synced_file(path: &Path, bytes: &[u8]) -> Result<(), ServiceError> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(storage_failure)?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(storage_failure)
}

fn write_annotation_journal(
    directory: &Path,
    journal: &AnnotationFileJournal,
    initial: bool,
) -> Result<(), ServiceError> {
    let bytes = serde_json::to_vec_pretty(journal).map_err(storage_failure)?;
    let target = directory.join("journal.json");
    if initial {
        write_new_synced_file(&target, &bytes)?;
        return sync_directory(directory);
    }
    let temporary = directory.join("journal.next");
    let backup = directory.join("journal.prev");
    if optional_regular_file(&temporary)?.is_some()
        || optional_regular_file(&backup)?.is_some()
        || optional_regular_file(&target)?.is_none()
    {
        return Err(ServiceError::Storage);
    }
    write_new_synced_file(&temporary, &bytes)?;
    sync_directory(directory)?;
    fs::rename(&target, &backup).map_err(storage_failure)?;
    sync_directory(directory)?;
    if let Err(error) = fs::rename(&temporary, &target) {
        let _ = fs::rename(&backup, &target);
        let _ = sync_directory(directory);
        return Err(storage_failure(error));
    }
    sync_directory(directory)?;
    fs::remove_file(&backup).map_err(storage_failure)?;
    sync_directory(directory)
}

fn sync_directory(path: &Path) -> Result<(), ServiceError> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(storage_failure)
    }
    #[cfg(windows)]
    {
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(storage_failure)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(ServiceError::Storage)
    }
}

fn optional_regular_file(path: &Path) -> Result<Option<PathBuf>, ServiceError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if is_symlink_or_reparse(&metadata) || !metadata.is_file() => {
            Err(ServiceError::Storage)
        }
        Ok(_) => Ok(Some(path.to_path_buf())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(storage_failure(error)),
    }
}

fn validate_staged_artifact(
    directory: &Path,
    name: &str,
    expected_sha256: Option<&str>,
    should_exist: bool,
) -> Result<(), ServiceError> {
    if should_exist != expected_sha256.is_some() {
        return Err(ServiceError::Conflict);
    }
    let path = directory.join(name);
    let actual = optional_regular_file(&path)?;
    match (actual, expected_sha256) {
        (None, None) => Ok(()),
        (Some(path), Some(expected)) => {
            let bytes = fs::read(path).map_err(storage_failure)?;
            if sha256_hex(&bytes) == expected {
                Ok(())
            } else {
                Err(ServiceError::Conflict)
            }
        }
        _ => Err(ServiceError::Conflict),
    }
}

fn validate_replacement_target_parent(root: &Path, target: &Path) -> Result<(), ServiceError> {
    if target.parent().is_none_or(|parent| {
        fs::canonicalize(parent).ok().is_none_or(|canonical| {
            canonical != root && !canonical_path_is_within(root, &canonical)
        })
    }) {
        return Err(ServiceError::Storage);
    }
    match fs::symlink_metadata(target) {
        Ok(metadata) if is_symlink_or_reparse(&metadata) || !metadata.is_file() => {
            Err(ServiceError::Storage)
        }
        Ok(_) => {
            validate_regular_file_within(root, target).map_err(storage_failure)?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(storage_failure(error)),
    }
}

fn replacement_artifact(
    target: &Path,
    operation_id: &str,
    label: &str,
    suffix: &str,
) -> Result<PathBuf, ServiceError> {
    let parent = target.parent().ok_or(ServiceError::Storage)?;
    Ok(parent.join(format!(
        ".remote-annotation-{}-{label}.{suffix}",
        sha256_hex(operation_id.as_bytes())
    )))
}

fn normalize_replacement_artifacts(
    root: &Path,
    target: &Path,
    temporary: &Path,
    displaced: &Path,
) -> Result<(), ServiceError> {
    if optional_regular_file(temporary)?.is_some() {
        validate_regular_file_within(root, temporary).map_err(storage_failure)?;
        fs::remove_file(temporary).map_err(storage_failure)?;
        sync_directory(temporary.parent().ok_or(ServiceError::Storage)?)?;
    }
    if optional_regular_file(displaced)?.is_some() {
        validate_regular_file_within(root, displaced).map_err(storage_failure)?;
        if target.exists() {
            fs::remove_file(displaced).map_err(storage_failure)?;
            sync_directory(displaced.parent().ok_or(ServiceError::Storage)?)?;
        } else {
            fs::rename(displaced, target).map_err(storage_failure)?;
            sync_directory(target.parent().ok_or(ServiceError::Storage)?)?;
        }
    }
    Ok(())
}

fn replace_file_from_stage(
    root: &Path,
    target: &Path,
    staged: &Path,
    operation_id: &str,
    label: &str,
) -> Result<(), ServiceError> {
    let staged = optional_regular_file(staged)?.ok_or(ServiceError::Storage)?;
    validate_replacement_target_parent(root, target)?;
    let temporary = replacement_artifact(target, operation_id, label, "tmp")?;
    let displaced = replacement_artifact(target, operation_id, label, "bak")?;
    normalize_replacement_artifacts(root, target, &temporary, &displaced)?;
    let bytes = fs::read(staged).map_err(storage_failure)?;
    write_new_synced_file(&temporary, &bytes)?;
    let parent = target.parent().ok_or(ServiceError::Storage)?;
    sync_directory(parent)?;
    validate_regular_file_within(root, &temporary).map_err(storage_failure)?;
    let had_target = target.exists();
    if had_target {
        fs::rename(target, &displaced).map_err(storage_failure)?;
        sync_directory(parent)?;
    }
    if let Err(error) = fs::rename(&temporary, target) {
        if had_target {
            let _ = fs::rename(&displaced, target);
            let _ = sync_directory(parent);
        }
        let _ = fs::remove_file(&temporary);
        let _ = sync_directory(parent);
        return Err(storage_failure(error));
    }
    sync_directory(parent)?;
    validate_regular_file_within(root, target).map_err(storage_failure)?;
    if had_target {
        fs::remove_file(displaced).map_err(storage_failure)?;
        sync_directory(parent)?;
    }
    Ok(())
}

fn restore_transaction_target(
    root: &Path,
    target: &Path,
    old_stage: Option<&Path>,
    new_stage: Option<&Path>,
    operation_id: &str,
    label: &str,
) -> Result<(), ServiceError> {
    validate_replacement_target_parent(root, target)?;
    let current = match optional_regular_file(target)? {
        Some(path) => Some(fs::read(path).map_err(storage_failure)?),
        None => None,
    };
    let new_bytes = new_stage
        .map(fs::read)
        .transpose()
        .map_err(storage_failure)?;
    match old_stage {
        Some(old_stage) => {
            let old_bytes = fs::read(old_stage).map_err(storage_failure)?;
            if current.as_deref() == Some(old_bytes.as_slice()) {
                return Ok(());
            }
            if current.is_some() && current.as_deref() != new_bytes.as_deref() {
                return Err(ServiceError::Conflict);
            }
            replace_file_from_stage(root, target, old_stage, operation_id, label)
        }
        None => match current {
            None => Ok(()),
            Some(current) if Some(current.as_slice()) == new_bytes.as_deref() => {
                fs::remove_file(target).map_err(storage_failure)?;
                sync_directory(target.parent().ok_or(ServiceError::Storage)?)
            }
            Some(_) => Err(ServiceError::Conflict),
        },
    }
}

fn roll_forward_transaction_target(
    root: &Path,
    target: &Path,
    old_stage: Option<&Path>,
    new_stage: &Path,
    operation_id: &str,
    label: &str,
) -> Result<(), ServiceError> {
    validate_replacement_target_parent(root, target)?;
    let current = match optional_regular_file(target)? {
        Some(path) => Some(fs::read(path).map_err(storage_failure)?),
        None => None,
    };
    let new_bytes = fs::read(new_stage).map_err(storage_failure)?;
    if current.as_deref() == Some(new_bytes.as_slice()) {
        return Ok(());
    }
    let old_bytes = old_stage
        .map(fs::read)
        .transpose()
        .map_err(storage_failure)?;
    if current.is_some() && current.as_deref() != old_bytes.as_deref() {
        return Err(ServiceError::Conflict);
    }
    replace_file_from_stage(root, target, new_stage, operation_id, label)
}

fn validate_roll_forward_transaction_target(
    root: &Path,
    target: &Path,
    old_stage: Option<&Path>,
    new_stage: Option<&Path>,
) -> Result<(), ServiceError> {
    validate_replacement_target_parent(root, target)?;
    let current = optional_regular_file(target)?
        .map(fs::read)
        .transpose()
        .map_err(storage_failure)?;
    if current.is_none() {
        return Ok(());
    }
    let old_bytes = old_stage
        .map(fs::read)
        .transpose()
        .map_err(storage_failure)?;
    let new_bytes = new_stage
        .map(fs::read)
        .transpose()
        .map_err(storage_failure)?;
    if current.as_deref() == old_bytes.as_deref() || current.as_deref() == new_bytes.as_deref() {
        Ok(())
    } else {
        Err(ServiceError::Conflict)
    }
}

fn cleanup_controlled_transaction_directory(
    root: &Path,
    directory: &Path,
) -> Result<(), ServiceError> {
    if directory.parent() != Some(root) {
        return Err(ServiceError::Storage);
    }
    let metadata = fs::symlink_metadata(directory).map_err(storage_failure)?;
    if is_symlink_or_reparse(&metadata) || !metadata.is_dir() {
        return Err(ServiceError::Storage);
    }
    let canonical = fs::canonicalize(directory).map_err(storage_failure)?;
    if canonical.parent() != Some(root) {
        return Err(ServiceError::Storage);
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(directory).map_err(storage_failure)? {
        let entry = entry.map_err(storage_failure)?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(storage_failure)?;
        if is_symlink_or_reparse(&metadata) || !metadata.is_file() {
            return Err(ServiceError::Storage);
        }
        files.push(path);
    }
    files.sort_by_key(|path| path.file_name() == Some(std::ffi::OsStr::new("journal.json")));
    for path in files {
        fs::remove_file(path).map_err(storage_failure)?;
    }
    fs::remove_dir(directory).map_err(storage_failure)?;
    sync_directory(root)
}

fn relative_managed_path(root: &Path, path: &Path) -> Result<String, ServiceError> {
    let relative = path.strip_prefix(root).map_err(|_| ServiceError::Storage)?;
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(ServiceError::Storage);
    }
    Ok(normalize_separator(&relative.to_string_lossy()))
}

fn adapter_temporary_path(path: &Path) -> PathBuf {
    let extension = path
        .extension()
        .map(|value| format!("{}.tmp", value.to_string_lossy()))
        .unwrap_or_else(|| "tmp".to_string());
    path.with_extension(extension)
}

fn native_source_version(
    format: &str,
    root: &Path,
    image_path: &Path,
) -> Result<String, ServiceError> {
    match format {
        "yolo-detect" | "yolo-seg" => Ok(yolo_adapter::current_source_version(root, image_path)),
        "voc-detect" => Ok(voc_adapter::current_source_version(root, image_path)),
        "labelme" => Ok(labelme::current_source_version(root, image_path)),
        _ => Err(ServiceError::AnnotationValidation),
    }
}

fn image_content_type(path: &Path) -> Result<&'static str, ServiceError> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jpg" | "jpeg") => Ok("image/jpeg"),
        Some("png") => Ok("image/png"),
        Some("bmp") => Ok("image/bmp"),
        Some("webp") => Ok("image/webp"),
        _ => Err(ServiceError::UnsupportedMedia),
    }
}

fn indexed_download_name(value: &str, sample_id: &str) -> Result<String, ServiceError> {
    let relative = validated_relative_asset_path(value)?;
    let file_name = relative
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(sample_id);
    Ok(sanitize_download_name(file_name, sample_id))
}

fn thumbnail_download_name(value: &str, sample_id: &str) -> Result<String, ServiceError> {
    let source_name = indexed_download_name(value, sample_id)?;
    let stem = Path::new(&source_name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(sample_id);
    Ok(format!(
        "{}-thumbnail.jpg",
        sanitize_download_name(stem, sample_id)
    ))
}

fn sanitize_download_name(value: &str, fallback: &str) -> String {
    let sanitized = value
        .chars()
        .map(|character| {
            if character.is_control() || matches!(character, '"' | '\\' | '/') {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    if sanitized.trim_matches(['.', ' ']).is_empty() {
        fallback.to_string()
    } else {
        sanitized
    }
}

fn metadata_etag(metadata: &fs::Metadata) -> String {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("\"meta-{:x}-{modified:x}\"", metadata.len())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
        "yolo-detect" | "yolo-seg" | "voc-detect" | "labelme" | "image-classification"
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
        ServiceError::AnnotationValidation => "annotation validation failed",
        ServiceError::NotFound => "project was not found",
        ServiceError::Conflict => "project mutation conflicted with existing state",
        ServiceError::RevisionConflict => "annotation revision conflict",
        ServiceError::UnsupportedMedia => "project media format is not supported",
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

fn remote_mutation_error(error: RemoteMutationError) -> ServiceError {
    match error {
        RemoteMutationError::NotFound => ServiceError::NotFound,
        RemoteMutationError::RevisionConflict => ServiceError::RevisionConflict,
        RemoteMutationError::Storage(error) => storage_failure(error),
    }
}

#[cfg(test)]
mod tests {
    use super::{canonical_project_path_is_direct_child, sync_directory, ServiceError};
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

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

    #[test]
    fn directory_sync_succeeds_for_a_directory_and_rejects_a_missing_path() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "image-annotation-directory-sync-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();

        assert_eq!(sync_directory(&directory), Ok(()));
        assert_eq!(
            sync_directory(&directory.join("missing")),
            Err(ServiceError::Storage)
        );

        fs::remove_dir(directory).unwrap();
    }
}

pub mod ai;
pub mod annotations;
pub mod asset_cache;
pub mod bridge;
mod commands;
pub mod credentials;
pub mod data_project_sync;
pub mod data_project_versioning;
pub mod data_projects;
pub mod datasets;
pub mod diagnostics;
pub mod domain;
pub mod http_backend;
pub mod hybrid;
pub mod importers;
mod platform;
pub mod project_fs;
pub mod publisher;
pub mod remote_admin;
pub mod storage;
pub mod sync_engine;
pub mod windows;

// The iOS cdylib target links the Rust facade separately from the static
// `libapp.a` consumed by Xcode.  Tauri's SwiftRs archive is normally carried
// through its rlib metadata, but rustc does not propagate that native link
// item while producing this intermediate cdylib.  Keep an explicit anchor so
// both device and simulator Rust builds link the Swift bridge archive before
// Xcode links the final app.
#[cfg(target_os = "ios")]
#[link(name = "Tauri", kind = "static")]
unsafe extern "C" {
    fn retain_object(object: *const std::ffi::c_void);
}

use datasets::{BuiltinDataset, DataSourceAnalysis, DownloadJob};
use domain::{
    AnnotationObject, AnnotationSaveResult, AnnotationState, AnnotationTask, AnnotationVersion,
    BackendDesign, BackendTask, ClassSample, DatasetExport, DatasetImage, DatasetProject,
    DatasetSnapshot, ProjectDetail, SampleRepository, TaskItem,
};
use platform::NativeBackdropStatus;
use serde::Serialize;
use std::{
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};
#[cfg(not(mobile))]
use tauri::{
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};
use tauri::{AppHandle, Manager, State, WebviewWindow};

#[cfg(not(mobile))]
const TRAY_ICON_ID: &str = "main-tray";
#[cfg(not(mobile))]
const TRAY_MENU_SHOW_ID: &str = "tray-show";
#[cfg(not(mobile))]
const TRAY_MENU_HIDE_ID: &str = "tray-hide";
#[cfg(not(mobile))]
const TRAY_MENU_ANNOTATE_ID: &str = "tray-annotate";
#[cfg(not(mobile))]
const TRAY_MENU_TASKS_ID: &str = "tray-backend-tasks";
#[cfg(not(mobile))]
const TRAY_MENU_EXPORT_ID: &str = "tray-export";
#[cfg(not(mobile))]
const TRAY_MENU_QUIT_ID: &str = "tray-quit";

#[cfg(not(mobile))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayAction {
    ShowWindow,
    HideWindow,
    StartAnnotation,
    BackendTasks,
    Export,
    Quit,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WindowState {
    decorated: bool,
    maximized: bool,
    fullscreen: bool,
    focused: bool,
    visible: bool,
    inner_size: String,
    scale_factor: f64,
    native_backdrop: NativeBackdropStatus,
}

type NativeBackdropState = Mutex<NativeBackdropStatus>;
type RepositoryState = Mutex<SampleRepository>;
type BackendTaskState = Mutex<Vec<BackendTask>>;

async fn data_project_worker<T, F>(app: AppHandle, operation: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(move || {
        let repository = app.state::<RepositoryState>();
        let _guard = repository.lock().map_err(|error| error.to_string())?;
        operation()
    })
    .await
    .map_err(|error| format!("data project worker failed: {error}"))?
}

#[tauri::command]
async fn list_data_projects(app: AppHandle) -> Result<Vec<data_projects::DataProject>, String> {
    data_project_worker(app, data_projects::list_data_projects).await
}

#[tauri::command]
async fn create_data_project(
    app: AppHandle,
    name: String,
    description: String,
) -> Result<data_projects::DataProject, String> {
    data_project_worker(app, move || {
        data_projects::create_data_project(&name, &description)
    })
    .await
}

#[tauri::command]
async fn assign_dataset_to_data_project(
    app: AppHandle,
    data_project_id: Option<String>,
    dataset_id: String,
) -> Result<(), String> {
    data_project_worker(app, move || {
        data_projects::assign_dataset_to_data_project(data_project_id.as_deref(), &dataset_id)
    })
    .await
}

#[tauri::command]
async fn get_data_project_working_tree(
    app: AppHandle,
    data_project_id: String,
) -> Result<data_projects::DataProjectWorkingTree, String> {
    data_project_worker(app, move || {
        data_project_versioning::get_data_project_versioned_working_tree(&data_project_id)
    })
    .await
}

#[tauri::command]
async fn commit_data_project(
    app: AppHandle,
    data_project_id: String,
    message: String,
) -> Result<data_projects::DataProjectCommit, String> {
    data_project_worker(app, move || {
        data_projects::commit_data_project(&data_project_id, &message)
    })
    .await
}

#[tauri::command]
async fn list_data_project_commits(
    app: AppHandle,
    data_project_id: String,
) -> Result<Vec<data_projects::DataProjectCommit>, String> {
    data_project_worker(app, move || {
        data_projects::list_data_project_commits(&data_project_id)
    })
    .await
}

// Restore operations may replace the branch working tree. An independently
// opened annotator can hold unsaved canvas edits that are absent from SQLite.
pub(crate) fn ensure_data_project_annotators_closed(
    app: &AppHandle,
    data_project_id: &str,
) -> Result<(), String> {
    let project = data_projects::list_data_projects()?
        .into_iter()
        .find(|project| project.id == data_project_id)
        .ok_or_else(|| "数据项目不存在或当前账号无权访问".to_string())?;
    for dataset_id in project.dataset_ids {
        if app
            .get_webview_window(&format!("annotation-{dataset_id}"))
            .is_some()
        {
            return Err(
                "请先保存并关闭该数据项目的独立标注窗口，再切换、合并或还原版本".to_string(),
            );
        }
    }
    Ok(())
}

#[tauri::command]
async fn list_data_project_branches(
    app: AppHandle,
    data_project_id: String,
) -> Result<serde_json::Value, String> {
    data_project_worker(app, move || {
        serde_json::to_value(data_project_versioning::list_data_project_branches(
            &data_project_id,
        )?)
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn create_data_project_branch(
    app: AppHandle,
    data_project_id: String,
    name: String,
    from_commit_id: Option<String>,
) -> Result<serde_json::Value, String> {
    data_project_worker(app, move || {
        serde_json::to_value(data_project_versioning::create_data_project_branch(
            &data_project_id,
            &name,
            from_commit_id.as_deref(),
        )?)
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn switch_data_project_branch(
    app: AppHandle,
    data_project_id: String,
    branch: String,
) -> Result<serde_json::Value, String> {
    ensure_data_project_annotators_closed(&app, &data_project_id)?;
    data_project_worker(app, move || {
        serde_json::to_value(data_project_versioning::switch_data_project_branch(
            &data_project_id,
            &branch,
        )?)
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn delete_data_project_branch(
    app: AppHandle,
    data_project_id: String,
    branch: String,
) -> Result<serde_json::Value, String> {
    data_project_worker(app, move || {
        serde_json::to_value(data_project_versioning::delete_data_project_branch(
            &data_project_id,
            &branch,
        )?)
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn data_project_checkout_status(
    app: AppHandle,
    data_project_id: String,
) -> Result<serde_json::Value, String> {
    data_project_worker(app, move || {
        serde_json::to_value(data_project_versioning::data_project_checkout_status(
            &data_project_id,
            None,
        )?)
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn get_data_project_dataset_groups(
    app: AppHandle,
    data_project_id: String,
    branch: Option<String>,
) -> Result<serde_json::Value, String> {
    data_project_worker(app, move || {
        serde_json::to_value(data_project_versioning::get_data_project_dataset_groups(
            &data_project_id,
            branch.as_deref(),
        )?)
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn set_data_project_dataset_groups(
    app: AppHandle,
    data_project_id: String,
    branch: Option<String>,
    expected_revision: i64,
    groups: Vec<data_project_versioning::DataProjectDatasetGroup>,
) -> Result<serde_json::Value, String> {
    data_project_worker(app, move || {
        serde_json::to_value(data_project_versioning::set_data_project_dataset_groups(
            &data_project_id,
            branch.as_deref(),
            expected_revision,
            groups,
        )?)
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn list_data_project_datasets(
    app: AppHandle,
    data_project_id: String,
) -> Result<Vec<DatasetProject>, String> {
    data_project_worker(app, move || {
        data_projects::list_data_project_datasets(&data_project_id)
    })
    .await
}

#[tauri::command]
async fn list_data_project_version_commits(
    app: AppHandle,
    data_project_id: String,
    branch: Option<String>,
) -> Result<serde_json::Value, String> {
    data_project_worker(app, move || {
        serde_json::to_value(data_project_versioning::list_data_project_version_commits(
            &data_project_id,
            branch.as_deref(),
        )?)
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn commit_data_project_branch(
    app: AppHandle,
    data_project_id: String,
    branch: String,
    message: String,
) -> Result<serde_json::Value, String> {
    data_project_worker(app, move || {
        serde_json::to_value(data_project_versioning::commit_data_project_branch(
            &data_project_id,
            &branch,
            &message,
        )?)
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn preview_data_project_merge(
    app: AppHandle,
    data_project_id: String,
    ours_branch: String,
    theirs_branch: String,
) -> Result<serde_json::Value, String> {
    data_project_worker(app, move || {
        serde_json::to_value(data_project_versioning::preview_data_project_merge(
            &data_project_id,
            &ours_branch,
            &theirs_branch,
        )?)
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn merge_data_project_branches(
    app: AppHandle,
    data_project_id: String,
    ours_branch: String,
    theirs_branch: String,
    message: String,
) -> Result<serde_json::Value, String> {
    ensure_data_project_annotators_closed(&app, &data_project_id)?;
    data_project_worker(app, move || {
        serde_json::to_value(data_project_versioning::merge_data_project_branches(
            &data_project_id,
            &ours_branch,
            &theirs_branch,
            &message,
        )?)
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn resolve_data_project_merge(
    app: AppHandle,
    data_project_id: String,
    session_id: String,
    conflict_key: String,
    resolution: String,
) -> Result<serde_json::Value, String> {
    ensure_data_project_annotators_closed(&app, &data_project_id)?;
    data_project_worker(app, move || {
        serde_json::to_value(data_project_versioning::resolve_data_project_merge(
            &data_project_id,
            &session_id,
            &conflict_key,
            &resolution,
        )?)
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn revert_data_project_commit(
    app: AppHandle,
    data_project_id: String,
    branch: String,
    commit_id: String,
    message: String,
) -> Result<serde_json::Value, String> {
    ensure_data_project_annotators_closed(&app, &data_project_id)?;
    data_project_worker(app, move || {
        serde_json::to_value(data_project_versioning::revert_data_project_commit(
            &data_project_id,
            &branch,
            &commit_id,
            &message,
        )?)
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn list_remote_data_projects(app: AppHandle) -> Result<serde_json::Value, String> {
    data_project_worker(app, move || {
        serde_json::to_value(data_project_sync::list_remote_data_projects()?)
            .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn create_remote_data_project(
    app: AppHandle,
    name: String,
    description: String,
    organization_id: Option<String>,
) -> Result<serde_json::Value, String> {
    data_project_worker(app, move || {
        serde_json::to_value(data_project_sync::create_remote_data_project(
            &name,
            &description,
            organization_id.as_deref(),
        )?)
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn connect_remote_data_project(
    app: AppHandle,
    data_project_id: String,
    remote_project_id: String,
) -> Result<serde_json::Value, String> {
    data_project_worker(app, move || {
        serde_json::to_value(data_project_sync::connect_remote_data_project(
            &data_project_id,
            &remote_project_id,
        )?)
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn get_data_project_sync_status(
    app: AppHandle,
    data_project_id: String,
) -> Result<serde_json::Value, String> {
    data_project_worker(app, move || {
        serde_json::to_value(data_project_sync::get_data_project_sync_status(
            &data_project_id,
        )?)
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn publish_data_project(
    app: AppHandle,
    data_project_id: String,
) -> Result<serde_json::Value, String> {
    ensure_data_project_annotators_closed(&app, &data_project_id)?;
    data_project_worker(app, move || {
        serde_json::to_value(data_project_sync::publish_data_project(&data_project_id)?)
            .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn sync_data_project(
    app: AppHandle,
    data_project_id: String,
) -> Result<serde_json::Value, String> {
    ensure_data_project_annotators_closed(&app, &data_project_id)?;
    data_project_worker(app, move || {
        serde_json::to_value(data_project_sync::sync_data_project(&data_project_id)?)
            .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn clone_remote_data_project(
    app: AppHandle,
    remote_project_id: String,
) -> Result<serde_json::Value, String> {
    data_project_worker(app, move || {
        serde_json::to_value(data_project_sync::clone_remote_data_project(
            &remote_project_id,
        )?)
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn list_data_project_cloud_members(
    app: AppHandle,
    data_project_id: String,
) -> Result<serde_json::Value, String> {
    data_project_worker(app, move || {
        serde_json::to_value(data_project_sync::list_data_project_cloud_members(
            &data_project_id,
        )?)
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn upsert_data_project_cloud_member(
    app: AppHandle,
    data_project_id: String,
    user_id: String,
    role: String,
) -> Result<serde_json::Value, String> {
    data_project_worker(app, move || {
        serde_json::to_value(data_project_sync::upsert_data_project_cloud_member(
            &data_project_id,
            &user_id,
            &role,
        )?)
        .map_err(|error| error.to_string())
    })
    .await
}

#[tauri::command]
async fn remove_data_project_cloud_member(
    app: AppHandle,
    data_project_id: String,
    user_id: String,
) -> Result<serde_json::Value, String> {
    data_project_worker(app, move || {
        serde_json::to_value(data_project_sync::remove_data_project_cloud_member(
            &data_project_id,
            &user_id,
        )?)
        .map_err(|error| error.to_string())
    })
    .await
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct OfflineStatus {
    online: bool,
    queued_operations: u64,
    conflict_count: u64,
    cached_assets: u64,
    last_sync_at: Option<String>,
    last_error: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct OfflineRuntimeState {
    // This starts false deliberately.  A stored access token is not proof
    // that the server is reachable; only a completed sync may mark it online.
    online: bool,
    last_sync_at: Option<String>,
    last_error: Option<String>,
}

static OFFLINE_RUNTIME_STATE: OnceLock<Mutex<OfflineRuntimeState>> = OnceLock::new();

fn offline_runtime_state() -> &'static Mutex<OfflineRuntimeState> {
    OFFLINE_RUNTIME_STATE.get_or_init(|| Mutex::new(OfflineRuntimeState::default()))
}

fn unix_timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

fn update_offline_runtime(success: bool, error: Option<String>) {
    if let Ok(mut state) = offline_runtime_state().lock() {
        state.online = success;
        state.last_error = error;
        if success {
            state.last_sync_at = Some(unix_timestamp());
        }
    }
}

fn offline_runtime_snapshot() -> OfflineRuntimeState {
    offline_runtime_state()
        .lock()
        .map(|state| state.clone())
        .unwrap_or_default()
}

#[cfg(any())]
mod legacy_account_commands {

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RemoteSessionStatus {
        authenticated: bool,
        server_url: Option<String>,
        account_id: Option<String>,
        device_id: String,
        access_expires_at: Option<i64>,
    }

    #[derive(Debug, Clone, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RemoteLoginResponse {
        #[serde(alias = "access_token")]
        access_token: String,
        #[serde(alias = "refresh_token")]
        refresh_token: String,
        #[serde(
            default,
            alias = "expiresAt",
            alias = "expires_at",
            alias = "expiresAtUnix",
            alias = "expires_at_unix"
        )]
        access_expires_at: Option<Value>,
        #[serde(default)]
        user: Option<Value>,
        #[serde(default)]
        account_id: Option<String>,
    }

    impl RemoteLoginResponse {
        fn access_expiry(&self) -> Option<i64> {
            let value = self.access_expires_at.as_ref()?;
            value
                .as_i64()
                .or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
                .or_else(|| {
                    value.as_str().and_then(|value| {
                        value.parse::<i64>().ok().or_else(|| {
                            DateTime::parse_from_rfc3339(value)
                                .ok()
                                .map(|date| date.timestamp())
                        })
                    })
                })
        }
    }

    fn json_field<'a>(value: &'a Value, names: &[&str]) -> Option<&'a Value> {
        names.iter().find_map(|name| value.get(*name))
    }

    fn remote_url(server_url: &str, path: &str) -> Result<String, String> {
        let server_url = server_url.trim_end_matches('/');
        if !(server_url.starts_with("http://") || server_url.starts_with("https://")) {
            return Err("server URL must use http or https".to_string());
        }
        let path = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        };
        Ok(format!("{server_url}{path}"))
    }

    fn remote_http_request(
        method: &str,
        url: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> Result<(u16, Value), String> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(45))
            .build()
            .map_err(|error| format!("failed to create remote client: {error}"))?;
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|error| format!("invalid HTTP method: {error}"))?;
        let mut request = client.request(method, url);
        if let Some(token) = token.filter(|value| !value.trim().is_empty()) {
            request = request.bearer_auth(token.trim());
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .map_err(|error| format!("remote request failed: {error}"))?;
        let status = response.status().as_u16();
        let payload = response.json::<Value>().unwrap_or_else(|_| json!({}));
        Ok((status, payload))
    }

    fn session_summary(
        session: Option<credentials::SessionCredentials>,
    ) -> Result<RemoteSessionStatus, String> {
        let device_id = credentials::try_device_id()?;
        Ok(match session {
            Some(session) => RemoteSessionStatus {
                authenticated: !session.access_token.is_empty(),
                server_url: Some(session.server_url),
                account_id: Some(session.account_id),
                device_id,
                access_expires_at: session.access_expires_at,
            },
            None => RemoteSessionStatus {
                authenticated: false,
                server_url: None,
                account_id: None,
                device_id,
                access_expires_at: None,
            },
        })
    }

    #[tauri::command]
    async fn auth_login(
        server_url: String,
        email: String,
        password: String,
        device_name: Option<String>,
    ) -> Result<RemoteSessionStatus, String> {
        tauri::async_runtime::spawn_blocking(move || {
        let url = remote_url(&server_url, "/api/v1/auth/login")?;
        let device_id = credentials::try_device_id()?;
        let body = json!({
            "email": email,
            "password": password,
            "deviceId": device_id,
            "platform": if cfg!(target_os = "android") { "android" } else if cfg!(target_os = "ios") { "ios" } else { "desktop" },
            "deviceName": device_name.unwrap_or_else(|| "Image Annotation".to_string())
        });
        let (status, payload) = remote_http_request("POST", &url, None, Some(body))?;
        if !(200..300).contains(&status) {
            return Err(format!("login failed ({status}): {}", payload));
        }
        let response: RemoteLoginResponse = serde_json::from_value(payload.clone())
            .map_err(|error| format!("login response is invalid: {error}"))?;
        let account_id = response
            .account_id
            .or_else(|| response.user.as_ref().and_then(|user| json_field(user, &["id", "userId"]).and_then(Value::as_str).map(ToString::to_string)))
            .or_else(|| {
                let (me_status, me_payload) =
                    remote_http_request("GET", &remote_url(&server_url, "/api/v1/auth/me").ok()?, Some(&response.access_token), None).ok()?;
                if !(200..300).contains(&me_status) {
                    return None;
                }
                json_field(&me_payload, &["id", "accountId", "userId"])
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            })
            .ok_or_else(|| "login response does not include account id (and /auth/me did not resolve it)".to_string())?;
        let session = credentials::SessionCredentials {
            server_url: server_url.trim_end_matches('/').to_string(),
            account_id: account_id.clone(),
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            device_id,
            access_expires_at: response.access_expiry(),
        };
        credentials::store_session(&account_id, &session)?;
        credentials::set_active_scope(&account_id)?;
        session_summary(Some(session))
    })
    .await
    .map_err(|error| format!("login worker failed: {error}"))?
    }

    #[tauri::command]
    async fn auth_refresh() -> Result<RemoteSessionStatus, String> {
        tauri::async_runtime::spawn_blocking(|| {
            let Some(current) = credentials::current_session()? else {
                return Err("no authenticated session".to_string());
            };
            if current.refresh_token.is_empty() {
                return Err("refresh token is unavailable".to_string());
            }
            let url = remote_url(&current.server_url, "/api/v1/auth/refresh")?;
            let (status, payload) = remote_http_request(
                "POST",
                &url,
                None,
                Some(
                    json!({ "refreshToken": current.refresh_token, "deviceId": current.device_id }),
                ),
            )?;
            if !(200..300).contains(&status) {
                return Err(format!("session refresh failed ({status}): {}", payload));
            }
            let response: RemoteLoginResponse = serde_json::from_value(payload)
                .map_err(|error| format!("refresh response is invalid: {error}"))?;
            let session = credentials::SessionCredentials {
                access_token: response.access_token,
                refresh_token: response.refresh_token,
                access_expires_at: response.access_expiry(),
                ..current.clone()
            };
            credentials::store_session(&session.account_id, &session)?;
            session_summary(Some(session))
        })
        .await
        .map_err(|error| format!("refresh worker failed: {error}"))?
    }

    #[tauri::command]
    async fn auth_logout() -> Result<(), String> {
        tauri::async_runtime::spawn_blocking(|| {
            let Some(current) = credentials::current_session()? else {
                return Ok(());
            };
            let url = remote_url(&current.server_url, "/api/v1/auth/logout")?;
            let _ = remote_http_request("POST", &url, Some(&current.access_token), None);
            credentials::delete_session(&current.account_id)
        })
        .await
        .map_err(|error| format!("logout worker failed: {error}"))?
    }

    #[tauri::command]
    fn auth_status() -> Result<RemoteSessionStatus, String> {
        session_summary(credentials::current_session()?)
    }

    #[tauri::command]
    async fn remote_request(
        method: String,
        path: String,
        body: Option<Value>,
    ) -> Result<Value, String> {
        tauri::async_runtime::spawn_blocking(move || {
            let current = credentials::refreshed_session()?
                .ok_or_else(|| "no authenticated session".to_string())?;
            let url = remote_url(&current.server_url, &path)?;
            let (status, payload) =
                remote_http_request(&method, &url, Some(&current.access_token), body)?;
            if !(200..300).contains(&status) {
                return Err(format!("remote request failed ({status}): {}", payload));
            }
            Ok(payload)
        })
        .await
        .map_err(|error| format!("remote request worker failed: {error}"))?
    }

    fn scoped_local_project_id(
        server_url: &str,
        account_id: &str,
        remote_project_id: &str,
    ) -> String {
        let scope = format!(
            "{}\n{}\n{}",
            server_url.trim_end_matches('/'),
            account_id,
            remote_project_id
        );
        format!("remote-{:x}", Sha256::digest(scope.as_bytes()))
    }

    fn validate_remote_project_id(project_id: &str) -> Result<(), String> {
        if project_id.trim().is_empty()
            || project_id.contains('/')
            || project_id.contains('\\')
            || project_id.contains('?')
            || project_id.contains('#')
        {
            return Err("remote project ID is invalid".to_string());
        }
        Ok(())
    }

    #[tauri::command]
    async fn remote_project_open(
        server_url: String,
        remote_project_id: String,
        mode: Option<String>,
        cache_policy: Option<String>,
    ) -> Result<Value, String> {
        tauri::async_runtime::spawn_blocking(move || {
        let current = credentials::refreshed_session()?
            .ok_or_else(|| "no authenticated session".to_string())?;
        let server_url = server_url.trim_end_matches('/').to_string();
        if server_url != current.server_url.trim_end_matches('/') {
            return Err("project server does not match the active account session".to_string());
        }
        validate_remote_project_id(&remote_project_id)?;
        let bootstrap_url = remote_url(
            &server_url,
            &format!("/api/v1/projects/{remote_project_id}/sync-bootstrap"),
        )?;
        let (status, payload) =
            remote_http_request("GET", &bootstrap_url, Some(&current.access_token), None)?;
        if !(200..300).contains(&status) {
            return Err(format!("project bootstrap failed ({status}): {}", payload));
        }
        let project = payload.get("project").cloned().unwrap_or_else(|| json!({}));
        let local_id = scoped_local_project_id(&server_url, &current.account_id, &remote_project_id);
        let name = project
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Remote project")
            .to_string();
        let paths = project_fs::ensure_workspace_project_dirs(&local_id)?;
        let manifest = project_fs::ProjectManifest {
            id: local_id.clone(),
            name,
            source_dataset_key: format!(
                "remote:{}:{}:{}",
                server_url, current.account_id, remote_project_id
            ),
            format: "remote".to_string(),
            root_path: paths.root.to_string_lossy().to_string(),
            created_at: format!(
                "{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_secs())
                    .unwrap_or_default()
            ),
            class_count: 0,
            image_count: payload
                .get("assets")
                .and_then(Value::as_array)
                .map(|items| items.len() as u32)
                .unwrap_or(0),
        };
        project_fs::write_manifest_to_path(&manifest, &paths.manifest)?;
        storage::initialize_project_database(&paths.sqlite)?;
        // configure_remote_project updates the existing project row. Upsert
        // the manifest first so a remote bootstrap cannot leave a config-only
        // local database that the list/sync paths cannot discover.
        storage::upsert_project_index(&paths.sqlite, &manifest, &[], &[])?;
        let mut connection =
            rusqlite::Connection::open(&paths.sqlite).map_err(|error| error.to_string())?;
        hybrid::configure_remote_project(
            &mut connection,
            &local_id,
            &server_url,
            &remote_project_id,
            &current.device_id,
            mode.as_deref().unwrap_or("cloud_linked"),
            cache_policy.as_deref().unwrap_or("on_demand"),
            true,
        )?;
        let sync = sync_engine::sync_project(&paths.sqlite, &local_id, None)?;
        Ok(json!({ "projectId": local_id.clone(), "localProjectId": local_id, "remoteProjectId": remote_project_id, "serverUrl": server_url, "accountId": current.account_id, "sync": sync }))
    })
    .await
    .map_err(|error| format!("project open worker failed: {error}"))?
    }
}

#[cfg(not(mobile))]
#[tauri::command]
fn start_drag_window(window: WebviewWindow) -> Result<(), String> {
    window.start_dragging().map_err(|err| err.to_string())
}

#[cfg(not(mobile))]
#[tauri::command]
fn minimize_window(window: WebviewWindow) -> Result<(), String> {
    window.minimize().map_err(|err| err.to_string())
}

#[cfg(not(mobile))]
#[tauri::command]
fn toggle_maximize_window(window: WebviewWindow) -> Result<(), String> {
    if window.is_maximized().map_err(|err| err.to_string())? {
        window.unmaximize().map_err(|err| err.to_string())
    } else {
        window.maximize().map_err(|err| err.to_string())
    }
}

#[cfg(not(mobile))]
#[tauri::command]
fn hide_to_tray(window: WebviewWindow) -> Result<(), String> {
    window.hide().map_err(|err| err.to_string())
}

#[cfg(not(mobile))]
#[tauri::command]
fn show_window(app: AppHandle) -> Result<(), String> {
    show_main_window(&app)
}

#[cfg(not(mobile))]
#[tauri::command]
fn close_window(window: WebviewWindow) -> Result<(), String> {
    window.hide().map_err(|err| err.to_string())
}

#[tauri::command]
fn window_state(
    window: WebviewWindow,
    backdrop_state: State<'_, NativeBackdropState>,
) -> Result<WindowState, String> {
    let inner_size = window.inner_size().map_err(|err| err.to_string())?;
    let native_backdrop = backdrop_state
        .lock()
        .map_err(|err| err.to_string())?
        .clone();

    Ok(WindowState {
        decorated: window.is_decorated().unwrap_or(false),
        maximized: window.is_maximized().unwrap_or(false),
        fullscreen: window.is_fullscreen().map_err(|err| err.to_string())?,
        focused: window.is_focused().map_err(|err| err.to_string())?,
        visible: window.is_visible().map_err(|err| err.to_string())?,
        inner_size: format!("{} x {}", inner_size.width, inner_size.height),
        scale_factor: window.scale_factor().unwrap_or(1.0),
        native_backdrop,
    })
}

#[tauri::command]
fn list_dataset_projects(
    repository: State<'_, RepositoryState>,
) -> Result<Vec<DatasetProject>, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    let mut projects = repository.dataset_projects();
    let session = credentials::current_session()?;
    projects.retain(|project| {
        let paths = project_fs::project_paths(&project.id);
        if storage::initialize_project_database(&paths.sqlite).is_err() {
            return false;
        }
        let connection = match rusqlite::Connection::open(&paths.sqlite) {
            Ok(connection) => connection,
            Err(_) => return true,
        };
        let deleted_at = connection
            .query_row(
                "SELECT deleted_at FROM projects WHERE id = ?1",
                [&project.id],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten();
        if deleted_at.is_some() {
            return false;
        }
        let remote = connection
            .query_row(
                "SELECT server_url, remote_project_id, account_id FROM remote_project_configs WHERE project_id = ?1",
                [&project.id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .ok();
        let Some((remote_server, remote_project_id, owner_account_id)) = remote else {
            return true;
        };
        let Some(session) = session.as_ref() else {
            return false;
        };
        if remote_server.trim_end_matches('/') != session.server_url.trim_end_matches('/') {
            return false;
        }
        if owner_account_id
            .as_deref()
            .is_some_and(|owner| owner != session.account_id)
        {
            return false;
        }
        let source_key = project_fs::read_manifest(&project.id)
            .map(|manifest| manifest.source_dataset_key)
            .unwrap_or_default();
        !source_key.starts_with("remote:")
            || project.id
                == commands::account::scoped_local_project_id(
                    &session.server_url,
                    &session.account_id,
                    &remote_project_id,
                )
    });
    Ok(projects)
}

#[tauri::command]
fn get_project_detail(
    repository: State<'_, RepositoryState>,
    project_id: String,
) -> Result<ProjectDetail, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository
        .project_detail(&project_id)
        .ok_or_else(|| format!("project not found: {project_id}"))
}

#[tauri::command]
fn get_backend_design() -> BackendDesign {
    domain::backend_design()
}

#[tauri::command]
fn backend_health() -> serde_json::Value {
    #[cfg(mobile)]
    {
        return serde_json::json!({
            "status": "mobile",
            "transport": "remote",
            "message": "移动端使用远程协作服务"
        });
    }
    #[cfg(not(mobile))]
    http_backend::desktop_health_payload()
}

#[tauri::command]
fn list_builtin_datasets() -> Result<Vec<BuiltinDataset>, String> {
    project_fs::ensure_test_data_dirs()?;
    Ok(datasets::builtin_datasets())
}

#[tauri::command]
async fn download_test_dataset(
    tasks: State<'_, BackendTaskState>,
    dataset_key: String,
) -> Result<DownloadJob, String> {
    record_backend_task(
        &tasks,
        BackendTask::new(
            format!("download-{dataset_key}"),
            format!("{} 导入", dataset_key.to_ascii_uppercase()),
            "dataset-import",
            "running",
            10,
            "正在准备测试数据集",
        ),
    )?;

    // Network and archive I/O must not block the native event loop or async workers.
    let download_key = dataset_key.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        datasets::download_test_dataset(&download_key)
    })
    .await
    .map_err(|error| format!("dataset download worker failed: {error}"))
    .and_then(|result| result);

    match result {
        Ok(job) => {
            record_backend_task(
                &tasks,
                BackendTask::new(
                    job.id.clone(),
                    format!("{} 导入", dataset_key.to_ascii_uppercase()),
                    "dataset-import",
                    "completed",
                    job.progress,
                    job.message.clone(),
                )
                .finished(),
            )?;
            Ok(job)
        }
        Err(error) => {
            record_backend_task(
                &tasks,
                BackendTask::new(
                    format!("download-{dataset_key}"),
                    format!("{} 导入", dataset_key.to_ascii_uppercase()),
                    "dataset-import",
                    "failed",
                    0,
                    error.clone(),
                )
                .finished(),
            )?;
            Err(error)
        }
    }
}

#[tauri::command]
fn create_dataset_project(
    tasks: State<'_, BackendTaskState>,
    name: String,
    dataset_type: String,
    demo_template: String,
) -> Result<DatasetProject, String> {
    let task_id = format!("create-{}", dataset_slug(&name, &demo_template));
    record_backend_task(
        &tasks,
        BackendTask::new(
            task_id.clone(),
            format!("{name} 创建"),
            "dataset-create",
            "running",
            20,
            "正在创建本地数据集工程",
        ),
    )?;

    match datasets::create_dataset_project(&name, &dataset_type, &demo_template) {
        Ok(project) => {
            record_backend_task(
                &tasks,
                BackendTask::new(
                    task_id,
                    format!("{} 创建", project.name),
                    "dataset-create",
                    "completed",
                    100,
                    format!(
                        "{} 已创建，包含 {} 张图片",
                        project.name, project.image_count
                    ),
                )
                .finished(),
            )?;
            Ok(project)
        }
        Err(error) => {
            record_backend_task(
                &tasks,
                BackendTask::new(
                    task_id,
                    format!("{name} 创建"),
                    "dataset-create",
                    "failed",
                    0,
                    error.clone(),
                )
                .finished(),
            )?;
            Err(error)
        }
    }
}

#[tauri::command]
fn create_project(
    tasks: State<'_, BackendTaskState>,
    name: String,
    dataset_type: String,
) -> Result<DatasetProject, String> {
    create_dataset_project(tasks, name, dataset_type, "empty".to_string())
}

#[tauri::command]
fn import_images(
    tasks: State<'_, BackendTaskState>,
    project_id: String,
    source_path: String,
) -> Result<DatasetProject, String> {
    let task_id = format!("import-images-{project_id}");
    record_backend_task(
        &tasks,
        BackendTask::new(
            task_id.clone(),
            "图片目录导入",
            "image-import",
            "running",
            20,
            "正在导入本地图片目录",
        ),
    )?;
    match datasets::import_images_into_project(&project_id, &source_path) {
        Ok(project) => {
            record_backend_task(
                &tasks,
                BackendTask::new(
                    task_id,
                    "图片目录导入",
                    "image-import",
                    "completed",
                    100,
                    format!("已导入并索引 {} 张图片", project.image_count),
                )
                .finished(),
            )?;
            Ok(project)
        }
        Err(error) => {
            record_backend_task(
                &tasks,
                BackendTask::new(task_id, "图片目录导入", "image-import", "failed", 0, &error)
                    .finished(),
            )?;
            Err(error)
        }
    }
}

#[tauri::command]
fn import_yolo_dataset(
    tasks: State<'_, BackendTaskState>,
    project_id: String,
    source_path: String,
) -> Result<DatasetProject, String> {
    let task_id = format!("import-yolo-{project_id}");
    record_backend_task(
        &tasks,
        BackendTask::new(
            task_id.clone(),
            "YOLO 数据集导入",
            "yolo-import",
            "running",
            20,
            "正在导入 YOLO 数据集目录",
        ),
    )?;
    match datasets::import_yolo_dataset_into_project(&project_id, &source_path) {
        Ok(project) => {
            record_backend_task(
                &tasks,
                BackendTask::new(
                    task_id,
                    "YOLO 数据集导入",
                    "yolo-import",
                    "completed",
                    100,
                    format!("已导入并索引 {} 张图片", project.image_count),
                )
                .finished(),
            )?;
            Ok(project)
        }
        Err(error) => {
            record_backend_task(
                &tasks,
                BackendTask::new(
                    task_id,
                    "YOLO 数据集导入",
                    "yolo-import",
                    "failed",
                    0,
                    &error,
                )
                .finished(),
            )?;
            Err(error)
        }
    }
}

#[tauri::command]
fn pick_data_source(selection_type: String) -> Result<Option<Vec<String>>, String> {
    datasets::pick_data_source(&selection_type)
}

#[tauri::command]
fn analyze_data_source(source_paths: Vec<String>) -> Result<DataSourceAnalysis, String> {
    datasets::analyze_data_source(&source_paths)
}

#[tauri::command]
fn import_files(
    tasks: State<'_, BackendTaskState>,
    project_id: String,
    source_paths: Vec<String>,
) -> Result<DatasetProject, String> {
    let task_id = format!("import-files-{project_id}");
    record_backend_task(
        &tasks,
        BackendTask::new(
            task_id.clone(),
            "选择文件导入",
            "file-import",
            "running",
            20,
            "正在复制选择的本机文件",
        ),
    )?;
    match datasets::import_files_into_project(&project_id, &source_paths) {
        Ok(project) => {
            record_backend_task(
                &tasks,
                BackendTask::new(
                    task_id,
                    "选择文件导入",
                    "file-import",
                    "completed",
                    100,
                    format!("已导入并索引 {} 张图片", project.image_count),
                )
                .finished(),
            )?;
            Ok(project)
        }
        Err(error) => {
            record_backend_task(
                &tasks,
                BackendTask::new(task_id, "选择文件导入", "file-import", "failed", 0, &error)
                    .finished(),
            )?;
            Err(error)
        }
    }
}

#[tauri::command]
fn open_local_dataset(
    tasks: State<'_, BackendTaskState>,
    source_path: String,
    dataset_type: String,
) -> Result<DatasetProject, String> {
    let task_id = format!("open-local-{}", dataset_slug(&source_path, "dataset"));
    record_backend_task(
        &tasks,
        BackendTask::new(
            task_id.clone(),
            "打开本机数据集",
            "local-dataset-open",
            "running",
            20,
            "正在索引本机目录",
        ),
    )?;
    match datasets::open_local_dataset(&source_path, &dataset_type) {
        Ok(project) => {
            record_backend_task(
                &tasks,
                BackendTask::new(
                    task_id,
                    "打开本机数据集",
                    "local-dataset-open",
                    "completed",
                    100,
                    format!("已索引 {} 张图片", project.image_count),
                )
                .finished(),
            )?;
            Ok(project)
        }
        Err(error) => {
            record_backend_task(
                &tasks,
                BackendTask::new(
                    task_id,
                    "打开本机数据集",
                    "local-dataset-open",
                    "failed",
                    0,
                    &error,
                )
                .finished(),
            )?;
            Err(error)
        }
    }
}

#[tauri::command]
fn rescan_project_assets(project_id: String) -> Result<DatasetProject, String> {
    datasets::rescan_project_assets(&project_id)
}

#[tauri::command]
fn generate_thumbnails(project_id: String) -> Result<u32, String> {
    datasets::generate_project_thumbnails(&project_id)
}

#[tauri::command]
fn get_dataset_download_job(dataset_key: String) -> Result<Option<DownloadJob>, String> {
    Ok(datasets::completed_job(&dataset_key))
}

#[tauri::command]
fn list_project_images(
    repository: State<'_, RepositoryState>,
    project_id: String,
    group_id: Option<String>,
    offset: Option<u32>,
    limit: Option<u32>,
) -> Result<Vec<DatasetImage>, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    Ok(repository.project_images_paged(&project_id, group_id, offset, limit))
}

#[tauri::command]
fn list_class_samples(
    repository: State<'_, RepositoryState>,
    project_id: String,
    class_id: Option<u32>,
    label: String,
    offset: Option<u32>,
    limit: Option<u32>,
) -> Result<Vec<ClassSample>, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    Ok(repository.class_samples(&project_id, class_id, &label, offset, limit))
}

#[tauri::command]
fn get_image_annotations(
    repository: State<'_, RepositoryState>,
    project_id: String,
    image_id: String,
) -> Result<Vec<AnnotationObject>, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    Ok(repository.image_annotations(&project_id, &image_id))
}

#[tauri::command]
fn get_image_annotation_state(
    repository: State<'_, RepositoryState>,
    project_id: String,
    image_id: String,
) -> Result<AnnotationState, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    Ok(repository.image_annotation_state(&project_id, &image_id))
}

#[tauri::command]
fn save_image_annotations(
    repository: State<'_, RepositoryState>,
    project_id: String,
    image_id: String,
    revision: Option<String>,
    objects: Vec<AnnotationObject>,
) -> Result<AnnotationSaveResult, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.save_image_annotations_with_revision(&project_id, &image_id, revision, objects)
}

#[tauri::command]
fn submit_image_annotations(
    repository: State<'_, RepositoryState>,
    project_id: String,
    image_id: String,
) -> Result<(), String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.submit_image_annotations(&project_id, &image_id)
}

#[tauri::command]
fn set_image_verified(
    repository: State<'_, RepositoryState>,
    project_id: String,
    image_id: String,
    verified: bool,
) -> Result<(), String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.set_image_verified(&project_id, &image_id, verified)
}

#[tauri::command]
fn load_external_annotations(
    repository: State<'_, RepositoryState>,
    project_id: String,
    image_id: String,
    source_path: String,
) -> Result<crate::importers::labelme::ExternalAnnotations, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.load_external_annotations(&project_id, &image_id, &source_path)
}

#[tauri::command]
fn export_annotation_file(
    repository: State<'_, RepositoryState>,
    project_id: String,
    image_id: String,
    format: String,
    output_path: Option<String>,
) -> Result<Option<String>, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.export_annotation_file(&project_id, &image_id, &format, output_path)
}

#[tauri::command]
async fn run_ai_annotation(
    repository: State<'_, RepositoryState>,
    project_id: String,
    image_id: String,
    options: serde_json::Value,
) -> Result<ai::AiRunResult, String> {
    let context = {
        let repository = repository.lock().map_err(|err| err.to_string())?;
        ai::prepare(&repository, &project_id, &image_id)?
    };
    tauri::async_runtime::spawn_blocking(move || ai::run(context, options))
        .await
        .map_err(|err| err.to_string())?
}

#[tauri::command]
fn cancel_ai_annotation(job_id: String) -> Result<bool, String> {
    ai::cancel(&job_id)
}

#[tauri::command]
fn get_ai_job_status(job_id: String) -> Result<Option<serde_json::Value>, String> {
    ai::status(&job_id)
}

#[tauri::command]
fn list_issues(
    repository: State<'_, RepositoryState>,
    project_id: String,
    include_closed: Option<bool>,
) -> Result<Vec<hybrid::IssueRecord>, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.project_issues(&project_id, include_closed.unwrap_or(false))
}

#[tauri::command]
fn create_issue(
    repository: State<'_, RepositoryState>,
    project_id: String,
    image_id: String,
    annotation_object_id: Option<String>,
    title: String,
    description: String,
    severity: String,
    assignee_id: Option<String>,
) -> Result<hybrid::IssueRecord, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.create_project_issue(
        &project_id,
        &image_id,
        annotation_object_id.as_deref(),
        &title,
        &description,
        &severity,
        assignee_id.as_deref(),
    )
}

#[tauri::command]
fn transition_issue(
    repository: State<'_, RepositoryState>,
    project_id: String,
    issue_id: String,
    next_status: String,
) -> Result<hybrid::IssueRecord, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.transition_project_issue(&project_id, &issue_id, &next_status)
}

#[tauri::command]
fn add_issue_comment(
    repository: State<'_, RepositoryState>,
    project_id: String,
    issue_id: String,
    content: String,
) -> Result<hybrid::IssueCommentRecord, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.add_project_issue_comment(&project_id, &issue_id, &content)
}

#[tauri::command]
fn list_issue_comments(
    repository: State<'_, RepositoryState>,
    project_id: String,
    issue_id: String,
) -> Result<Vec<hybrid::IssueCommentRecord>, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.project_issue_comments(&project_id, &issue_id)
}

#[tauri::command]
fn get_project_sync_summary(
    repository: State<'_, RepositoryState>,
    project_id: String,
) -> Result<hybrid::SyncSummary, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.project_sync_summary(&project_id)
}

fn project_matches_active_scope(
    project_id: &str,
    session: Option<&credentials::SessionCredentials>,
) -> Result<bool, String> {
    let paths = project_fs::project_paths(project_id);
    if !paths.manifest.exists() || !paths.sqlite.exists() {
        return Ok(false);
    }
    storage::initialize_project_database(&paths.sqlite)?;
    let connection =
        rusqlite::Connection::open(&paths.sqlite).map_err(|error| error.to_string())?;
    let deleted = connection
        .query_row(
            "SELECT deleted_at FROM projects WHERE id = ?1",
            [project_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten();
    if deleted.is_some() {
        return Ok(false);
    }
    let remote = connection
        .query_row(
            "SELECT server_url, remote_project_id, account_id FROM remote_project_configs WHERE project_id = ?1",
            [project_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .ok();
    let Some((server_url, remote_project_id, owner_account_id)) = remote else {
        // Local-only projects have no account scope and remain visible to all
        // sessions on this installation.
        return Ok(true);
    };
    let Some(session) = session else {
        return Ok(false);
    };
    if owner_account_id
        .as_deref()
        .is_some_and(|owner| owner != session.account_id)
    {
        return Ok(false);
    }
    let Some(manifest) = project_fs::read_manifest(project_id) else {
        return Ok(false);
    };
    // Mobile remote-projects use an explicit account-scoped source key and a
    // hash-derived local id. Older desktop projects can have a remote config
    // while retaining their historical local id/source key; keep those
    // projects visible and let their legacy project credential authorize sync.
    if !manifest.source_dataset_key.starts_with("remote:") {
        return Ok(server_url.trim_end_matches('/') == session.server_url.trim_end_matches('/'));
    }
    let expected_source = format!(
        "remote:{}:{}:{}",
        server_url.trim_end_matches('/'),
        session.account_id,
        remote_project_id
    );
    Ok(
        server_url.trim_end_matches('/') == session.server_url.trim_end_matches('/')
            && project_id
                == commands::account::scoped_local_project_id(
                    &session.server_url,
                    &session.account_id,
                    &remote_project_id,
                )
            && manifest.source_dataset_key == expected_source,
    )
}

fn active_scope_project_ids(project_id: Option<&str>) -> Result<Vec<String>, String> {
    let session = credentials::current_session()?;
    let mut project_ids = Vec::new();
    for manifest in project_fs::list_project_manifests() {
        if project_id.is_some_and(|requested| requested != manifest.id) {
            continue;
        }
        if project_matches_active_scope(&manifest.id, session.as_ref())? {
            project_ids.push(manifest.id);
        }
    }
    if let Some(requested) = project_id {
        if !project_ids.iter().any(|item| item == requested) {
            return Err("项目不存在，已删除，或不属于当前账号范围".to_string());
        }
    }
    Ok(project_ids)
}

fn project_has_remote_config(project_id: &str) -> Result<bool, String> {
    let paths = project_fs::project_paths(project_id);
    if !paths.sqlite.exists() {
        return Ok(false);
    }
    let connection =
        rusqlite::Connection::open(&paths.sqlite).map_err(|error| error.to_string())?;
    let remote: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM remote_project_configs WHERE project_id = ?1 LIMIT 1",
            [project_id],
            |row| row.get(0),
        )
        .ok();
    Ok(remote.is_some())
}

fn latest_timestamp(current: &mut Option<String>, candidate: Option<String>) {
    let Some(candidate) = candidate.filter(|value| !value.trim().is_empty()) else {
        return;
    };
    if current
        .as_ref()
        .map(|value| candidate > *value)
        .unwrap_or(true)
    {
        *current = Some(candidate);
    }
}

fn collect_offline_status(
    project_ids: &[String],
    authenticated: bool,
) -> Result<OfflineStatus, String> {
    let runtime = offline_runtime_snapshot();
    let mut queued_operations = 0_u64;
    let mut conflict_count = 0_u64;
    let mut cached_assets = 0_u64;
    let mut last_sync_at = runtime.last_sync_at.clone();
    let mut last_error = runtime.last_error.clone();

    for project_id in project_ids {
        let paths = project_fs::project_paths(project_id);
        if !paths.sqlite.exists() {
            continue;
        }
        let connection =
            rusqlite::Connection::open(&paths.sqlite).map_err(|error| error.to_string())?;
        let summary = hybrid::sync_summary(&connection, project_id)?;
        queued_operations = queued_operations
            .saturating_add(u64::from(summary.pending_operations))
            .saturating_add(u64::from(summary.failed_operations));
        conflict_count = conflict_count.saturating_add(u64::from(summary.conflict_count));
        cached_assets =
            cached_assets.saturating_add(asset_cache::summary(&connection)?.entry_count);
        latest_timestamp(&mut last_sync_at, summary.last_pulled_at);
        latest_timestamp(&mut last_sync_at, summary.last_pushed_at);

        let database_error = connection
            .query_row(
                "SELECT error_message FROM sync_outbox WHERE status IN ('failed', 'rejected', 'conflict') AND error_message IS NOT NULL AND error_message != '' ORDER BY updated_at DESC, rowid DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok();
        if database_error.is_some() && last_error.is_none() {
            last_error = database_error;
        }
    }

    Ok(OfflineStatus {
        // Never infer reachability from the existence of credentials.  The
        // runtime flag is set only after a completed sync and is reset after a
        // network/auth failure; logging out also forces this to false.
        online: authenticated && runtime.online,
        queued_operations,
        conflict_count,
        cached_assets,
        last_sync_at,
        last_error,
    })
}

#[tauri::command]
async fn offline_status(project_id: Option<String>) -> Result<OfflineStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let session = credentials::current_session()?;
        let project_ids = active_scope_project_ids(project_id.as_deref())?;
        collect_offline_status(&project_ids, session.is_some())
    })
    .await
    .map_err(|error| format!("offline status worker failed: {error}"))?
}

#[tauri::command]
async fn flush_offline_queue(project_id: Option<String>) -> Result<OfflineStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        // Resolve the scope before touching the network.  This prevents a
        // stale project from one account from ever reusing the current
        // account's bearer token.
        let session = credentials::refreshed_session()?;
        let project_ids = active_scope_project_ids(project_id.as_deref())?;
        let mut first_error = None;
        let mut attempted = 0_u32;

        for id in &project_ids {
            if !project_has_remote_config(id)? {
                continue;
            }
            attempted += 1;
            let paths = project_fs::project_paths(id);
            if let Err(error) = sync_engine::sync_project(&paths.sqlite, id, None) {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }

        if let Some(error) = first_error {
            update_offline_runtime(false, Some(error));
        } else if attempted > 0 {
            update_offline_runtime(true, None);
        }

        collect_offline_status(&project_ids, session.is_some())
    })
    .await
    .map_err(|error| format!("offline sync worker failed: {error}"))?
}

#[tauri::command]
fn configure_remote_project(
    project_id: String,
    server_url: String,
    remote_project_id: String,
    device_id: String,
    mode: String,
    cache_policy: String,
    auto_sync: bool,
) -> Result<hybrid::RemoteProjectConfig, String> {
    let paths = project_fs::project_paths(&project_id);
    storage::initialize_project_database(&paths.sqlite)?;
    let mut connection =
        rusqlite::Connection::open(&paths.sqlite).map_err(|err| err.to_string())?;
    hybrid::configure_remote_project(
        &mut connection,
        &project_id,
        &server_url,
        &remote_project_id,
        &device_id,
        &mode,
        &cache_policy,
        auto_sync,
    )?;
    if let Some(session) = credentials::current_session()? {
        if session.server_url.trim_end_matches('/') == server_url.trim_end_matches('/') {
            connection
                .execute(
                    "UPDATE remote_project_configs SET account_id = ?2 WHERE project_id = ?1",
                    rusqlite::params![project_id, session.account_id],
                )
                .map_err(|error| error.to_string())?;
        }
    }
    hybrid::remote_project_config(&connection, &project_id)?
        .ok_or_else(|| "remote project configuration was not persisted".to_string())
}

#[tauri::command]
async fn sync_project(
    project_id: String,
    access_token: Option<String>,
) -> Result<sync_engine::SyncRunResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let paths = project_fs::project_paths(&project_id);
        sync_engine::sync_project(&paths.sqlite, &project_id, access_token.as_deref())
    })
    .await
    .map_err(|error| format!("sync worker failed: {error}"))?
}

fn cache_remote_asset_inner(
    project_id: String,
    image_id: String,
    access_token: Option<String>,
    pinned: Option<bool>,
) -> Result<asset_cache::AssetCacheRecord, String> {
    let paths = project_fs::project_paths(&project_id);
    storage::initialize_project_database(&paths.sqlite)?;
    let connection =
        rusqlite::Connection::open(&paths.sqlite).map_err(|error| error.to_string())?;
    if let Some(record) = asset_cache::touch(&connection, &image_id, pinned.unwrap_or(false))? {
        return Ok(record);
    }
    let server_url = connection
        .query_row(
            "SELECT server_url FROM remote_project_configs WHERE project_id = ?1",
            [&project_id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| format!("remote project is not configured: {error}"))?;
    let remote_asset_id = connection
        .query_row(
            "SELECT remote_id FROM images WHERE id = ?1 AND remote_id IS NOT NULL",
            [&image_id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| format!("image has not been published: {error}"))?;
    let access_token = access_token
        .filter(|value| !value.trim().is_empty())
        .or(credentials::read_access_token(&project_id)?)
        .ok_or_else(|| "project credential is not configured".to_string())?;
    let ticket =
        asset_cache::request_download_ticket(&server_url, &remote_asset_id, access_token.trim())?;
    let extension = std::path::Path::new(&ticket.file_name)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_owned);
    let fetched = asset_cache::fetch(
        &asset_cache::default_cache_root(&paths.sqlite),
        &asset_cache::CacheFetchRequest {
            download_url: ticket.download_url,
            content_hash: ticket.content_hash,
            expected_size: Some(ticket.byte_size),
            extension,
        },
    )?;
    asset_cache::register(&connection, &image_id, &fetched, pinned.unwrap_or(false))
}

#[tauri::command]
async fn cache_remote_asset(
    project_id: String,
    image_id: String,
    access_token: Option<String>,
    pinned: Option<bool>,
) -> Result<asset_cache::AssetCacheRecord, String> {
    tauri::async_runtime::spawn_blocking(move || {
        cache_remote_asset_inner(project_id, image_id, access_token, pinned)
    })
    .await
    .map_err(|error| format!("asset cache worker failed: {error}"))?
}

#[tauri::command]
fn get_asset_cache_summary(project_id: String) -> Result<asset_cache::AssetCacheSummary, String> {
    let paths = project_fs::project_paths(&project_id);
    storage::initialize_project_database(&paths.sqlite)?;
    let connection =
        rusqlite::Connection::open(&paths.sqlite).map_err(|error| error.to_string())?;
    asset_cache::summary(&connection)
}

#[tauri::command]
fn get_hybrid_diagnostics(project_id: String) -> Result<diagnostics::HybridDiagnostics, String> {
    let paths = project_fs::project_paths(&project_id);
    storage::initialize_project_database(&paths.sqlite)?;
    diagnostics::collect(&paths.sqlite, &project_id)
}

#[tauri::command]
fn cleanup_asset_cache(
    project_id: String,
    target_bytes: u64,
) -> Result<asset_cache::AssetCacheCleanupResult, String> {
    let paths = project_fs::project_paths(&project_id);
    storage::initialize_project_database(&paths.sqlite)?;
    let mut connection =
        rusqlite::Connection::open(&paths.sqlite).map_err(|error| error.to_string())?;
    asset_cache::cleanup_registered(&mut connection, target_bytes)
}

#[tauri::command]
fn list_sync_conflicts(project_id: String) -> Result<Vec<hybrid::SyncConflict>, String> {
    let paths = project_fs::project_paths(&project_id);
    storage::initialize_project_database(&paths.sqlite)?;
    let connection = rusqlite::Connection::open(&paths.sqlite).map_err(|err| err.to_string())?;
    hybrid::list_conflicts(&connection, &project_id)
}

#[tauri::command]
fn resolve_sync_conflict(
    project_id: String,
    conflict_id: String,
    resolution: String,
) -> Result<(), String> {
    let paths = project_fs::project_paths(&project_id);
    storage::initialize_project_database(&paths.sqlite)?;
    let mut connection =
        rusqlite::Connection::open(&paths.sqlite).map_err(|err| err.to_string())?;
    hybrid::resolve_conflict(&mut connection, &project_id, &conflict_id, &resolution)
}

#[tauri::command]
fn list_project_folders(project_id: String) -> Result<hybrid::FolderWorkspace, String> {
    let paths = project_fs::project_paths(&project_id);
    storage::initialize_project_database(&paths.sqlite)?;
    let mut connection =
        rusqlite::Connection::open(&paths.sqlite).map_err(|err| err.to_string())?;
    hybrid::folder_workspace(&mut connection, &project_id)
}

#[tauri::command]
fn migrate_legacy_project_folders(
    project_id: String,
    names: Vec<String>,
    assignments: std::collections::HashMap<String, String>,
) -> Result<hybrid::FolderWorkspace, String> {
    let paths = project_fs::project_paths(&project_id);
    storage::initialize_project_database(&paths.sqlite)?;
    let mut connection =
        rusqlite::Connection::open(&paths.sqlite).map_err(|err| err.to_string())?;
    hybrid::migrate_legacy_folders(&mut connection, &project_id, &names, &assignments)
}

#[tauri::command]
fn create_project_folder(
    project_id: String,
    name: String,
    parent_id: Option<String>,
) -> Result<hybrid::FolderWorkspace, String> {
    let paths = project_fs::project_paths(&project_id);
    storage::initialize_project_database(&paths.sqlite)?;
    let mut connection =
        rusqlite::Connection::open(&paths.sqlite).map_err(|err| err.to_string())?;
    hybrid::create_folder(&mut connection, &project_id, &name, parent_id.as_deref())
}

#[tauri::command]
fn rename_project_folder(
    project_id: String,
    folder_id: String,
    name: String,
) -> Result<hybrid::FolderWorkspace, String> {
    let paths = project_fs::project_paths(&project_id);
    storage::initialize_project_database(&paths.sqlite)?;
    let mut connection =
        rusqlite::Connection::open(&paths.sqlite).map_err(|err| err.to_string())?;
    hybrid::rename_folder(&mut connection, &project_id, &folder_id, &name)
}

#[tauri::command]
fn delete_project_folder(
    project_id: String,
    folder_id: String,
) -> Result<hybrid::FolderWorkspace, String> {
    let paths = project_fs::project_paths(&project_id);
    storage::initialize_project_database(&paths.sqlite)?;
    let mut connection =
        rusqlite::Connection::open(&paths.sqlite).map_err(|err| err.to_string())?;
    hybrid::delete_folder(&mut connection, &project_id, &folder_id)
}

#[tauri::command]
fn move_image_to_project_folder(
    project_id: String,
    image_id: String,
    folder_id: String,
) -> Result<hybrid::FolderWorkspace, String> {
    let paths = project_fs::project_paths(&project_id);
    storage::initialize_project_database(&paths.sqlite)?;
    let mut connection =
        rusqlite::Connection::open(&paths.sqlite).map_err(|err| err.to_string())?;
    hybrid::move_image_to_folder(&mut connection, &project_id, &image_id, &folder_id)
}

#[tauri::command]
fn store_project_credential(project_id: String, access_token: String) -> Result<(), String> {
    credentials::store_access_token(&project_id, &access_token)
}

#[tauri::command]
fn clear_project_credential(project_id: String) -> Result<(), String> {
    credentials::delete_access_token(&project_id)
}

#[tauri::command]
fn list_remote_project_members(
    project_id: String,
) -> Result<Vec<remote_admin::RemoteProjectMember>, String> {
    let paths = project_fs::project_paths(&project_id);
    storage::initialize_project_database(&paths.sqlite)?;
    remote_admin::list_members(&paths.sqlite, &project_id)
}

#[tauri::command]
fn upsert_remote_project_member(
    project_id: String,
    user_id: String,
    role: String,
) -> Result<remote_admin::RemoteProjectMember, String> {
    let paths = project_fs::project_paths(&project_id);
    storage::initialize_project_database(&paths.sqlite)?;
    remote_admin::upsert_member(&paths.sqlite, &project_id, &user_id, &role)
}

#[tauri::command]
fn remove_remote_project_member(project_id: String, user_id: String) -> Result<(), String> {
    let paths = project_fs::project_paths(&project_id);
    storage::initialize_project_database(&paths.sqlite)?;
    remote_admin::remove_member(&paths.sqlite, &project_id, &user_id)
}

#[tauri::command]
async fn publish_project(
    project_id: String,
    server_url: String,
    device_id: String,
    mode: String,
    cache_policy: String,
    access_token: Option<String>,
) -> Result<publisher::PublishResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let paths = project_fs::project_paths(&project_id);
        storage::initialize_project_database(&paths.sqlite)?;
        publisher::publish(
            &paths,
            &project_id,
            &server_url,
            &device_id,
            &mode,
            &cache_policy,
            access_token.as_deref(),
        )
    })
    .await
    .map_err(|error| format!("publish worker failed: {error}"))?
}

#[tauri::command]
fn get_annotation_history(
    repository: State<'_, RepositoryState>,
    project_id: String,
    image_id: String,
) -> Result<Vec<AnnotationVersion>, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.annotation_history(&project_id, &image_id)
}

#[tauri::command]
fn restore_annotation_version(
    repository: State<'_, RepositoryState>,
    project_id: String,
    image_id: String,
    revision: String,
) -> Result<AnnotationSaveResult, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.restore_annotation_version(&project_id, &image_id, &revision)
}

#[tauri::command]
fn create_annotation_task(
    repository: State<'_, RepositoryState>,
    project_id: String,
    name: String,
    image_ids: Option<Vec<String>>,
) -> Result<AnnotationTask, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.create_annotation_task_for_images(&project_id, &name, image_ids.as_deref())
}

#[tauri::command]
fn list_tasks(
    repository: State<'_, RepositoryState>,
    project_id: String,
) -> Result<Vec<AnnotationTask>, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.annotation_tasks(&project_id)
}

#[tauri::command]
fn list_task_items(
    repository: State<'_, RepositoryState>,
    project_id: String,
    task_id: String,
) -> Result<Vec<TaskItem>, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.task_items(&project_id, &task_id)
}

#[tauri::command]
fn review_task_item(
    repository: State<'_, RepositoryState>,
    project_id: String,
    image_id: String,
    decision: String,
    note: String,
) -> Result<(), String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.review_task_item(&project_id, &image_id, &decision, &note)
}

#[tauri::command]
fn list_review_queue(
    repository: State<'_, RepositoryState>,
    project_id: String,
) -> Result<Vec<DatasetImage>, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.review_queue(&project_id)
}

#[tauri::command]
fn create_dataset_snapshot(
    repository: State<'_, RepositoryState>,
    project_id: String,
    name: String,
) -> Result<DatasetSnapshot, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.create_dataset_snapshot(&project_id, &name)
}

#[tauri::command]
fn upgrade_dataset_snapshot_bridge(
    repository: State<'_, RepositoryState>,
    project_id: String,
    snapshot_id: String,
) -> Result<DatasetSnapshot, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.upgrade_dataset_snapshot_bridge(&project_id, &snapshot_id)
}

#[tauri::command]
fn list_snapshots(
    repository: State<'_, RepositoryState>,
    project_id: String,
) -> Result<Vec<DatasetSnapshot>, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.dataset_snapshots(&project_id)
}

#[tauri::command]
fn export_dataset(
    repository: State<'_, RepositoryState>,
    project_id: String,
    snapshot_id: String,
    format: String,
) -> Result<DatasetExport, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.export_dataset(&project_id, &snapshot_id, &format)
}

#[tauri::command]
fn list_exports(
    repository: State<'_, RepositoryState>,
    project_id: String,
) -> Result<Vec<DatasetExport>, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.dataset_exports(&project_id)
}

#[tauri::command]
async fn get_file_asset_path(
    repository: State<'_, RepositoryState>,
    project_id: String,
    image_id: String,
) -> Result<String, String> {
    let local_path = {
        let repository = repository.lock().map_err(|err| err.to_string())?;
        repository.image_path(&project_id, &image_id)
    };
    if let Some(path) = local_path {
        return Ok(path.to_string_lossy().to_string());
    }
    let cache_project_id = project_id.clone();
    let cache_image_id = image_id.clone();
    tauri::async_runtime::spawn_blocking(move || {
        cache_remote_asset_inner(cache_project_id, cache_image_id, None, Some(false))
    })
    .await
    .map_err(|error| format!("asset cache worker failed: {error}"))?
    .map(|record| record.local_path)
    .map_err(|error| format!("image unavailable: {project_id}/{image_id}: {error}"))
}

#[tauri::command]
async fn open_annotation_window(
    app: AppHandle,
    repository: State<'_, RepositoryState>,
    project_id: String,
    image_id: Option<String>,
) -> Result<(), String> {
    // Must stay async: sync commands run on the main thread, and creating a
    // webview there deadlocks on the WebView2 controller completion, leaving
    // the window permanently blank and the whole app frozen.
    let first_image = {
        let repository = repository.lock().map_err(|err| err.to_string())?;
        image_id.or_else(|| {
            repository
                .project_images(&project_id, None)
                .first()
                .map(|image| image.id.clone())
        })
    };

    #[cfg(not(mobile))]
    {
        return windows::open_annotation_window(&app, &project_id, first_image.as_deref());
    }

    #[cfg(mobile)]
    {
        let _ = app;
        let _ = first_image;
        Err("annotation windows are not supported on mobile".to_string())
    }
}

#[tauri::command]
fn list_backend_tasks(tasks: State<'_, BackendTaskState>) -> Result<Vec<BackendTask>, String> {
    let mut items = tasks.lock().map_err(|err| err.to_string())?.clone();
    items.sort_by(|left, right| right.started_at.cmp(&left.started_at));
    Ok(items)
}

#[tauri::command]
fn get_backend_task(
    tasks: State<'_, BackendTaskState>,
    task_id: String,
) -> Result<Option<BackendTask>, String> {
    let tasks = tasks.lock().map_err(|err| err.to_string())?;
    Ok(tasks.iter().find(|task| task.id == task_id).cloned())
}

#[tauri::command]
fn clear_completed_backend_tasks(tasks: State<'_, BackendTaskState>) -> Result<(), String> {
    let mut tasks = tasks.lock().map_err(|err| err.to_string())?;
    clear_completed_backend_task_items(&mut tasks);
    Ok(())
}

#[tauri::command]
fn retry_backend_task(tasks: State<'_, BackendTaskState>, task_id: String) -> Result<(), String> {
    let mut tasks = tasks.lock().map_err(|err| err.to_string())?;
    if let Some(task) = tasks.iter_mut().find(|task| task.id == task_id) {
        task.status = "running".to_string();
        task.progress = 0;
        task.message = "已加入重试队列".to_string();
        task.finished_at = None;
        return Ok(());
    }
    Err(format!("backend task not found: {task_id}"))
}

#[tauri::command]
async fn open_backend_task_tray(app: AppHandle) -> Result<(), String> {
    #[cfg(not(mobile))]
    {
        windows::open_backend_tasks_window(&app)
    }

    #[cfg(mobile)]
    {
        let _ = app;
        Ok(())
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut app = tauri::Builder::default()
        .plugin(project_window_chrome::init())
        .plugin(project_resource_monitor::init())
        .manage(NativeBackdropState::new(NativeBackdropStatus::pending()))
        .manage(RepositoryState::new(SampleRepository::new()))
        .manage(BackendTaskState::new(Vec::new()))
        .plugin(tauri_plugin_notification::init())
        .plugin(commands::hosting_files::init())
        .plugin(tauri_plugin_opener::init());

    #[cfg(not(mobile))]
    {
        app = app
            .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
                let _ = show_main_window(app);
            }))
            .on_menu_event(|app, event| {
                if let Some(action) = tray_action_from_menu_id(event.id().as_ref()) {
                    if let Err(error) = apply_tray_action(app, action) {
                        eprintln!("tray menu action failed: {error}");
                    }
                }
            })
            .on_tray_icon_event(|app, event| {
                if event.id().as_ref() != TRAY_ICON_ID {
                    return;
                }

                if let Some(action) = tray_action_from_icon_event(&event) {
                    if let Err(error) = apply_tray_action(app, action) {
                        eprintln!("tray icon action failed: {error}");
                    }
                }
            })
            .invoke_handler(tauri::generate_handler![
                project_resource_monitor::project_resource_snapshot,
                start_drag_window,
                minimize_window,
                toggle_maximize_window,
                hide_to_tray,
                show_window,
                close_window,
                window_state,
                backend_health,
                list_dataset_projects,
                list_data_projects,
                create_data_project,
                assign_dataset_to_data_project,
                get_data_project_working_tree,
                commit_data_project,
                list_data_project_commits,
                list_data_project_branches,
                create_data_project_branch,
                switch_data_project_branch,
                delete_data_project_branch,
                data_project_checkout_status,
                get_data_project_dataset_groups,
                set_data_project_dataset_groups,
                list_data_project_datasets,
                list_data_project_version_commits,
                commit_data_project_branch,
                preview_data_project_merge,
                merge_data_project_branches,
                resolve_data_project_merge,
                revert_data_project_commit,
                list_remote_data_projects,
                create_remote_data_project,
                connect_remote_data_project,
                get_data_project_sync_status,
                publish_data_project,
                sync_data_project,
                clone_remote_data_project,
                list_data_project_cloud_members,
                upsert_data_project_cloud_member,
                remove_data_project_cloud_member,
                get_project_detail,
                get_backend_design,
                list_builtin_datasets,
                download_test_dataset,
                create_dataset_project,
                create_project,
                import_images,
                import_yolo_dataset,
                pick_data_source,
                analyze_data_source,
                import_files,
                open_local_dataset,
                rescan_project_assets,
                generate_thumbnails,
                get_dataset_download_job,
                list_project_images,
                list_class_samples,
                get_image_annotations,
                get_image_annotation_state,
                save_image_annotations,
                submit_image_annotations,
                set_image_verified,
                load_external_annotations,
                export_annotation_file,
                run_ai_annotation,
                cancel_ai_annotation,
                get_ai_job_status,
                list_issues,
                create_issue,
                transition_issue,
                add_issue_comment,
                list_issue_comments,
                get_project_sync_summary,
                configure_remote_project,
                sync_project,
                offline_status,
                flush_offline_queue,
                cache_remote_asset,
                get_asset_cache_summary,
                cleanup_asset_cache,
                get_hybrid_diagnostics,
                list_sync_conflicts,
                resolve_sync_conflict,
                list_project_folders,
                migrate_legacy_project_folders,
                create_project_folder,
                rename_project_folder,
                delete_project_folder,
                move_image_to_project_folder,
                store_project_credential,
                clear_project_credential,
                list_remote_project_members,
                upsert_remote_project_member,
                remove_remote_project_member,
                publish_project,
                get_annotation_history,
                restore_annotation_version,
                create_annotation_task,
                list_tasks,
                list_task_items,
                commands::collaboration::claim_task_item,
                commands::collaboration::release_task_item,
                review_task_item,
                list_review_queue,
                create_dataset_snapshot,
                upgrade_dataset_snapshot_bridge,
                list_snapshots,
                export_dataset,
                list_exports,
                get_file_asset_path,
                open_annotation_window,
                list_backend_tasks,
                get_backend_task,
                clear_completed_backend_tasks,
                retry_backend_task,
                open_backend_task_tray,
                commands::account::auth_login,
                commands::account::auth_refresh,
                commands::account::auth_logout,
                commands::account::auth_status,
                commands::account::remote_request,
                commands::hosting::hosting_upload_archive,
                commands::hosting::hosting_download_export,
                commands::hosting_files::hosting_open_export,
                commands::account::remote_project_open,
                commands::invitations::auth_accept_invitation
            ])
            .setup(|app| {
                if let Ok(root) = app.path().app_data_dir() {
                    let _ = project_fs::configure_storage_roots(root);
                }
                sync_engine::start_auto_sync();
                if let Err(error) = http_backend::start_background_backend(app.handle().clone()) {
                    eprintln!("local http backend failed to start: {error}");
                }
                setup_system_tray(app.handle())?;

                if let Some(window) = app.get_webview_window("main") {
                    let backdrop_status = platform::configure_window(&window);

                    if let Ok(mut state) = app.state::<NativeBackdropState>().lock() {
                        *state = backdrop_status;
                    }

                    let app_handle = app.handle().clone();
                    window.on_window_event(move |event| {
                        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                            api.prevent_close();
                            let _ = hide_main_window(&app_handle);
                        }
                    });
                }

                Ok(())
            });
    }

    #[cfg(mobile)]
    {
        app = app.invoke_handler(tauri::generate_handler![
            project_resource_monitor::project_resource_snapshot,
            window_state,
            backend_health,
            list_dataset_projects,
            list_data_projects,
            create_data_project,
            assign_dataset_to_data_project,
            get_data_project_working_tree,
            commit_data_project,
            list_data_project_commits,
            list_data_project_branches,
            create_data_project_branch,
            switch_data_project_branch,
            delete_data_project_branch,
            data_project_checkout_status,
            get_data_project_dataset_groups,
            set_data_project_dataset_groups,
            list_data_project_datasets,
            list_data_project_version_commits,
            commit_data_project_branch,
            preview_data_project_merge,
            merge_data_project_branches,
            resolve_data_project_merge,
            revert_data_project_commit,
            list_remote_data_projects,
            create_remote_data_project,
            connect_remote_data_project,
            get_data_project_sync_status,
            publish_data_project,
            sync_data_project,
            clone_remote_data_project,
            list_data_project_cloud_members,
            upsert_data_project_cloud_member,
            remove_data_project_cloud_member,
            get_project_detail,
            get_backend_design,
            list_builtin_datasets,
            create_dataset_project,
            create_project,
            list_project_images,
            get_file_asset_path,
            get_image_annotations,
            get_image_annotation_state,
            save_image_annotations,
            submit_image_annotations,
            set_image_verified,
            list_issues,
            create_issue,
            transition_issue,
            add_issue_comment,
            list_issue_comments,
            get_project_sync_summary,
            configure_remote_project,
            sync_project,
            offline_status,
            flush_offline_queue,
            cache_remote_asset,
            get_asset_cache_summary,
            cleanup_asset_cache,
            get_hybrid_diagnostics,
            list_sync_conflicts,
            resolve_sync_conflict,
            list_project_folders,
            migrate_legacy_project_folders,
            create_project_folder,
            rename_project_folder,
            delete_project_folder,
            move_image_to_project_folder,
            store_project_credential,
            clear_project_credential,
            list_remote_project_members,
            upsert_remote_project_member,
            remove_remote_project_member,
            publish_project,
            get_annotation_history,
            restore_annotation_version,
            create_annotation_task,
            list_tasks,
            list_task_items,
            commands::collaboration::claim_task_item,
            commands::collaboration::release_task_item,
            review_task_item,
            list_review_queue,
            commands::account::auth_login,
            commands::account::auth_refresh,
            commands::account::auth_logout,
            commands::account::auth_status,
            commands::account::remote_request,
            commands::hosting::hosting_upload_archive,
            commands::hosting::hosting_download_export,
            commands::hosting_files::hosting_open_export,
            commands::account::remote_project_open,
            commands::invitations::auth_accept_invitation
        ]);
        app = app.setup(|app| {
            if let Ok(root) = app.path().app_data_dir() {
                let _ = project_fs::configure_storage_roots(root);
            }
            Ok(())
        });
    }

    app.run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(not(mobile))]
fn main_window(app: &AppHandle) -> Result<WebviewWindow, String> {
    app.get_webview_window("main")
        .ok_or_else(|| "main window not found".to_string())
}

#[cfg(not(mobile))]
fn show_main_window(app: &AppHandle) -> Result<(), String> {
    let window = main_window(app)?;
    let _ = window.unminimize();
    window.show().map_err(|err| err.to_string())?;
    window.set_focus().map_err(|err| err.to_string())?;
    Ok(())
}

#[cfg(not(mobile))]
fn hide_main_window(app: &AppHandle) -> Result<(), String> {
    main_window(app)?.hide().map_err(|err| err.to_string())
}

#[cfg(not(mobile))]
fn setup_system_tray(app: &AppHandle) -> tauri::Result<()> {
    let show_item = MenuItem::with_id(app, TRAY_MENU_SHOW_ID, "显示主窗口", true, None::<&str>)?;
    let hide_item = MenuItem::with_id(app, TRAY_MENU_HIDE_ID, "隐藏到托盘", true, None::<&str>)?;
    let annotate_item = MenuItem::with_id(
        app,
        TRAY_MENU_ANNOTATE_ID,
        "打开标注工作台",
        true,
        None::<&str>,
    )?;
    let tasks_item = MenuItem::with_id(app, TRAY_MENU_TASKS_ID, "后台任务", true, None::<&str>)?;
    let export_item = MenuItem::with_id(app, TRAY_MENU_EXPORT_ID, "导出中心", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit_item = MenuItem::with_id(app, TRAY_MENU_QUIT_ID, "退出应用", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &show_item,
            &hide_item,
            &annotate_item,
            &tasks_item,
            &export_item,
            &separator,
            &quit_item,
        ],
    )?;
    let icon_bytes = include_bytes!("../icons/icon.png");
    let icon = app
        .default_window_icon()
        .cloned()
        .unwrap_or_else(|| Image::new_owned(icon_bytes.to_vec(), 64, 64));

    TrayIconBuilder::with_id(TRAY_ICON_ID)
        .icon(icon)
        .tooltip("Image Annotation")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .build(app)?;

    Ok(())
}

#[cfg(not(mobile))]
fn tray_action_from_menu_id(id: &str) -> Option<TrayAction> {
    match id {
        TRAY_MENU_SHOW_ID => Some(TrayAction::ShowWindow),
        TRAY_MENU_HIDE_ID => Some(TrayAction::HideWindow),
        TRAY_MENU_ANNOTATE_ID => Some(TrayAction::StartAnnotation),
        TRAY_MENU_TASKS_ID => Some(TrayAction::BackendTasks),
        TRAY_MENU_EXPORT_ID => Some(TrayAction::Export),
        TRAY_MENU_QUIT_ID => Some(TrayAction::Quit),
        _ => None,
    }
}

#[cfg(not(mobile))]
fn tray_action_from_icon_event(event: &TrayIconEvent) -> Option<TrayAction> {
    match event {
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        }
        | TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        } => Some(TrayAction::ShowWindow),
        _ => None,
    }
}

#[cfg(not(mobile))]
fn apply_tray_action(app: &AppHandle, action: TrayAction) -> Result<(), String> {
    match action {
        TrayAction::ShowWindow | TrayAction::StartAnnotation | TrayAction::Export => {
            show_main_window(app)
        }
        TrayAction::BackendTasks => {
            // Window creation must not run on the main thread (see
            // open_annotation_window); defer to a worker thread.
            let handle = app.clone();
            std::thread::spawn(move || {
                if let Err(error) = windows::open_backend_tasks_window(&handle) {
                    eprintln!("backend tasks window failed: {error}");
                }
            });
            Ok(())
        }
        TrayAction::HideWindow => hide_main_window(app),
        TrayAction::Quit => {
            app.exit(0);
            Ok(())
        }
    }
}

fn record_backend_task(
    tasks: &State<'_, BackendTaskState>,
    task: BackendTask,
) -> Result<(), String> {
    let mut tasks = tasks.lock().map_err(|err| err.to_string())?;
    upsert_backend_task(&mut tasks, task);
    Ok(())
}

fn upsert_backend_task(tasks: &mut Vec<BackendTask>, task: BackendTask) {
    if let Some(existing) = tasks.iter_mut().find(|existing| existing.id == task.id) {
        *existing = task;
    } else {
        tasks.push(task);
    }
}

fn clear_completed_backend_task_items(tasks: &mut Vec<BackendTask>) {
    tasks.retain(|task| task.status != "completed");
}

fn dataset_slug(name: &str, fallback: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = false;
    for character in name.trim().to_ascii_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
            last_dash = false;
        } else if !last_dash && !slug.is_empty() {
            slug.push('-');
            last_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        fallback.to_string()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(not(mobile))]
    fn tray_menu_ids_map_to_actions() {
        assert_eq!(
            tray_action_from_menu_id(TRAY_MENU_SHOW_ID),
            Some(TrayAction::ShowWindow)
        );
        assert_eq!(
            tray_action_from_menu_id(TRAY_MENU_HIDE_ID),
            Some(TrayAction::HideWindow)
        );
        assert_eq!(
            tray_action_from_menu_id(TRAY_MENU_ANNOTATE_ID),
            Some(TrayAction::StartAnnotation)
        );
        assert_eq!(
            tray_action_from_menu_id(TRAY_MENU_TASKS_ID),
            Some(TrayAction::BackendTasks)
        );
        assert_eq!(
            tray_action_from_menu_id(TRAY_MENU_EXPORT_ID),
            Some(TrayAction::Export)
        );
        assert_eq!(
            tray_action_from_menu_id(TRAY_MENU_QUIT_ID),
            Some(TrayAction::Quit)
        );
        assert_eq!(tray_action_from_menu_id("unknown"), None);
    }

    #[test]
    fn backend_repository_exposes_dataset_project_details() {
        let project_id = "repository-detail-unit";
        let paths = project_fs::ensure_workspace_project_dirs(project_id).unwrap();
        project_fs::write_manifest(&project_fs::ProjectManifest {
            id: project_id.to_string(),
            name: "Repository Detail Unit".to_string(),
            source_dataset_key: "unit-fixture".to_string(),
            format: "yolo-detect".to_string(),
            root_path: paths.root.to_string_lossy().to_string(),
            created_at: "0".to_string(),
            class_count: 1,
            image_count: 1,
        })
        .unwrap();

        let repository = domain::SampleRepository::new();
        let projects = repository.dataset_projects();
        assert!(projects.iter().all(|project| !project.id.is_empty()));
        let project = projects
            .iter()
            .find(|project| project.id == project_id)
            .expect("fixture project is listed");
        let detail = repository
            .project_detail(&project.id)
            .expect("project detail exists for listed project");
        assert_eq!(detail.project.id, project.id);
        assert!(detail.tag_groups.iter().any(|group| group.name == "train"));

        let _ = std::fs::remove_dir_all(paths.root);
    }

    #[test]
    fn backend_design_documents_layered_runtime() {
        let design = domain::backend_design();

        assert!(design
            .layers
            .iter()
            .any(|layer| layer.name == "Command API"));
        assert!(design.layers.iter().any(|layer| layer.name == "Project FS"));
        assert!(design.storage_plan.contains("data/workspaces/default"));
    }

    #[test]
    fn backend_task_registry_upserts_and_clears_completed_tasks() {
        let mut tasks = Vec::new();

        upsert_backend_task(
            &mut tasks,
            BackendTask::new(
                "download-coco128",
                "COCO128 导入",
                "dataset-import",
                "running",
                42,
                "正在导入 COCO128",
            ),
        );
        upsert_backend_task(
            &mut tasks,
            BackendTask::new(
                "download-coco128",
                "COCO128 导入",
                "dataset-import",
                "completed",
                100,
                "COCO128 已下载并导入",
            )
            .finished(),
        );
        upsert_backend_task(
            &mut tasks,
            BackendTask::new(
                "create-demo",
                "Demo 数据集创建",
                "dataset-create",
                "failed",
                0,
                "创建失败",
            )
            .finished(),
        );

        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].status, "completed");

        clear_completed_backend_task_items(&mut tasks);

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "create-demo");
    }
}

mod project_resource_monitor;

mod project_window_chrome;

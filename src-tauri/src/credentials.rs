use chrono::DateTime;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
use keyring::Entry;
#[cfg(any(target_os = "android", target_os = "ios"))]
use keyring_core::{api::CredentialStoreApi, Entry, Error as KeyringError};

const CREDENTIAL_SERVICE: &str = "com.imageannotation.desktop";
const DEVICE_ID_SCOPE: &str = "__device-id";
const REFRESH_SKEW_SECONDS: i64 = 30;
static DEVICE_ID: OnceLock<Result<String, String>> = OnceLock::new();
static REFRESH_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static ACTIVE_SCOPE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// A server session is scoped to both account and server.  Keeping the scope
/// in the stored record prevents a token issued by one deployment being sent
/// to another deployment after a user changes the server URL.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionCredentials {
    pub server_url: String,
    pub account_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub device_id: String,
    pub access_expires_at: Option<i64>,
}

pub trait CredentialStore {
    fn store(&self, scope: &str, value: &SessionCredentials) -> Result<(), String>;
    fn read(&self, scope: &str) -> Result<Option<SessionCredentials>, String>;
    fn delete(&self, scope: &str) -> Result<(), String>;
}

/// Platform-neutral session helpers used by Tauri commands and the sync
/// engine. Desktop uses the OS keyring; iOS uses the protected Keychain store;
/// Android uses SharedPreferences encrypted by an Android Keystore key. The
/// concrete stores stay behind this trait so callers never handle token files.
pub fn store_session(scope: &str, value: &SessionCredentials) -> Result<(), String> {
    default_store().store(scope, value)
}

pub fn read_session(scope: &str) -> Result<Option<SessionCredentials>, String> {
    default_store().read(scope)
}

pub fn delete_session(scope: &str) -> Result<(), String> {
    default_store().delete(scope)
}

pub fn current_session() -> Result<Option<SessionCredentials>, String> {
    read_session(&active_scope())
}

fn active_scope() -> String {
    std::env::var("IMAGE_ANNOTATION_ACCOUNT_SCOPE")
        .ok()
        .filter(|scope| !scope.trim().is_empty())
        .or_else(|| {
            fs::read_to_string(storage_scope_path())
                .ok()
                .map(|scope| scope.trim().to_string())
        })
        .unwrap_or_else(|| "current".to_string())
}

/// Derive a stable native-store scope from both deployment and account. A
/// user id is not globally unique across servers, so storing sessions under
/// the account id alone could make a token from one deployment replace the
/// token for another deployment.
pub fn session_scope(server_url: &str, account_id: &str) -> String {
    let scope = format!(
        "{}\n{}",
        server_url.trim_end_matches('/').to_ascii_lowercase(),
        account_id
    );
    format!("session-{:x}", Sha256::digest(scope.as_bytes()))
}

/// Read the active session and rotate its access token when it is expired or
/// inside the refresh skew window. All callers share the same process mutex,
/// which matters because refresh tokens are single-use and may be consumed by
/// the desktop sync worker and a UI request at the same time.
pub fn refreshed_session() -> Result<Option<SessionCredentials>, String> {
    refresh_current_session(false)
}

/// Read the active session and, when requested or required by expiry, rotate
/// its access token under the process-wide refresh mutex. Manual refresh and
/// background callers must use this entry point so a one-time refresh token is
/// never consumed concurrently by two HTTP workers.
pub fn refresh_current_session(force: bool) -> Result<Option<SessionCredentials>, String> {
    current_session()?
        .map(|session| refresh_session_if_needed(session, force))
        .transpose()
}

pub fn set_active_scope(scope: &str) -> Result<(), String> {
    let _guard = ACTIVE_SCOPE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "active account scope lock is poisoned".to_string())?;
    persist_active_scope(scope)
}

/// A slow logout for A must not clear a newer login for B.
pub fn deactivate_scope(scope: &str) -> Result<(), String> {
    let _guard = ACTIVE_SCOPE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "active account scope lock is poisoned".to_string())?;
    if active_scope() == scope {
        persist_active_scope("current")?;
    }
    Ok(())
}

fn persist_active_scope(scope: &str) -> Result<(), String> {
    if scope.trim().is_empty() {
        return Err("account scope is empty".to_string());
    }
    std::env::set_var("IMAGE_ANNOTATION_ACCOUNT_SCOPE", scope);
    let path = storage_scope_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| format!("failed to create session directory: {err}"))?;
    }
    fs::write(&path, scope.as_bytes())
        .map_err(|err| format!("failed to persist active account: {err}"))?;
    set_private_permissions(&path);
    Ok(())
}

fn storage_scope_path() -> PathBuf {
    crate::project_fs::storage_roots()
        .app_data
        .join("active-session-scope")
}

/// Return the stable per-install identifier, failing closed when the native
/// secure store cannot be opened. The identifier is generated once and kept
/// in the same protected store as session tokens; it is never derived from a
/// filesystem path or host name.
pub fn try_device_id() -> Result<String, String> {
    DEVICE_ID
        .get_or_init(|| {
            let result = (|| {
                if let Some(session) = read_session(DEVICE_ID_SCOPE)? {
                    if !session.device_id.trim().is_empty() {
                        return Ok(session.device_id);
                    }
                }
                let value = Uuid::new_v4().to_string();
                store_session(
                    DEVICE_ID_SCOPE,
                    &SessionCredentials {
                        device_id: value.clone(),
                        ..SessionCredentials::default()
                    },
                )?;
                Ok(value)
            })();
            result
        })
        .clone()
}

/// Compatibility wrapper for non-authentication callers. Authentication uses
/// [`try_device_id`] and reports secure-store failures to the UI.
pub fn device_id() -> String {
    try_device_id().unwrap_or_else(|_| "device-unavailable".to_string())
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
struct DesktopCredentialStore;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
impl CredentialStore for DesktopCredentialStore {
    fn store(&self, scope: &str, value: &SessionCredentials) -> Result<(), String> {
        credential_entry(scope)?
            .set_password(
                &serde_json::to_string(value)
                    .map_err(|err| format!("failed to encode session: {err}"))?,
            )
            .map_err(|err| format!("failed to store session credential: {err}"))
    }

    fn read(&self, scope: &str) -> Result<Option<SessionCredentials>, String> {
        match credential_entry(scope)?.get_password() {
            Ok(value) => serde_json::from_str(&value)
                .map(Some)
                .map_err(|err| format!("stored session is invalid: {err}")),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(format!("failed to read session credential: {error}")),
        }
    }

    fn delete(&self, scope: &str) -> Result<(), String> {
        match credential_entry(scope)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(format!("failed to delete session credential: {error}")),
        }
    }
}

#[cfg(any(target_os = "android", target_os = "ios"))]
struct MobileCredentialStore;

#[cfg(any(target_os = "android", target_os = "ios"))]
impl CredentialStore for MobileCredentialStore {
    fn store(&self, scope: &str, value: &SessionCredentials) -> Result<(), String> {
        let serialized = serde_json::to_string(value)
            .map_err(|err| format!("failed to encode session: {err}"))?;
        native_entry(scope)?
            .set_password(&serialized)
            .map_err(|err| format!("failed to store mobile session in native secure store: {err}"))
    }

    fn read(&self, scope: &str) -> Result<Option<SessionCredentials>, String> {
        match native_entry(scope)?.get_password() {
            Ok(value) => serde_json::from_str(&value)
                .map(Some)
                .map_err(|err| format!("stored mobile session is invalid: {err}")),
            Err(KeyringError::NoEntry) => Ok(None),
            Err(error) => Err(format!("failed to read mobile session: {error}")),
        }
    }

    fn delete(&self, scope: &str) -> Result<(), String> {
        match native_entry(scope)?.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(error) => Err(format!("failed to delete mobile session: {error}")),
        }
    }
}

fn default_store() -> Box<dyn CredentialStore + Send + Sync> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        Box::new(MobileCredentialStore)
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        Box::new(DesktopCredentialStore)
    }
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn native_entry(scope: &str) -> Result<Entry, String> {
    if scope.trim().is_empty()
        || scope
            .chars()
            .any(|ch| !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_')
    {
        return Err("invalid account scope".to_string());
    }

    #[cfg(target_os = "ios")]
    {
        let store = apple_native_keyring_store::protected::Store::new()
            .map_err(|err| format!("failed to open iOS Keychain: {err}"))?;
        return store
            .build(CREDENTIAL_SERVICE, scope, None)
            .map_err(|err| format!("failed to create iOS Keychain entry: {err}"));
    }

    #[cfg(target_os = "android")]
    {
        let store = android_native_keyring_store::Store::new()
            .map_err(|err| format!("failed to open Android Keystore: {err}"))?;
        return store
            .build(CREDENTIAL_SERVICE, scope, None)
            .map_err(|err| format!("failed to create Android Keystore entry: {err}"));
    }
}

fn set_private_permissions(path: &PathBuf) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = fs::metadata(path) {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o600);
            let _ = fs::set_permissions(path, permissions);
        }
    }
}

pub fn store_access_token(project_id: &str, access_token: &str) -> Result<(), String> {
    if project_id.trim().is_empty() {
        return Err("project ID is required".to_string());
    }
    if access_token.trim().is_empty() {
        return Err("access token is required".to_string());
    }
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        let mut session = read_session(project_id)?.unwrap_or_default();
        session.access_token = access_token.to_string();
        session.account_id = project_id.to_string();
        return store_session(project_id, &session);
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    credential_entry(project_id)?
        .set_password(access_token)
        .map_err(|err| format!("failed to store project credential: {err}"))
}

pub fn read_access_token(project_id: &str) -> Result<Option<String>, String> {
    // Project-scoped keyring values are retained only for the legacy
    // store/clear command names. They may be stale, manually supplied, or
    // belong to a previous account, so they must never participate in
    // authentication. Resolve only through the active server+account session;
    // that path validates the project binding and refreshes the session when
    // its access token is expired.
    session_token_for_project(project_id)
}

fn session_token_for_project(project_id: &str) -> Result<Option<String>, String> {
    let Some(session) = current_session()? else {
        return Ok(None);
    };
    let paths = crate::project_fs::project_paths(project_id);
    if !paths.sqlite.exists() {
        return Ok(None);
    }
    let connection =
        rusqlite::Connection::open(&paths.sqlite).map_err(|error| error.to_string())?;
    let configured_remote = connection
        .query_row(
            "SELECT c.server_url, c.remote_project_id, c.account_id, p.source_dataset_key
             FROM remote_project_configs c
             JOIN projects p ON p.id = c.project_id
             WHERE c.project_id = ?1 AND p.deleted_at IS NULL",
            [project_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .ok();
    let Some((configured_server, remote_project_id, owner_account_id, source_dataset_key)) =
        configured_remote
    else {
        return Ok(None);
    };
    if configured_server.trim_end_matches('/') != session.server_url.trim_end_matches('/') {
        return Ok(None);
    }
    if owner_account_id
        .as_deref()
        .is_some_and(|owner| owner != session.account_id)
    {
        return Ok(None);
    }
    let expected_source = format!(
        "remote:{}:{}:{}",
        configured_server.trim_end_matches('/'),
        session.account_id,
        remote_project_id
    );
    // New remote-origin projects are strictly scoped by source key. A legacy
    // desktop project keeps its historical source key and must continue to
    // use its project-specific OS credential instead of inheriting a current
    // account token implicitly.
    if source_dataset_key.starts_with("remote:") && source_dataset_key != expected_source {
        return Ok(None);
    }
    if !source_dataset_key.starts_with("remote:") && owner_account_id.is_none() {
        return Ok(None);
    }
    let session = refresh_session_if_needed(session, false)?;
    Ok((!session.access_token.is_empty()).then_some(session.access_token))
}

fn refresh_session_if_needed(
    session: SessionCredentials,
    force: bool,
) -> Result<SessionCredentials, String> {
    if !force && (session_token_is_fresh(&session) || session.refresh_token.trim().is_empty()) {
        return Ok(session);
    }

    let lock = REFRESH_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock
        .lock()
        .map_err(|_| "session refresh lock is poisoned".to_string())?;
    // Another sync worker may have refreshed the same account while this
    // worker waited for the mutex. Re-read the native store before spending
    // the one-time refresh token.
    let current = current_session()?.ok_or("account logged out while refreshing")?;
    if current.account_id != session.account_id
        || current.server_url != session.server_url
        || current.device_id != session.device_id
    {
        return Err("active account changed while refreshing".to_string());
    }
    let changed_while_waiting = current.access_token != session.access_token
        || current.refresh_token != session.refresh_token
        || current.access_expires_at != session.access_expires_at;
    if (changed_while_waiting && session_token_is_fresh(&current))
        || (!force && (session_token_is_fresh(&current) || current.refresh_token.trim().is_empty()))
    {
        return Ok(current);
    }
    if current.refresh_token.trim().is_empty() {
        return if force {
            Err("refresh token is unavailable".to_string())
        } else {
            Ok(current)
        };
    }

    let server_url = current.server_url.trim_end_matches('/');
    if !(server_url.starts_with("http://") || server_url.starts_with("https://")) {
        return Err("session server URL must use http or https".to_string());
    }
    let response = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("failed to create session refresh client: {error}"))?
        .post(format!("{server_url}/api/v1/auth/refresh"))
        .json(&serde_json::json!({
            "refreshToken": current.refresh_token,
            "deviceId": current.device_id,
        }))
        .send()
        .map_err(|error| format!("session refresh request failed: {error}"))?;
    let status = response.status();
    let payload = response
        .json::<Value>()
        .unwrap_or_else(|_| Value::Object(Default::default()));
    if !status.is_success() {
        return Err(format!(
            "session refresh failed ({}): {payload}",
            status.as_u16()
        ));
    }

    let access_token = json_string(&payload, &["accessToken", "access_token"])
        .ok_or_else(|| "session refresh response has no access token".to_string())?;
    let refresh_token = json_string(&payload, &["refreshToken", "refresh_token"])
        .unwrap_or_else(|| current.refresh_token.clone());
    let access_expires_at = json_expiry(
        &payload,
        &[
            "expiresAtUnix",
            "expires_at_unix",
            "expiresAt",
            "expires_at",
        ],
    );
    let refreshed = SessionCredentials {
        access_token,
        refresh_token,
        access_expires_at,
        ..current
    };
    // Refresh only its own entry. Login alone chooses the active scope; a
    // response arriving after logout/account switch cannot revive that scope.
    let scope = session_scope(&refreshed.server_url, &refreshed.account_id);
    store_session(&scope, &refreshed)?;
    Ok(refreshed)
}

fn session_token_is_fresh(session: &SessionCredentials) -> bool {
    if session.access_token.trim().is_empty() {
        return false;
    }
    session.access_expires_at.map_or(true, |expires_at| {
        expires_at > unix_now().saturating_add(REFRESH_SKEW_SECONDS)
    })
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn json_string(payload: &Value, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        payload
            .get(*name)
            .and_then(Value::as_str)
            .map(ToString::to_string)
    })
}

fn json_expiry(payload: &Value, names: &[&str]) -> Option<i64> {
    names.iter().find_map(|name| {
        let value = payload.get(*name)?;
        value
            .as_i64()
            .or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
            .or_else(|| value.as_str().and_then(parse_expiry_string))
    })
}

fn parse_expiry_string(value: &str) -> Option<i64> {
    value.parse::<i64>().ok().or_else(|| {
        DateTime::parse_from_rfc3339(value)
            .ok()
            .map(|date| date.timestamp())
    })
}

pub fn delete_access_token(project_id: &str) -> Result<(), String> {
    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        return delete_session(project_id);
    }
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    match credential_entry(project_id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(format!("failed to delete project credential: {error}")),
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn credential_entry(project_id: &str) -> Result<Entry, String> {
    Entry::new(CREDENTIAL_SERVICE, &format!("project:{project_id}"))
        .map_err(|err| format!("failed to open system credential store: {err}"))
}

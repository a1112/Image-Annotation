//! Runs the real native API against an explicitly isolated acceptance root.
//! Credentials are read from a private file and never written to stdout.
use image_annotation_lib::{credentials, http_backend, project_fs};
use serde_json::{json, Value};
use std::{fs, path::PathBuf};
use uuid::Uuid;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::var_os("IMAGE_ANNOTATION_DATA_DIR")
        .map(PathBuf::from)
        .ok_or("IMAGE_ANNOTATION_DATA_DIR is required for this acceptance backend")?;
    project_fs::configure_storage_roots(&root)?;
    if let Some(path) = std::env::var_os("IMAGE_ANNOTATION_ACCEPTANCE_CREDENTIALS") {
        let input: Value = serde_json::from_slice(&fs::read(path)?)?;
        let server = input["serverUrl"].as_str().ok_or("serverUrl missing")?;
        let device_id = Uuid::new_v4().to_string();
        let response = reqwest::blocking::Client::new()
            .post(format!(
                "{}/api/v1/auth/login",
                server.trim_end_matches('/')
            ))
            .json(&json!({
                "email": input["email"], "password": input["password"],
                "deviceId": &device_id,
                "deviceName": "Data project native acceptance", "platform": "desktop"
            }))
            .send()?;
        if !response.status().is_success() {
            return Err(format!("acceptance login failed ({})", response.status()).into());
        }
        let session: Value = response.json()?;
        let account = session["user"]["id"]
            .as_str()
            .or_else(|| session["accountId"].as_str())
            .ok_or("account ID missing")?;
        let stored = credentials::SessionCredentials {
            server_url: server.to_string(),
            account_id: account.to_string(),
            access_token: session["accessToken"]
                .as_str()
                .ok_or("access token missing")?
                .to_string(),
            refresh_token: session["refreshToken"]
                .as_str()
                .ok_or("refresh token missing")?
                .to_string(),
            device_id,
            access_expires_at: session["expiresAtUnix"].as_i64(),
        };
        // Every rebuilt acceptance executable owns a fresh test-only entry.
        // Reusing an entry created by an older ad-hoc executable would ask
        // macOS to widen that old entry's trusted-code ACL during a test run.
        let scope = format!("acceptance-{}", Uuid::new_v4());
        credentials::store_session(&scope, &stored)?;
        credentials::set_active_scope(&scope)?;
    }
    println!(
        "isolated native acceptance backend: {}",
        http_backend::backend_base_url()
    );
    http_backend::run_foreground_backend()?;
    Ok(())
}

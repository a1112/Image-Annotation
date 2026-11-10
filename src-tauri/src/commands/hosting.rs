//! Authenticated archive transfer for both desktop and mobile. Tokens stay in
//! the native credential store; the WebView only passes archive contents/IDs.
use crate::{credentials, project_fs};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

const MAX_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
const MAX_EXPORT_BYTES: u64 = 1024 * 1024 * 1024;

fn validated_id(value: &str) -> Result<Uuid, String> {
    Uuid::parse_str(value).map_err(|_| "invalid hosting resource ID".to_string())
}

fn archive_format(value: &str) -> Result<&str, String> {
    match value {
        "native" | "coco" | "yolo" | "labelme" => Ok(value),
        _ => Err("unsupported dataset archive format".to_string()),
    }
}

fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        // Authorization headers must never follow an unexpected redirect.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| error.to_string())
}

fn response_error(response: reqwest::blocking::Response) -> String {
    let status = response.status();
    let mut body = String::new();
    let _ = response.take(4096).read_to_string(&mut body);
    format!("托管请求失败 ({status}): {body}")
}

pub(crate) fn upload_archive(
    dataset_id: &str,
    format: &str,
    file_name: &str,
    base64: &str,
    operation_id: Option<&str>,
) -> Result<Value, String> {
    let dataset_id = validated_id(dataset_id)?;
    let format = archive_format(format)?;
    let operation_id = operation_id
        .map(validated_id)
        .transpose()?
        .unwrap_or_else(Uuid::new_v4);
    if !file_name.to_ascii_lowercase().ends_with(".zip") {
        return Err("请选择 ZIP 数据集归档".to_string());
    }
    if base64.len() > MAX_ARCHIVE_BYTES.div_ceil(3) * 4 {
        return Err("客户端导入归档上限为 64 MiB".to_string());
    }
    let bytes = STANDARD
        .decode(base64)
        .map_err(|_| "invalid archive encoding".to_string())?;
    if bytes.is_empty() || bytes.len() > MAX_ARCHIVE_BYTES || !bytes.starts_with(b"PK") {
        return Err("归档为空、超过 64 MiB 或不是 ZIP 文件".to_string());
    }
    let current =
        credentials::refreshed_session()?.ok_or_else(|| "请先登录协作服务器".to_string())?;
    let url = format!(
        "{}/api/v1/datasets/{dataset_id}/imports?format={format}",
        current.server_url.trim_end_matches('/')
    );
    let response = client()?
        .post(url)
        .bearer_auth(&current.access_token)
        .header("content-type", "application/zip")
        .header("x-operation-id", operation_id.to_string())
        .header("x-content-sha256", format!("{:x}", Sha256::digest(&bytes)))
        .body(bytes)
        .send()
        .map_err(|error| format!("归档上传失败: {error}"))?;
    if !response.status().is_success() {
        return Err(response_error(response));
    }
    response
        .json()
        .map_err(|error| format!("归档作业响应无效: {error}"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadedExport {
    pub path: String,
    pub file_name: String,
    pub byte_size: u64,
    pub sha256: String,
}

pub(crate) fn export_directory(server_url: &str, account_id: &str) -> PathBuf {
    let scope = credentials::session_scope(server_url, account_id);
    // Android's Tauri `appDataDir` is the package data root, while
    // FileProvider's `files-path` is rooted at `Context.filesDir`. Keep the
    // exported archive under that exact provider root.
    #[cfg(target_os = "android")]
    let app_data = project_fs::storage_roots().app_data.join("files");
    #[cfg(not(target_os = "android"))]
    let app_data = project_fs::storage_roots().app_data;
    #[cfg(target_os = "ios")]
    if let Some(home) = std::env::var_os("HOME") {
        // Documents is visible in Files for this app. Credentials and offline
        // drafts stay in Application Support; only requested exports go here.
        return PathBuf::from(home)
            .join("Documents")
            .join("Dataset Exports")
            .join(scope);
    }
    app_data.join("exports").join(scope)
}

fn persist_export(
    mut response: reqwest::blocking::Response,
    directory: &Path,
    job_id: Uuid,
) -> Result<DownloadedExport, String> {
    if response
        .content_length()
        .is_some_and(|size| size > MAX_EXPORT_BYTES)
    {
        return Err("导出文件超过客户端 1 GiB 下载上限".to_string());
    }
    let expected_hash = response
        .headers()
        .get("x-content-sha256")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let expected_size = response.content_length();
    fs::create_dir_all(directory).map_err(|error| format!("无法创建导出目录: {error}"))?;
    let file_name = format!("dataset-export-{job_id}.zip");
    let target = directory.join(&file_name);
    let partial = directory.join(format!(".{job_id}-{}.part", Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&partial)
            .map_err(|error| error.to_string())?;
        let mut digest = Sha256::new();
        let mut size = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = response
                .read(&mut buffer)
                .map_err(|error| format!("下载中断: {error}"))?;
            if count == 0 {
                break;
            }
            size = size
                .checked_add(count as u64)
                .ok_or_else(|| "export size overflow".to_string())?;
            if size > MAX_EXPORT_BYTES {
                return Err("导出文件超过客户端 1 GiB 下载上限".to_string());
            }
            digest.update(&buffer[..count]);
            file.write_all(&buffer[..count])
                .map_err(|error| format!("导出写入失败: {error}"))?;
        }
        let sha256 = format!("{:x}", digest.finalize());
        if expected_size.is_some_and(|expected| expected != size)
            || expected_hash.is_some_and(|expected| !expected.eq_ignore_ascii_case(&sha256))
        {
            return Err("导出文件内容校验失败，请重新下载".to_string());
        }
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        fs::rename(&partial, &target).map_err(|error| error.to_string())?;
        Ok(DownloadedExport {
            path: target.to_string_lossy().to_string(),
            file_name,
            byte_size: size,
            sha256,
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

pub(crate) fn download_export(job_id: &str) -> Result<DownloadedExport, String> {
    let job_id = validated_id(job_id)?;
    let current =
        credentials::refreshed_session()?.ok_or_else(|| "请先登录协作服务器".to_string())?;
    let url = format!(
        "{}/api/v1/dataset-jobs/{job_id}/download",
        current.server_url.trim_end_matches('/')
    );
    let response = client()?
        .get(url)
        .bearer_auth(&current.access_token)
        .send()
        .map_err(|error| format!("导出下载失败: {error}"))?;
    if !response.status().is_success() {
        return Err(response_error(response));
    }
    persist_export(
        response,
        &export_directory(&current.server_url, &current.account_id),
        job_id,
    )
}

#[tauri::command]
pub async fn hosting_upload_archive(
    dataset_id: String,
    format: String,
    file_name: String,
    base64: String,
    operation_id: Option<String>,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        upload_archive(
            &dataset_id,
            &format,
            &file_name,
            &base64,
            operation_id.as_deref(),
        )
    })
    .await
    .map_err(|error| format!("archive upload worker failed: {error}"))?
}

#[tauri::command]
pub async fn hosting_download_export(job_id: String) -> Result<DownloadedExport, String> {
    tauri::async_runtime::spawn_blocking(move || download_export(&job_id))
        .await
        .map_err(|error| format!("archive download worker failed: {error}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn response(bytes: &'static [u8], hash: &str) -> reqwest::blocking::Response {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nX-Content-SHA256: {hash}\r\nConnection: close\r\n\r\n", bytes.len());
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request);
            socket.write_all(header.as_bytes()).unwrap();
            socket.write_all(bytes).unwrap();
        });
        reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://{address}/export"))
            .send()
            .unwrap()
    }

    #[test]
    fn archive_transfer_rejects_untrusted_paths_formats_and_content_before_credentials() {
        assert!(upload_archive("../outside", "native", "x.zip", "UEs=", None).is_err());
        let id = Uuid::new_v4().to_string();
        assert!(upload_archive(&id, "native&format=other", "x.zip", "UEs=", None).is_err());
        assert!(upload_archive(&id, "native", "x.txt", "UEs=", None).is_err());
        assert!(upload_archive(&id, "native", "x.zip", "not base64!", None).is_err());
        assert!(upload_archive(&id, "native", "x.zip", "YWJj", None).is_err());
        assert!(upload_archive(&id, "native", "x.zip", "UEs=", Some("../op")).is_err());
    }

    #[test]
    fn export_paths_are_scoped_by_server_and_account() {
        let first = export_directory("https://example.test/a", "one");
        assert_ne!(first, export_directory("https://example.test/b", "one"));
        assert_ne!(first, export_directory("https://example.test/a", "two"));
        assert!(first.starts_with(project_fs::storage_roots().app_data.join("exports")));
    }

    #[test]
    fn export_download_verifies_hash_and_never_keeps_partial_corrupt_files() {
        let directory =
            std::env::temp_dir().join(format!("hosting-export-test-{}", Uuid::new_v4()));
        let data = b"PK archive fixture";
        let expected = format!("{:x}", Sha256::digest(data));
        let result = persist_export(response(data, &expected), &directory, Uuid::new_v4()).unwrap();
        assert_eq!(fs::read(&result.path).unwrap(), data);
        assert_eq!(result.sha256, expected);
        let failed_id = Uuid::new_v4();
        assert!(persist_export(response(data, &"0".repeat(64)), &directory, failed_id).is_err());
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }
}

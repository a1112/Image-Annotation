//! Native hand-off for downloaded dataset exports.
//!
//! The WebView never supplies an arbitrary path to this command.  The native
//! side derives the current server/account scope from the protected session
//! store and accepts only the deterministic export filename for the supplied
//! UUID.  Android then hands a FileProvider URI to the system share/save
//! surface; it does not upload or send the file itself.

use super::hosting;
use crate::credentials;
use serde::Serialize;
use std::{fs, path::PathBuf};
use tauri::{plugin::TauriPlugin, AppHandle, Manager, Runtime};
use uuid::Uuid;

#[cfg(not(target_os = "android"))]
use tauri::plugin::Builder;
#[cfg(target_os = "android")]
use tauri::plugin::{Builder, PluginHandle};

#[cfg(target_os = "android")]
const PLUGIN_IDENTIFIER: &str = "com.imageannotation.desktop";

#[cfg(target_os = "android")]
#[derive(Debug)]
pub struct HostingFiles<R: Runtime>(PluginHandle<R>);

#[cfg(target_os = "android")]
impl<R: Runtime> HostingFiles<R> {
    fn open_export(&self, path: &str, file_name: &str) -> Result<(), String> {
        self.0
            .run_mobile_plugin::<()>(
                "openExport",
                serde_json::json!({
                    "path": path,
                    "fileName": file_name,
                }),
            )
            .map_err(|error| format!("Android 文件分享失败: {error}"))
    }
}

/// Register the Android-only native share plugin.  iOS uses its existing
/// Documents/Files entry, while desktop uses the opener plugin directly.
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("hosting-files")
        .setup(|app, api| {
            #[cfg(target_os = "android")]
            {
                let handle =
                    api.register_android_plugin(PLUGIN_IDENTIFIER, "HostingFilesPlugin")?;
                app.manage(HostingFiles(handle));
            }
            #[cfg(not(target_os = "android"))]
            {
                let _ = (app, api);
            }
            Ok(())
        })
        .build()
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostingOpenExportResult {
    pub path: String,
    pub file_name: String,
    pub action: String,
    pub message: String,
}

fn export_path_for_current_session(job_id: &str) -> Result<(PathBuf, String), String> {
    let job_id = Uuid::parse_str(job_id).map_err(|_| "导出作业 ID 无效".to_string())?;
    let session =
        credentials::refreshed_session()?.ok_or_else(|| "请先登录协作服务器".to_string())?;
    let directory = hosting::export_directory(&session.server_url, &session.account_id);
    let file_name = format!("dataset-export-{job_id}.zip");
    Ok((canonical_export_file(&directory, &file_name)?, file_name))
}

/// Resolve an export only after checking both the directory and the final
/// file through the filesystem.  Canonicalizing the final file is important:
/// a regular-looking entry inside `exports/` may otherwise be a symlink into
/// another account's data or an arbitrary local path.
fn canonical_export_file(directory: &std::path::Path, file_name: &str) -> Result<PathBuf, String> {
    if file_name.is_empty()
        || file_name.contains('/')
        || file_name.contains('\\')
        || file_name == "."
        || file_name == ".."
    {
        return Err("导出文件名无效".to_string());
    }

    let target = directory.join(file_name);
    let canonical_directory =
        fs::canonicalize(directory).map_err(|_| "导出目录不存在，请先下载导出文件".to_string())?;
    let canonical_target =
        fs::canonicalize(&target).map_err(|_| "导出文件不存在，请先下载完成导出".to_string())?;
    if !canonical_target.is_file()
        || canonical_target.parent() != Some(canonical_directory.as_path())
    {
        return Err("导出文件路径不在当前账号的导出目录中".to_string());
    }
    Ok(canonical_target)
}

#[tauri::command]
pub async fn hosting_open_export(
    app: AppHandle,
    job_id: String,
) -> Result<HostingOpenExportResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let (path, file_name) = export_path_for_current_session(&job_id)?;

        #[cfg(target_os = "android")]
        {
            let path_string = path.to_string_lossy().into_owned();
            let state = app
                .try_state::<HostingFiles<tauri::Wry>>()
                .ok_or_else(|| "Android 文件分享插件尚未初始化".to_string())?;
            state.open_export(&path_string, &file_name)?;
            return Ok(HostingOpenExportResult {
                path: path_string,
                file_name,
                action: "share".to_string(),
                message: "已打开系统分享或保存面板".to_string(),
            });
        }

        #[cfg(target_os = "ios")]
        {
            let path_string = path.to_string_lossy().into_owned();
            let _ = app;
            return Ok(HostingOpenExportResult {
                path: path_string,
                file_name,
                action: "files".to_string(),
                message:
                    "导出文件已保存到“文件”应用中的 Image Annotation/Documents/Dataset Exports。"
                        .to_string(),
            });
        }

        #[cfg(not(mobile))]
        {
            use tauri_plugin_opener::OpenerExt;
            app.opener()
                .reveal_item_in_dir(&path)
                .map_err(|error| format!("无法在文件管理器中显示导出文件: {error}"))?;
            let path_string = path.to_string_lossy().into_owned();
            return Ok(HostingOpenExportResult {
                path: path_string,
                file_name,
                action: "reveal".to_string(),
                message: "已在文件管理器中显示导出文件".to_string(),
            });
        }
    })
    .await
    .map_err(|error| format!("打开导出文件 worker failed: {error}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn canonical_export_file_accepts_a_real_scoped_export() {
        let root = std::env::temp_dir().join(format!("hosting-files-test-{}", Uuid::new_v4()));
        let directory = root.join("exports");
        fs::create_dir_all(&directory).unwrap();
        let id = Uuid::new_v4();
        let valid = format!("dataset-export-{id}.zip");
        let target = directory.join(&valid);
        fs::File::create(&target)
            .unwrap()
            .write_all(b"PK\x03\x04")
            .unwrap();

        let expected = fs::canonicalize(&target).unwrap();
        assert_eq!(canonical_export_file(&directory, &valid).unwrap(), expected);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn canonical_export_file_rejects_missing_files() {
        let root = std::env::temp_dir().join(format!("hosting-files-test-{}", Uuid::new_v4()));
        let directory = root.join("exports");
        fs::create_dir_all(&directory).unwrap();

        let error = canonical_export_file(&directory, "dataset-export-missing.zip").unwrap_err();
        assert!(error.contains("不存在"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn canonical_export_file_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!("hosting-files-test-{}", Uuid::new_v4()));
        let directory = root.join("exports");
        let outside = root.join("outside.zip");
        fs::create_dir_all(&directory).unwrap();
        fs::File::create(&outside).unwrap();
        symlink(&outside, directory.join("dataset-export-escape.zip")).unwrap();

        let error = canonical_export_file(&directory, "dataset-export-escape.zip").unwrap_err();
        assert!(
            error.contains("当前账号") || error.contains("不存在"),
            "{error}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn export_path_rejects_invalid_uuid_before_reading_credentials() {
        let error = export_path_for_current_session("../outside").unwrap_err();
        assert_eq!(error, "导出作业 ID 无效");
    }
}

//! UI adapter for the storage policy. Selection completes before any background
//! sync, HTTP server, dataset download, or other project access can start.
#[cfg(all(not(mobile), not(all(dev, debug_assertions))))]
use std::path::Path;
use tauri::AppHandle;
#[cfg(not(all(dev, debug_assertions)))]
use tauri::Manager;

pub fn initialize(app: &AppHandle) -> Result<(), String> {
    #[cfg(all(dev, debug_assertions))]
    {
        let _ = app;
        // Existing developer datasets retain their exact on-disk layout.
        crate::project_fs::initialize_development_storage()
    }
    #[cfg(not(all(dev, debug_assertions)))]
    {
        let app_data = app.path().app_data_dir().map_err(|error| {
            let message = format!("无法确定当前用户的应用数据目录：{error}。请检查当前用户的主目录设置后重新启动。未写入任何项目数据。");
            #[cfg(not(mobile))]
            rfd::MessageDialog::new().set_title("数据目录不可用").set_description(&message).show();
            message
        })?;
        #[cfg(not(mobile))]
        let root = {
            let legacy_root = crate::project_fs::development_workspace_root();
            crate::runtime_storage::resolve_root(&app_data, &legacy_root, &mut NativeStorageUi)
                .ok_or_else(|| "已取消数据目录选择，应用未启动数据服务。".to_string())?
        };
        #[cfg(mobile)]
        let root = {
            // Mobile uses its platform-owned app data; desktop dialogs are not
            // available there. An existing saved choice still must not fall back.
            match crate::runtime_storage::read_selection(&app_data)? {
                Some(root) => crate::runtime_storage::prepare_existing(&root)?,
                None => crate::runtime_storage::prepare_new(&app_data)?,
            }
        };
        crate::project_fs::initialize_storage(root)
    }
}

#[cfg(all(not(mobile), not(all(dev, debug_assertions))))]
struct NativeStorageUi;

#[cfg(all(not(mobile), not(all(dev, debug_assertions))))]
impl crate::runtime_storage::StorageUi for NativeStorageUi {
    fn choose_root(
        &mut self,
        app_data: &Path,
        legacy_root: &Path,
        legacy: &Result<bool, String>,
    ) -> crate::runtime_storage::RootChoice {
        use crate::runtime_storage::RootChoice;
        use rfd::{MessageButtons, MessageDialog, MessageDialogResult};
        let legacy_message = match legacy {
            Ok(true) => format!("检测到旧工作目录：{}", legacy_root.display()),
            Ok(false) => {
                "未在此版本的旧工作目录发现数据；其他版本或位置的数据仍需手动选择。".into()
            }
            Err(error) => format!("无法检查旧工作目录：{error}"),
        };
        loop {
            let result = MessageDialog::new()
                .set_title("选择数据存储位置")
                .set_description(format!(
                    "{legacy_message}\n\n是 / Yes：使用当前用户的应用数据目录（新安装推荐）：\n{}\n\n否 / No：选择包含 data 文件夹的已有工作目录。\n取消 / Cancel：退出，稍后再选择。\n\n使用应用数据目录不会迁移已有项目；旧数据将保留在原处，列表仅显示所选位置的数据。本机链接的图片目录保持原位置。推荐的当前用户应用数据目录通常无需额外权限。应用不会主动申请管理员或全磁盘访问权限；若已有目录受限，可按系统提示处理，或选择可读写的位置。",
                    app_data.display()
                ))
                .set_buttons(MessageButtons::YesNoCancel)
                .show();
            match result {
                MessageDialogResult::Yes => return RootChoice::AppData,
                MessageDialogResult::No => {
                    let mut picker =
                        rfd::FileDialog::new().set_title("选择包含 data 文件夹的已有工作目录");
                    if matches!(legacy, Ok(true)) {
                        picker = picker.set_directory(legacy_root);
                    }
                    if let Some(root) = picker.pick_folder() {
                        return RootChoice::Existing(root);
                    }
                    // Dismissing the picker returns to the choice; it never
                    // implies consent to create a different workspace.
                }
                _ => return RootChoice::Cancel,
            }
        }
    }

    fn recover(&mut self, error: &str) -> crate::runtime_storage::RecoveryChoice {
        use crate::runtime_storage::RecoveryChoice;
        use rfd::{MessageButtons, MessageDialog, MessageDialogResult};
        match MessageDialog::new()
            .set_title("数据目录暂不可用")
            .set_description(format!("{error}\n\n未切换到空白数据目录，原有项目未移动或删除。\n\n是 / Yes：修复目录访问或挂载磁盘后重试。\n否 / No：重新选择数据存储位置。\n取消 / Cancel：退出。"))
            .set_buttons(MessageButtons::YesNoCancel)
            .show() {
            MessageDialogResult::Yes => RecoveryChoice::Retry,
            MessageDialogResult::No => RecoveryChoice::ChooseAgain,
            _ => RecoveryChoice::Cancel,
        }
    }
}

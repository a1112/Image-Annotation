pub mod annotations;
pub mod datasets;
pub mod domain;
pub mod http_backend;
pub mod importers;
mod platform;
pub mod project_fs;
pub mod storage;
pub mod windows;

use datasets::{BuiltinDataset, DownloadJob};
use domain::{
    AnnotationObject, AnnotationSaveResult, AnnotationState, AnnotationTask, AnnotationVersion,
    BackendDesign, BackendTask, ClassSample, DatasetExport, DatasetImage, DatasetProject, DatasetSnapshot,
    ProjectDetail, SampleRepository, TaskItem,
};
use platform::NativeBackdropStatus;
use serde::Serialize;
use std::sync::Mutex;
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
    Ok(repository.dataset_projects())
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
    http_backend::desktop_health_payload()
}

#[tauri::command]
fn list_builtin_datasets() -> Result<Vec<BuiltinDataset>, String> {
    project_fs::ensure_test_data_dirs()?;
    Ok(datasets::builtin_datasets())
}

#[tauri::command]
fn download_test_dataset(
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

    match datasets::download_test_dataset(&dataset_key) {
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
                BackendTask::new(task_id, "YOLO 数据集导入", "yolo-import", "failed", 0, &error)
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
                BackendTask::new(task_id, "打开本机数据集", "local-dataset-open", "failed", 0, &error)
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
) -> Result<AnnotationTask, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.create_annotation_task(&project_id, &name)
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
fn claim_task_item(
    repository: State<'_, RepositoryState>,
    project_id: String,
    task_id: String,
    image_id: String,
) -> Result<(), String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.claim_task_item(&project_id, &task_id, &image_id)
}

#[tauri::command]
fn release_task_item(
    repository: State<'_, RepositoryState>,
    project_id: String,
    task_id: String,
    image_id: String,
) -> Result<(), String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository.release_task_item(&project_id, &task_id, &image_id)
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
fn get_file_asset_path(
    repository: State<'_, RepositoryState>,
    project_id: String,
    image_id: String,
) -> Result<String, String> {
    let repository = repository.lock().map_err(|err| err.to_string())?;
    repository
        .image_path(&project_id, &image_id)
        .map(|path| path.to_string_lossy().to_string())
        .ok_or_else(|| format!("image not found: {project_id}/{image_id}"))
}

#[tauri::command]
fn open_annotation_window(
    app: AppHandle,
    repository: State<'_, RepositoryState>,
    project_id: String,
    image_id: Option<String>,
) -> Result<(), String> {
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
fn open_backend_task_tray(app: AppHandle) -> Result<(), String> {
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
        .manage(NativeBackdropState::new(NativeBackdropStatus::pending()))
        .manage(RepositoryState::new(SampleRepository::new()))
        .manage(BackendTaskState::new(Vec::new()))
        .plugin(tauri_plugin_notification::init())
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
                start_drag_window,
                minimize_window,
                toggle_maximize_window,
                hide_to_tray,
                show_window,
                close_window,
                window_state,
                backend_health,
                list_dataset_projects,
                get_project_detail,
                get_backend_design,
                list_builtin_datasets,
                download_test_dataset,
                create_dataset_project,
                create_project,
                import_images,
                import_yolo_dataset,
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
                get_annotation_history,
                restore_annotation_version,
                create_annotation_task,
                list_tasks,
                list_task_items,
                claim_task_item,
                release_task_item,
                review_task_item,
                list_review_queue,
                create_dataset_snapshot,
                list_snapshots,
                export_dataset,
                list_exports,
                get_file_asset_path,
                open_annotation_window,
                list_backend_tasks,
                get_backend_task,
                clear_completed_backend_tasks,
                retry_backend_task,
                open_backend_task_tray
            ])
            .setup(|app| {
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
            window_state,
            backend_health,
            list_dataset_projects,
            get_project_detail,
            get_backend_design,
            list_builtin_datasets,
            download_test_dataset,
            create_dataset_project,
            create_project,
            import_images,
            import_yolo_dataset,
            open_local_dataset,
            rescan_project_assets,
            generate_thumbnails,
            get_dataset_download_job,
            list_project_images,
            get_image_annotations,
            get_image_annotation_state,
            save_image_annotations,
            submit_image_annotations,
            get_annotation_history,
            restore_annotation_version,
            create_annotation_task,
            list_tasks,
            list_task_items,
            claim_task_item,
            release_task_item,
            review_task_item,
            list_review_queue,
            create_dataset_snapshot,
            list_snapshots,
            export_dataset,
            list_exports,
            get_file_asset_path,
            open_annotation_window,
            list_backend_tasks,
            get_backend_task,
            clear_completed_backend_tasks,
            retry_backend_task,
            open_backend_task_tray
        ]);
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
    let tasks_item =
        MenuItem::with_id(app, TRAY_MENU_TASKS_ID, "后台任务", true, None::<&str>)?;
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
        TrayAction::BackendTasks => windows::open_backend_tasks_window(app),
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
        let repository = domain::SampleRepository::new();
        let projects = repository.dataset_projects();
        assert!(projects.iter().all(|project| !project.id.is_empty()));

        if let Some(project) = projects.first() {
            let detail = repository
                .project_detail(&project.id)
                .expect("project detail exists for listed project");
            assert_eq!(detail.project.id, project.id);
            assert!(detail.tag_groups.iter().any(|group| group.name == "train"));
        }
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

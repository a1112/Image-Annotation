use crate::{
    datasets::{self, DownloadJob},
    domain::{
        AnnotationObject, BackendTask, DatasetProject, DatasetSnapshot, SampleRepository,
    },
    project_fs,
};
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{Arc, Mutex, OnceLock},
    thread,
    time::Duration,
};
use tauri::AppHandle;

const BACKEND_BIND_ADDR: &str = "127.0.0.1:17310";
const BACKEND_BASE_URL: &str = "http://127.0.0.1:17310";

pub fn backend_bind_addr() -> &'static str {
    BACKEND_BIND_ADDR
}

pub fn backend_base_url() -> &'static str {
    BACKEND_BASE_URL
}

pub fn health_payload() -> Value {
    health_payload_for("standalone-backend", &["datasets", "assets", "annotations", "tasks"])
}

pub fn desktop_health_payload() -> Value {
    health_payload_for(
        "tauri-desktop",
        &["datasets", "assets", "annotations", "tasks", "windows", "tray"],
    )
}

fn health_payload_for(runtime: &str, capabilities: &[&str]) -> Value {
    json!({
        "status": "ok",
        "service": "image-annotation-rust-backend",
        "version": env!("CARGO_PKG_VERSION"),
        "runtime": runtime,
        "capabilities": capabilities
    })
}

pub struct BackendRuntime {
    repository: Mutex<SampleRepository>,
    tasks: Mutex<Vec<BackendTask>>,
    app: Option<AppHandle>,
}

impl BackendRuntime {
    fn standalone() -> Self {
        Self {
            repository: Mutex::new(SampleRepository::new()),
            tasks: Mutex::new(Vec::new()),
            app: None,
        }
    }

    fn tauri_desktop(app: AppHandle) -> Self {
        Self {
            repository: Mutex::new(SampleRepository::new()),
            tasks: Mutex::new(Vec::new()),
            app: Some(app),
        }
    }

    fn health_payload(&self) -> Value {
        if self.app.is_some() {
            desktop_health_payload()
        } else {
            health_payload()
        }
    }
}

static BACKEND_STARTED: OnceLock<()> = OnceLock::new();

pub fn start_background_backend(app: AppHandle) -> Result<(), String> {
    if BACKEND_STARTED.get().is_some() {
        return Ok(());
    }
    let listener = match TcpListener::bind(BACKEND_BIND_ADDR) {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            let _ = BACKEND_STARTED.set(());
            return Ok(());
        }
        Err(error) => return Err(error.to_string()),
    };
    let runtime = Arc::new(BackendRuntime::tauri_desktop(app));
    BACKEND_STARTED
        .set(())
        .map_err(|_| "backend startup state already initialized".to_string())?;
    thread::spawn(move || serve(listener, runtime));
    Ok(())
}

pub fn run_foreground_backend() -> Result<(), String> {
    let listener = match TcpListener::bind(BACKEND_BIND_ADDR) {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            println!("Rust backend already listening on {BACKEND_BASE_URL}");
            loop {
                thread::sleep(Duration::from_secs(3600));
            }
        }
        Err(error) => return Err(error.to_string()),
    };
    serve(listener, Arc::new(BackendRuntime::standalone()));
    Ok(())
}

fn serve(listener: TcpListener, runtime: Arc<BackendRuntime>) {
    for stream in listener.incoming().flatten() {
        let runtime = runtime.clone();
        thread::spawn(move || {
            if let Err(error) = handle_stream(stream, runtime) {
                eprintln!("local http backend request failed: {error}");
            }
        });
    }
}

fn handle_stream(mut stream: TcpStream, runtime: Arc<BackendRuntime>) -> Result<(), String> {
    let request = read_request(&mut stream)?;
    let response = route_request(request, runtime);
    let response_bytes = response.as_bytes();
    stream
        .write_all(&response_bytes)
        .map_err(|err| err.to_string())
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    body: String,
}

fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, String> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).map_err(|err| err.to_string())?;
        if read == 0 {
            return Err("empty http request".to_string());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find_header_end(&buffer) {
            break position;
        }
        if buffer.len() > 1024 * 1024 {
            return Err("http headers too large".to_string());
        }
    };

    let header_text = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = header_text.lines();
    let request_line = lines.next().ok_or_else(|| "missing request line".to_string())?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .ok_or_else(|| "missing method".to_string())?
        .to_string();
    let path = request_parts
        .next()
        .ok_or_else(|| "missing path".to_string())?
        .to_string();
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let body_start = header_end + 4;
    while buffer.len().saturating_sub(body_start) < content_length {
        let read = stream.read(&mut chunk).map_err(|err| err.to_string())?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body_end = (body_start + content_length).min(buffer.len());
    let body = String::from_utf8_lossy(&buffer[body_start..body_end]).to_string();

    Ok(HttpRequest { method, path, body })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn route_request(request: HttpRequest, runtime: Arc<BackendRuntime>) -> HttpResponse {
    if request.method == "OPTIONS" {
        return HttpResponse::empty(204);
    }
    if request.method == "GET" && request.path == "/api/health" {
        return HttpResponse::json(200, json!({ "ok": true, "data": runtime.health_payload() }));
    }
    if request.method == "POST" && request.path.starts_with("/api/invoke/") {
        let command = request.path.trim_start_matches("/api/invoke/");
        let args = if request.body.trim().is_empty() {
            json!({})
        } else {
            match serde_json::from_str::<Value>(&request.body) {
                Ok(value) => value,
                Err(error) => {
                    return HttpResponse::json(400, json!({ "ok": false, "error": error.to_string() }))
                }
            }
        };
        return match dispatch_command(&runtime, command, args) {
            Ok(data) => HttpResponse::json(200, json!({ "ok": true, "data": data })),
            Err(error) => HttpResponse::json(500, json!({ "ok": false, "error": error })),
        };
    }
    if request.method == "GET" && request.path.starts_with("/api/assets/") {
        return asset_response(&runtime, &request.path);
    }
    HttpResponse::json(404, json!({ "ok": false, "error": "not found" }))
}

fn dispatch_command(
    runtime: &BackendRuntime,
    command: &str,
    args: Value,
) -> Result<Value, String> {
    match command {
        "backend_health" => Ok(runtime.health_payload()),
        "list_builtin_datasets" => {
            project_fs::ensure_test_data_dirs()?;
            serde_json::to_value(datasets::builtin_datasets()).map_err(|err| err.to_string())
        }
        "download_test_dataset" => {
            let dataset_key = string_arg(&args, "datasetKey")?;
            record_task(
                runtime,
                BackendTask::new(
                    format!("download-{dataset_key}"),
                    format!("{} 导入", dataset_key.to_ascii_uppercase()),
                    "dataset-import",
                    "running",
                    10,
                    "正在准备测试数据集",
                ),
            )?;
            let job = datasets::download_test_dataset(&dataset_key);
            match job {
                Ok(job) => {
                    record_task(
                        runtime,
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
                    serde_json::to_value(job).map_err(|err| err.to_string())
                }
                Err(error) => {
                    record_task(
                        runtime,
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
        "get_dataset_download_job" => {
            let dataset_key = string_arg(&args, "datasetKey")?;
            serde_json::to_value(datasets::completed_job(&dataset_key)).map_err(|err| err.to_string())
        }
        "list_dataset_projects" => {
            let repository = runtime.repository.lock().map_err(|err| err.to_string())?;
            serde_json::to_value(repository.dataset_projects()).map_err(|err| err.to_string())
        }
        "get_project_detail" => {
            let project_id = string_arg(&args, "projectId")?;
            let repository = runtime.repository.lock().map_err(|err| err.to_string())?;
            let detail = repository
                .project_detail(&project_id)
                .ok_or_else(|| format!("project not found: {project_id}"))?;
            serde_json::to_value(detail).map_err(|err| err.to_string())
        }
        "list_project_images" => {
            let project_id = string_arg(&args, "projectId")?;
            let group_id = optional_string_arg(&args, "groupId");
            let offset = optional_u32_arg(&args, "offset");
            let limit = optional_u32_arg(&args, "limit");
            let repository = runtime.repository.lock().map_err(|err| err.to_string())?;
            serde_json::to_value(repository.project_images_paged(&project_id, group_id, offset, limit))
                .map_err(|err| err.to_string())
        }
        "list_class_samples" => {
            let project_id = string_arg(&args, "projectId")?;
            let class_id = optional_u32_arg(&args, "classId");
            let label = string_arg(&args, "label")?;
            let offset = optional_u32_arg(&args, "offset");
            let limit = optional_u32_arg(&args, "limit");
            let repository = runtime.repository.lock().map_err(|err| err.to_string())?;
            serde_json::to_value(repository.class_samples(&project_id, class_id, &label, offset, limit))
                .map_err(|err| err.to_string())
        }
        "get_image_annotations" => {
            let project_id = string_arg(&args, "projectId")?;
            let image_id = string_arg(&args, "imageId")?;
            let repository = runtime.repository.lock().map_err(|err| err.to_string())?;
            serde_json::to_value(repository.image_annotations(&project_id, &image_id))
                .map_err(|err| err.to_string())
        }
        "get_image_annotation_state" => {
            let project_id = string_arg(&args, "projectId")?;
            let image_id = string_arg(&args, "imageId")?;
            let repository = runtime.repository.lock().map_err(|err| err.to_string())?;
            serde_json::to_value(repository.image_annotation_state(&project_id, &image_id))
                .map_err(|err| err.to_string())
        }
        "save_image_annotations" => {
            let project_id = string_arg(&args, "projectId")?;
            let image_id = string_arg(&args, "imageId")?;
            let revision = optional_string_arg(&args, "revision");
            let objects: Vec<AnnotationObject> = serde_json::from_value(
                args.get("objects").cloned().unwrap_or_else(|| json!([])),
            )
            .map_err(|err| err.to_string())?;
            let repository = runtime.repository.lock().map_err(|err| err.to_string())?;
            serde_json::to_value(repository.save_image_annotations_with_revision(
                &project_id, &image_id, revision, objects,
            )?)
            .map_err(|err| err.to_string())
        }
        "submit_image_annotations" => {
            let project_id = string_arg(&args, "projectId")?;
            let image_id = string_arg(&args, "imageId")?;
            let repository = runtime.repository.lock().map_err(|err| err.to_string())?;
            repository.submit_image_annotations(&project_id, &image_id)?;
            Ok(Value::Null)
        }
        "create_dataset_project" => {
            let name = string_arg(&args, "name")?;
            let dataset_type = string_arg(&args, "datasetType")?;
            let demo_template = string_arg(&args, "demoTemplate")?;
            serde_json::to_value(with_dataset_task(
                runtime,
                format!("create-{}", slug(&name, &demo_template)),
                format!("{name} 创建"),
                "dataset-create",
                "正在创建本地数据集工程",
                || datasets::create_dataset_project(&name, &dataset_type, &demo_template),
            )?)
            .map_err(|err| err.to_string())
        }
        "create_project" => {
            let name = string_arg(&args, "name")?;
            let dataset_type = string_arg(&args, "datasetType")?;
            serde_json::to_value(datasets::create_dataset_project(&name, &dataset_type, "empty"))
                .map_err(|err| err.to_string())
        }
        "import_images" => {
            let project_id = string_arg(&args, "projectId")?;
            let source_path = string_arg(&args, "sourcePath")?;
            serde_json::to_value(with_dataset_task(
                runtime,
                format!("import-images-{project_id}"),
                "图片目录导入".to_string(),
                "image-import",
                "正在导入本地图片目录",
                || datasets::import_images_into_project(&project_id, &source_path),
            )?)
            .map_err(|err| err.to_string())
        }
        "import_yolo_dataset" => {
            let project_id = string_arg(&args, "projectId")?;
            let source_path = string_arg(&args, "sourcePath")?;
            serde_json::to_value(with_dataset_task(
                runtime,
                format!("import-yolo-{project_id}"),
                "YOLO 数据集导入".to_string(),
                "yolo-import",
                "正在导入 YOLO 数据集目录",
                || datasets::import_yolo_dataset_into_project(&project_id, &source_path),
            )?)
            .map_err(|err| err.to_string())
        }
        "open_local_dataset" => {
            let source_path = string_arg(&args, "sourcePath")?;
            let dataset_type = string_arg(&args, "datasetType")?;
            serde_json::to_value(with_dataset_task(
                runtime,
                format!("open-local-{}", slug(&source_path, "dataset")),
                "打开本机数据集".to_string(),
                "local-dataset-open",
                "正在索引本机目录",
                || datasets::open_local_dataset(&source_path, &dataset_type),
            )?)
            .map_err(|err| err.to_string())
        }
        "rescan_project_assets" => {
            let project_id = string_arg(&args, "projectId")?;
            serde_json::to_value(datasets::rescan_project_assets(&project_id)?)
                .map_err(|err| err.to_string())
        }
        "generate_thumbnails" => {
            let project_id = string_arg(&args, "projectId")?;
            serde_json::to_value(datasets::generate_project_thumbnails(&project_id)?)
                .map_err(|err| err.to_string())
        }
        "list_snapshots" => {
            let project_id = string_arg(&args, "projectId")?;
            let repository = runtime.repository.lock().map_err(|err| err.to_string())?;
            serde_json::to_value(repository.dataset_snapshots(&project_id)).map_err(|err| err.to_string())
        }
        "create_dataset_snapshot" => {
            let project_id = string_arg(&args, "projectId")?;
            let name = string_arg(&args, "name")?;
            let repository = runtime.repository.lock().map_err(|err| err.to_string())?;
            serde_json::to_value(repository.create_dataset_snapshot(&project_id, &name)?)
                .map_err(|err| err.to_string())
        }
        "list_exports" => {
            let project_id = string_arg(&args, "projectId")?;
            let repository = runtime.repository.lock().map_err(|err| err.to_string())?;
            serde_json::to_value(repository.dataset_exports(&project_id)).map_err(|err| err.to_string())
        }
        "export_dataset" => {
            let project_id = string_arg(&args, "projectId")?;
            let snapshot_id = string_arg(&args, "snapshotId")?;
            let format = string_arg(&args, "format")?;
            let repository = runtime.repository.lock().map_err(|err| err.to_string())?;
            serde_json::to_value(repository.export_dataset(&project_id, &snapshot_id, &format)?)
                .map_err(|err| err.to_string())
        }
        "list_backend_tasks" => {
            let mut tasks = runtime.tasks.lock().map_err(|err| err.to_string())?.clone();
            tasks.sort_by(|left, right| right.started_at.cmp(&left.started_at));
            serde_json::to_value(tasks).map_err(|err| err.to_string())
        }
        "clear_completed_backend_tasks" => {
            let mut tasks = runtime.tasks.lock().map_err(|err| err.to_string())?;
            tasks.retain(|task| task.status != "completed");
            Ok(Value::Null)
        }
        "get_backend_task" => {
            let task_id = string_arg(&args, "taskId")?;
            let tasks = runtime.tasks.lock().map_err(|err| err.to_string())?;
            serde_json::to_value(tasks.iter().find(|task| task.id == task_id).cloned())
                .map_err(|err| err.to_string())
        }
        "retry_backend_task" => {
            let task_id = string_arg(&args, "taskId")?;
            let mut tasks = runtime.tasks.lock().map_err(|err| err.to_string())?;
            let task = tasks
                .iter_mut()
                .find(|task| task.id == task_id)
                .ok_or_else(|| format!("backend task not found: {task_id}"))?;
            task.status = "running".to_string();
            task.progress = 0;
            task.message = "已加入重试队列".to_string();
            task.finished_at = None;
            Ok(Value::Null)
        }
        "open_annotation_window" => {
            let app = runtime
                .app
                .as_ref()
                .ok_or_else(desktop_capability_unavailable)?;
            let project_id = string_arg(&args, "projectId")?;
            let image_id = optional_string_arg(&args, "imageId").or_else(|| {
                runtime
                    .repository
                    .lock()
                    .ok()
                    .and_then(|repository| {
                        repository
                            .project_images(&project_id, None)
                            .first()
                            .map(|image| image.id.clone())
                    })
            });
            crate::windows::open_annotation_window(app, &project_id, image_id.as_deref())?;
            Ok(Value::Null)
        }
        "open_backend_task_tray" => {
            let app = runtime
                .app
                .as_ref()
                .ok_or_else(desktop_capability_unavailable)?;
            crate::windows::open_backend_tasks_window(app)?;
            Ok(Value::Null)
        }
        "get_file_asset_path" => {
            let project_id = string_arg(&args, "projectId")?;
            let image_id = string_arg(&args, "imageId")?;
            let repository = runtime.repository.lock().map_err(|err| err.to_string())?;
            let path = repository
                .image_path(&project_id, &image_id)
                .ok_or_else(|| format!("image not found: {project_id}/{image_id}"))?;
            Ok(json!(path.to_string_lossy().to_string()))
        }
        _ => Err(format!("unknown backend command: {command}")),
    }
}

fn asset_response(runtime: &BackendRuntime, path: &str) -> HttpResponse {
    let parts: Vec<_> = path.trim_start_matches("/api/assets/").split('/').collect();
    if parts.len() != 2 {
        return HttpResponse::json(400, json!({ "ok": false, "error": "invalid asset path" }));
    }
    let project_id = percent_decode(parts[0]);
    let image_id = percent_decode(parts[1]);
    let repository = match runtime.repository.lock() {
        Ok(repository) => repository,
        Err(error) => {
            return HttpResponse::json(500, json!({ "ok": false, "error": error.to_string() }))
        }
    };
    let Some(path) = repository.image_path(&project_id, &image_id) else {
        return HttpResponse::json(404, json!({ "ok": false, "error": "image not found" }));
    };
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return HttpResponse::json(404, json!({ "ok": false, "error": error.to_string() }))
        }
    };
    HttpResponse::bytes(200, content_type_for_path(&path), bytes)
}

fn desktop_capability_unavailable() -> String {
    "desktop_capability_unavailable: 当前后台不是 Tauri 桌面运行时".to_string()
}

fn with_dataset_task(
    runtime: &BackendRuntime,
    task_id: String,
    title: String,
    kind: &'static str,
    running_message: &'static str,
    action: impl FnOnce() -> Result<DatasetProject, String>,
) -> Result<DatasetProject, String> {
    record_task(
        runtime,
        BackendTask::new(&task_id, &title, kind, "running", 20, running_message),
    )?;
    match action() {
        Ok(project) => {
            record_task(
                runtime,
                BackendTask::new(
                    task_id,
                    title,
                    kind,
                    "completed",
                    100,
                    format!("已索引 {} 张图片", project.image_count),
                )
                .finished(),
            )?;
            Ok(project)
        }
        Err(error) => {
            record_task(
                runtime,
                BackendTask::new(task_id, title, kind, "failed", 0, error.clone()).finished(),
            )?;
            Err(error)
        }
    }
}

fn record_task(runtime: &BackendRuntime, task: BackendTask) -> Result<(), String> {
    let mut tasks = runtime.tasks.lock().map_err(|err| err.to_string())?;
    if let Some(existing) = tasks.iter_mut().find(|existing| existing.id == task.id) {
        *existing = task;
    } else {
        tasks.push(task);
    }
    Ok(())
}

fn string_arg(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| format!("missing string argument: {key}"))
}

fn optional_string_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(ToString::to_string)
}

fn optional_u32_arg(args: &Value, key: &str) -> Option<u32> {
    args.get(key).and_then(Value::as_u64).and_then(|value| u32::try_from(value).ok())
}

fn slug(name: &str, fallback: &str) -> String {
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

fn percent_decode(value: &str) -> String {
    let mut output = Vec::new();
    let mut bytes = value.as_bytes().iter().copied();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let hi = bytes.next();
            let lo = bytes.next();
            if let (Some(hi), Some(lo)) = (hi, lo) {
                if let Ok(hex) = u8::from_str_radix(&format!("{}{}", hi as char, lo as char), 16) {
                    output.push(hex);
                    continue;
                }
            }
        }
        output.push(byte);
    }
    String::from_utf8_lossy(&output).to_string()
}

fn content_type_for_path(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()).unwrap_or_default().to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "bmp" => "image/bmp",
        _ => "application/octet-stream",
    }
}

struct HttpResponse {
    status: u16,
    content_type: String,
    body: Vec<u8>,
}

impl HttpResponse {
    fn json(status: u16, body: Value) -> Self {
        Self {
            status,
            content_type: "application/json; charset=utf-8".to_string(),
            body: serde_json::to_vec(&body).unwrap_or_else(|_| b"{\"ok\":false}".to_vec()),
        }
    }

    fn bytes(status: u16, content_type: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type: content_type.into(),
            body,
        }
    }

    fn empty(status: u16) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8".to_string(),
            body: Vec::new(),
        }
    }

    fn as_bytes(&self) -> Vec<u8> {
        let status_text = match self.status {
            200 => "OK",
            204 => "No Content",
            400 => "Bad Request",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "OK",
        };
        let mut response = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: content-type\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nConnection: close\r\n\r\n",
            self.status,
            status_text,
            self.content_type,
            self.body.len()
        )
        .into_bytes();
        response.extend_from_slice(&self.body);
        response
    }
}

#[allow(dead_code)]
fn _download_job_type_guard(_: DownloadJob, _: DatasetSnapshot) {}

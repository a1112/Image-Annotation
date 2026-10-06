use crate::{domain::{validate_annotation_bounds, validate_annotation_objects, AnnotationObject, BBox, Point, SampleRepository}, project_fs, storage};
use serde::Serialize;
use serde_json::{json, Value};
use std::{collections::{BTreeMap, HashMap, HashSet}, io::{BufRead, BufReader, Read, Write}, path::{Path, PathBuf}, process::{Command, Stdio}, sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex, OnceLock}, thread, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};

pub struct AiContext {
    pub image_path: PathBuf,
    pub image_id: String,
    pub width: u32,
    pub height: u32,
    pub classes: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AiRunResult {
    pub job_id: String,
    pub image_id: String,
    pub objects: Vec<AnnotationObject>,
}

static JOBS: OnceLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = OnceLock::new();
static CANCELED_EARLY: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
static PROGRESS: OnceLock<Mutex<HashMap<String, Arc<Mutex<Value>>>>> = OnceLock::new();
static WORKERS: OnceLock<Mutex<HashMap<String, Arc<Mutex<Option<PythonWorker>>>>>> = OnceLock::new();

struct PythonWorker {
    child: Arc<Mutex<std::process::Child>>,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    stderr: Option<thread::JoinHandle<Result<Vec<u8>, std::io::Error>>>,
}

impl PythonWorker {
    fn start(script: &str) -> Result<Self, String> {
        let python = std::env::var("IMAGE_ANNOTATION_PYTHON").unwrap_or_else(|_| "python".into());
        let mut command = Command::new(python);
        command.arg("-c").arg(script).arg("--server")
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)] {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command.spawn().map_err(|error| format!("start Python AI bridge: {error}"))?;
        let stdin = child.stdin.take().ok_or("AI stdin unavailable")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("AI stdout unavailable")?);
        let mut stderr = child.stderr.take().ok_or("AI stderr unavailable")?;
        let reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut chunk = [0_u8; 8192];
            loop {
                let read = stderr.read(&mut chunk)?;
                if read == 0 { break; }
                let keep = (64 * 1024_usize).saturating_sub(bytes.len()).min(read);
                bytes.extend_from_slice(&chunk[..keep]);
            }
            Ok(bytes)
        });
        Ok(Self { child: Arc::new(Mutex::new(child)), stdin, stdout, stderr: Some(reader) })
    }

    fn is_dead(&self) -> Result<bool, String> {
        Ok(self.child.lock().map_err(|error| error.to_string())?
            .try_wait().map_err(|error| error.to_string())?.is_some())
    }

    fn execute(&mut self, request: &Value, canceled: &Arc<AtomicBool>, progress: Arc<Mutex<Value>>) -> Result<Value, String> {
        if canceled.load(Ordering::SeqCst) { return Err("AI inference canceled".into()); }
        let payload = serde_json::to_vec(request).map_err(|error| error.to_string())?;
        self.stdin.write_all(&payload).map_err(|error| format!("write AI request: {error}"))?;
        self.stdin.write_all(b"\n").map_err(|error| format!("write AI request: {error}"))?;
        self.stdin.flush().map_err(|error| format!("flush AI request: {error}"))?;

        let done = Arc::new(AtomicBool::new(false));
        let monitor_done = done.clone();
        let monitor_child = self.child.clone();
        let monitor_canceled = canceled.clone();
        let start = Instant::now();
        let monitor = thread::spawn(move || {
            while !monitor_done.load(Ordering::SeqCst) {
                if monitor_canceled.load(Ordering::SeqCst) || start.elapsed() > Duration::from_secs(180) {
                    if let Ok(mut child) = monitor_child.lock() { let _ = child.kill(); }
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
        });

        let response = loop {
            let mut line = Vec::new();
            match self.stdout.read_until(b'\n', &mut line) {
                Ok(0) => break Err("AI bridge closed before returning a result".to_string()),
                Err(error) => break Err(format!("read AI response: {error}")),
                Ok(_) if line.len() > 32 * 1024 * 1024 => break Err("AI response exceeds 32 MiB".to_string()),
                Ok(_) => {
                    let value: Value = match serde_json::from_slice(&line) {
                        Ok(value) => value,
                        Err(error) => break Err(format!("AI response is malformed: {error}")),
                    };
                    if value.get("event").and_then(Value::as_str) == Some("progress") {
                        if let Ok(mut guard) = progress.lock() { *guard = value; }
                        continue;
                    }
                    break Ok(value);
                }
            }
        };
        done.store(true, Ordering::SeqCst);
        let _ = monitor.join();
        if canceled.load(Ordering::SeqCst) { return Err("AI inference canceled".into()); }
        if start.elapsed() > Duration::from_secs(180) { return Err("AI inference timed out after 180 seconds".into()); }
        response
    }
}

impl Drop for PythonWorker {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() { let _ = child.kill(); let _ = child.wait(); }
        if let Some(reader) = self.stderr.take() { let _ = reader.join(); }
    }
}

fn jobs() -> &'static Mutex<HashMap<String, Arc<AtomicBool>>> {
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn canceled_early() -> &'static Mutex<HashSet<String>> {
    CANCELED_EARLY.get_or_init(|| Mutex::new(HashSet::new()))
}

fn progress_registry() -> &'static Mutex<HashMap<String, Arc<Mutex<Value>>>> {
    PROGRESS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn status(job_id: &str) -> Result<Option<Value>, String> {
    let registry = progress_registry().lock().map_err(|err| err.to_string())?;
    registry.get(job_id).map(|value| value.lock().map(|guard| guard.clone()).map_err(|err| err.to_string())).transpose()
}

pub fn cancel(job_id: &str) -> Result<bool, String> {
    let jobs = jobs().lock().map_err(|err| err.to_string())?;
    if let Some(token) = jobs.get(job_id) {
        token.store(true, Ordering::SeqCst);
        Ok(true)
    } else {
        let mut early = canceled_early().lock().map_err(|err| err.to_string())?;
        if early.len() >= 512 { early.clear(); }
        early.insert(job_id.to_string());
        Ok(true)
    }
}

pub fn prepare(repository: &SampleRepository, project_id: &str, image_id: &str) -> Result<AiContext, String> {
    let image_path = repository.image_path(project_id, image_id).ok_or_else(|| format!("image not found: {image_id}"))?;
    let (width, height) = image::image_dimensions(&image_path).map_err(|err| err.to_string())?;
    let classes = storage::read_classes(&project_fs::project_paths(project_id).sqlite)?
        .into_iter().map(|class| class.label).collect::<Vec<_>>();
    if classes.is_empty() { return Err("project has no classes for AI results".into()); }
    Ok(AiContext { image_path, image_id: image_id.to_string(), width, height, classes })
}

fn pair(value: &Value, context: &AiContext) -> Result<Point, String> {
    let array = value.as_array().ok_or("AI point must be a pair")?;
    if array.len() != 2 { return Err("AI point must have two coordinates".into()); }
    let x = array[0].as_f64().ok_or("AI point x must be numeric")?;
    let y = array[1].as_f64().ok_or("AI point y must be numeric")?;
    if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 || x > context.width as f64 || y > context.height as f64 {
        return Err("AI point lies outside image bounds".into());
    }
    Ok(Point { x, y })
}

pub fn decode_shapes(context: &AiContext, response: &Value) -> Result<Vec<AnnotationObject>, String> {
    if response.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(response.get("error").and_then(Value::as_str).unwrap_or("AI bridge failed").to_string());
    }
    let shapes = response.get("shapes").and_then(Value::as_array).ok_or("AI response has no shapes array")?;
    if shapes.len() > 1000 { return Err("AI response has too many shapes".into()); }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|err| err.to_string())?.as_nanos();
    let mut objects = Vec::with_capacity(shapes.len());
    for (index, shape) in shapes.iter().enumerate() {
        let kind = shape.get("shape_type").and_then(Value::as_str).ok_or("AI shape has no type")?;
        let label = shape.get("label").and_then(Value::as_str)
            .or_else(|| context.classes.first().map(String::as_str)).ok_or("AI shape has no class")?;
        let class_id = context.classes.iter().position(|name| name == label)
            .ok_or_else(|| format!("AI class '{label}' is not in the project"))? as u32;
        let points = shape.get("points").and_then(Value::as_array).ok_or("AI shape has no points")?
            .iter().map(|item| pair(item, context)).collect::<Result<Vec<_>, _>>()?;
        let id = format!("ai-{now}-{index}");
        let mut object = match kind {
            "rectangle" => {
                if points.len() != 2 { return Err(format!("AI rectangle {index} needs two points")); }
                let bbox = BBox {
                    x: points[0].x.min(points[1].x), y: points[0].y.min(points[1].y),
                    width: (points[0].x - points[1].x).abs(), height: (points[0].y - points[1].y).abs(),
                };
                if bbox.width <= 0.0 || bbox.height <= 0.0 { return Err(format!("AI rectangle {index} is empty")); }
                AnnotationObject::bbox(id, class_id, label.to_string(), bbox)
            }
            "polygon" => {
                if points.len() < 3 { return Err(format!("AI polygon {index} needs three points")); }
                AnnotationObject::polygon(id, class_id, label.to_string(), points)
            }
            "mask" | "circle" | "oriented_rectangle" => {
                let expected = if kind == "oriented_rectangle" { 4 } else { 2 };
                if points.len() != expected { return Err(format!("AI {kind} {index} needs {expected} points")); }
                let mask_data = if kind == "mask" {
                    Some(shape.get("mask_data").and_then(Value::as_str)
                        .filter(|value| !value.is_empty())
                        .ok_or("AI mask has no PNG data")?.to_string())
                } else { None };
                AnnotationObject { id, class_id, label: label.to_string(), object_type: kind.to_string(),
                    bbox: None, polygon: None, points: Some(points), mask_data, attributes: BTreeMap::new() }
            }
            other => return Err(format!("unsupported AI shape type '{other}'")),
        };
        object.attributes.insert("source".into(), json!("ai"));
        if let Some(description) = shape.get("description").and_then(Value::as_str) {
            object.attributes.insert("description".into(), json!(description));
            if let Ok(metadata) = serde_json::from_str::<Value>(description) {
                if let Some(score) = metadata.get("score").and_then(Value::as_f64) {
                    if !score.is_finite() || !(0.0..=1.0).contains(&score) { return Err("AI confidence is outside [0,1]".into()); }
                    object.attributes.insert("confidence".into(), json!(score));
                }
            }
        }
        if let Some(flags) = shape.get("flags").and_then(Value::as_object) {
            object.attributes.insert("flags".into(), Value::Object(flags.clone()));
        }
        let duplicate = objects.iter().any(|existing: &AnnotationObject| {
            existing.class_id == object.class_id && existing.object_type == object.object_type
                && existing.bbox.as_ref().zip(object.bbox.as_ref()).map(|(a,b)|
                    (a.x-b.x).abs() < 1.0 && (a.y-b.y).abs() < 1.0 &&
                    (a.width-b.width).abs() < 1.0 && (a.height-b.height).abs() < 1.0).unwrap_or(false)
        });
        if !duplicate { objects.push(object); }
    }
    validate_annotation_objects(&objects)?;
    validate_annotation_bounds(&objects, context.width, context.height)?;
    Ok(objects)
}

fn bridge_script(provider: &str) -> Result<&'static str, String> {
    let source = match provider {
        "onnx" => include_str!("../../scripts/ai/onnx_detection_bridge.py"),
        "osam" => include_str!("../../scripts/ai/labelme_ai_bridge.py"),
        _ => return Err(format!("unsupported AI provider: {provider}")),
    };
    Ok(source)
}

fn run_process(script: &str, request: &Value, canceled: &AtomicBool, progress: Option<Arc<Mutex<Value>>>) -> Result<Value, String> {
    if canceled.load(Ordering::SeqCst) { return Err("AI inference canceled".into()); }
    let python = std::env::var("IMAGE_ANNOTATION_PYTHON").unwrap_or_else(|_| "python".into());
    let mut command = Command::new(python);
    command.arg("-c").arg(script).arg("--server").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)] {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn().map_err(|err| format!("start Python AI bridge: {err}"))?;
    let payload = serde_json::to_vec(request).map_err(|err| err.to_string())?;
    let mut stdin = child.stdin.take().ok_or("AI stdin unavailable")?;
    stdin.write_all(&payload).map_err(|err| err.to_string())?;
    stdin.write_all(b"\n").map_err(|err| err.to_string())?;
    drop(stdin);
    let stdout = child.stdout.take().ok_or("AI stdout unavailable")?;
    let mut stderr = child.stderr.take().ok_or("AI stderr unavailable")?;
    let out_reader = thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut last = Vec::new();
        loop {
            let mut line = Vec::new();
            if reader.read_until(b'\n', &mut line)? == 0 { break; }
            if line.len() > 32 * 1024 * 1024 { return Err(std::io::Error::other("AI response exceeds 32 MiB")); }
            let parsed = serde_json::from_slice::<Value>(&line).ok();
            if parsed.as_ref().and_then(|value| value.get("event")).and_then(Value::as_str) == Some("progress") {
                if let (Some(progress), Some(event)) = (&progress, parsed) {
                    if let Ok(mut guard) = progress.lock() { *guard = event; }
                }
            } else if !line.iter().all(u8::is_ascii_whitespace) {
                last = line;
            }
        }
        Ok(last)
    });
    let err_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 8192];
        loop {
            let read = stderr.read(&mut chunk)?;
            if read == 0 { break; }
            let keep = (64 * 1024_usize).saturating_sub(bytes.len()).min(read);
            bytes.extend_from_slice(&chunk[..keep]);
        }
        Ok::<_, std::io::Error>(bytes)
    });
    let start = Instant::now();
    let status = loop {
        if canceled.load(Ordering::SeqCst) {
            let _ = child.kill(); let _ = child.wait();
            let _ = out_reader.join(); let _ = err_reader.join();
            return Err("AI inference canceled".into());
        }
        if start.elapsed() > Duration::from_secs(180) {
            let _ = child.kill(); let _ = child.wait();
            let _ = out_reader.join(); let _ = err_reader.join();
            return Err("AI inference timed out after 180 seconds".into());
        }
        if let Some(status) = child.try_wait().map_err(|err| err.to_string())? { break status; }
        thread::sleep(Duration::from_millis(50));
    };
    let stdout = out_reader.join().map_err(|_| "AI stdout reader failed")?.map_err(|err| err.to_string())?;
    let stderr = err_reader.join().map_err(|_| "AI stderr reader failed")?.map_err(|err| err.to_string())?;
    if stdout.is_empty() { return Err(format!("AI bridge returned no JSON: {}", String::from_utf8_lossy(&stderr))); }
    let response: Value = serde_json::from_slice(&stdout).map_err(|err| format!("AI response is malformed: {err}"))?;
    if !status.success() && response.get("ok").and_then(Value::as_bool) != Some(false) {
        return Err(format!("AI bridge exited with {status}: {}", String::from_utf8_lossy(&stderr)));
    }
    Ok(response)
}

fn run_cached_process(provider: &str, script: &str, request: &Value, canceled: &Arc<AtomicBool>, progress: Arc<Mutex<Value>>) -> Result<Value, String> {
    let slot = {
        let mut workers = WORKERS.get_or_init(|| Mutex::new(HashMap::new()))
            .lock().map_err(|error| error.to_string())?;
        workers.entry(provider.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(None))).clone()
    };
    let mut worker = slot.lock().map_err(|error| error.to_string())?;
    if worker.as_ref().is_some_and(|active| active.is_dead().unwrap_or(true)) { worker.take(); }
    if worker.is_none() { *worker = Some(PythonWorker::start(script)?); }
    let result = worker.as_mut().unwrap().execute(request, canceled, progress);
    if result.is_err() { worker.take(); }
    result
}

pub fn run(context: AiContext, options: Value) -> Result<AiRunResult, String> {
    let job_id = options.get("jobId").and_then(Value::as_str).ok_or("AI jobId is required")?.to_string();
    if job_id.len() > 80 || !job_id.chars().all(|character| character.is_ascii_alphanumeric() || character == '-') {
        return Err("AI jobId must be an alphanumeric identifier".into());
    }
    let provider = options.get("provider").and_then(Value::as_str).ok_or("AI provider is required")?;
    let script = bridge_script(provider)?;
    let mut request = json!({"image_path": context.image_path, "classes": context.classes});
    if provider == "onnx" {
        let model_path = options.get("modelPath").and_then(Value::as_str).ok_or("select an ONNX model")?;
        if !Path::new(model_path).is_file() || !model_path.to_ascii_lowercase().ends_with(".onnx") {
            return Err("select an existing .onnx model file".into());
        }
        request["model_path"] = json!(model_path);
        request["operation"] = json!("infer");
        request["layout"] = options.get("layout").cloned().unwrap_or(json!("yolo8"));
        request["input_size"] = options.get("inputSize").cloned().unwrap_or(json!(640));
    } else {
        request["model"] = options.get("model").cloned().unwrap_or(json!("sam2:latest"));
        request["output_format"] = options.get("outputFormat").cloned().unwrap_or(json!("polygon"));
        request["prompt_type"] = options.get("promptType").cloned().unwrap_or(json!("points"));
        request["points"] = options.get("points").cloned().unwrap_or(json!([]));
        request["point_labels"] = options.get("pointLabels").cloned().unwrap_or(json!([]));
        request["texts"] = options.get("texts").cloned().unwrap_or(json!([]));
        if request["prompt_type"] == "text" {
            for text in request["texts"].as_array().ok_or("AI texts must be an array")? {
                let label = text.as_str().ok_or("AI text must be a string")?;
                if !context.classes.iter().any(|class| class == label) {
                    return Err(format!("AI text '{label}' is not a project class"));
                }
            }
        } else {
            let points = request["points"].as_array().ok_or("AI points must be an array")?;
            let labels = request["point_labels"].as_array().ok_or("AI pointLabels must be an array")?;
            if points.is_empty() || points.len() != labels.len() { return Err("AI points and labels must have the same non-zero length".into()); }
            for point in points { pair(point, &context)?; }
            if labels.iter().any(|label| !matches!(label.as_i64(), Some(0..=3))) { return Err("AI point label must be 0, 1, 2 or 3".into()); }
        }
    }
    request["score_threshold"] = options.get("scoreThreshold").cloned().unwrap_or(json!(0.25));
    request["iou_threshold"] = options.get("iouThreshold").cloned().unwrap_or(json!(0.45));
    for key in ["score_threshold", "iou_threshold"] {
        let value = request[key].as_f64().ok_or_else(|| format!("{key} must be numeric"))?;
        if !value.is_finite() || !(0.0..=1.0).contains(&value) { return Err(format!("{key} must be within [0,1]")); }
    }
    let token = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(Mutex::new(json!({"event":"progress","stage":"loading","percent":null})));
    {
        let mut registry = jobs().lock().map_err(|err| err.to_string())?;
        if registry.contains_key(&job_id) { return Err("duplicate AI jobId".into()); }
        if canceled_early().lock().map_err(|err| err.to_string())?.remove(&job_id) {
            token.store(true, Ordering::SeqCst);
        }
        registry.insert(job_id.clone(), token.clone());
    }
    progress_registry().lock().map_err(|err| err.to_string())?.insert(job_id.clone(), progress.clone());
    let result = run_cached_process(provider, script, &request, &token, progress).and_then(|mut response| {
        if provider == "osam" && request["prompt_type"] == "points" {
            let default_class = options.get("defaultClass").and_then(Value::as_str)
                .ok_or("choose a project class for AI prompts")?;
            if !context.classes.iter().any(|class| class == default_class) { return Err("AI prompt class is not in the project".into()); }
            if let Some(shapes) = response.get_mut("shapes").and_then(Value::as_array_mut) {
                for shape in shapes {
                    if shape.get("label").is_none() { shape["label"] = json!(default_class); }
                }
            }
        }
        decode_shapes(&context, &response)
    });
    jobs().lock().map_err(|err| err.to_string())?.remove(&job_id);
    progress_registry().lock().map_err(|err| err.to_string())?.remove(&job_id);
    Ok(AiRunResult { job_id, image_id: context.image_id, objects: result? })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_python_worker_reuses_process_and_cancels_a_busy_request() {
        if Command::new("python").arg("--version").output().is_err() { return; }
        let script = "import sys,json,time\ncount=0\nfor line in sys.stdin:\n count+=1\n request=json.loads(line)\n if request.get('slow'): time.sleep(10)\n print(json.dumps({'ok':True,'count':count}),flush=True)";
        let progress = || Arc::new(Mutex::new(json!({})));
        let token = Arc::new(AtomicBool::new(false));
        assert_eq!(run_cached_process("test-cache", script, &json!({}), &token, progress()).unwrap()["count"], 1);
        assert_eq!(run_cached_process("test-cache", script, &json!({}), &token, progress()).unwrap()["count"], 2);
        let canceled = Arc::new(AtomicBool::new(false));
        let signal = canceled.clone();
        let start = Instant::now();
        let worker = thread::spawn(move || run_cached_process("test-cache", script, &json!({"slow":true}), &signal, progress()));
        thread::sleep(Duration::from_millis(150));
        canceled.store(true, Ordering::SeqCst);
        assert!(worker.join().unwrap().unwrap_err().contains("canceled"));
        assert!(start.elapsed() < Duration::from_secs(3));
        assert_eq!(run_cached_process("test-cache", script, &json!({}), &token, progress()).unwrap()["count"], 1);
    }

    #[test]
    fn decodes_detection_with_confidence_and_rejects_wrong_class_or_bounds() {
        let context = AiContext {
            image_path: PathBuf::from("sample.png"), image_id: "sample".into(),
            width: 100, height: 80, classes: vec!["毛刺".into()],
        };
        let result = decode_shapes(&context, &json!({"ok":true,"shapes":[{
            "shape_type":"rectangle","label":"毛刺","points":[[10,20],[30,40]],
            "description":"{\"score\":0.83}"}]})).unwrap();
        assert_eq!(result[0].bbox.as_ref().unwrap().width, 20.0);
        assert_eq!(result[0].attributes["confidence"], json!(0.83));
        assert!(decode_shapes(&context, &json!({"ok":true,"shapes":[{
            "shape_type":"rectangle","label":"other","points":[[10,20],[30,40]]}]})).is_err());
        assert!(decode_shapes(&context, &json!({"ok":true,"shapes":[{
            "shape_type":"rectangle","label":"毛刺","points":[[10,20],[130,40]]}]})).is_err());
    }

    #[test]
    fn python_bridge_returns_json_and_can_be_canceled() {
        if Command::new("python").arg("--version").output().is_err() { return; }
        let token = AtomicBool::new(false);
        let progress = Arc::new(Mutex::new(json!({"stage":"loading"})));
        let response = run_process("import sys,json; sys.stdin.read(); print(json.dumps({'event':'progress','bytes_done':5,'bytes_total':10})); print(json.dumps({'ok':True,'shapes':[]}))", &json!({}), &token, Some(progress.clone())).unwrap();
        assert_eq!(response["ok"], true);
        assert_eq!(progress.lock().unwrap()["bytes_done"], 5);
        let cancellation = Arc::new(AtomicBool::new(false));
        let signal = cancellation.clone();
        let start = Instant::now();
        let worker = thread::spawn(move || run_process("import sys,time; sys.stdin.read(); time.sleep(10)", &json!({}), &signal, None));
        thread::sleep(Duration::from_millis(150));
        cancellation.store(true, Ordering::SeqCst);
        assert!(worker.join().unwrap().unwrap_err().contains("canceled"));
        assert!(start.elapsed() < Duration::from_secs(3));
    }
}

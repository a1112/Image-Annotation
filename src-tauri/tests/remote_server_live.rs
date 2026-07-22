use std::{
    error::Error,
    fs,
    io::Cursor,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use image::{DynamicImage, ImageBuffer, ImageFormat, Rgb};
use image_annotation_lib::remote_server::{serve, ServerConfig};
use reqwest::{header, Client, Method, StatusCode};
use serde_json::{json, Value};
use tokio::{net::TcpListener, sync::oneshot};

const READER_TOKEN: &str = "live-reader-token-0123456789abcdef0123456789abcdef";
const EDITOR_TOKEN: &str = "live-editor-token-0123456789abcdef0123456789abcdef";
const ADMIN_TOKEN: &str = "live-admin-token-0123456789abcdef0123456789abcdef";

#[tokio::test]
async fn remote_sample_server_supports_the_complete_tcp_workflow() -> TestResult {
    let data_dir = unique_temp_root();
    fs::create_dir_all(&data_dir)?;
    let cleanup = TempDataRoot(data_dir.clone());
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let address = listener.local_addr()?;
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(serve(listener, server_config(data_dir), async move {
        let _ = shutdown_rx.await;
    }));
    let client = Client::new();
    let base_url = format!("http://{address}/api/v1");

    let health = request(
        &client,
        Method::GET,
        &format!("{base_url}/health"),
        None,
        None,
    )
    .await?;
    assert_eq!(health.status, StatusCode::OK);
    assert_eq!(health.json["data"]["runtime"], "standalone");

    let unauthorized = request(
        &client,
        Method::GET,
        &format!("{base_url}/projects"),
        None,
        None,
    )
    .await?;
    assert_eq!(unauthorized.status, StatusCode::UNAUTHORIZED);

    let created = request(
        &client,
        Method::POST,
        &format!("{base_url}/projects"),
        Some(ADMIN_TOKEN),
        Some(json!({
            "name": "Live Remote Dataset",
            "datasetType": "yolo-detect"
        })),
    )
    .await?;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.json);
    let project_id = json_string(&created.json, "/data/id")?;

    let boundary = "image-annotation-live-boundary";
    let upload = client
        .post(format!("{base_url}/projects/{project_id}/imports"))
        .bearer_auth(EDITOR_TOKEN)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(dataset_multipart_body(
            boundary,
            &[
                ("images/train/sample.png", png_fixture()?.as_slice()),
                ("labels/train/sample.txt", b"0 0.5 0.5 0.5 0.5\n"),
                (
                    "data.yaml",
                    b"path: .\ntrain: images/train\nnames: [object, region]\n",
                ),
            ],
        ))
        .send()
        .await?;
    let upload_status = upload.status();
    let upload: Value = serde_json::from_slice(&upload.bytes().await?)?;
    assert_eq!(upload_status, StatusCode::CREATED, "{upload}");
    assert_eq!(upload["data"]["state"], "analyzed");
    assert_eq!(upload["data"]["detectedFormat"], "yolo-detect");
    assert!(!upload["data"]["tree"].as_array().unwrap().is_empty());
    let import_id = json_string(&upload, "/data/id")?;

    let committed = request(
        &client,
        Method::POST,
        &format!("{base_url}/imports/{import_id}/commit"),
        Some(EDITOR_TOKEN),
        Some(json!({"format": "yolo-detect"})),
    )
    .await?;
    assert_eq!(committed.status, StatusCode::OK, "{}", committed.json);
    assert_eq!(committed.json["data"]["state"], "completed");

    let samples = request(
        &client,
        Method::GET,
        &format!("{base_url}/projects/{project_id}/samples?classId=0"),
        Some(READER_TOKEN),
        None,
    )
    .await?;
    assert_eq!(samples.status, StatusCode::OK, "{}", samples.json);
    assert_eq!(samples.json["data"]["total"], 1);
    let sample_id = json_string(&samples.json, "/data/items/0/id")?;
    let sample_base = format!("{base_url}/projects/{project_id}/samples/{sample_id}");

    for suffix in ["content", "thumbnail"] {
        let response = client
            .get(format!("{sample_base}/{suffix}"))
            .bearer_auth(READER_TOKEN)
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::OK, "{suffix}");
        assert!(response.bytes().await?.len() > 16, "{suffix}");
    }

    let annotations_url = format!("{sample_base}/annotations");
    let initial = request(
        &client,
        Method::GET,
        &annotations_url,
        Some(READER_TOKEN),
        None,
    )
    .await?;
    assert_eq!(initial.status, StatusCode::OK, "{}", initial.json);

    let first = save_annotations(&client, &annotations_url, None, "live-box-1").await?;
    assert_eq!(first.status, StatusCode::OK, "{}", first.json);
    let first_revision = json_string(&first.json, "/data/revision")?;
    let old_class = request(
        &client,
        Method::GET,
        &format!("{base_url}/projects/{project_id}/samples?classId=0"),
        Some(READER_TOKEN),
        None,
    )
    .await?;
    assert_eq!(old_class.json["data"]["total"], 0, "{}", old_class.json);
    let new_class = request(
        &client,
        Method::GET,
        &format!("{base_url}/projects/{project_id}/samples?classId=1"),
        Some(READER_TOKEN),
        None,
    )
    .await?;
    assert_eq!(new_class.json["data"]["total"], 1, "{}", new_class.json);
    let second = save_annotations(
        &client,
        &annotations_url,
        Some(&first_revision),
        "live-box-2",
    )
    .await?;
    assert_eq!(second.status, StatusCode::OK, "{}", second.json);
    let stale = save_annotations(
        &client,
        &annotations_url,
        Some(&first_revision),
        "stale-box",
    )
    .await?;
    assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.json);
    assert_eq!(stale.json["error"]["code"], "revision_conflict");

    let submitted = request(
        &client,
        Method::POST,
        &format!("{sample_base}/submit"),
        Some(EDITOR_TOKEN),
        None,
    )
    .await?;
    assert_eq!(submitted.status, StatusCode::OK, "{}", submitted.json);
    let reviewed = request(
        &client,
        Method::POST,
        &format!("{sample_base}/review"),
        Some(EDITOR_TOKEN),
        Some(json!({"decision": "approved", "note": "live tcp review"})),
    )
    .await?;
    assert_eq!(reviewed.status, StatusCode::OK, "{}", reviewed.json);
    assert_eq!(reviewed.json["data"]["qaStatus"], "通过");

    let deleted_sample = request(
        &client,
        Method::DELETE,
        &sample_base,
        Some(ADMIN_TOKEN),
        None,
    )
    .await?;
    assert_eq!(
        deleted_sample.status,
        StatusCode::OK,
        "{}",
        deleted_sample.json
    );
    let restored_sample = request(
        &client,
        Method::POST,
        &format!("{sample_base}/restore"),
        Some(ADMIN_TOKEN),
        None,
    )
    .await?;
    assert_eq!(
        restored_sample.status,
        StatusCode::OK,
        "{}",
        restored_sample.json
    );

    let project_url = format!("{base_url}/projects/{project_id}");
    let deleted_project = request(
        &client,
        Method::DELETE,
        &project_url,
        Some(ADMIN_TOKEN),
        None,
    )
    .await?;
    assert_eq!(
        deleted_project.status,
        StatusCode::OK,
        "{}",
        deleted_project.json
    );
    let restored_project = request(
        &client,
        Method::POST,
        &format!("{project_url}/restore"),
        Some(ADMIN_TOKEN),
        None,
    )
    .await?;
    assert_eq!(
        restored_project.status,
        StatusCode::OK,
        "{}",
        restored_project.json
    );

    shutdown_tx.send(()).expect("live server should be running");
    server.await??;
    drop(cleanup);
    Ok(())
}

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

struct TempDataRoot(PathBuf);

impl Drop for TempDataRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct JsonResponse {
    status: StatusCode,
    json: Value,
}

async fn request(
    client: &Client,
    method: Method,
    url: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> TestResult<JsonResponse> {
    let mut request = client.request(method, url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if let Some(body) = body {
        request = request
            .header(header::CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&body)?);
    }
    let response = request.send().await?;
    let status = response.status();
    let json = serde_json::from_slice(&response.bytes().await?)?;
    Ok(JsonResponse { status, json })
}

async fn save_annotations(
    client: &Client,
    url: &str,
    revision: Option<&str>,
    object_id: &str,
) -> TestResult<JsonResponse> {
    let mut payload = json!({
        "objects": [{
            "id": object_id,
            "classId": 1,
            "label": "region",
            "type": "bbox",
            "bbox": {"x": 1.0, "y": 1.0, "width": 4.0, "height": 4.0},
            "attributes": {}
        }]
    });
    if let Some(revision) = revision {
        payload["revision"] = Value::String(revision.to_string());
    }
    request(client, Method::PUT, url, Some(EDITOR_TOKEN), Some(payload)).await
}

fn json_string(value: &Value, pointer: &str) -> TestResult<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("missing string at {pointer}: {value}").into())
}

fn dataset_multipart_body(boundary: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();
    for (path, bytes) in files {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"files\"; filename=\"{path}\"\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

fn png_fixture() -> TestResult<Vec<u8>> {
    let image = ImageBuffer::from_pixel(8, 8, Rgb([48, 112, 208]));
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image).write_to(&mut bytes, ImageFormat::Png)?;
    Ok(bytes.into_inner())
}

fn server_config(data_dir: PathBuf) -> ServerConfig {
    ServerConfig {
        bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        data_dir,
        reader_token: Some(READER_TOKEN.to_string()),
        editor_token: Some(EDITOR_TOKEN.to_string()),
        admin_token: Some(ADMIN_TOKEN.to_string()),
        allowed_origins: Vec::new(),
        max_upload_bytes: 8 * 1024 * 1024,
    }
}

fn unique_temp_root() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "image-annotation-live-{}-{nonce}",
        std::process::id()
    ))
}

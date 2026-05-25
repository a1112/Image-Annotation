use image_annotation_lib::{
    datasets::builtin_datasets,
    domain::{AnnotationObject, BBox},
    http_backend::{backend_bind_addr, backend_base_url, health_payload},
    importers::yolo::{annotations_to_yolo_lines, parse_yolo_bbox_line, parse_yolo_polygon_line},
    project_fs::{safe_extract_path, test_data_root, workspace_data_root},
    windows::{annotation_route, backend_tasks_route},
};
use serde_json::Value;
use std::path::Path;

#[test]
fn test_data_root_is_project_local_data_test_data() {
    let root = test_data_root();

    assert!(root.ends_with(Path::new("data").join("test_data")));
}

#[test]
fn workspace_data_root_is_project_local_default_workspace() {
    let root = workspace_data_root();

    assert!(root.ends_with(Path::new("data").join("workspaces").join("default")));
}

#[test]
fn tauri_asset_protocol_allows_project_test_data_images() {
    let config_path = test_data_root()
        .parent()
        .expect("data directory exists")
        .parent()
        .expect("workspace root exists")
        .join("src-tauri")
        .join("tauri.conf.json");
    let config: Value =
        serde_json::from_str(&std::fs::read_to_string(config_path).expect("tauri config exists"))
            .expect("tauri config is valid json");

    let asset_protocol = &config["app"]["security"]["assetProtocol"];
    assert_eq!(asset_protocol["enable"], true);
    assert!(asset_protocol["scope"]
        .as_array()
        .expect("asset scope is an array")
        .iter()
        .any(|entry| entry == "../data/test_data/**"));
    assert!(asset_protocol["scope"]
        .as_array()
        .expect("asset scope is an array")
        .iter()
        .any(|entry| entry == "../data/workspaces/**"));
}

#[test]
fn builtin_dataset_catalog_contains_small_ultralytics_sets() {
    let keys: Vec<_> = builtin_datasets()
        .into_iter()
        .map(|dataset| dataset.key)
        .collect();

    assert!(keys.contains(&"coco128".to_string()));
    assert!(keys.contains(&"coco8".to_string()));
    assert!(keys.contains(&"coco8-seg".to_string()));
    assert!(keys.contains(&"coco128-seg".to_string()));
}

#[test]
fn safe_extract_path_rejects_zip_slip_entries() {
    let root = Path::new("F:/project/Image-Annotation/data/test_data/projects/coco128");

    assert!(safe_extract_path(root, "images/train2017/000000000009.jpg").is_some());
    assert!(safe_extract_path(root, "../outside.txt").is_none());
    assert!(safe_extract_path(root, "nested/../../outside.txt").is_none());
}

#[test]
fn yolo_bbox_lines_convert_to_pixel_bbox() {
    let parsed = parse_yolo_bbox_line("2 0.5 0.25 0.2 0.1", 640, 480).unwrap();

    assert_eq!(parsed.class_id, 2);
    assert_eq!(parsed.bbox.x, 256.0);
    assert_eq!(parsed.bbox.y, 96.0);
    assert_eq!(parsed.bbox.width, 128.0);
    assert_eq!(parsed.bbox.height, 48.0);
}

#[test]
fn yolo_bbox_objects_write_normalized_detection_lines() {
    let objects = vec![AnnotationObject::bbox(
        "ann-1".to_string(),
        2,
        "car".to_string(),
        BBox {
            x: 256.0,
            y: 96.0,
            width: 128.0,
            height: 48.0,
        },
    )];

    let lines = annotations_to_yolo_lines(&objects, 640, 480).unwrap();

    assert_eq!(lines, "2 0.500000 0.250000 0.200000 0.100000\n");
}

#[test]
fn yolo_polygon_lines_convert_to_pixel_points() {
    let parsed = parse_yolo_polygon_line("4 0.1 0.2 0.5 0.2 0.5 0.8", 100, 200).unwrap();

    assert_eq!(parsed.class_id, 4);
    assert_eq!(parsed.polygon.len(), 3);
    assert_eq!(parsed.polygon[0].x, 10.0);
    assert_eq!(parsed.polygon[0].y, 40.0);
    assert_eq!(parsed.polygon[2].x, 50.0);
    assert_eq!(parsed.polygon[2].y, 160.0);
}

#[test]
fn annotation_window_route_uses_hash_router() {
    assert_eq!(
        annotation_route("coco128", Some("000000000009")),
        "#/annotate/coco128/000000000009"
    );
}

#[test]
fn annotation_window_reuse_script_jumps_to_requested_image() {
    let script = image_annotation_lib::windows::annotation_navigation_script(
        &annotation_route("coco128", Some("000000000009")),
    );

    assert!(script.contains("window.location.hash"));
    assert!(script.contains("#/annotate/coco128/000000000009"));
}

#[test]
fn backend_tasks_window_route_uses_hash_router() {
    assert_eq!(backend_tasks_route(), "#/backend-tasks");
}

#[test]
fn local_http_backend_uses_stable_loopback_contract() {
    assert_eq!(backend_bind_addr(), "127.0.0.1:17310");
    assert_eq!(backend_base_url(), "http://127.0.0.1:17310");
    assert_eq!(health_payload()["status"], "ok");
    assert_eq!(health_payload()["runtime"], "standalone-backend");
    assert!(!health_payload()["capabilities"]
        .as_array()
        .expect("capabilities array")
        .iter()
        .any(|item| item == "windows"));
}

#[test]
fn desktop_http_backend_health_exposes_window_capability() {
    let health = image_annotation_lib::http_backend::desktop_health_payload();

    assert_eq!(health["runtime"], "tauri-desktop");
    assert!(health["capabilities"]
        .as_array()
        .expect("capabilities array")
        .iter()
        .any(|item| item == "windows"));
}

//! Opt-in end-to-end check of the real Rust publisher and two SQLite clients.
//! The credentials file is read locally; tokens and passwords are never logged.
use image_annotation_lib::{hybrid, project_fs, publisher, storage, sync_engine};
use reqwest::blocking::Client;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::{fs, path::Path, time::Duration};
use uuid::Uuid;

fn sync(path: &Path, token: &str) {
    let result = sync_engine::sync_project(path, "native-acceptance", Some(token)).unwrap();
    assert_eq!(result.failed, 0, "native outbox had rejected operations");
}

fn save(path: &Path, objects: &str) {
    let current = storage::read_annotation_payload(path, "local-image").unwrap();
    storage::save_annotation_payload(
        path,
        "local-image",
        current.as_ref().map(|value| value.revision.as_str()),
        objects,
    )
    .unwrap();
}

#[test]
#[ignore = "requires IMAGE_ANNOTATION_TEST_CREDENTIALS and a deployed acceptance server"]
fn desktop_publish_and_two_native_clients_complete_the_offline_review_loop() {
    let credentials: Value = serde_json::from_slice(
        &fs::read(
            std::env::var("IMAGE_ANNOTATION_TEST_CREDENTIALS").expect("credentials file required"),
        )
        .unwrap(),
    )
    .unwrap();
    let base = std::env::var("IMAGE_ANNOTATION_TEST_SERVER").expect("server URL required");
    let base = base.trim_end_matches('/');
    let client = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();
    let session: Value = client.post(format!("{base}/api/v1/auth/login"))
        .json(&json!({"email":credentials["email"],"password":credentials["password"],"deviceId":Uuid::new_v4().to_string(),"deviceName":"Rust native acceptance","platform":"desktop"}))
        .send().unwrap().error_for_status().unwrap().json().unwrap();
    let token = session["accessToken"].as_str().unwrap();
    let root = std::env::temp_dir().join(format!("annotation-cloud-native-{}", Uuid::new_v4()));
    let paths = project_fs::ProjectPaths {
        raw: root.join("assets/original"),
        annotations: root.join("annotations/native"),
        exports: root.join("exports"),
        imports: root.join("imports"),
        snapshots: root.join("snapshots"),
        thumbnails: root.join("assets/thumbnails"),
        sqlite: root.join("project.sqlite"),
        manifest: root.join("project.json"),
        root: root.clone(),
    };
    fs::create_dir_all(&paths.raw).unwrap();
    image::RgbImage::from_pixel(8, 8, image::Rgb([32, 128, 220]))
        .save(paths.raw.join("native.png"))
        .unwrap();
    fs::copy(paths.raw.join("native.png"), paths.raw.join("unopened.png")).unwrap();
    fs::write(paths.raw.join("unopened.txt"), "0 0.5 0.5 0.5 0.5\n").unwrap();
    storage::initialize_project_database(&paths.sqlite).unwrap();
    let manifest = project_fs::ProjectManifest {
        id: "native-acceptance".into(),
        name: format!("原生离线协作验收 {}", Uuid::new_v4()),
        source_dataset_key: "native-acceptance".into(),
        format: "yolo-detect".into(),
        root_path: paths.root.to_string_lossy().into(),
        created_at: "0".into(),
        class_count: 1,
        image_count: 2,
    };
    storage::upsert_project_index(
        &paths.sqlite,
        &manifest,
        &[
            storage::StoredImage {
                id: "local-image".into(),
                file_name: "native.png".into(),
                width: 8,
                height: 8,
                split: "train".into(),
                status: "草稿".into(),
                qa_status: String::new(),
                review_note: None,
            },
            storage::StoredImage {
                id: "unopened-image".into(),
                file_name: "unopened.png".into(),
                width: 8,
                height: 8,
                split: "train".into(),
                status: "已标注".into(),
                qa_status: String::new(),
                review_note: None,
            },
        ],
        &[storage::StoredClass {
            id: 0,
            label: "目标".into(),
            color: "#2080dc".into(),
        }],
    )
    .unwrap();
    save(&paths.sqlite, "[]");
    Connection::open(&paths.sqlite).unwrap().execute("INSERT INTO images(id,file_name,width,height,split,status,deleted_at) VALUES('deleted-image','missing-deleted.png',8,8,'train','草稿','1')",[]).unwrap();
    // A terminal error from an earlier account/permission state must not
    // permanently prevent initialization, or block a later offline submit.
    let mut rejected_db = Connection::open(&paths.sqlite).unwrap();
    let tx = rejected_db.transaction().unwrap();
    let rejected = hybrid::enqueue_operation(&tx, "native-acceptance", "annotation", "unopened-image", "update", None, json!({"objects":[]})).unwrap();
    tx.execute("UPDATE sync_outbox SET status='rejected',attempted_at='1',error_message='previous permission denied' WHERE id=?1", [&rejected]).unwrap();
    tx.commit().unwrap();
    storage::submit_image_for_review(&paths.sqlite, "unopened-image").unwrap();
    let published = publisher::publish(
        &paths,
        "native-acceptance",
        base,
        &Uuid::new_v4().to_string(),
        "mirrored",
        "on_demand",
        Some(token),
    )
    .unwrap();
    assert!(published.created_remote_project);
    assert_eq!(published.uploaded_assets, 2);
    assert_eq!(published.sync.failed, 0);
    assert_eq!(published.conflicts, 0);
    assert_eq!(rejected_db.query_row("SELECT status FROM sync_outbox WHERE id=?1", [&rejected], |row| row.get::<_, String>(0)).unwrap(), "superseded");
    let remote = &published.remote_project_id;
    let repeated = publisher::publish(&paths, "native-acceptance", base, &Uuid::new_v4().to_string(), "mirrored", "on_demand", Some(token)).unwrap();
    assert!(!repeated.created_remote_project);
    assert_eq!(repeated.conflicts, 0, "re-publish must not enqueue redundant project updates");
    assert_eq!(repeated.sync.failed, 0);
    let initial: Value = client
        .get(format!("{base}/api/v1/projects/{remote}/sync-bootstrap"))
        .bearer_auth(token)
        .send()
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(
        initial["annotations"].as_array().unwrap().len(),
        2,
        "untouched source labels must publish too"
    );
    let imported = initial["annotations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| {
            a["objects"]
                .as_array()
                .is_some_and(|objects| !objects.is_empty())
        })
        .unwrap();
    assert_eq!(imported["objects"][0]["bbox"]["width"], 4.0);
    assert_eq!(
        imported["status"], "submitted",
        "imported source annotations must precede offline submission"
    );
    let task =
        storage::create_annotation_task_record(&paths.sqlite, "离线双设备任务", &["local-image"])
            .unwrap();
    sync(&paths.sqlite, token);
    assert_eq!(
        storage::list_task_item_records(&paths.sqlite, &task.id)
            .unwrap()
            .len(),
        1
    );
    let second = root.join("second.sqlite");
    fs::copy(&paths.sqlite, &second).unwrap();
    Connection::open(&second)
        .unwrap()
        .execute(
            "UPDATE remote_project_configs SET device_id=?1",
            [Uuid::new_v4().to_string()],
        )
        .unwrap();
    save(&paths.sqlite, r#"[{"id":"desktop-draft"}]"#);
    save(&second, r#"[{"id":"mobile-offline-draft"}]"#);
    sync(&paths.sqlite, token);
    let result = sync_engine::sync_project(&second, "native-acceptance", Some(token)).unwrap();
    assert!(result.conflicts > 0);
    let mut second_db = Connection::open(&second).unwrap();
    let conflict = hybrid::list_conflicts(&second_db, "native-acceptance")
        .unwrap()
        .remove(0);
    assert!(conflict.base.is_some_and(|base| base != Value::Null));
    hybrid::resolve_conflict(&mut second_db, "native-acceptance", &conflict.id, "local").unwrap();
    drop(second_db);
    sync(&second, token);
    // Consecutive offline saves and submit must preserve their causal order.
    save(&second, r#"[{"id":"offline-save-1"}]"#);
    save(&second, r#"[{"id":"offline-save-2"}]"#);
    storage::submit_image_for_review(&second, "local-image").unwrap();
    sync(&second, token);
    sync(&paths.sqlite, token);
    storage::review_image(&paths.sqlite, "local-image", "rejected", "调整边界后重提").unwrap();
    sync(&paths.sqlite, token);
    sync(&second, token);
    let snapshot: Value = client
        .get(format!("{base}/api/v1/projects/{remote}/sync-bootstrap"))
        .bearer_auth(token)
        .send()
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(
        snapshot["issues"].as_array().unwrap().len(),
        1,
        "review and local issue must share identity"
    );
    assert_eq!(snapshot["qaReviews"].as_array().unwrap().len(), 1);
    let db = Connection::open(&second).unwrap();
    let (objects,qa):(String,String) = db.query_row("SELECT a.object_json,i.qa_status FROM annotations a JOIN images i ON i.id=a.image_id WHERE i.id=?1",params!["local-image"],|row|Ok((row.get(0)?,row.get(1)?))).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&objects).unwrap()[0]["id"],
        "offline-save-2"
    );
    assert_eq!(qa, "驳回");
    assert_eq!(
        hybrid::sync_summary(&db, "native-acceptance")
            .unwrap()
            .failed_operations,
        0
    );
    drop(db);
    client
        .delete(format!("{base}/api/v1/projects/{remote}"))
        .bearer_auth(token)
        .send()
        .unwrap()
        .error_for_status()
        .unwrap();
    sync(&paths.sqlite, token);
    sync(&second, token);
    let deleted: Option<String> = Connection::open(&second)
        .unwrap()
        .query_row(
            "SELECT deleted_at FROM projects WHERE id='native-acceptance'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(deleted.is_some());
    let _ = client
        .post(format!("{base}/api/v1/auth/logout"))
        .bearer_auth(token)
        .json(&json!({"refreshToken":session["refreshToken"]}))
        .send();
    fs::remove_dir_all(root).unwrap();
}

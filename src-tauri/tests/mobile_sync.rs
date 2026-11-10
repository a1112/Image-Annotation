use image_annotation_lib::{asset_cache, hybrid, storage};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::path::PathBuf;

struct Fixture {
    root: PathBuf,
    db: Connection,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "annotation-mobile-sync-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("project.sqlite");
        storage::initialize_project_database(&path).unwrap();
        let mut db = Connection::open(path).unwrap();
        db.execute("INSERT INTO projects(id,name,source_dataset_key,format,created_at) VALUES('p','Project','remote','native','0')",[]).unwrap();
        hybrid::apply_remote_snapshot(&mut db,"p",vec![
            ("asset".into(),"remote-image".into(),Some(1),json!({"id":"remote-image","clientKey":"local-image","fileName":"sample.png","width":20,"height":20})),
            ("annotation".into(),"remote-annotation".into(),Some(1),json!({"id":"remote-annotation","imageId":"remote-image","revision":1,"objects":[{"id":"base"}],"status":"draft"})),
        ],"1").unwrap();
        Self { root, db }
    }
    fn save(&mut self, objects: Value) -> String {
        let tx = self.db.transaction().unwrap();
        tx.execute(
            "UPDATE annotations SET object_json=?1 WHERE id='local-image'",
            [objects.to_string()],
        )
        .unwrap();
        let id = hybrid::mark_annotation_pending(
            &tx,
            &self.root.join("project.sqlite"),
            "local-image",
            None,
            json!({"imageId":"local-image","objects":objects}),
        )
        .unwrap();
        tx.commit().unwrap();
        id
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn never_sent_drafts_coalesce_but_attempted_operations_remain_immutable() {
    let mut f = Fixture::new();
    let first = f.save(json!([{"id":"first"}]));
    let second = f.save(json!([{"id":"second"}]));
    assert_eq!(first, second);
    let ready = hybrid::list_ready_outbox(&f.db, "p", 100).unwrap();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].payload["objects"][0]["id"], "second");
    f.db.execute(
        "UPDATE sync_outbox SET attempted_at='1',status='syncing' WHERE id=?1",
        [&first],
    )
    .unwrap();
    let third = f.save(json!([{"id":"third"}]));
    assert_ne!(first, third);
    assert!(hybrid::list_ready_outbox(&f.db, "p", 100)
        .unwrap()
        .is_empty());
    let original: String =
        f.db.query_row(
            "SELECT payload_json FROM sync_outbox WHERE id=?1",
            [first],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&original).unwrap()["objects"][0]["id"],
        "second"
    );
}

#[test]
fn acknowledgement_rebases_successor_and_does_not_erase_newer_draft() {
    let mut f = Fixture::new();
    let first = f.save(json!([{"id":"first"}]));
    let operation = hybrid::list_ready_outbox(&f.db, "p", 100)
        .unwrap()
        .remove(0);
    f.db.execute(
        "UPDATE sync_outbox SET attempted_at='1',status='syncing' WHERE id=?1",
        [first],
    )
    .unwrap();
    f.save(json!([{"id":"newer"}]));
    let accepted = json!({"id":"remote-annotation","imageId":"remote-image","revision":2,"objects":[{"id":"first"}],"status":"draft"});
    hybrid::mark_outbox_applied(&mut f.db, &operation, Some(2), Some(&accepted)).unwrap();
    let ready = hybrid::list_ready_outbox(&f.db, "p", 100).unwrap();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].base_revision, Some(2));
    hybrid::apply_remote_change(
        &mut f.db,
        "p",
        "annotation",
        "remote-annotation",
        "update",
        Some(2),
        &accepted,
    )
    .unwrap();
    let objects: String =
        f.db.query_row(
            "SELECT object_json FROM annotations WHERE id='local-image'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&objects).unwrap()[0]["id"],
        "newer"
    );
    assert!(hybrid::list_conflicts(&f.db, "p").unwrap().is_empty());
}

#[test]
fn submission_waits_for_draft_confirmation() {
    let mut f = Fixture::new();
    f.save(json!([{"id":"draft"}]));
    let tx = f.db.transaction().unwrap();
    hybrid::mark_submission_pending(&tx, &f.root.join("project.sqlite"), "local-image").unwrap();
    tx.commit().unwrap();
    let ready = hybrid::list_ready_outbox(&f.db, "p", 100).unwrap();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].operation, "update");
    hybrid::mark_outbox_applied(&mut f.db,&ready[0],Some(2),Some(&json!({"id":"remote-annotation","imageId":"remote-image","revision":2,"objects":[{"id":"draft"}]}))).unwrap();
    let ready = hybrid::list_ready_outbox(&f.db, "p", 100).unwrap();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].operation, "submit");
    assert_eq!(ready[0].base_revision, Some(2));
}

#[test]
fn remote_uuid_maps_to_dirty_local_entity_and_preserves_three_way_conflict() {
    let mut f = Fixture::new();
    f.save(json!([{"id":"local"}]));
    let remote = json!({"id":"remote-annotation","imageId":"remote-image","revision":3,"objects":[{"id":"remote"}]});
    for _ in 0..2 {
        hybrid::apply_remote_change(
            &mut f.db,
            "p",
            "annotation",
            "remote-annotation",
            "update",
            Some(3),
            &remote,
        )
        .unwrap();
    }
    let conflicts = hybrid::list_conflicts(&f.db, "p").unwrap();
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].entity_id, "local-image");
    assert_eq!(
        conflicts[0].base.as_ref().unwrap()["objects"][0]["id"],
        "base"
    );
    assert_eq!(conflicts[0].local["objects"][0]["id"], "local");
    assert_eq!(conflicts[0].remote["objects"][0]["id"], "remote");
    hybrid::resolve_conflict(&mut f.db, "p", &conflicts[0].id, "local").unwrap();
    let ready = hybrid::list_ready_outbox(&f.db, "p", 100).unwrap();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].base_revision, Some(3));
    assert_eq!(ready[0].payload["objects"][0]["id"], "local");
}

#[test]
fn change_page_and_cursor_roll_back_together() {
    let mut f = Fixture::new();
    let result = hybrid::apply_remote_page(
        &mut f.db,
        "p",
        vec![
            (
                "asset".into(),
                "new-image".into(),
                "update".into(),
                Some(1),
                json!({"id":"new-image","fileName":"new.png"}),
            ),
            (
                "task_item".into(),
                "bad-item".into(),
                "update".into(),
                Some(1),
                json!({"id":"bad-item","taskId":"task"}),
            ),
        ],
        "9",
    );
    assert!(result.is_err());
    assert_eq!(hybrid::current_pull_cursor(&f.db, "p").unwrap(), "1");
    assert_eq!(
        f.db.query_row(
            "SELECT COUNT(*) FROM images WHERE id='new-image'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

#[test]
fn choosing_remote_cancels_stale_pending_writes() {
    let mut f = Fixture::new();
    f.save(json!([{"id":"local"}]));
    let remote = json!({"id":"remote-annotation","imageId":"remote-image","revision":4,"objects":[{"id":"remote"}]});
    hybrid::apply_remote_change(
        &mut f.db,
        "p",
        "annotation",
        "remote-annotation",
        "update",
        Some(4),
        &remote,
    )
    .unwrap();
    let conflict = hybrid::list_conflicts(&f.db, "p").unwrap().remove(0);
    hybrid::resolve_conflict(&mut f.db, "p", &conflict.id, "remote").unwrap();
    assert!(hybrid::list_ready_outbox(&f.db, "p", 100)
        .unwrap()
        .is_empty());
    let objects: String =
        f.db.query_row(
            "SELECT object_json FROM annotations WHERE id='local-image'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&objects).unwrap()[0]["id"],
        "remote"
    );
}

#[test]
fn unsynchronized_operations_protect_originals_from_cache_cleanup() {
    let mut f = Fixture::new();
    let path = f.root.join("original.bin");
    std::fs::write(&path, b"draft-image").unwrap();
    let record = asset_cache::CacheFetchResult {
        path: path.clone(),
        byte_size: 11,
        content_hash: "a".repeat(64),
        cache_hit: false,
    };
    asset_cache::register(&f.db, "local-image", &record, false).unwrap();
    let tx = f.db.transaction().unwrap();
    hybrid::enqueue_operation(
        &tx,
        "p",
        "annotation",
        "local-image",
        "update",
        Some(1),
        json!({"objects":[]}),
    )
    .unwrap();
    tx.commit().unwrap();
    let cleaned = asset_cache::cleanup_registered(&mut f.db, 0).unwrap();
    assert_eq!(cleaned.removed_count, 0);
    assert!(path.exists());
    f.db.execute("UPDATE sync_outbox SET status='synced'", params![])
        .unwrap();
    let cleaned = asset_cache::cleanup_registered(&mut f.db, 0).unwrap();
    assert_eq!(cleaned.removed_count, 1);
    assert!(!path.exists());
}

#[test]
fn rejected_operations_remain_visible_without_automatic_retry() {
    let mut f = Fixture::new();
    f.save(json!([{ "id": "local-review" }]));
    let operation = hybrid::list_ready_outbox(&f.db, "p", 100)
        .unwrap()
        .remove(0);
    hybrid::mark_outbox_failed(&f.db, &operation, "角色无权修改此标注", false).unwrap();
    let summary = hybrid::sync_summary(&f.db, "p").unwrap();
    assert_eq!(summary.failed_operations, 1);
    assert_eq!(summary.pending_operations, 0);
    assert!(hybrid::list_ready_outbox(&f.db, "p", 100)
        .unwrap()
        .is_empty());
}

#[test]
fn opening_a_remote_cache_does_not_enqueue_project_admin_writes() {
    let mut f = Fixture::new();
    hybrid::cache_remote_project(
        &mut f.db,
        "p",
        "https://example.test",
        "remote-p",
        "device",
        "cloud_linked",
        "on_demand",
        true,
    )
    .unwrap();
    assert!(hybrid::list_ready_outbox(&f.db, "p", 100)
        .unwrap()
        .is_empty());
    assert_eq!(
        hybrid::sync_summary(&f.db, "p").unwrap().pending_operations,
        0
    );
    hybrid::configure_remote_project(
        &mut f.db,
        "p",
        "https://example.test",
        "remote-p",
        "device",
        "mirrored",
        "full_mirror",
        true,
    )
    .unwrap();
    assert_eq!(
        hybrid::list_ready_outbox(&f.db, "p", 100).unwrap()[0].entity_type,
        "project"
    );
}

#[test]
fn task_creation_is_durable_and_remote_items_reuse_the_desktop_rows() {
    let mut f = Fixture::new();
    let task = storage::create_annotation_task_record(
        &f.root.join("project.sqlite"),
        "Shared task",
        &["local-image"],
    )
    .unwrap();
    let ready = hybrid::list_ready_outbox(&f.db, "p", 100).unwrap();
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].entity_type, "task");
    assert_eq!(ready[0].payload["imageIds"][0], "local-image");
    hybrid::mark_outbox_applied(
        &mut f.db,
        &ready[0],
        Some(1),
        Some(&json!({"id":"remote-task","name":"Shared task","revision":1})),
    )
    .unwrap();
    hybrid::apply_remote_change(&mut f.db,"p","task_item","remote-task-item","update",Some(1),&json!({"id":"remote-task-item","taskId":"remote-task","imageId":"remote-image","status":"assigned"})).unwrap();
    let items = storage::list_task_item_records(&f.root.join("project.sqlite"), &task.id).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].image_id, "local-image");
    let remote: String =
        f.db.query_row(
            "SELECT remote_id FROM task_items WHERE task_id=?1",
            [task.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remote, "remote-task-item");
}

#[test]
fn review_and_its_rejection_issue_share_one_client_identity() {
    let mut f = Fixture::new();
    let path = f.root.join("project.sqlite");
    let tx = f.db.transaction().unwrap();
    let issue = hybrid::record_review_operation(&tx, &path, "local-image", "rejected", "Fix this")
        .unwrap()
        .unwrap();
    tx.commit().unwrap();
    let operations = hybrid::list_ready_outbox(&f.db, "p", 100).unwrap();
    assert_eq!(operations.len(), 2);
    let review = operations
        .iter()
        .find(|item| item.entity_type == "qa_review")
        .unwrap();
    let create = operations
        .iter()
        .find(|item| item.entity_type == "issue")
        .unwrap();
    assert_eq!(review.payload["issueClientKey"], issue);
    assert_eq!(create.entity_id, issue);
}

#[test]
fn project_deletion_keeps_dirty_work_until_explicit_resolution() {
    let mut f = Fixture::new();
    f.save(json!([{"id":"precious-draft"}]));
    hybrid::apply_remote_change(
        &mut f.db,
        "p",
        "project",
        "remote-project",
        "delete",
        Some(2),
        &json!({"deletedAt":"now"}),
    )
    .unwrap();
    let deleted: Option<String> =
        f.db.query_row("SELECT deleted_at FROM projects WHERE id='p'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(deleted.is_none());
    let conflict = hybrid::list_conflicts(&f.db, "p")
        .unwrap()
        .into_iter()
        .find(|item| item.entity_type == "project")
        .unwrap();
    hybrid::resolve_conflict(&mut f.db, "p", &conflict.id, "local").unwrap();
    let mode: String =
        f.db.query_row("SELECT mode FROM projects WHERE id='p'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(mode, "local_only");
    let objects: String =
        f.db.query_row(
            "SELECT object_json FROM annotations WHERE id='local-image'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&objects).unwrap()[0]["id"],
        "precious-draft"
    );
    assert!(hybrid::list_ready_outbox(&f.db, "p", 100)
        .unwrap()
        .is_empty());
}

#[test]
fn metadata_echo_does_not_conflict_but_remote_asset_deletion_preserves_drafts() {
    let mut f = Fixture::new();
    f.save(json!([{"id":"local"}]));
    let asset = json!({"id":"remote-image","clientKey":"local-image","fileName":"sample.png","width":20,"height":20});
    hybrid::apply_remote_change(
        &mut f.db,
        "p",
        "asset",
        "remote-image",
        "update",
        Some(2),
        &asset,
    )
    .unwrap();
    assert!(hybrid::list_conflicts(&f.db, "p").unwrap().is_empty());
    hybrid::apply_remote_change(
        &mut f.db,
        "p",
        "asset",
        "remote-image",
        "delete",
        Some(3),
        &asset,
    )
    .unwrap();
    assert_eq!(hybrid::list_conflicts(&f.db, "p").unwrap().len(), 1);
    assert_eq!(
        storage::read_images(&f.root.join("project.sqlite"), None)
            .unwrap()
            .len(),
        1
    );
}

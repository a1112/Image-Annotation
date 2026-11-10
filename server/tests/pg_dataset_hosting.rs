//! Opt-in PostgreSQL coverage for dataset hosting invariants.
//!
//! These checks deliberately use an isolated database only. They exercise the
//! database-side quota lock and immutable manifest contract directly, which
//! is useful even when the HTTP/S3 fixture is not running.

use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, PgPool, Row};
use uuid::Uuid;

fn enabled() -> bool {
    std::env::var("RUN_PG_INTEGRATION").ok().as_deref() == Some("1")
}

async fn pool() -> Option<PgPool> {
    if !enabled() {
        return None;
    }
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL is required when integration is enabled");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .expect("connect to isolated PostgreSQL integration database");
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .expect("read integration database name");
    assert!(
        database.contains("_staging"),
        "refusing dataset hosting integration against non-staging database"
    );
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("apply server migrations");
    Some(pool)
}

#[derive(Debug, Clone, Copy)]
struct Fixture {
    organization_id: Uuid,
    user_id: Uuid,
    dataset_id: Uuid,
}

async fn fixture(pool: &PgPool) -> Fixture {
    let suffix = Uuid::new_v4();
    let organization_id: Uuid =
        sqlx::query_scalar("INSERT INTO organizations (name) VALUES ($1) RETURNING id")
            .bind(format!("dataset-hosting-{suffix}"))
            .fetch_one(pool)
            .await
            .expect("create integration organization");
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, organization_id, display_name, email, status) VALUES ($1, $2, $3, $4, 'active')",
    )
    .bind(user_id)
    .bind(organization_id)
    .bind("dataset integration")
    .bind(format!("dataset-{suffix}@example.invalid"))
    .execute(pool)
    .await
    .expect("create integration user");
    sqlx::query("UPDATE organizations SET owner_id = $2 WHERE id = $1")
        .bind(organization_id)
        .bind(user_id)
        .execute(pool)
        .await
        .expect("set integration organization owner");
    let dataset_id: Uuid = sqlx::query_scalar(
        "INSERT INTO datasets (organization_id, owner_id, name) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(organization_id)
    .bind(user_id)
    .bind(format!("dataset-{suffix}"))
    .fetch_one(pool)
    .await
    .expect("create integration dataset");
    sqlx::query("INSERT INTO dataset_members (dataset_id, user_id, role) VALUES ($1, $2, 'owner')")
        .bind(dataset_id)
        .bind(user_id)
        .execute(pool)
        .await
        .expect("create dataset owner membership");
    sqlx::query(
        "INSERT INTO organization_quotas (organization_id) VALUES ($1) ON CONFLICT (organization_id) DO NOTHING",
    )
    .bind(organization_id)
    .execute(pool)
    .await
    .expect("create integration quota");
    sqlx::query(
        "UPDATE organization_quotas SET limit_bytes = 1024, max_assets = 10, used_bytes = 0, used_assets = 0, reserved_bytes = 0, reserved_assets = 0 WHERE organization_id = $1",
    )
    .bind(organization_id)
    .execute(pool)
    .await
    .expect("set small integration quota");
    Fixture {
        organization_id,
        user_id,
        dataset_id,
    }
}

async fn cleanup(pool: &PgPool, fixture: Fixture) {
    sqlx::query("DELETE FROM datasets WHERE id = $1")
        .bind(fixture.dataset_id)
        .execute(pool)
        .await
        .expect("clean integration dataset");
    sqlx::query("DELETE FROM dataset_blobs WHERE organization_id = $1")
        .bind(fixture.organization_id)
        .execute(pool)
        .await
        .expect("clean dataset blobs");
    sqlx::query("UPDATE organizations SET owner_id = NULL WHERE id = $1")
        .bind(fixture.organization_id)
        .execute(pool)
        .await
        .expect("clear integration organization owner");
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(fixture.user_id)
        .execute(pool)
        .await
        .expect("clean integration user");
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(fixture.organization_id)
        .execute(pool)
        .await
        .expect("clean integration organization");
}

#[tokio::test]
async fn dataset_job_quota_has_one_concurrent_winner() {
    let Some(pool) = pool().await else { return };
    let fixture = fixture(&pool).await;
    let first_pool = pool.clone();
    let second_pool = pool.clone();
    let first_op = Uuid::new_v4().to_string();
    let second_op = Uuid::new_v4().to_string();
    let first = tokio::spawn(async move {
        let mut tx = first_pool.begin().await?;
        let reservation: Option<Uuid> =
            sqlx::query_scalar("SELECT reserve_dataset_job_quota($1, $2, 1024, $3)")
                .bind(fixture.dataset_id)
                .bind(fixture.user_id)
                .bind(first_op)
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok::<Option<Uuid>, sqlx::Error>(reservation)
    });
    let second = tokio::spawn(async move {
        let mut tx = second_pool.begin().await?;
        let reservation: Option<Uuid> =
            sqlx::query_scalar("SELECT reserve_dataset_job_quota($1, $2, 1024, $3)")
                .bind(fixture.dataset_id)
                .bind(fixture.user_id)
                .bind(second_op)
                .fetch_one(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok::<Option<Uuid>, sqlx::Error>(reservation)
    });
    let first = first.await.expect("first quota task did not panic");
    let second = second.await.expect("second quota task did not panic");
    let reservations = [first, second]
        .into_iter()
        .filter_map(|result| result.ok().flatten())
        .collect::<Vec<_>>();
    assert_eq!(
        reservations.len(),
        1,
        "exactly one concurrent reservation may consume the quota"
    );
    sqlx::query("SELECT release_dataset_job_quota($1)")
        .bind(reservations[0])
        .execute(&pool)
        .await
        .expect("release winning quota reservation");
    let quota = sqlx::query(
        "SELECT used_bytes, reserved_bytes, used_assets, reserved_assets FROM organization_quotas WHERE organization_id = $1",
    )
    .bind(fixture.organization_id)
    .fetch_one(&pool)
    .await
    .expect("read quota after release");
    assert_eq!(quota.get::<i64, _>("used_bytes"), 0);
    assert_eq!(quota.get::<i64, _>("reserved_bytes"), 0);
    cleanup(&pool, fixture).await;
}

#[tokio::test]
async fn dataset_version_manifest_is_immutable_and_annotations_are_independent() {
    let Some(pool) = pool().await else { return };
    let fixture = fixture(&pool).await;
    let blob_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO dataset_blobs (id, organization_id, object_key, file_name, content_hash, mime_type, width, height, byte_size) VALUES ($1, $2, $3, 'frame.png', repeat('b', 64), 'image/png', 4, 4, 16)",
    )
    .bind(blob_id)
    .bind(fixture.organization_id)
    .bind(format!(
        "organizations/{}/dataset-blobs/{}",
        fixture.organization_id, blob_id
    ))
    .execute(&pool)
    .await
    .expect("insert dataset blob");
    let initial_annotation = json!([{
        "id": "box-1",
        "type": "bbox",
        "classId": 0,
        "label": "cat",
        "attributes": {},
        "bbox": {"x": 1, "y": 1, "width": 2, "height": 2}
    }]);
    let second_annotation = json!([{
        "id": "box-2",
        "type": "bbox",
        "classId": 1,
        "label": "dog",
        "attributes": {},
        "bbox": {"x": 0, "y": 0, "width": 3, "height": 3}
    }]);
    let first_item: Uuid = sqlx::query_scalar(
        "INSERT INTO dataset_items (dataset_id, blob_id, client_key, file_name, annotation_json, annotation_hash) VALUES ($1, $2, 'source-a', 'frame.png', $3, 'hash-a') RETURNING id",
    )
    .bind(fixture.dataset_id)
    .bind(blob_id)
    .bind(&initial_annotation)
    .fetch_one(&pool)
    .await
    .expect("insert first dataset item");
    sqlx::query(
        "INSERT INTO dataset_items (dataset_id, blob_id, client_key, file_name, annotation_json, annotation_hash) VALUES ($1, $2, 'source-b', 'frame-copy.png', $3, 'hash-b')",
    )
    .bind(fixture.dataset_id)
    .bind(blob_id)
    .bind(&second_annotation)
    .execute(&pool)
    .await
    .expect("insert independent annotation item");
    let manifest = json!({
        "datasetId": fixture.dataset_id,
        "schema": {"classes": ["cat", "dog"]},
        "items": [{"id": first_item, "clientKey": "source-a", "annotation": initial_annotation}]
    });
    let version_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dataset_versions (dataset_id, version_number, name, manifest_json, manifest_hash, manifest_byte_size, item_count, created_by) VALUES ($1, 1, 'v1', $2, repeat('c', 64), 1, 1, $3) RETURNING id",
    )
    .bind(fixture.dataset_id)
    .bind(&manifest)
    .bind(fixture.user_id)
    .fetch_one(&pool)
    .await
    .expect("insert immutable dataset version");
    sqlx::query(
        "UPDATE dataset_items SET annotation_json = $2, annotation_hash = 'hash-new', revision = revision + 1 WHERE id = $1",
    )
    .bind(first_item)
    .bind(&second_annotation)
    .execute(&pool)
    .await
    .expect("update working dataset annotation");
    let stored_manifest: Value =
        sqlx::query_scalar("SELECT manifest_json FROM dataset_versions WHERE id = $1")
            .bind(version_id)
            .fetch_one(&pool)
            .await
            .expect("read immutable version manifest");
    assert_eq!(stored_manifest, manifest);
    let counts = sqlx::query(
        "SELECT COUNT(*) AS items, COUNT(DISTINCT blob_id) AS blobs FROM dataset_items WHERE dataset_id = $1",
    )
    .bind(fixture.dataset_id)
    .fetch_one(&pool)
    .await
    .expect("count independent dataset items");
    assert_eq!(counts.get::<i64, _>("items"), 2);
    assert_eq!(counts.get::<i64, _>("blobs"), 1);
    cleanup(&pool, fixture).await;
}

#[tokio::test]
async fn dataset_acl_does_not_cross_organization_boundary() {
    let Some(pool) = pool().await else { return };
    let fixture = fixture(&pool).await;
    let suffix = Uuid::new_v4();
    let other_org: Uuid =
        sqlx::query_scalar("INSERT INTO organizations (name) VALUES ($1) RETURNING id")
            .bind(format!("other-org-{suffix}"))
            .fetch_one(&pool)
            .await
            .expect("create second organization");
    let other_user = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, organization_id, display_name, email, status) VALUES ($1, $2, 'other', $3, 'active')",
    )
    .bind(other_user)
    .bind(other_org)
    .bind(format!("other-{suffix}@example.invalid"))
    .execute(&pool)
    .await
    .expect("create second organization user");
    let visible: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM dataset_members dm JOIN datasets d ON d.id = dm.dataset_id JOIN users u ON u.id = dm.user_id WHERE dm.dataset_id = $1 AND dm.user_id = $2 AND dm.deleted_at IS NULL AND d.deleted_at IS NULL AND d.organization_id = u.organization_id)",
    )
    .bind(fixture.dataset_id)
    .bind(other_user)
    .fetch_one(&pool)
    .await
    .expect("evaluate cross-organization dataset ACL");
    assert!(!visible, "dataset membership must be organization scoped");
    sqlx::query("UPDATE organizations SET owner_id = NULL WHERE id = $1")
        .bind(other_org)
        .execute(&pool)
        .await
        .expect("clear second organization owner");
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(other_user)
        .execute(&pool)
        .await
        .expect("clean second organization user");
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(other_org)
        .execute(&pool)
        .await
        .expect("clean second organization");
    cleanup(&pool, fixture).await;
}

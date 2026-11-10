//! Opt-in PostgreSQL invariants for image-transform tasks.
//!
//! The black-box S3/HTTP flow is covered by
//! `scripts/verify-image-transforms.py`; this test keeps the durable task
//! contract checked against an isolated staging database without requiring a
//! second HTTP client dependency in the server crate.

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
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect to isolated PostgreSQL integration database");
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .expect("read integration database name");
    assert!(
        database.contains("_staging"),
        "refusing image-transform integration against non-staging database"
    );
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("apply server migrations");
    Some(pool)
}

async fn fixture(pool: &PgPool) -> (Uuid, Uuid, Uuid) {
    let suffix = Uuid::new_v4();
    let organization_id: Uuid =
        sqlx::query_scalar("INSERT INTO organizations (name) VALUES ($1) RETURNING id")
            .bind(format!("image-transform-{suffix}"))
            .fetch_one(pool)
            .await
            .expect("create integration organization");
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, organization_id, display_name, email, status) VALUES ($1, $2, 'image transform integration', $3, 'active')",
    )
    .bind(user_id)
    .bind(organization_id)
    .bind(format!("image-transform-{suffix}@example.invalid"))
    .execute(pool)
    .await
    .expect("create integration user");
    (organization_id, user_id, Uuid::new_v4())
}

async fn cleanup(pool: &PgPool, organization_id: Uuid, user_id: Uuid, task_project_id: Uuid) {
    sqlx::query("DELETE FROM image_transform_tasks WHERE project_id = $1")
        .bind(task_project_id)
        .execute(pool)
        .await
        .expect("clean image transform tasks");
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("clean integration user");
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(organization_id)
        .execute(pool)
        .await
        .expect("clean integration organization");
}

async fn insert_task(
    pool: &PgPool,
    user_id: Uuid,
    project_id: Uuid,
    operation_id: &str,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(
        r#"
        INSERT INTO image_transform_tasks (
            operation_id, source_scope, project_id, source_hash,
            source_object_key, source_mime_type, source_byte_size,
            target_format, requested_by
        ) VALUES ($1, 'data-project', $2, $3, $4, 'image/png', 4, 'png', $5)
        RETURNING id
        "#,
    )
    .bind(operation_id)
    .bind(project_id)
    .bind("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    .bind(format!("data-projects/{project_id}/objects/source"))
    .bind(user_id)
    .fetch_one(pool)
    .await
}

#[tokio::test]
async fn image_transform_schema_is_idempotent_and_recovers_running_tasks() {
    let Some(pool) = pool().await else { return };
    let (organization_id, user_id, project_id) = fixture(&pool).await;
    let operation_id = format!("pg-transform-{project_id}");
    let task_id = insert_task(&pool, user_id, project_id, &operation_id)
        .await
        .expect("insert image transform task");

    let duplicate: Option<Uuid> = sqlx::query_scalar(
        r#"
        INSERT INTO image_transform_tasks (
            operation_id, source_scope, project_id, source_hash,
            source_object_key, source_mime_type, source_byte_size,
            target_format, requested_by
        ) VALUES ($1, 'data-project', $2, $3, $4, 'image/png', 4, 'png', $5)
        ON CONFLICT (requested_by, operation_id) DO NOTHING
        RETURNING id
        "#,
    )
    .bind(&operation_id)
    .bind(project_id)
    .bind("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    .bind(format!("data-projects/{project_id}/objects/source"))
    .bind(user_id)
    .fetch_optional(&pool)
    .await
    .expect("idempotent duplicate insert");
    assert!(
        duplicate.is_none(),
        "operation id must not create a second task"
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM image_transform_tasks WHERE requested_by = $1 AND operation_id = $2",
    )
    .bind(user_id)
    .bind(&operation_id)
    .fetch_one(&pool)
    .await
    .expect("count idempotent tasks");
    assert_eq!(count, 1);

    sqlx::query("UPDATE image_transform_tasks SET status = 'running' WHERE id = $1")
        .bind(task_id)
        .execute(&pool)
        .await
        .expect("mark task running");
    sqlx::query(
        "UPDATE image_transform_tasks SET status = 'queued', started_at = NULL, updated_at = now() WHERE status = 'running'",
    )
    .execute(&pool)
    .await
    .expect("recover running task");
    let status: String =
        sqlx::query_scalar("SELECT status FROM image_transform_tasks WHERE id = $1")
            .bind(task_id)
            .fetch_one(&pool)
            .await
            .expect("read recovered task");
    assert_eq!(status, "queued");

    cleanup(&pool, organization_id, user_id, project_id).await;
}

#[tokio::test]
async fn image_transform_constraints_reject_invalid_format_and_partial_crop() {
    let Some(pool) = pool().await else { return };
    let (organization_id, user_id, project_id) = fixture(&pool).await;
    let invalid_format = sqlx::query(
        r#"
        INSERT INTO image_transform_tasks (
            operation_id, source_scope, project_id, source_hash,
            source_object_key, source_mime_type, source_byte_size,
            target_format, requested_by
        ) VALUES ('invalid-format', 'data-project', $1, $2, 'source', 'image/png', 4, 'gif', $3)
        "#,
    )
    .bind(project_id)
    .bind("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
    .bind(user_id)
    .execute(&pool)
    .await;
    assert!(
        invalid_format.is_err(),
        "unsupported target format must be rejected"
    );

    let partial_crop = sqlx::query(
        r#"
        INSERT INTO image_transform_tasks (
            operation_id, source_scope, project_id, source_hash,
            source_object_key, source_mime_type, source_byte_size,
            target_format, crop_x, crop_width, requested_by
        ) VALUES ('partial-crop', 'data-project', $1, $2, 'source', 'image/png', 4, 'png', 0, 1, $3)
        "#,
    )
    .bind(project_id)
    .bind("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc")
    .bind(user_id)
    .execute(&pool)
    .await;
    assert!(partial_crop.is_err(), "partial crop must be rejected");

    cleanup(&pool, organization_id, user_id, project_id).await;
}

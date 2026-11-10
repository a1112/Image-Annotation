//! Opt-in PostgreSQL checks for the mobile collaboration migration.
//!
//! These tests are intentionally gated so a normal `cargo test` never touches
//! a developer or production database.  CI/deployment smoke jobs set both
//! `RUN_PG_INTEGRATION=1` and an isolated `DATABASE_URL`.

use sqlx::{postgres::PgPoolOptions, Row};
use uuid::Uuid;

fn enabled() -> bool {
    std::env::var("RUN_PG_INTEGRATION").ok().as_deref() == Some("1")
}

async fn pool() -> Option<sqlx::PgPool> {
    if !enabled() {
        return None;
    }
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL is required when integration is enabled");
    Some(
        PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .expect("connect to isolated PostgreSQL integration database"),
    )
}

#[tokio::test]
async fn collaboration_migration_has_account_and_ticket_tables() {
    let Some(pool) = pool().await else { return };
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("apply server migrations");
    for table in [
        "devices",
        "sessions",
        "project_invitations",
        "object_tickets",
    ] {
        let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(table)
            .fetch_one(&pool)
            .await
            .expect("inspect migration table");
        assert!(exists, "missing migrated table {table}");
    }
    let hash_column: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_name = 'object_tickets' AND column_name = 'content_hash')",
    )
    .fetch_one(&pool)
    .await
    .expect("inspect object ticket columns");
    assert!(hash_column);
    let thumbnail_columns: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.columns WHERE table_name = 'assets' AND column_name IN ('thumbnail_content_hash', 'thumbnail_mime_type', 'thumbnail_byte_size')",
    )
    .fetch_one(&pool)
    .await
    .expect("inspect thumbnail columns");
    assert_eq!(thumbnail_columns, 3);
}

#[tokio::test]
async fn task_item_claim_update_has_one_winner() {
    let Some(pool) = pool().await else { return };
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("apply server migrations");
    let suffix = Uuid::new_v4();
    let organization_id: Uuid =
        sqlx::query_scalar("INSERT INTO organizations (name) VALUES ($1) RETURNING id")
            .bind(format!("integration-{suffix}"))
            .fetch_one(&pool)
            .await
            .expect("create isolated organization");
    let user_id: Uuid = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, organization_id, display_name, email, status) VALUES ($1, $2, 'integration', $3, 'active')")
        .bind(user_id)
        .bind(organization_id)
        .bind(format!("{suffix}@example.invalid"))
        .execute(&pool)
        .await
        .expect("create isolated user");
    let project_id: Uuid = sqlx::query_scalar("INSERT INTO projects (organization_id, name, mode, status, created_by, updated_by) VALUES ($1, $2, 'cloud_linked', 'active', $3, $3) RETURNING id")
        .bind(organization_id)
        .bind(format!("integration-{suffix}"))
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .expect("create isolated project");
    sqlx::query(
        "INSERT INTO project_members (project_id, user_id, role) VALUES ($1, $2, 'annotator')",
    )
    .bind(project_id)
    .bind(user_id)
    .execute(&pool)
    .await
    .expect("create isolated membership");
    let asset_id: Uuid = sqlx::query_scalar("INSERT INTO assets (project_id, file_name, object_key, content_hash, mime_type, width, height, byte_size, created_by, updated_by) VALUES ($1, 'x.png', $2, repeat('a', 64), 'image/png', 1, 1, 1, $3, $3) RETURNING id")
        .bind(project_id)
        .bind(format!("integration/{suffix}"))
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .expect("create isolated asset");
    let task_id: Uuid = sqlx::query_scalar("INSERT INTO tasks (project_id, name, status, created_by, updated_by) VALUES ($1, 'integration', 'unassigned', $2, $2) RETURNING id")
        .bind(project_id)
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .expect("create isolated task");
    sqlx::query("INSERT INTO task_items (task_id, image_id, status) VALUES ($1, $2, 'unassigned')")
        .bind(task_id)
        .bind(asset_id)
        .execute(&pool)
        .await
        .expect("create isolated task item");

    let mut first = pool.begin().await.expect("begin first claim");
    let claimed = sqlx::query("UPDATE task_items SET status = 'in_progress', assignee_id = $3, revision = revision + 1 WHERE task_id = $1 AND image_id = $2 AND status = 'unassigned' RETURNING id")
        .bind(task_id)
        .bind(asset_id)
        .bind(user_id)
        .fetch_optional(&mut *first)
        .await
        .expect("claim item");
    assert!(claimed.is_some());
    first.commit().await.expect("commit first claim");
    let second = sqlx::query("UPDATE task_items SET status = 'in_progress', assignee_id = $3, revision = revision + 1 WHERE task_id = $1 AND image_id = $2 AND status = 'unassigned' RETURNING id")
        .bind(task_id)
        .bind(asset_id)
        .bind(user_id)
        .fetch_optional(&pool)
        .await
        .expect("second claim");
    assert!(second.is_none(), "a claimed item must have one winner");
    let row =
        sqlx::query("SELECT status, revision FROM task_items WHERE task_id = $1 AND image_id = $2")
            .bind(task_id)
            .bind(asset_id)
            .fetch_one(&pool)
            .await
            .expect("read claimed item");
    assert_eq!(row.get::<String, _>("status"), "in_progress");
    assert_eq!(row.get::<i64, _>("revision"), 2);

    // Remove only the uniquely tagged rows; this test must be safe on an
    // isolated shared CI database.
    sqlx::query("DELETE FROM task_items WHERE task_id = $1")
        .bind(task_id)
        .execute(&pool)
        .await
        .expect("clean task items");
    sqlx::query("DELETE FROM tasks WHERE id = $1")
        .bind(task_id)
        .execute(&pool)
        .await
        .expect("clean task");
    sqlx::query("DELETE FROM assets WHERE id = $1")
        .bind(asset_id)
        .execute(&pool)
        .await
        .expect("clean asset");
    sqlx::query("DELETE FROM project_members WHERE project_id = $1")
        .bind(project_id)
        .execute(&pool)
        .await
        .expect("clean membership");
    sqlx::query("DELETE FROM projects WHERE id = $1")
        .bind(project_id)
        .execute(&pool)
        .await
        .expect("clean project");
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("clean user");
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(organization_id)
        .execute(&pool)
        .await
        .expect("clean isolated organization");
}

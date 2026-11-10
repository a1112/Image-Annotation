//! Opt-in PostgreSQL coverage for the project-level cloud repository.
//!
//! These checks only use an explicitly marked staging database.  They verify
//! the additive migration and the project boundary around ACL/membership and
//! content-addressed object rows; HTTP/S3 behaviour is exercised by the
//! deployment harness with the same fixture shape.

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
        "refusing data-project integration against non-staging database"
    );
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("apply server migrations");
    Some(pool)
}

#[tokio::test]
async fn data_project_migration_has_acl_graph_and_object_tables() {
    let Some(pool) = pool().await else { return };
    for table in [
        "data_projects",
        "data_project_members",
        "data_project_datasets",
        "data_project_blobs",
        "data_project_commits",
        "data_project_refs",
    ] {
        let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(table)
            .fetch_one(&pool)
            .await
            .expect("inspect data-project migration table");
        assert!(exists, "missing migrated table {table}");
    }
    let role_constraint: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = 'data_project_members'::regclass AND contype = 'c' LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("inspect data-project ACL role constraint");
    for role in ["owner", "admin", "editor", "viewer"] {
        assert!(role_constraint.contains(role), "ACL role missing: {role}");
    }
}

#[tokio::test]
async fn data_project_objects_are_scoped_by_project() {
    let Some(pool) = pool().await else { return };
    let suffix = Uuid::new_v4();
    let organization_id: Uuid =
        sqlx::query_scalar("INSERT INTO organizations (name) VALUES ($1) RETURNING id")
            .bind(format!("data-project-{suffix}"))
            .fetch_one(&pool)
            .await
            .expect("create integration organization");
    let user_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users (id, organization_id, display_name, email, status) VALUES ($1, $2, $3, $4, 'active')",
    )
    .bind(user_id)
    .bind(organization_id)
    .bind("data-project integration")
    .bind(format!("data-project-{suffix}@example.invalid"))
    .execute(&pool)
    .await
    .expect("create integration user");

    let first_project: Uuid = sqlx::query_scalar(
        "INSERT INTO data_projects (organization_id, owner_id, name, created_by, updated_by) VALUES ($1, $2, 'first', $2, $2) RETURNING id",
    )
    .bind(organization_id)
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("create first data project");
    let second_project: Uuid = sqlx::query_scalar(
        "INSERT INTO data_projects (organization_id, owner_id, name, created_by, updated_by) VALUES ($1, $2, 'second', $2, $2) RETURNING id",
    )
    .bind(organization_id)
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .expect("create second data project");
    for project_id in [first_project, second_project] {
        sqlx::query(
            "INSERT INTO data_project_members (project_id, user_id, role) VALUES ($1, $2, 'owner')",
        )
        .bind(project_id)
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("create project owner membership");
    }
    let hash = "a".repeat(64);
    for project_id in [first_project, second_project] {
        sqlx::query(
            "INSERT INTO data_project_blobs (project_id, content_hash, byte_size, kind, object_key, created_by) VALUES ($1, $2, 4, 'other', $3, $4)",
        )
        .bind(project_id)
        .bind(&hash)
        .bind(format!("data-projects/{project_id}/objects/{hash}"))
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("insert project-scoped object");
    }
    let rows = sqlx::query(
        "SELECT project_id, object_key FROM data_project_blobs WHERE content_hash = $1 ORDER BY project_id",
    )
    .bind(&hash)
    .fetch_all(&pool)
    .await
    .expect("read project-scoped objects");
    assert_eq!(rows.len(), 2);
    assert_ne!(
        rows[0].get::<Uuid, _>("project_id"),
        rows[1].get::<Uuid, _>("project_id")
    );
    assert_ne!(
        rows[0].get::<String, _>("object_key"),
        rows[1].get::<String, _>("object_key")
    );

    sqlx::query("DELETE FROM data_projects WHERE id IN ($1, $2)")
        .bind(first_project)
        .bind(second_project)
        .execute(&pool)
        .await
        .expect("clean data projects");
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("clean integration user");
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(organization_id)
        .execute(&pool)
        .await
        .expect("clean integration organization");
}

//! Opt-in PostgreSQL coverage for the browser workspace persistence boundary.
//!
//! These checks intentionally never fall back to an in-process fake.  They run
//! only against a database explicitly marked with RUN_PG_INTEGRATION=1 and a
//! *_staging database.

use serde_json::json;
use sqlx::{postgres::PgPoolOptions, PgPool};
use uuid::Uuid;

fn enabled() -> bool {
    std::env::var("RUN_PG_INTEGRATION").ok().as_deref() == Some("1")
}

async fn pool() -> Option<PgPool> {
    if !enabled() {
        return None;
    }
    let url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL is required when web workspace integration is enabled");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .expect("connect to isolated PostgreSQL staging database");
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .expect("read integration database name");
    assert!(
        database.contains("_staging"),
        "refusing web workspace integration against a non-staging database"
    );
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("apply server migrations");
    Some(pool)
}

#[tokio::test]
async fn web_workspace_migration_has_durable_branch_draft_and_merge_tables() {
    let Some(pool) = pool().await else { return };
    for table in [
        "data_project_branches",
        "data_project_worktrees",
        "data_project_worktree_metadata",
        "data_project_worktree_entries",
        "data_project_active_branches",
        "data_project_merge_sessions",
        "data_project_tasks",
        "data_project_task_items",
        "data_project_exports",
    ] {
        let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(table)
            .fetch_one(&pool)
            .await
            .expect("inspect web workspace table");
        assert!(exists, "missing web workspace table {table}");
    }
    let project_metadata_column: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns WHERE table_name = 'data_project_worktrees' AND column_name = 'project_metadata_json')",
    )
    .fetch_one(&pool)
    .await
    .expect("inspect project metadata column");
    assert!(project_metadata_column);
}

#[tokio::test]
async fn web_workspace_dataset_assignment_guard_and_branch_state_are_cas() {
    let Some(pool) = pool().await else { return };
    let suffix = Uuid::new_v4();
    let organization_id: Uuid =
        sqlx::query_scalar("INSERT INTO organizations (name) VALUES ($1) RETURNING id")
            .bind(format!("web-workspace-{suffix}"))
            .fetch_one(&pool)
            .await
            .expect("create organization");
    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, organization_id, display_name, email, status) VALUES ($1, $2, 'web workspace', $3, 'active')")
        .bind(user_id)
        .bind(organization_id)
        .bind(format!("web-workspace-{suffix}@example.invalid"))
        .execute(&pool)
        .await
        .expect("create user");
    let project_one: Uuid = sqlx::query_scalar("INSERT INTO data_projects (organization_id, owner_id, name, created_by, updated_by) VALUES ($1, $2, 'web-one', $2, $2) RETURNING id")
        .bind(organization_id)
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .expect("create first data project");
    let project_two: Uuid = sqlx::query_scalar("INSERT INTO data_projects (organization_id, owner_id, name, created_by, updated_by) VALUES ($1, $2, 'web-two', $2, $2) RETURNING id")
        .bind(organization_id)
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .expect("create second data project");
    for project_id in [project_one, project_two] {
        sqlx::query(
            "INSERT INTO data_project_members (project_id, user_id, role) VALUES ($1, $2, 'owner')",
        )
        .bind(project_id)
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("create project membership");
        sqlx::query("INSERT INTO data_project_refs (project_id, ref_name, updated_by) VALUES ($1, 'main', $2)")
            .bind(project_id)
            .bind(user_id)
            .execute(&pool)
            .await
            .expect("create main ref");
        sqlx::query("INSERT INTO data_project_branches (project_id, branch_name, protected, created_by) VALUES ($1, 'main', true, $2)")
            .bind(project_id)
            .bind(user_id)
            .execute(&pool)
            .await
            .expect("create main branch");
        sqlx::query("INSERT INTO data_project_worktrees (project_id, branch_name, updated_by) VALUES ($1, 'main', $2)")
            .bind(project_id)
            .bind(user_id)
            .execute(&pool)
            .await
            .expect("create web worktree");
    }
    let dataset_id: Uuid = sqlx::query_scalar("INSERT INTO datasets (organization_id, owner_id, name) VALUES ($1, $2, 'web-dataset') RETURNING id")
        .bind(organization_id)
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .expect("create dataset");
    sqlx::query("INSERT INTO dataset_members (dataset_id, user_id, role) VALUES ($1, $2, 'owner')")
        .bind(dataset_id)
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("create dataset membership");
    sqlx::query("INSERT INTO data_project_datasets (project_id, dataset_key, dataset_id, metadata_json) VALUES ($1, 'canonical', $2, $3)")
        .bind(project_one)
        .bind(dataset_id)
        .bind(json!({ "classes": ["cat"] }))
        .execute(&pool)
        .await
        .expect("assign dataset to first project");
    // Existing 0005 graph rows are retained for compatibility, so migration
    // must not fail merely because an old import already has duplicate
    // membership.  The web assignment handler locks this dataset row and
    // rejects a new cross-project owner before inserting such a row.
    let legacy_duplicate = sqlx::query("INSERT INTO data_project_datasets (project_id, dataset_key, dataset_id) VALUES ($1, 'canonical', $2)")
        .bind(project_two)
        .bind(dataset_id)
        .execute(&pool)
        .await;
    assert!(
        legacy_duplicate.is_ok(),
        "legacy graph rows must remain readable"
    );
    let owner_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM data_project_datasets WHERE dataset_id = $1")
            .bind(dataset_id)
            .fetch_one(&pool)
            .await
            .expect("count legacy dataset mappings");
    assert_eq!(owner_count, 2);

    sqlx::query("INSERT INTO data_project_active_branches (project_id, user_id, branch_name) VALUES ($1, $2, 'main')")
        .bind(project_one)
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("store active web branch");
    let first_revision: i64 = sqlx::query_scalar("SELECT revision FROM data_project_worktrees WHERE project_id = $1 AND branch_name = 'main'")
        .bind(project_one)
        .fetch_one(&pool)
        .await
        .expect("read web worktree revision");
    let updated = sqlx::query("UPDATE data_project_worktrees SET revision = revision + 1 WHERE project_id = $1 AND branch_name = 'main' AND revision = $2")
        .bind(project_one)
        .bind(first_revision)
        .execute(&pool)
        .await
        .expect("advance web worktree revision");
    assert_eq!(updated.rows_affected(), 1);
    let stale = sqlx::query("UPDATE data_project_worktrees SET revision = revision + 1 WHERE project_id = $1 AND branch_name = 'main' AND revision = $2")
        .bind(project_one)
        .bind(first_revision)
        .execute(&pool)
        .await
        .expect("check stale CAS update");
    assert_eq!(
        stale.rows_affected(),
        0,
        "stale web draft must not overwrite a newer revision"
    );
    let project_metadata = json!({
        "schemaVersion": 1,
        "datasetGroups": [{
            "id": "group-a",
            "name": "Road",
            "datasetKeys": ["canonical"],
            "position": 0
        }]
    });
    let metadata_update = sqlx::query(
        "UPDATE data_project_worktrees SET project_metadata_json = $2 WHERE project_id = $1 AND branch_name = 'main'",
    )
    .bind(project_one)
    .bind(&project_metadata)
    .execute(&pool)
    .await
    .expect("persist project dataset groups");
    assert_eq!(metadata_update.rows_affected(), 1);
    let persisted_metadata: serde_json::Value = sqlx::query_scalar(
        "SELECT project_metadata_json FROM data_project_worktrees WHERE project_id = $1 AND branch_name = 'main'",
    )
    .bind(project_one)
    .fetch_one(&pool)
    .await
    .expect("read project dataset groups");
    assert_eq!(persisted_metadata, project_metadata);

    sqlx::query("DELETE FROM data_projects WHERE id IN ($1, $2)")
        .bind(project_one)
        .bind(project_two)
        .execute(&pool)
        .await
        .expect("clean data projects");
    sqlx::query("DELETE FROM datasets WHERE id = $1")
        .bind(dataset_id)
        .execute(&pool)
        .await
        .expect("clean dataset");
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("clean user");
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(organization_id)
        .execute(&pool)
        .await
        .expect("clean organization");
}

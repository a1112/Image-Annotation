use super::{projects::write_audit, projects::write_change, ApiError};
use crate::{
    auth::{require_project_permission, AuthUser, ProjectPermission},
    state::AppState,
};
use axum::{
    extract::{Path, Query, State},
    Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct IssueQuery {
    status: Option<String>,
    severity: Option<String>,
    assignee_id: Option<Uuid>,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateIssue {
    image_id: Uuid,
    annotation_object_id: Option<String>,
    title: String,
    #[serde(default)]
    description: String,
    severity: String,
    assignee_id: Option<Uuid>,
    due_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateIssue {
    title: Option<String>,
    description: Option<String>,
    severity: Option<String>,
    assignee_id: Option<Uuid>,
    due_at: Option<DateTime<Utc>>,
    base_revision: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransitionIssue {
    status: String,
    base_revision: i64,
}

#[derive(Debug, Deserialize)]
pub struct AddComment {
    content: String,
}

pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
    Query(query): Query<IssueQuery>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, project_id, ProjectPermission::View).await?;
    let rows = sqlx::query(
        r#"
        SELECT id, project_id, image_id, annotation_object_id, title, description,
            severity, status, source, reporter_id, assignee_id, due_at,
            revision, created_at, updated_at, resolved_at
        FROM issues
        WHERE project_id = $1 AND deleted_at IS NULL
          AND ($2::text IS NULL OR status = $2)
          AND ($3::text IS NULL OR severity = $3)
          AND ($4::uuid IS NULL OR assignee_id = $4)
        ORDER BY updated_at DESC
        LIMIT $5 OFFSET $6
        "#,
    )
    .bind(project_id)
    .bind(query.status)
    .bind(query.severity)
    .bind(query.assignee_id)
    .bind(query.limit.unwrap_or(100).clamp(1, 500))
    .bind(query.offset.unwrap_or(0).max(0))
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(Value::Array(rows.iter().map(issue_json).collect())))
}

pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
    Json(input): Json<CreateIssue>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, project_id, ProjectPermission::Review).await?;
    validate_severity(&input.severity)?;
    if input.title.trim().is_empty() {
        return Err(ApiError::bad_request("issue title is required"));
    }
    let image_exists = sqlx::query(
        "SELECT 1 FROM assets WHERE id = $1 AND project_id = $2 AND deleted_at IS NULL",
    )
    .bind(input.image_id)
    .bind(project_id)
    .fetch_optional(&state.pool)
    .await?
    .is_some();
    if !image_exists {
        return Err(ApiError::not_found("image not found"));
    }
    let id = Uuid::new_v4();
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        r#"
        INSERT INTO issues (
            id, project_id, image_id, annotation_object_id, title, description,
            severity, status, source, reporter_id, assignee_id, due_at
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, 'open', 'manual', $8, $9, $10)
        RETURNING id, project_id, image_id, annotation_object_id, title, description,
            severity, status, source, reporter_id, assignee_id, due_at,
            revision, created_at, updated_at, resolved_at
        "#,
    )
    .bind(id)
    .bind(project_id)
    .bind(input.image_id)
    .bind(input.annotation_object_id)
    .bind(input.title.trim())
    .bind(input.description)
    .bind(&input.severity)
    .bind(user.id)
    .bind(input.assignee_id)
    .bind(input.due_at)
    .fetch_one(&mut *tx)
    .await?;
    let payload = issue_json(&row);
    sqlx::query(
        "INSERT INTO issue_events (issue_id, event_type, after_json, actor_id) VALUES ($1, 'created', $2, $3)",
    )
    .bind(id)
    .bind(&payload)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    write_change(&mut tx, project_id, "issue", &id.to_string(), "create", 1, payload.clone())
        .await?;
    write_audit(&mut tx, project_id, user.id, "issue.create", "issue", &id.to_string())
        .await?;
    tx.commit().await?;
    Ok(Json(payload))
}

pub async fn get(
    State(state): State<AppState>,
    user: AuthUser,
    Path(issue_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let row = find_issue(&state, issue_id).await?;
    let project_id: Uuid = row.get("project_id");
    require_project_permission(&state, user.id, project_id, ProjectPermission::View).await?;
    let comments = sqlx::query(
        "SELECT id, author_id, content, revision, created_at, updated_at FROM issue_comments WHERE issue_id = $1 AND deleted_at IS NULL ORDER BY created_at",
    )
    .bind(issue_id)
    .fetch_all(&state.pool)
    .await?;
    let events = sqlx::query(
        "SELECT id, event_type, before_json, after_json, actor_id, created_at FROM issue_events WHERE issue_id = $1 ORDER BY created_at",
    )
    .bind(issue_id)
    .fetch_all(&state.pool)
    .await?;
    let mut payload = issue_json(&row);
    payload["comments"] = Value::Array(
        comments
            .iter()
            .map(|row| {
                json!({
                    "id": row.get::<Uuid, _>("id"),
                    "authorId": row.get::<Uuid, _>("author_id"),
                    "content": row.get::<String, _>("content"),
                    "revision": row.get::<i64, _>("revision"),
                    "createdAt": row.get::<DateTime<Utc>, _>("created_at"),
                    "updatedAt": row.get::<DateTime<Utc>, _>("updated_at")
                })
            })
            .collect(),
    );
    payload["events"] = Value::Array(
        events
            .iter()
            .map(|row| {
                json!({
                    "id": row.get::<Uuid, _>("id"),
                    "eventType": row.get::<String, _>("event_type"),
                    "before": row.get::<Option<Value>, _>("before_json"),
                    "after": row.get::<Option<Value>, _>("after_json"),
                    "actorId": row.get::<Uuid, _>("actor_id"),
                    "createdAt": row.get::<DateTime<Utc>, _>("created_at")
                })
            })
            .collect(),
    );
    Ok(Json(payload))
}

pub async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path(issue_id): Path<Uuid>,
    Json(input): Json<UpdateIssue>,
) -> Result<Json<Value>, ApiError> {
    let current = find_issue(&state, issue_id).await?;
    let project_id: Uuid = current.get("project_id");
    require_project_permission(&state, user.id, project_id, ProjectPermission::Review).await?;
    if let Some(severity) = &input.severity {
        validate_severity(severity)?;
    }
    let current_revision: i64 = current.get("revision");
    if current_revision != input.base_revision {
        return Err(ApiError::conflict(
            "ISSUE_REVISION_CONFLICT",
            "issue revision has changed",
            json!({ "baseRevision": input.base_revision, "currentRevision": current_revision, "remoteIssue": issue_json(&current) }),
        ));
    }
    let before = issue_json(&current);
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        r#"
        UPDATE issues SET
            title = COALESCE($2, title),
            description = COALESCE($3, description),
            severity = COALESCE($4, severity),
            assignee_id = COALESCE($5, assignee_id),
            due_at = COALESCE($6, due_at),
            revision = revision + 1,
            updated_at = now()
        WHERE id = $1 AND revision = $7 AND deleted_at IS NULL
        RETURNING id, project_id, image_id, annotation_object_id, title, description,
            severity, status, source, reporter_id, assignee_id, due_at,
            revision, created_at, updated_at, resolved_at
        "#,
    )
    .bind(issue_id)
    .bind(input.title)
    .bind(input.description)
    .bind(input.severity)
    .bind(input.assignee_id)
    .bind(input.due_at)
    .bind(input.base_revision)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::conflict("ISSUE_REVISION_CONFLICT", "issue changed", Value::Null))?;
    let after = issue_json(&row);
    write_issue_event(&mut tx, issue_id, "updated", Some(before), Some(after.clone()), user.id)
        .await?;
    write_change(
        &mut tx,
        project_id,
        "issue",
        &issue_id.to_string(),
        "update",
        row.get("revision"),
        after.clone(),
    )
    .await?;
    write_audit(&mut tx, project_id, user.id, "issue.update", "issue", &issue_id.to_string())
        .await?;
    tx.commit().await?;
    Ok(Json(after))
}

pub async fn transition(
    State(state): State<AppState>,
    user: AuthUser,
    Path(issue_id): Path<Uuid>,
    Json(input): Json<TransitionIssue>,
) -> Result<Json<Value>, ApiError> {
    let current = find_issue(&state, issue_id).await?;
    let project_id: Uuid = current.get("project_id");
    require_project_permission(&state, user.id, project_id, ProjectPermission::Review).await?;
    let current_status: String = current.get("status");
    let current_revision: i64 = current.get("revision");
    if current_revision != input.base_revision {
        return Err(ApiError::conflict(
            "ISSUE_REVISION_CONFLICT",
            "issue revision has changed",
            json!({ "baseRevision": input.base_revision, "currentRevision": current_revision }),
        ));
    }
    if !valid_transition(&current_status, &input.status) {
        return Err(ApiError::bad_request(format!(
            "invalid issue transition: {current_status} -> {}",
            input.status
        )));
    }
    let before = issue_json(&current);
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        r#"
        UPDATE issues SET status = $2, revision = revision + 1, updated_at = now(),
            resolved_at = CASE WHEN $2 IN ('resolved', 'closed') THEN now() ELSE NULL END
        WHERE id = $1 AND revision = $3
        RETURNING id, project_id, image_id, annotation_object_id, title, description,
            severity, status, source, reporter_id, assignee_id, due_at,
            revision, created_at, updated_at, resolved_at
        "#,
    )
    .bind(issue_id)
    .bind(&input.status)
    .bind(input.base_revision)
    .fetch_one(&mut *tx)
    .await?;
    let after = issue_json(&row);
    write_issue_event(
        &mut tx,
        issue_id,
        "status_changed",
        Some(before),
        Some(after.clone()),
        user.id,
    )
    .await?;
    write_change(
        &mut tx,
        project_id,
        "issue",
        &issue_id.to_string(),
        "transition",
        row.get("revision"),
        after.clone(),
    )
    .await?;
    write_audit(
        &mut tx,
        project_id,
        user.id,
        "issue.transition",
        "issue",
        &issue_id.to_string(),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(after))
}

pub async fn comment(
    State(state): State<AppState>,
    user: AuthUser,
    Path(issue_id): Path<Uuid>,
    Json(input): Json<AddComment>,
) -> Result<Json<Value>, ApiError> {
    if input.content.trim().is_empty() {
        return Err(ApiError::bad_request("comment content is required"));
    }
    let issue = find_issue(&state, issue_id).await?;
    let project_id: Uuid = issue.get("project_id");
    require_project_permission(&state, user.id, project_id, ProjectPermission::View).await?;
    let comment_id = Uuid::new_v4();
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        "INSERT INTO issue_comments (id, issue_id, author_id, content) VALUES ($1, $2, $3, $4) RETURNING created_at",
    )
    .bind(comment_id)
    .bind(issue_id)
    .bind(user.id)
    .bind(input.content.trim())
    .fetch_one(&mut *tx)
    .await?;
    let payload = json!({
        "id": comment_id,
        "issueId": issue_id,
        "authorId": user.id,
        "content": input.content.trim(),
        "createdAt": row.get::<DateTime<Utc>, _>("created_at")
    });
    write_issue_event(
        &mut tx,
        issue_id,
        "commented",
        None,
        Some(payload.clone()),
        user.id,
    )
    .await?;
    write_change(
        &mut tx,
        project_id,
        "issue_comment",
        &comment_id.to_string(),
        "comment",
        1,
        payload.clone(),
    )
    .await?;
    write_audit(
        &mut tx,
        project_id,
        user.id,
        "issue.comment",
        "issue_comment",
        &comment_id.to_string(),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(payload))
}

async fn find_issue(state: &AppState, issue_id: Uuid) -> Result<sqlx::postgres::PgRow, ApiError> {
    sqlx::query(
        r#"
        SELECT id, project_id, image_id, annotation_object_id, title, description,
            severity, status, source, reporter_id, assignee_id, due_at,
            revision, created_at, updated_at, resolved_at
        FROM issues WHERE id = $1 AND deleted_at IS NULL
        "#,
    )
    .bind(issue_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("issue not found"))
}

async fn write_issue_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    issue_id: Uuid,
    event_type: &str,
    before: Option<Value>,
    after: Option<Value>,
    actor_id: Uuid,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO issue_events (issue_id, event_type, before_json, after_json, actor_id) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(issue_id)
    .bind(event_type)
    .bind(before)
    .bind(after)
    .bind(actor_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn validate_severity(severity: &str) -> Result<(), ApiError> {
    if matches!(
        severity,
        "blocker" | "critical" | "major" | "minor" | "suggestion"
    ) {
        Ok(())
    } else {
        Err(ApiError::bad_request("invalid issue severity"))
    }
}

fn valid_transition(current: &str, next: &str) -> bool {
    matches!(
        (current, next),
        ("open", "in_progress")
            | ("in_progress", "resolved")
            | ("resolved", "pending_review")
            | ("pending_review", "closed")
            | ("pending_review", "reopened")
            | ("reopened", "in_progress")
            | ("closed", "reopened")
    )
}

fn issue_json(row: &sqlx::postgres::PgRow) -> Value {
    json!({
        "id": row.get::<Uuid, _>("id"),
        "projectId": row.get::<Uuid, _>("project_id"),
        "imageId": row.get::<Uuid, _>("image_id"),
        "annotationObjectId": row.get::<Option<String>, _>("annotation_object_id"),
        "title": row.get::<String, _>("title"),
        "description": row.get::<String, _>("description"),
        "severity": row.get::<String, _>("severity"),
        "status": row.get::<String, _>("status"),
        "source": row.get::<String, _>("source"),
        "reporterId": row.get::<Uuid, _>("reporter_id"),
        "assigneeId": row.get::<Option<Uuid>, _>("assignee_id"),
        "dueAt": row.get::<Option<DateTime<Utc>>, _>("due_at"),
        "revision": row.get::<i64, _>("revision"),
        "createdAt": row.get::<DateTime<Utc>, _>("created_at"),
        "updatedAt": row.get::<DateTime<Utc>, _>("updated_at"),
        "resolvedAt": row.get::<Option<DateTime<Utc>>, _>("resolved_at")
    })
}

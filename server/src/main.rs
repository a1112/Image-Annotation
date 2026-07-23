mod auth;
mod config;
mod routes;
mod state;

use anyhow::Result;
use axum::{
    routing::{delete, get, patch, post, put},
    Router,
};
use config::Config;
use sqlx::postgres::PgPoolOptions;
use state::AppState;
use std::time::Duration;
use tower_http::{
    cors::{Any, CorsLayer},
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "image_annotation_server=info,tower_http=info".into()),
        )
        .init();
    let config = Config::from_env()?;
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&config.database_url)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;

    let mut aws_loader = aws_config::from_env();
    if let Some(endpoint) = &config.s3_endpoint {
        aws_loader = aws_loader.endpoint_url(endpoint);
    }
    let aws = aws_loader.load().await;
    let state = AppState {
        pool,
        s3: aws_sdk_s3::Client::new(&aws),
        config: config.clone(),
    };
    let request_id = axum::http::HeaderName::from_static("x-request-id");
    let app = Router::new()
        .route("/health", get(routes::health))
        .route("/api/v1/projects", get(routes::projects::list).post(routes::projects::create))
        .route(
            "/api/v1/projects/{project_id}",
            get(routes::projects::get)
                .patch(routes::projects::update)
                .delete(routes::projects::delete),
        )
        .route(
            "/api/v1/projects/{project_id}/members",
            get(routes::projects::members).post(routes::projects::add_member),
        )
        .route(
            "/api/v1/projects/{project_id}/members/{member_id}",
            delete(routes::projects::remove_member),
        )
        .route(
            "/api/v1/projects/{project_id}/images",
            get(routes::assets::list_images),
        )
        .route(
            "/api/v1/projects/{project_id}/assets/upload-session",
            post(routes::assets::create_upload_session),
        )
        .route(
            "/api/v1/projects/{project_id}/assets/{asset_id}/complete",
            post(routes::assets::complete_upload),
        )
        .route(
            "/api/v1/assets/{asset_id}/download-url",
            get(routes::assets::download_url),
        )
        .route(
            "/api/v1/projects/{project_id}/images/{image_id}/annotation",
            get(routes::annotations::get).put(routes::annotations::put),
        )
        .route(
            "/api/v1/projects/{project_id}/images/{image_id}/annotation/versions",
            get(routes::annotations::versions),
        )
        .route(
            "/api/v1/projects/{project_id}/images/{image_id}/submit",
            post(routes::annotations::submit),
        )
        .route(
            "/api/v1/projects/{project_id}/images/{image_id}/lease",
            post(routes::annotations::acquire_lease)
                .delete(routes::annotations::release_lease),
        )
        .route(
            "/api/v1/projects/{project_id}/issues",
            get(routes::issues::list).post(routes::issues::create),
        )
        .route(
            "/api/v1/issues/{issue_id}",
            get(routes::issues::get).patch(routes::issues::update),
        )
        .route(
            "/api/v1/issues/{issue_id}/transition",
            post(routes::issues::transition),
        )
        .route(
            "/api/v1/issues/{issue_id}/comments",
            post(routes::issues::comment),
        )
        .route(
            "/api/v1/issues/{issue_id}/attachments",
            get(routes::attachments::list).post(routes::attachments::create_upload_session),
        )
        .route(
            "/api/v1/issues/{issue_id}/attachments/{attachment_id}/complete",
            post(routes::attachments::complete_upload),
        )
        .route(
            "/api/v1/issue-attachments/{attachment_id}/download-url",
            get(routes::attachments::download_url),
        )
        .route("/api/v1/sync/push", post(routes::sync::push))
        .route(
            "/api/v1/projects/{project_id}/changes",
            get(routes::sync::changes),
        )
        .route(
            "/api/v1/projects/{project_id}/sync-bootstrap",
            get(routes::sync::bootstrap),
        )
        .with_state(state)
        .layer(PropagateRequestIdLayer::new(request_id.clone()))
        .layer(SetRequestIdLayer::new(request_id, MakeRequestUuid))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::new().allow_origin(Any).allow_headers(Any).allow_methods(Any));

    let listener = tokio::net::TcpListener::bind(&config.bind_address).await?;
    tracing::info!(address = %config.bind_address, "server listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

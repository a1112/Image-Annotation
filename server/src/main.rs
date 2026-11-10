mod auth;
mod config;
mod routes;
mod state;

use anyhow::Result;
use axum::{
    routing::{delete, get, post},
    Router,
};
use config::Config;
use sqlx::postgres::PgPoolOptions;
use state::AppState;
use std::time::Duration;
use tower_http::{
    cors::{Any, CorsLayer},
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    services::ServeDir,
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

    // The first administrator is deliberately created out of band so a
    // public HTTP endpoint cannot be abused on a fresh deployment.  Values may
    // be supplied as flags or the BOOTSTRAP_ADMIN_* environment variables.
    if std::env::args().nth(1).as_deref() == Some("bootstrap-admin") {
        let input = bootstrap_admin_request()?;
        let result = routes::account::bootstrap_admin(&pool, input)
            .await
            .map_err(anyhow::Error::msg)?;
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }

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
    let dataset_job_worker = routes::dataset_jobs::spawn_worker(state.clone());
    let image_transform_worker = routes::image_transforms::spawn_worker(state.clone());
    let web_dist = std::env::var("WEB_DIST_DIR").unwrap_or_else(|_| "../dist-web".to_string());
    let web_commands = Router::new()
        .route(
            "/api/v1/web/commands/{command}",
            post(routes::web_workspace::command),
        )
        .layer(axum::extract::DefaultBodyLimit::max(32 * 1024 * 1024));
    let web_uploads = Router::new()
        .route(
            "/api/v1/web/datasets/{dataset_id}/images/upload",
            axum::routing::put(routes::web_workspace::upload_image),
        )
        .layer(axum::extract::DefaultBodyLimit::disable());
    let request_id = axum::http::HeaderName::from_static("x-request-id");
    let app = Router::new()
        .route("/health", get(routes::health))
        .merge(web_commands)
        .merge(web_uploads)
        .route(
            "/api/v1/web/datasets/{dataset_id}/images/{image_id}/content",
            get(routes::web_workspace::download_image_content),
        )
        .route(
            "/api/v1/web/projects/{project_id}/objects/{hash}",
            get(routes::web_workspace::download_object),
        )
        .route("/api/v1/web/auth/login", post(routes::web_auth::login))
        .route("/api/v1/web/auth/refresh", post(routes::web_auth::refresh))
        .route("/api/v1/web/auth/session", get(routes::web_auth::session))
        .route("/api/v1/web/auth/logout", post(routes::web_auth::logout))
        .route(
            "/api/v1/projects/{project_id}/image-transforms",
            post(routes::image_transforms::create_project_transform),
        )
        .route(
            "/api/v1/data-projects/{project_id}/image-transforms",
            get(routes::image_transforms::list_data_project_transforms)
                .post(routes::image_transforms::create_data_project_transform),
        )
        .route(
            "/api/v1/image-transforms/{task_id}",
            get(routes::image_transforms::get_transform),
        )
        .route(
            "/api/v1/image-transforms/{task_id}/download",
            get(routes::image_transforms::download_transform),
        )
        .nest_service(
            "/app",
            ServeDir::new(web_dist).append_index_html_on_directories(true),
        )
        .route("/api/v1/auth/login", post(routes::account::login))
        .route("/api/v1/auth/refresh", post(routes::account::refresh))
        .route(
            "/api/v1/auth/invitations/{token}/accept",
            post(routes::account::accept_invitation),
        )
        .route("/api/v1/auth/me", get(routes::account::me))
        .route("/api/v1/auth/logout", post(routes::account::logout))
        .route(
            "/api/v1/auth/devices/{device_id}",
            delete(routes::account::revoke_device),
        )
        .route(
            "/api/v1/projects",
            get(routes::projects::list).post(routes::projects::create),
        )
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
            "/api/v1/data-projects",
            get(routes::data_projects::list).post(routes::data_projects::create),
        )
        .route(
            "/api/v1/data-projects/{project_id}",
            get(routes::data_projects::get)
                .patch(routes::data_projects::update)
                .delete(routes::data_projects::delete),
        )
        .route(
            "/api/v1/data-projects/{project_id}/members",
            get(routes::data_projects::members).post(routes::data_projects::add_member),
        )
        .route(
            "/api/v1/data-projects/{project_id}/members/{member_id}",
            delete(routes::data_projects::remove_member),
        )
        .route(
            "/api/v1/data-projects/{project_id}/graph",
            get(routes::data_projects::graph),
        )
        .route(
            "/api/v1/data-projects/{project_id}/status",
            get(routes::data_projects::status),
        )
        .route(
            "/api/v1/data-projects/{project_id}/push",
            post(routes::data_projects::push),
        )
        .route(
            "/api/v1/data-projects/{project_id}/objects/{hash}",
            get(routes::data_projects::get_blob).put(routes::data_projects::put_blob),
        )
        .route(
            "/api/v1/projects/{project_id}/invitations",
            post(routes::account::invite),
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
            post(routes::annotations::acquire_lease).delete(routes::annotations::release_lease),
        )
        .route(
            "/api/v1/projects/{project_id}/tasks",
            get(routes::collaboration::list_tasks).post(routes::collaboration::create_task),
        )
        .route(
            "/api/v1/tasks/{task_id}/items",
            get(routes::collaboration::list_task_items),
        )
        .route(
            "/api/v1/tasks/{task_id}/items/{image_id}/claim",
            post(routes::collaboration::claim).delete(routes::collaboration::release),
        )
        .route(
            "/api/v1/projects/{project_id}/review-queue",
            get(routes::collaboration::review_queue),
        )
        .route(
            "/api/v1/projects/{project_id}/images/{image_id}/review",
            post(routes::collaboration::review),
        )
        .route(
            "/api/v1/projects/{project_id}/label-schema",
            get(routes::collaboration::get_schema).put(routes::collaboration::put_schema),
        )
        .route(
            "/api/v1/datasets",
            get(routes::datasets::list).post(routes::datasets::create),
        )
        .route(
            "/api/v1/datasets/{dataset_id}",
            get(routes::datasets::get)
                .patch(routes::datasets::update)
                .delete(routes::datasets::delete),
        )
        .route(
            "/api/v1/datasets/{dataset_id}/members",
            get(routes::datasets::list_members).post(routes::datasets::add_member),
        )
        .route(
            "/api/v1/datasets/{dataset_id}/members/{member_id}",
            delete(routes::datasets::remove_member),
        )
        .route(
            "/api/v1/datasets/{dataset_id}/items",
            get(routes::datasets::list_items),
        )
        .route(
            "/api/v1/datasets/{dataset_id}/capture",
            post(routes::datasets::capture),
        )
        .route(
            "/api/v1/datasets/{dataset_id}/versions",
            get(routes::datasets::list_versions).post(routes::datasets::create_version),
        )
        .route(
            "/api/v1/datasets/{dataset_id}/versions/{version_id}",
            get(routes::datasets::get_version),
        )
        .route(
            "/api/v1/datasets/{dataset_id}/reuse",
            post(routes::datasets::reuse),
        )
        .route(
            "/api/v1/storage/quota",
            get(routes::datasets::get_storage_quota).patch(routes::datasets::update_storage_quota),
        )
        .route(
            "/api/v1/datasets/{dataset_id}/jobs/import",
            post(routes::dataset_jobs::create_import_job),
        )
        // Keep the short archive paths used by the native bridge and the
        // first mobile hosting client. They are aliases of the durable job
        // endpoints above, not a second implementation or queue.
        .route(
            "/api/v1/datasets/{dataset_id}/imports",
            post(routes::dataset_jobs::create_import_job),
        )
        .route(
            "/api/v1/datasets/{dataset_id}/jobs/export",
            post(routes::dataset_jobs::create_export_job),
        )
        .route(
            "/api/v1/datasets/{dataset_id}/exports",
            post(routes::dataset_jobs::create_export_job),
        )
        .route(
            "/api/v1/datasets/{dataset_id}/jobs",
            get(routes::dataset_jobs::list_jobs),
        )
        .route(
            "/api/v1/dataset-jobs/{job_id}",
            get(routes::dataset_jobs::get_job),
        )
        .route(
            "/api/v1/dataset-jobs/{job_id}/download",
            get(routes::dataset_jobs::download_job),
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
        .route(
            "/api/v1/object-tickets/{token}",
            get(routes::objects::get).put(routes::objects::put),
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
        .layer(axum::middleware::from_fn(private_api_responses))
        .layer(PropagateRequestIdLayer::new(request_id.clone()))
        .layer(SetRequestIdLayer::new(request_id, MakeRequestUuid))
        .layer(TraceLayer::new_for_http())
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_headers(Any)
                .allow_methods(Any),
        );

    let listener = tokio::net::TcpListener::bind(&config.bind_address).await?;
    tracing::info!(address = %config.bind_address, "server listening");
    let server_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    dataset_job_worker.abort();
    let _ = dataset_job_worker.await;
    image_transform_worker.abort();
    let _ = image_transform_worker.await;
    server_result?;
    Ok(())
}

async fn private_api_responses(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let is_api = request.uri().path().starts_with("/api/");
    let mut response = next.run(request).await;
    if is_api {
        response.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("private, no-store"),
        );
        response.headers_mut().insert(
            "x-content-type-options",
            axum::http::HeaderValue::from_static("nosniff"),
        );
    }
    response
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

fn bootstrap_admin_request() -> Result<routes::account::BootstrapAdminRequest> {
    let mut values = std::collections::HashMap::<String, String>::new();
    let args: Vec<String> = std::env::args().skip(2).collect();
    let mut index = 0;
    while index + 1 < args.len() {
        if let Some(name) = args[index].strip_prefix("--") {
            values.insert(name.replace('-', "_"), args[index + 1].clone());
            index += 2;
        } else {
            index += 1;
        }
    }
    let value = |flag: &str, env: &str| -> Result<String> {
        values
            .get(flag)
            .cloned()
            .or_else(|| std::env::var(env).ok())
            .filter(|item| !item.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("missing --{flag} or {env}"))
    };
    Ok(routes::account::BootstrapAdminRequest {
        email: value("email", "BOOTSTRAP_ADMIN_EMAIL")?,
        password: value("password", "BOOTSTRAP_ADMIN_PASSWORD")?,
        display_name: value("display_name", "BOOTSTRAP_ADMIN_DISPLAY_NAME")?,
        organization_name: value("organization_name", "BOOTSTRAP_ADMIN_ORGANIZATION")?,
    })
}

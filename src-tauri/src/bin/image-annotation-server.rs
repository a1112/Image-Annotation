use std::error::Error;

use image_annotation_lib::{project_fs, remote_server};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    init_tracing();

    let config = remote_server::ServerConfig::parse();
    config.validate()?;
    tokio::fs::create_dir_all(&config.data_dir).await?;
    project_fs::configure_workspace_data_root(config.data_dir.clone())
        .map_err(std::io::Error::other)?;

    let listener = TcpListener::bind(config.bind).await?;
    let local_addr = listener.local_addr()?;
    let app = remote_server::build_router(config)?;

    tracing::info!(%local_addr, "remote sample server listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("image_annotation=info,tower_http=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn shutdown_signal() {
    match tokio::signal::ctrl_c().await {
        Ok(()) => tracing::info!("shutdown signal received"),
        Err(error) => tracing::error!(%error, "failed to listen for shutdown signal"),
    }
}

use std::error::Error;

use image_annotation_lib::remote_server;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    init_tracing();

    let config = remote_server::ServerConfig::parse();
    let listener = TcpListener::bind(config.bind).await?;
    let local_addr = listener.local_addr()?;

    tracing::info!(%local_addr, "remote sample server listening");
    remote_server::serve(listener, config, remote_server::shutdown_signal()).await?;

    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("image_annotation=info,tower_http=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

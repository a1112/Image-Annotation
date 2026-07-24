use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_address: String,
    pub database_url: String,
    pub jwt_secret: String,
    pub s3_bucket: String,
    pub s3_endpoint: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            bind_address: std::env::var("BIND_ADDRESS")
                .unwrap_or_else(|_| "0.0.0.0:8080".to_string()),
            database_url: std::env::var("DATABASE_URL")
                .context("DATABASE_URL is required")?,
            jwt_secret: std::env::var("JWT_SECRET").context("JWT_SECRET is required")?,
            s3_bucket: std::env::var("S3_BUCKET")
                .unwrap_or_else(|_| "image-annotation".to_string()),
            s3_endpoint: std::env::var("S3_ENDPOINT").ok(),
        })
    }
}

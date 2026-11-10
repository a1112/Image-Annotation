use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_address: String,
    pub database_url: String,
    pub jwt_secret: String,
    pub s3_bucket: String,
    pub s3_endpoint: Option<String>,
    pub public_base_url: String,
    pub access_ttl_seconds: i64,
    pub refresh_ttl_seconds: i64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            bind_address: std::env::var("BIND_ADDRESS")
                .or_else(|_| std::env::var("SERVER_ADDR"))
                .unwrap_or_else(|_| "0.0.0.0:8080".to_string()),
            database_url: std::env::var("DATABASE_URL").context("DATABASE_URL is required")?,
            jwt_secret: std::env::var("JWT_SECRET").context("JWT_SECRET is required")?,
            s3_bucket: std::env::var("S3_BUCKET")
                .unwrap_or_else(|_| "image-annotation".to_string()),
            s3_endpoint: std::env::var("S3_ENDPOINT").ok(),
            public_base_url: std::env::var("PUBLIC_BASE_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string())
                .trim_end_matches('/')
                .to_string(),
            access_ttl_seconds: std::env::var("ACCESS_TTL_SECONDS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(900),
            refresh_ttl_seconds: std::env::var("REFRESH_TTL_SECONDS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(2_592_000),
        })
    }
}

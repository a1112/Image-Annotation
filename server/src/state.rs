use crate::config::Config;
use aws_sdk_s3::Client as S3Client;
use sqlx::{Pool, Postgres};

#[derive(Clone)]
pub struct AppState {
    pub pool: Pool<Postgres>,
    pub s3: S3Client,
    pub config: Config,
}

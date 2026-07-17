use std::{
    ffi::OsString,
    fmt,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use axum::http::HeaderValue;
use clap::Parser;

const DEFAULT_BIND: &str = "127.0.0.1:17311";
const DEFAULT_MAX_UPLOAD_MIB: &str = "2048";

#[derive(Clone, Parser)]
#[command(
    name = "image-annotation-server",
    version,
    about = "Authenticated remote sample service"
)]
pub struct ServerConfig {
    #[arg(long, env = "IMAGE_ANNOTATION_BIND", default_value = DEFAULT_BIND)]
    pub bind: SocketAddr,

    #[arg(
        long,
        env = "IMAGE_ANNOTATION_DATA_DIR",
        default_value_os_t = default_data_dir()
    )]
    pub data_dir: PathBuf,

    #[arg(long, env = "IMAGE_ANNOTATION_READER_TOKEN")]
    pub reader_token: Option<String>,

    #[arg(long, env = "IMAGE_ANNOTATION_EDITOR_TOKEN")]
    pub editor_token: Option<String>,

    #[arg(long, env = "IMAGE_ANNOTATION_ADMIN_TOKEN")]
    pub admin_token: Option<String>,

    #[arg(
        long = "allowed-origin",
        env = "IMAGE_ANNOTATION_ALLOWED_ORIGINS",
        value_delimiter = ',',
        value_name = "ORIGIN"
    )]
    pub allowed_origins: Vec<String>,

    #[arg(
        long = "max-upload-mib",
        env = "IMAGE_ANNOTATION_MAX_UPLOAD_MIB",
        default_value = DEFAULT_MAX_UPLOAD_MIB,
        value_parser = parse_upload_mib
    )]
    pub max_upload_bytes: usize,
}

impl ServerConfig {
    pub fn parse() -> Self {
        <Self as Parser>::parse()
    }

    pub fn try_parse_from<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        <Self as Parser>::try_parse_from(args)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let tokens = [
            self.configured_reader_token(),
            self.configured_editor_token(),
            self.configured_admin_token(),
        ];

        if !self.bind.ip().is_loopback() && tokens.iter().all(|token| token.is_none()) {
            return Err(ConfigError::new(
                "token_required_for_non_loopback_bind",
                "at least one role token is required for a non-loopback bind address",
            ));
        }

        for (index, token) in tokens.iter().enumerate() {
            if token.is_some() && tokens[index + 1..].contains(token) {
                return Err(ConfigError::new(
                    "duplicate_role_token",
                    "role tokens must be unique",
                ));
            }
        }

        if self.max_upload_bytes == 0 {
            return Err(ConfigError::new(
                "invalid_max_upload_bytes",
                "maximum upload size must be greater than zero",
            ));
        }

        for origin in &self.allowed_origins {
            if origin == "*" || origin.parse::<HeaderValue>().is_err() {
                return Err(ConfigError::new(
                    "invalid_allowed_origin",
                    "allowed origins must be explicit valid header values",
                ));
            }
        }

        Ok(())
    }

    pub(crate) fn configured_reader_token(&self) -> Option<&str> {
        configured_token(self.reader_token.as_deref())
    }

    pub(crate) fn configured_editor_token(&self) -> Option<&str> {
        configured_token(self.editor_token.as_deref())
    }

    pub(crate) fn configured_admin_token(&self) -> Option<&str> {
        configured_token(self.admin_token.as_deref())
    }
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerConfig")
            .field("bind", &self.bind)
            .field("data_dir", &self.data_dir)
            .field(
                "reader_token_configured",
                &self.configured_reader_token().is_some(),
            )
            .field(
                "editor_token_configured",
                &self.configured_editor_token().is_some(),
            )
            .field(
                "admin_token_configured",
                &self.configured_admin_token().is_some(),
            )
            .field("allowed_origins", &self.allowed_origins)
            .field("max_upload_bytes", &self.max_upload_bytes)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    code: &'static str,
    message: &'static str,
}

impl ConfigError {
    const fn new(code: &'static str, message: &'static str) -> Self {
        Self { code, message }
    }

    pub const fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ConfigError {}

fn configured_token(token: Option<&str>) -> Option<&str> {
    token.filter(|token| !token.trim().is_empty())
}

fn parse_upload_mib(value: &str) -> Result<usize, String> {
    let mib = value
        .parse::<usize>()
        .map_err(|_| "maximum upload size must be a positive integer in MiB".to_string())?;

    mib.checked_mul(1024 * 1024)
        .filter(|bytes| *bytes > 0)
        .ok_or_else(|| "maximum upload size is out of range".to_string())
}

fn default_data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")))
        .join("data")
        .join("workspaces")
        .join("default")
}

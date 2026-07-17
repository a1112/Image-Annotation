use axum::http::{header, HeaderMap};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::config::ServerConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Reader,
    Editor,
    Admin,
}

impl Role {
    pub const fn allows(self, required: Self) -> bool {
        self as u8 >= required as u8
    }
}

#[derive(Clone)]
pub(crate) struct TokenAuthenticator {
    credentials: Vec<Credential>,
}

#[derive(Clone)]
struct Credential {
    role: Role,
    digest: [u8; 32],
}

impl TokenAuthenticator {
    pub(crate) fn from_config(config: &ServerConfig) -> Self {
        let credentials = [
            (Role::Reader, config.configured_reader_token()),
            (Role::Editor, config.configured_editor_token()),
            (Role::Admin, config.configured_admin_token()),
        ]
        .into_iter()
        .filter_map(|(role, token)| {
            token.map(|token| Credential {
                role,
                digest: digest_token(token),
            })
        })
        .collect();

        Self { credentials }
    }

    pub(crate) fn authenticate(&self, headers: &HeaderMap) -> Option<Role> {
        let token = bearer_token(headers)?;
        let candidate = digest_token(token);
        let mut matched_role = None;

        for credential in &self.credentials {
            if bool::from(candidate.ct_eq(&credential.digest)) {
                matched_role = Some(credential.role);
            }
        }

        matched_role
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;

    (scheme.eq_ignore_ascii_case("Bearer") && !token.is_empty() && !token.contains(' '))
        .then_some(token)
}

fn digest_token(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

use crate::db::Repository;
use crate::error::{AppError, AppResult};
use crate::proxy::auth::{default_token_realms, UpstreamAuth};
use crate::server::AppState;

pub const MAX_GROUP_DEPTH: u32 = 5;

/// The member repository that owns cached bytes: the only name allowed in
/// cache keys, storage paths and `repository_id`.
#[derive(Clone, Copy, Debug)]
pub struct CacheRepo<'a>(pub &'a Repository);

#[derive(Debug)]
pub enum Outcome<T> {
    Found(T),
    NotFound,
}

#[derive(Clone, Debug)]
pub struct Upstream {
    pub base: reqwest::Url,
    pub auth: Option<UpstreamAuth>,
    pub token_realms: Vec<reqwest::Url>,
    pub dl_allow_private: bool,
}

impl Upstream {
    pub fn for_member(state: &AppState, member: &Repository) -> AppResult<Self> {
        let raw = member.upstream_url.as_deref().ok_or_else(|| {
            AppError::Internal(format!(
                "proxy repository {} has no upstream_url configured",
                member.name
            ))
        })?;
        let base = reqwest::Url::parse(raw).map_err(|e| {
            AppError::Internal(format!(
                "proxy repository {} has an invalid upstream_url: {e}",
                member.name
            ))
        })?;
        let creds = state
            .upstream_auth
            .get(&member.name)
            .cloned()
            .unwrap_or_default();
        let token_realms = if creds.token_realms.is_empty() {
            default_token_realms(&base)
        } else {
            creds.token_realms
        };
        Ok(Self {
            base,
            auth: creds.auth,
            token_realms,
            dl_allow_private: creds.dl_allow_private,
        })
    }
}

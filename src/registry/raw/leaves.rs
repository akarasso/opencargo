//! What one member of a walk answers for a raw path.

use super::upstream::{RawArtifact, RawUpstream};
use crate::domain::{CacheRepo, Format, Outcome};
use crate::policy::{self, Source};
use crate::proxy::{IntoPayload, Payload};
use crate::registry::resolve::{Cx, Leaf, ResolveError, Upstream};

pub const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";

pub struct FileLeaf {
    pub path: String,
}

#[async_trait::async_trait]
impl Leaf for FileLeaf {
    type Out = Payload;

    async fn hosted(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
    ) -> Result<Outcome<Payload>, ResolveError> {
        let Some(file) = cx.raw.file(member.0.id, &self.path).await? else {
            return Ok(Outcome::NotFound);
        };
        let mut payload = Payload::file(file.physical_key, file.size.max(0) as u64);
        payload.content_type = Some(
            file.content_type
                .unwrap_or_else(|| DEFAULT_CONTENT_TYPE.to_string()),
        );
        payload.digest = Some(file.sha256);
        Ok(Outcome::Found(payload))
    }

    async fn proxy(
        &self,
        cx: &Cx<'_>,
        member: CacheRepo<'_>,
        up: &Upstream,
    ) -> Result<Outcome<Payload>, ResolveError> {
        let artifact = RawArtifact {
            path: self.path.clone(),
        };
        let cached = cx.proxy.fetch(&RawUpstream, up, member, &artifact).await?;
        if let Outcome::Found(cached) = &cached {
            let digest = cached.entry.digest.clone();
            policy::record(cx, member, up, Format::Raw, &self.path, None, || Source::Raw {
                digest,
            });
        }
        Ok(cached.into_payload())
    }
}

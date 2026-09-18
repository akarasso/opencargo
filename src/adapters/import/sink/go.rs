//! go: the module zip off the source's GOPROXY endpoint, published under
//! the raw module path and version; every read on the target is escaped.

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::{Method, StatusCode, Url};

use crate::adapters::import::http::Gate;
use crate::adapters::import::target::{publish_outcome, Body, TReq, TargetLane};
use crate::domain::import::GapKind;
use crate::domain::Format;
use crate::ports::import::{CopyError, Copied, Gap, Origin, PkgExtra, Planned, Presence, Sink, VersionExtra};
use crate::registry::go::escape::{escape, unescape};

const TARGET_CAP: u64 = 100 << 20;

pub struct GoSink {
    gate: Arc<Gate>,
    lane: Arc<TargetLane>,
}

impl GoSink {
    pub fn new(gate: Arc<Gate>, lane: Arc<TargetLane>) -> Self {
        Self { gate, lane }
    }
}

#[async_trait]
impl Sink for GoSink {
    fn format(&self) -> Format {
        Format::Go
    }

    async fn present(&self, p: &Planned) -> Result<Presence, CopyError> {
        let url = self.lane.url(&format!(
            "{}/{}/@v/{}.info",
            p.target_repo,
            escape(&p.target_name),
            escape(&p.item.coord.version)
        ));
        let resp = self.lane.send(&TReq::new(Method::GET, url)).await?;
        match resp.status() {
            s if s.is_success() => Ok(Presence::Present),
            StatusCode::NOT_FOUND | StatusCode::GONE => Ok(Presence::Absent),
            _ => Err(publish_outcome(resp).await.err().unwrap_or(CopyError::Transient("target read".into()))),
        }
    }

    async fn copy(&self, p: &Planned) -> Result<Copied, CopyError> {
        let bad = |e: url::ParseError| CopyError::Permanent(e.to_string());
        let url = match &p.item.origin {
            Origin::Go { proxy, module, version } => Url::parse(proxy)
                .map_err(bad)?
                .join(&format!("{}/@v/{}.zip", escape(module), escape(version)))
                .map_err(bad)?,
            Origin::Asset { endpoint, repo, path } => Url::parse(&format!("{endpoint}{repo}/{path}")).map_err(bad)?,
            _ => return Err(CopyError::Permanent("go sink handed a non-go origin".into())),
        };
        let spooled = self.gate.spool(url, &p.item.want).await?;
        if spooled.size > TARGET_CAP {
            return Err(CopyError::TooLarge { size: spooled.size, limit: TARGET_CAP });
        }
        let module = unescape(&p.target_name);
        let version = unescape(&p.item.coord.version);
        let target = self.lane.url(&format!("{}/{module}/@v/{version}", p.target_repo));
        let body = Body::Framed {
            prefix: Default::default(),
            file: spooled.path.clone(),
            file_len: spooled.size,
            base64: false,
            suffix: Default::default(),
        };
        let req = TReq::new(Method::PUT, target).header("content-type", "application/zip").body(body).publish();
        publish_outcome(self.lane.send(&req).await?).await?;
        let note = spooled.unverified.then(|| {
            crate::domain::import::tagged(GapKind::CopiedUnverified, "the source announced no checksum for this module zip")
        });
        Ok(Copied { bytes: spooled.size, sha256: spooled.sha256.clone(), note, gaps: Vec::new() })
    }

    async fn seal(&self, _: &str, _: &str, _: &PkgExtra, _: &[(String, VersionExtra)]) -> Result<Vec<Gap>, CopyError> {
        Ok(Vec::new())
    }
}

//! Any registry speaking the OCI distribution protocol, opencargo included.
//! There is no catalogue in the spec, so the repositories are exactly the
//! ones `--source-repo` names: operator-scoped, hence complete.

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Url;

use crate::adapters::import::http::{link_next, FetchError, Gate, Req};
use crate::domain::import::GapKind;
use crate::ports::import::{
    redact, Coord, Cursor, Digests, Discovered, Gap, Item, Origin, PkgExtra, Probe, Source, SourceError,
    SourceFilter, SourceFormat, VersionExtra,
};

const PAGE: usize = 100;

pub struct Distribution {
    gate: Arc<Gate>,
}

impl Distribution {
    pub fn new(gate: Arc<Gate>) -> Self {
        Self { gate }
    }
}

/// `team/app` is `app` in namespace `team`; a bare name has none.
pub fn split_image(image: &str) -> (String, String) {
    match image.rsplit_once('/') {
        Some((ns, name)) => (ns.to_string(), name.to_string()),
        None => (String::new(), image.to_string()),
    }
}

pub fn oci_item(registry: &str, image: &str, tag: &str, digest: Option<String>) -> Item {
    let (ns, name) = split_image(image);
    Item {
        source_ref: match &digest {
            Some(d) => format!("oci:{registry}{image}:{tag}@{d}"),
            None => format!("oci:{registry}{image}:{tag}"),
        },
        format: SourceFormat::Oci,
        coord: Coord { repo: ns, name, version: tag.to_string() },
        published_at: None,
        size: None,
        want: Digests { sha256: digest, ..Default::default() },
        origin: Origin::Oci { registry: registry.to_string(), image: image.to_string(), reference: tag.to_string() },
        pkg: PkgExtra::default(),
        extra: VersionExtra::default(),
    }
}

#[async_trait]
impl Source for Distribution {
    fn kind(&self) -> &'static str {
        "distribution"
    }

    async fn probe(&self) -> Result<Probe, SourceError> {
        let url = self.gate.from().join("v2/").map_err(|e| SourceError::Refused(e.to_string()))?;
        let resp = self.gate.ok(&Req::get(url)).await?;
        let version = resp.headers().get("docker-distribution-api-version").and_then(|v| v.to_str().ok()).map(String::from);
        Ok(Probe { product: "distribution".into(), version, authenticated_as: None, capabilities: Vec::new() })
    }

    async fn streams(&self, f: &SourceFilter) -> Result<Vec<String>, SourceError> {
        if f.source_repos.is_empty() {
            return Err(SourceError::Refused(
                "the distribution protocol has no catalogue: name each repository with --source-repo".into(),
            ));
        }
        Ok(f.source_repos.clone())
    }

    async fn discover(
        &self,
        f: &SourceFilter,
        stream: &str,
        at: Cursor,
        out: &mut dyn Discovered,
    ) -> Result<(Cursor, bool), SourceError> {
        let bad = |e: url::ParseError| SourceError::Refused(e.to_string());
        let (last, page_no) = match at.as_deref().and_then(|c| c.split_once(' ')) {
            Some((n, last)) => (Some(last.to_string()), n.parse::<u32>().unwrap_or(0)),
            None => (None, 0),
        };
        let mut url: Url = self.gate.from().join(&format!("v2/{stream}/tags/list")).map_err(bad)?;
        url.query_pairs_mut().append_pair("n", &PAGE.to_string());
        if let Some(l) = &last {
            url.query_pairs_mut().append_pair("last", l);
        }
        if f.max_pages > 0 && page_no >= f.max_pages {
            out.gap(Gap::new(GapKind::ListingIncomplete, stream, format!("stopped after --max-pages {}", f.max_pages)));
            return Ok((at, true));
        }
        let resp = match self.gate.ok(&Req::get(url.clone()).header("accept", "application/json")).await {
            Ok(r) => r,
            Err(FetchError::NotFound(_)) => {
                out.gap(Gap::new(GapKind::ListingIncomplete, stream, "the source has no such repository"));
                return Ok((None, true));
            }
            Err(e) => return Err(e.into()),
        };
        let next = link_next(resp.headers(), &url);
        let doc: serde_json::Value = resp.json().await.map_err(|e| SourceError::Unavailable(e.without_url().to_string()))?;
        let registry = redact(self.gate.from());
        let tags: Vec<&str> = doc.get("tags").and_then(|t| t.as_array()).into_iter().flatten().filter_map(|t| t.as_str()).collect();
        for tag in &tags {
            out.item(oci_item(&registry, stream, tag, None));
        }
        let last = next
            .and_then(|n| n.query_pairs().find(|(k, _)| k == "last").map(|(_, v)| v.to_string()))
            .or_else(|| (tags.len() >= PAGE).then(|| tags.last().map(|t| t.to_string())).flatten());
        Ok(match last {
            Some(l) => (Some(format!("{} {l}", page_no + 1)), false),
            None => (None, true),
        })
    }
}

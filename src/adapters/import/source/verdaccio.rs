//! Verdaccio, or any npm registry serving `/-/v1/search`: the search is the
//! listing, a packument per package gives the versions.
//!
//! The stop is observed, never assumed: a short page ends the walk, a page
//! repeating the previous one is a clamp (Verdaccio >= 6.8 re-serves offset
//! 10 000) and says so, and a first page with nothing on it is not proof of
//! an empty registry.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Url;

use super::npm_items;
use crate::adapters::import::http::{FetchError, Gate, Req};
use crate::adapters::import::sink::npm_url;
use crate::domain::import::GapKind;
use crate::ports::import::{Cursor, Discovered, Gap, Probe, Source, SourceError, SourceFilter};

pub const REPO: &str = "verdaccio";
const WEB_DATA: &str = "verdaccio-web-data";

pub struct Verdaccio {
    gate: Arc<Gate>,
    page: usize,
}

impl Verdaccio {
    pub fn new(gate: Arc<Gate>) -> Self {
        Self { gate, page: 250 }
    }

    pub fn with_page(mut self, page: usize) -> Self {
        self.page = page.max(1);
        self
    }

    fn url(&self, path: &str) -> Result<Url, SourceError> {
        self.gate.from().join(path).map_err(|e| SourceError::Refused(e.to_string()))
    }

    async fn web_data(&self) -> Option<Vec<String>> {
        let url = self.url("-/verdaccio/data/packages").ok()?;
        let v = self.gate.get_json(url).await.ok()?;
        Some(
            v.as_array()?
                .iter()
                .filter_map(|p| p.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect(),
        )
    }
}

fn page_hash(names: &[String]) -> String {
    let mut h = DefaultHasher::new();
    names.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn parse_cursor(at: &Cursor) -> (usize, Option<String>) {
    match at.as_deref().and_then(|c| c.split_once(':')) {
        Some((from, hash)) => (from.parse().unwrap_or(0), Some(hash.to_string()).filter(|h| !h.is_empty())),
        None => (0, None),
    }
}

#[async_trait]
impl Source for Verdaccio {
    fn kind(&self) -> &'static str {
        "verdaccio"
    }

    async fn probe(&self) -> Result<Probe, SourceError> {
        let url = self.url("-/v1/search?text=&size=1&from=0")?;
        let resp = self.gate.ok(&Req::get(url).header("accept", "application/json")).await?;
        let version = resp
            .headers()
            .get("x-powered-by")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("verdaccio/"))
            .map(String::from);
        let mut capabilities = Vec::new();
        if self.web_data().await.is_some() {
            capabilities.push(WEB_DATA.to_string());
        }
        let authenticated_as = match self.url("-/whoami") {
            Ok(u) => self
                .gate
                .get_json(u)
                .await
                .ok()
                .and_then(|v| v.get("username").and_then(|u| u.as_str()).map(String::from)),
            Err(_) => None,
        };
        Ok(Probe { product: "verdaccio".into(), version, authenticated_as, capabilities })
    }

    async fn streams(&self, _f: &SourceFilter) -> Result<Vec<String>, SourceError> {
        Ok(vec!["search".into()])
    }

    async fn discover(
        &self,
        f: &SourceFilter,
        stream: &str,
        at: Cursor,
        out: &mut dyn Discovered,
    ) -> Result<(Cursor, bool), SourceError> {
        let (from, previous) = parse_cursor(&at);
        let page_no = from / self.page;
        if f.max_pages > 0 && page_no as u32 >= f.max_pages {
            out.gap(Gap::new(
                GapKind::ListingIncomplete,
                stream,
                format!("stopped after --max-pages {} pages of {}", f.max_pages, self.page),
            ));
            return Ok((at, true));
        }
        let url = self.url(&format!("-/v1/search?text=&size={}&from={from}", self.page))?;
        let doc = self.gate.get_json(url).await?;
        let names: Vec<String> = doc
            .get("objects")
            .and_then(|o| o.as_array())
            .map(|objs| {
                objs.iter()
                    .filter_map(|o| o.pointer("/package/name").and_then(|n| n.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let hash = page_hash(&names);
        if !names.is_empty() && previous.as_deref() == Some(hash.as_str()) {
            out.gap(Gap::new(
                GapKind::ListingIncomplete,
                stream,
                format!("the search re-served the same page at offset {from}: the source clamps its listing there"),
            ));
            return Ok((at, true));
        }
        if names.is_empty() && from == 0 {
            match self.web_data().await {
                Some(all) if all.is_empty() => return Ok((None, true)),
                _ => {
                    out.gap(Gap::new(
                        GapKind::ListingIncomplete,
                        stream,
                        "an empty search listed nothing, and nothing proves the source empty",
                    ));
                    return Ok((None, true));
                }
            }
        }
        for name in &names {
            let url = npm_url(self.gate.from(), name);
            match self.gate.json(&Req::get(url).header("accept", "application/json"), 64 << 20).await {
                Ok(p) => {
                    for it in npm_items(self.gate.from(), REPO, &p) {
                        out.item(it);
                    }
                }
                Err(FetchError::Auth(m)) => return Err(SourceError::Auth(m)),
                Err(e) => out.gap(Gap::new(GapKind::ListingIncomplete, format!("{REPO}/{name}"), format!("packument unreadable: {e}"))),
            }
        }
        let done = names.len() < self.page;
        Ok((Some(format!("{}:{hash}", from + names.len())), done))
    }
}

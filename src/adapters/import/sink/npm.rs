//! npm: the source's full packument is the publish body's authority; the
//! tarball rides as a base64 attachment streamed from the spool.

use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use reqwest::{Method, StatusCode, Url};
use serde_json::{json, Value};

use super::{npm_url, tarball_name};
use crate::adapters::import::http::{FetchError, Gate, Req};
use crate::adapters::import::target::{base64_len, body_json, publish_outcome, Body, TReq, TargetLane};
use crate::domain::import::GapKind;
use crate::domain::Format;
use crate::ports::import::{
    CopyError, Copied, Gap, Origin, PkgExtra, Planned, Presence, Sink, VersionExtra,
};

const MAX_README: u64 = 256 * 1024;

pub struct NpmSink {
    gate: Arc<Gate>,
    lane: Arc<TargetLane>,
    max_body: u64,
}

impl NpmSink {
    pub fn new(gate: Arc<Gate>, lane: Arc<TargetLane>, max_body: u64) -> Self {
        Self { gate, lane, max_body }
    }

    fn target(&self, repo: &str, name: &str) -> Url {
        self.lane.url(&format!("{repo}/{name}"))
    }
}

/// `package/package.json` and the first `package/README*` of a tarball.
pub fn read_tarball(path: &std::path::Path) -> (Option<Value>, Option<String>) {
    let Ok(file) = std::fs::File::open(path) else { return (None, None) };
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let (mut manifest, mut readme) = (None, None);
    let Ok(entries) = archive.entries() else { return (None, None) };
    for entry in entries.flatten() {
        let Ok(p) = entry.path().map(|p| p.to_string_lossy().to_string()) else { continue };
        let Some((_, rel)) = p.split_once('/') else { continue };
        let is_manifest = rel == "package.json" && manifest.is_none();
        let is_readme = !rel.contains('/') && rel.to_ascii_lowercase().starts_with("readme") && readme.is_none();
        if !(is_manifest || is_readme) {
            continue;
        }
        let mut s = String::new();
        if entry.take(MAX_README).read_to_string(&mut s).is_err() {
            continue;
        }
        if is_manifest {
            manifest = serde_json::from_str(&s).ok();
        } else {
            readme = Some(s);
        }
        if manifest.is_some() && readme.is_some() {
            break;
        }
    }
    (manifest, readme)
}

fn non_empty(v: Option<&Value>) -> Option<String> {
    v.and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()).map(String::from)
}

/// The JSON publish body around a placeholder for the base64 data, split so
/// the attachment streams between the two halves.
fn frame(body: &Value, marker: &str) -> Result<(Bytes, Bytes), CopyError> {
    let text = serde_json::to_string(body).map_err(|e| CopyError::Permanent(e.to_string()))?;
    let quoted = format!("\"{marker}\"");
    let (prefix, suffix) = text
        .split_once(&quoted)
        .ok_or_else(|| CopyError::Permanent("publish body lost its attachment".into()))?;
    Ok((Bytes::from(format!("{prefix}\"")), Bytes::from(format!("\"{suffix}"))))
}

#[async_trait]
impl Sink for NpmSink {
    fn format(&self) -> Format {
        Format::Npm
    }

    async fn present(&self, p: &Planned) -> Result<Presence, CopyError> {
        let url = self.target(&p.target_repo, &p.target_name);
        let resp = self.lane.send(&TReq::new(Method::GET, url).header("accept", "application/json")).await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(Presence::Absent);
        }
        if !resp.status().is_success() {
            return Err(publish_outcome(resp).await.err().unwrap_or(CopyError::Transient("target read".into())));
        }
        let doc = body_json(resp, 64 << 20).await?;
        let Some(v) = doc.get("versions").and_then(|v| v.get(&p.item.coord.version)) else {
            return Ok(Presence::Absent);
        };
        let have = v.pointer("/dist/shasum").and_then(|s| s.as_str()).unwrap_or_default();
        Ok(match &p.item.want.sha1 {
            Some(want) if want.eq_ignore_ascii_case(have) => Presence::Same,
            Some(_) => Presence::Different(format!("sha1 {have}")),
            None => Presence::Present,
        })
    }

    async fn copy(&self, p: &Planned) -> Result<Copied, CopyError> {
        let Origin::Npm { registry, package } = &p.item.origin else {
            return Err(CopyError::Permanent("npm sink handed a non-npm origin".into()));
        };
        let registry = Url::parse(registry).map_err(|e| CopyError::Permanent(e.to_string()))?;
        let version = &p.item.coord.version;
        let packument_url = npm_url(&registry, package);
        let packument = match self.gate.json(&Req::get(packument_url.clone()).header("accept", "application/json"), 64 << 20).await {
            Ok(v) => Some(v),
            Err(FetchError::NotFound(_)) => None,
            Err(e) => return Err(e.into()),
        };
        let listed = packument.as_ref().and_then(|d| d.get("versions")).and_then(|v| v.get(version)).cloned();
        let tarball = listed
            .as_ref()
            .and_then(|v| v.pointer("/dist/tarball"))
            .and_then(|t| t.as_str())
            .and_then(|t| Url::parse(t).ok())
            .unwrap_or_else(|| {
                let base = npm_url(&registry, package);
                Url::parse(&format!("{base}/-/{}", tarball_name(package, version))).unwrap_or(base)
            });
        let live = |ptr: &str| listed.as_ref().and_then(|v| v.pointer(ptr)).and_then(|s| s.as_str()).map(String::from);
        let want = match (live("/dist/shasum"), live("/dist/integrity")) {
            (None, None) => p.item.want.clone(),
            (sha1, integrity) => crate::ports::import::Digests { sha1, integrity, ..Default::default() },
        };
        let spooled = self.gate.spool(tarball, &want).await?;
        let path: PathBuf = spooled.path.clone();
        let (manifest, tar_readme) = tokio::task::spawn_blocking(move || read_tarball(&path))
            .await
            .map_err(|e| CopyError::Permanent(e.to_string()))?;
        let Some(mut meta) = listed.or(manifest) else {
            return Err(CopyError::Permanent(format!(
                "{} lists no version {version} and its tarball carries no package.json: nothing to publish its metadata from",
                crate::ports::import::redact(&packument_url)
            )));
        };
        if let Some(obj) = meta.as_object_mut() {
            obj.insert("name".into(), json!(p.target_name));
            obj.insert("version".into(), json!(version));
            let dist = obj.entry("dist").or_insert_with(|| json!({}));
            if let Some(d) = dist.as_object_mut() {
                d.remove("tarball");
                d.insert("shasum".into(), json!(spooled.sha1));
                d.insert("integrity".into(), json!(spooled.integrity()));
            }
        }
        let top = packument.as_ref();
        let description = non_empty(top.and_then(|d| d.get("description"))).or_else(|| non_empty(meta.get("description")));
        let readme = non_empty(top.and_then(|d| d.get("readme")))
            .or_else(|| non_empty(meta.get("readme")))
            .or(tar_readme.filter(|r| !r.trim().is_empty()));
        let mut gaps = Vec::new();
        if description.is_none() && readme.is_none() {
            gaps.push(Gap::new(
                GapKind::SourceOnlyFeature,
                format!("{}/{}", p.item.coord.repo, p.item.coord.name),
                "the source has neither a description nor a README for this package",
            ));
        }
        let marker = format!("opencargo-import-{}", uuid::Uuid::new_v4());
        let key = tarball_name(&p.target_name, version);
        let body = json!({
            "_id": p.target_name,
            "name": p.target_name,
            "description": description,
            "readme": readme,
            "dist-tags": {},
            "versions": { version: meta },
            "_attachments": { key: { "content_type": "application/octet-stream", "length": spooled.size, "data": marker } },
        });
        let (prefix, suffix) = frame(&body, &marker)?;
        let total = prefix.len() as u64 + base64_len(spooled.size) + suffix.len() as u64;
        if total > self.max_body {
            let framing = (prefix.len() + suffix.len()) as u64;
            let limit = self.max_body.saturating_sub(framing) / 4 * 3;
            return Err(CopyError::TooLarge { size: spooled.size, limit });
        }
        let req = TReq::new(Method::PUT, self.target(&p.target_repo, &p.target_name))
            .header("content-type", "application/json")
            .body(Body::Framed { prefix, file: spooled.path.clone(), file_len: spooled.size, base64: true, suffix })
            .publish();
        publish_outcome(self.lane.send(&req).await?).await?;
        let note = spooled.unverified.then(|| {
            crate::domain::import::tagged(GapKind::CopiedUnverified, "the source announced no checksum for this tarball")
        });
        Ok(Copied { bytes: spooled.size, sha256: spooled.sha256.clone(), note, gaps })
    }

    async fn seal(
        &self,
        repo: &str,
        name: &str,
        pkg: &PkgExtra,
        versions: &[(String, VersionExtra)],
    ) -> Result<Vec<Gap>, CopyError> {
        let mut gaps = Vec::new();
        for (tag, version) in &pkg.dist_tags {
            if !versions.iter().any(|(v, _)| v == version) {
                gaps.push(Gap::new(
                    GapKind::SourceOnlyFeature,
                    format!("{repo}/{name}"),
                    format!("dist-tag {tag} points at {version}, which was not copied"),
                ));
                continue;
            }
            let url = self.lane.url(&format!("{repo}/-/package/{name}/dist-tags/{tag}"));
            let req = TReq::new(Method::PUT, url)
                .header("content-type", "application/json")
                .body(Body::Bytes(Bytes::from(json!(version).to_string())));
            publish_outcome(self.lane.send(&req).await?).await?;
        }
        Ok(gaps)
    }
}

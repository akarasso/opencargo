//! OCI: manifests and blobs over the distribution protocol, blob by blob,
//! the source stream piped into chunked PATCHes, never spooled whole. A tag
//! is a mutable pointer, so a moved one is re-pointed and annotated.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use reqwest::{Method, StatusCode, Url};
use serde_json::Value;
use sha2::Digest as _;

use crate::adapters::import::http::{FetchError, Gate, Req};
use crate::adapters::import::target::{error_text, Body, TReq, TargetLane};
use crate::domain::Format;
use crate::ports::import::{CopyError, Copied, Gap, Origin, PkgExtra, Planned, Presence, Sink, VersionExtra};

pub const MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json";
const MAX_MANIFEST: u64 = 4 << 20;
const MAX_RESTARTS: u32 = 3;

pub struct OciSink {
    gate: Arc<Gate>,
    lane: Arc<TargetLane>,
    chunk: u64,
    pushed: Mutex<HashSet<String>>,
    orphaned: AtomicU64,
}

impl OciSink {
    pub fn new(gate: Arc<Gate>, lane: Arc<TargetLane>, chunk: u64) -> Self {
        Self { gate, lane, chunk: chunk.max(1), pushed: Mutex::new(HashSet::new()), orphaned: AtomicU64::new(0) }
    }

    /// Bytes appended to uploads the sink abandoned, which the target
    /// keeps until its own sweep.
    pub fn orphaned_bytes(&self) -> u64 {
        self.orphaned.load(Ordering::SeqCst)
    }
}

fn sha256(data: &[u8]) -> String {
    format!("sha256:{}", sha2::Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect::<String>())
}

struct Source<'a> {
    registry: Url,
    image: &'a str,
}

impl Source<'_> {
    fn url(&self, kind: &str, reference: &str) -> Result<Url, CopyError> {
        self.registry
            .join(&format!("v2/{}/{kind}/{reference}", self.image))
            .map_err(|e| CopyError::Permanent(e.to_string()))
    }
}

struct Dest<'a> {
    repo: &'a str,
    name: &'a str,
}

impl Dest<'_> {
    fn path(&self, rest: &str) -> String {
        format!("v2/{}/{}/{rest}", self.repo, self.name)
    }
}

fn header(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers().get(name).and_then(|v| v.to_str().ok()).map(String::from)
}

/// The offset an upload's `Range: 0-{end}` reports: `0-0` means nothing,
/// since no chunk is ever a single byte.
fn offset_of(range: Option<String>) -> Option<u64> {
    let end: u64 = range?.trim().strip_prefix("0-")?.parse().ok()?;
    Some(if end == 0 { 0 } else { end + 1 })
}

enum Chunked {
    Done,
    /// Start over from a fresh upload.
    Restart(String),
}

impl OciSink {
    async fn source_manifest(&self, src: &Source<'_>, reference: &str) -> Result<(Bytes, String, String), CopyError> {
        let resp = self.gate.ok(&Req::get(src.url("manifests", reference)?).header("accept", MANIFEST_TYPES)).await?;
        let media = header(&resp, "content-type").unwrap_or_else(|| "application/vnd.oci.image.manifest.v1+json".into());
        let url = src.url("manifests", reference)?;
        let body = crate::adapters::import::http::read_capped(resp, MAX_MANIFEST, &url).await?;
        let digest = sha256(&body);
        if reference.starts_with("sha256:") && reference != digest {
            return Err(CopyError::Permanent(format!("manifest {reference} hashes to {digest}")));
        }
        Ok((body, media.split(';').next().unwrap_or_default().trim().to_string(), digest))
    }

    async fn target_digest(&self, dst: &Dest<'_>, reference: &str) -> Result<Option<String>, CopyError> {
        let req = TReq::new(Method::HEAD, self.lane.url(&dst.path(&format!("manifests/{reference}")))).header("accept", MANIFEST_TYPES);
        let resp = self.lane.send(&req).await?;
        match resp.status() {
            s if s.is_success() => Ok(header(&resp, "docker-content-digest")),
            StatusCode::NOT_FOUND => Ok(None),
            _ => Err(CopyError::Transient(error_text(resp).await)),
        }
    }

    async fn has_blob(&self, dst: &Dest<'_>, digest: &str) -> Result<bool, CopyError> {
        let resp = self.lane.send(&TReq::new(Method::HEAD, self.lane.url(&dst.path(&format!("blobs/{digest}"))))).await?;
        Ok(resp.status().is_success())
    }

    async fn start(&self, dst: &Dest<'_>) -> Result<(Url, u64), CopyError> {
        let resp = self.lane.send(&TReq::new(Method::POST, self.lane.url(&dst.path("blobs/uploads/")))).await?;
        if resp.status() != StatusCode::ACCEPTED {
            let status = resp.status();
            let text = error_text(resp).await;
            return Err(if status == StatusCode::FORBIDDEN || status == StatusCode::UNAUTHORIZED {
                CopyError::Refused(text)
            } else {
                CopyError::Permanent(format!("starting an upload: {text}"))
            });
        }
        let min: u64 = header(&resp, "oci-chunk-min-length").and_then(|v| v.parse().ok()).unwrap_or(0);
        let location = header(&resp, "location").ok_or_else(|| CopyError::Permanent("upload without a Location".into()))?;
        let url = self.lane.base().join(&location).map_err(|e| CopyError::Permanent(e.to_string()))?;
        Ok((url, min))
    }

    async fn status(&self, upload: &Url) -> Result<Option<u64>, CopyError> {
        let resp = self.lane.send(&TReq::new(Method::GET, upload.clone())).await?;
        Ok(if resp.status().is_success() { offset_of(header(&resp, "range")) } else { None })
    }

    /// One pass: a fresh upload fed from a fresh source stream.
    async fn push_once(
        &self,
        src: &Source<'_>,
        dst: &Dest<'_>,
        digest: &str,
        chunk: u64,
    ) -> Result<Chunked, CopyError> {
        let (mut upload, min) = self.start(dst).await?;
        let chunk = chunk.max(min).max(1) as usize;
        let resp = self.gate.ok(&Req::get(src.url("blobs", digest)?)).await?;
        let mut stream = resp.bytes_stream();
        let mut hasher = sha2::Sha256::new();
        let (mut sent, mut buf, mut ended) = (0u64, BytesMut::new(), false);
        let limit = self.gate.max_artifact_size();
        loop {
            while buf.len() < chunk && !ended {
                match stream.next().await {
                    Some(Ok(b)) => {
                        hasher.update(&b);
                        buf.extend_from_slice(&b);
                        if sent + buf.len() as u64 > limit {
                            return Err(CopyError::TooLarge { size: sent + buf.len() as u64, limit });
                        }
                    }
                    Some(Err(e)) => return Err(CopyError::Transient(format!("source blob {digest}: {}", e.without_url()))),
                    None => ended = true,
                }
            }
            if buf.is_empty() {
                break;
            }
            let piece = buf.split_to(buf.len().min(chunk)).freeze();
            let end = sent + piece.len() as u64 - 1;
            let req = TReq::new(Method::PATCH, upload.clone())
                .header("content-type", "application/octet-stream")
                .header("content-range", format!("{sent}-{end}"))
                .body(Body::Bytes(piece.clone()));
            let outcome = self.lane.send(&req).await;
            let resp = match outcome {
                Ok(r) => r,
                Err(CopyError::Transient(m)) => match self.status(&upload).await? {
                    Some(at) if at == end + 1 => {
                        sent = at;
                        continue;
                    }
                    Some(at) if at == sent => {
                        buf = { let mut b = BytesMut::from(&piece[..]); b.extend_from_slice(&buf); b };
                        continue;
                    }
                    _ => {
                        self.orphaned.fetch_add(end + 1, Ordering::SeqCst);
                        return Ok(Chunked::Restart(m));
                    }
                },
                Err(e) => return Err(e),
            };
            match resp.status() {
                StatusCode::ACCEPTED | StatusCode::NO_CONTENT | StatusCode::OK => {
                    if let Some(l) = header(&resp, "location") {
                        upload = self.lane.base().join(&l).unwrap_or(upload);
                    }
                    sent = end + 1;
                }
                StatusCode::RANGE_NOT_SATISFIABLE => {
                    let at = offset_of(header(&resp, "range"));
                    if at == Some(end + 1) {
                        sent = end + 1;
                    } else if at == Some(sent) {
                        buf = { let mut b = BytesMut::from(&piece[..]); b.extend_from_slice(&buf); b };
                    } else {
                        self.orphaned.fetch_add(sent, Ordering::SeqCst);
                        return Ok(Chunked::Restart(format!("the upload is at {at:?}, the sink at {sent}")));
                    }
                }
                StatusCode::NOT_FOUND => {
                    self.orphaned.fetch_add(sent, Ordering::SeqCst);
                    return Ok(Chunked::Restart("the target lost the upload".into()));
                }
                StatusCode::PAYLOAD_TOO_LARGE => {
                    self.orphaned.fetch_add(end + 1, Ordering::SeqCst);
                    return Ok(Chunked::Restart("too many segments".into()));
                }
                StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => return Err(CopyError::Refused(error_text(resp).await)),
                s if s.is_server_error() => {
                    self.orphaned.fetch_add(end + 1, Ordering::SeqCst);
                    return Ok(Chunked::Restart(error_text(resp).await));
                }
                _ => return Err(CopyError::Permanent(format!("uploading {digest}: {}", error_text(resp).await))),
            }
        }
        let computed = format!("sha256:{}", hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>());
        if computed != digest {
            return Err(CopyError::Permanent(format!("checksum mismatch: the source served {computed} for blob {digest}")));
        }
        let mut done = upload.clone();
        done.query_pairs_mut().append_pair("digest", digest);
        let resp = self.lane.send(&TReq::new(Method::PUT, done)).await?;
        match resp.status() {
            StatusCode::CREATED | StatusCode::OK | StatusCode::NO_CONTENT => Ok(Chunked::Done),
            StatusCode::NOT_FOUND => Ok(Chunked::Restart("the target lost the upload at completion".into())),
            s if s.is_server_error() => Ok(Chunked::Restart(error_text(resp).await)),
            StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => Err(CopyError::Refused(error_text(resp).await)),
            _ => Err(CopyError::Permanent(format!("completing {digest}: {}", error_text(resp).await))),
        }
    }

    async fn push_blob(&self, src: &Source<'_>, dst: &Dest<'_>, digest: &str) -> Result<u64, CopyError> {
        if self.has_blob(dst, digest).await? {
            return Ok(0);
        }
        let mut chunk = self.chunk;
        for restart in 0..=MAX_RESTARTS {
            match self.push_once(src, dst, digest, chunk).await? {
                Chunked::Done => return Ok(1),
                Chunked::Restart(why) if restart < MAX_RESTARTS => {
                    tracing::warn!(blob = %digest, restart = restart + 1, "restarting the upload: {why}");
                    chunk = chunk.saturating_mul(2);
                }
                Chunked::Restart(why) => {
                    return Err(CopyError::Permanent(format!("blob {digest} failed after {MAX_RESTARTS} restarts: {why}")))
                }
            }
        }
        unreachable!("the loop returns on its last pass")
    }

    /// Copies one manifest and everything it references, children first,
    /// and puts it under `reference`.
    async fn push_manifest(&self, src: &Source<'_>, dst: &Dest<'_>, reference: &str) -> Result<(String, u64), CopyError> {
        let (body, media, digest) = self.source_manifest(src, reference).await?;
        let key = format!("{}/{}@{digest}", dst.repo, dst.name);
        let already = self.pushed.lock().unwrap_or_else(|p| p.into_inner()).contains(&key);
        let mut bytes = 0u64;
        if !already {
            let doc: Value = serde_json::from_slice(&body).map_err(|e| CopyError::Permanent(format!("manifest {digest}: {e}")))?;
            if let Some(children) = doc.get("manifests").and_then(|m| m.as_array()) {
                for child in children {
                    let Some(d) = child.get("digest").and_then(|d| d.as_str()) else { continue };
                    let (_, n) = Box::pin(self.push_manifest(src, dst, d)).await?;
                    bytes += n;
                }
            }
            let mut blobs: Vec<(String, u64)> = Vec::new();
            for desc in doc.get("config").into_iter().chain(doc.get("layers").and_then(|l| l.as_array()).into_iter().flatten()) {
                if let Some(d) = desc.get("digest").and_then(|d| d.as_str()) {
                    blobs.push((d.to_string(), desc.get("size").and_then(|s| s.as_u64()).unwrap_or(0)));
                }
            }
            for (d, size) in blobs {
                if size > self.gate.max_artifact_size() {
                    return Err(CopyError::TooLarge { size, limit: self.gate.max_artifact_size() });
                }
                if self.push_blob(src, dst, &d).await? > 0 {
                    bytes += size;
                }
            }
        }
        let req = TReq::new(Method::PUT, self.lane.url(&dst.path(&format!("manifests/{reference}"))))
            .header("content-type", media)
            .body(Body::Bytes(body.clone()));
        let resp = self.lane.send(&req).await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = error_text(resp).await;
            return Err(match status {
                StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => CopyError::Refused(text),
                s if s.is_server_error() => CopyError::Transient(text),
                _ => CopyError::Permanent(format!("putting manifest {reference}: {text}")),
            });
        }
        self.pushed.lock().unwrap_or_else(|p| p.into_inner()).insert(key);
        Ok((digest, bytes + body.len() as u64))
    }

    fn endpoints<'a>(&self, p: &'a Planned) -> Result<(Source<'a>, Dest<'a>, &'a str), CopyError> {
        let Origin::Oci { registry, image, reference } = &p.item.origin else {
            return Err(CopyError::Permanent("oci sink handed a non-oci origin".into()));
        };
        let registry = Url::parse(registry).map_err(|e| CopyError::Permanent(e.to_string()))?;
        Ok((Source { registry, image }, Dest { repo: &p.target_repo, name: &p.target_name }, reference))
    }

    async fn source_digest(&self, src: &Source<'_>, reference: &str) -> Result<String, CopyError> {
        let resp = match self.gate.ok(&Req::head(src.url("manifests", reference)?).header("accept", MANIFEST_TYPES)).await {
            Ok(r) => r,
            Err(FetchError::NotFound(u)) => return Err(CopyError::Permanent(format!("{u}: gone from the source"))),
            Err(e) => return Err(e.into()),
        };
        match header(&resp, "docker-content-digest") {
            Some(d) => Ok(d),
            None => Ok(self.source_manifest(src, reference).await?.2),
        }
    }
}

#[async_trait]
impl Sink for OciSink {
    fn format(&self) -> Format {
        Format::Oci
    }

    async fn present(&self, p: &Planned) -> Result<Presence, CopyError> {
        let (src, dst, reference) = self.endpoints(p)?;
        let Some(have) = self.target_digest(&dst, &p.item.coord.version).await? else {
            return Ok(Presence::Absent);
        };
        let want = match &p.item.want.sha256 {
            Some(d) => d.clone(),
            None => self.source_digest(&src, reference).await?,
        };
        Ok(if want == have { Presence::Same } else { Presence::Different(have) })
    }

    async fn copy(&self, p: &Planned) -> Result<Copied, CopyError> {
        let (src, dst, reference) = self.endpoints(p)?;
        let before = self.target_digest(&dst, &p.item.coord.version).await?;
        let orphaned = self.orphaned_bytes();
        let (digest, bytes) = self.push_manifest(&src, &dst, reference).await?;
        if reference != p.item.coord.version {
            let (body, media, _) = self.source_manifest(&src, reference).await?;
            let req = TReq::new(Method::PUT, self.lane.url(&dst.path(&format!("manifests/{}", p.item.coord.version))))
                .header("content-type", media)
                .body(Body::Bytes(body));
            let resp = self.lane.send(&req).await?;
            if !resp.status().is_success() {
                return Err(CopyError::Permanent(error_text(resp).await));
            }
        }
        let mut notes = Vec::new();
        if let Some(old) = before.filter(|old| *old != digest) {
            notes.push(format!("Retagged {old} -> {digest}"));
        }
        let lost = self.orphaned_bytes() - orphaned;
        if lost > 0 {
            notes.push(format!("{lost} bytes left on abandoned target uploads"));
        }
        Ok(Copied { bytes, sha256: digest, note: (!notes.is_empty()).then(|| notes.join("; ")), gaps: Vec::new() })
    }

    async fn seal(&self, _: &str, _: &str, _: &PkgExtra, _: &[(String, VersionExtra)]) -> Result<Vec<Gap>, CopyError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_byte_range_resumes_from_zero_not_one() {
        assert_eq!(offset_of(Some("0-0".into())), Some(0));
        assert_eq!(offset_of(Some("0-1023".into())), Some(1024));
        assert_eq!(offset_of(None), None);
    }
}

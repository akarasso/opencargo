use std::time::Instant;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use crate::proxy::engine::Cached;
use crate::registry::oci::upstream::{upstream_name, OciArtifact, OciUpstream};
use crate::registry::resolve::{CacheRepo, Upstream};

use super::super::rules::PolicyConfig;
use super::super::{Pending, Shared, Source};
use super::{fetch_or_peek, parse_time, read_json};

const INDEX_TYPES: [&str; 2] = [
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
];
const CREATED: &str = "org.opencontainers.image.created";
const ATTESTATION: &str = "attestation-manifest";

/// The blob request is made under the upstream's own name (`library/nginx`
/// on Hub), never the client's.
pub fn oci_blob(up: &Upstream, name: &str, digest: &str) -> OciArtifact {
    OciArtifact::Blob {
        name: upstream_name(up, name),
        digest: digest.to_string(),
    }
}

fn is_index(json: &Value) -> bool {
    match json["mediaType"].as_str() {
        Some(media_type) => INDEX_TYPES.contains(&media_type),
        None => json["manifests"].is_array() && json.get("config").is_none(),
    }
}

/// The child digests of an index body, bare hex, attestations skipped.
pub fn oci_children(body: &[u8]) -> Vec<String> {
    match serde_json::from_slice::<Value>(body) {
        Ok(json) => children_of(&json),
        Err(_) => Vec::new(),
    }
}

fn children_of(json: &Value) -> Vec<String> {
    if !is_index(json) {
        return Vec::new();
    }
    json["manifests"]
        .as_array()
        .map(|manifests| {
            manifests
                .iter()
                .filter(|m| {
                    m["annotations"]["vnd.docker.reference.type"].as_str() != Some(ATTESTATION)
                })
                .filter_map(|m| m["digest"].as_str())
                .map(|d| d.trim_start_matches("sha256:").to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Writer-inline, in arrival order: an index is parked, a manifest that
/// spends a parked index's count releases it carrying the manifest as
/// `served`, a sibling of an already released index is suppressed, and a
/// manifest nobody parked records on its own. The body is read and
/// parsed here once; the event leaves with the parsed body it is dated
/// from.
pub(crate) async fn oci_classify(shared: &Shared, mut p: Pending) -> Option<Pending> {
    let Source::Oci { body, .. } = &p.source else {
        return Some(p);
    };
    let Ok(bytes) = shared.proxy.bytes(body).await else {
        return Some(p);
    };
    let Ok(json) = serde_json::from_slice::<Value>(&bytes) else {
        return Some(p);
    };
    let children = children_of(&json);
    if !children.is_empty() {
        park(shared, p, children);
        return None;
    }
    let Some(hex) = body.entry.digest.clone() else {
        return Some(p);
    };
    let served = body.clone();
    match pop_recent(shared, (p.member.id, p.name.clone(), hex)) {
        None => {
            attach(&mut p, None, json);
            Some(p)
        }
        Some(seq) => release(shared, seq, served, json),
    }
}

fn attach(p: &mut Pending, child: Option<Cached>, json: Value) {
    if let Source::Oci { served, parsed, .. } = &mut p.source {
        *served = child;
        *parsed = Some(json);
    }
}

fn park(shared: &Shared, p: Pending, children: Vec<String>) {
    let seq = shared.seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let now = Instant::now();
    let mut recent = shared.recent_children.lock().unwrap();
    for child in children {
        recent
            .entry((p.member.id, p.name.clone(), child))
            .or_default()
            .push_back((seq, now));
    }
    drop(recent);
    shared.parked.lock().unwrap().insert(seq, (p, now));
}

/// Expiry is decided here from each entry's own stamp, never by the sweep.
/// The first entry whose index is still parked is spent; when none is, the
/// oldest is (a sibling of an already released pull, `skopeo copy --all`).
fn pop_recent(shared: &Shared, key: super::super::ChildKey) -> Option<u64> {
    let ttl = shared.tuning.child_ttl;
    let mut recent = shared.recent_children.lock().unwrap();
    let queue = recent.get_mut(&key)?;
    queue.retain(|(_, at)| at.elapsed() < ttl);
    let pick = {
        let parked = shared.parked.lock().unwrap();
        queue
            .iter()
            .position(|(seq, _)| parked.contains_key(seq))
            .unwrap_or(0)
    };
    let popped = queue.remove(pick).map(|(seq, _)| seq);
    if queue.is_empty() {
        recent.remove(&key);
    }
    popped
}

fn release(shared: &Shared, seq: u64, served: Cached, json: Value) -> Option<Pending> {
    let (mut index, _) = shared.parked.lock().unwrap().remove(&seq)?;
    supersede(shared, index.member.id, &index.name, seq);
    attach(&mut index, Some(served), json);
    Some(index)
}

/// A release ends every older pull of the same image: their unspent
/// children are no longer the first GET after an index, so a later bare
/// digest pull records on its own, while the siblings of the pull just
/// released (`skopeo copy --all`) stay suppressed.
fn supersede(shared: &Shared, member_id: i64, name: &str, seq: u64) {
    let mut recent = shared.recent_children.lock().unwrap();
    recent.retain(|(member, image, _), queue| {
        if *member == member_id && image == name {
            queue.retain(|(s, _)| *s >= seq);
        }
        !queue.is_empty()
    });
}

/// Indexes parked past `child_ttl`, released with no served child.
pub(crate) fn release_parked(shared: &Shared) -> Vec<Pending> {
    let ttl = shared.tuning.child_ttl;
    let mut parked = shared.parked.lock().unwrap();
    let due: Vec<u64> = parked
        .iter()
        .filter(|(_, (_, at))| at.elapsed() >= ttl)
        .map(|(seq, _)| *seq)
        .collect();
    due.into_iter()
        .filter_map(|seq| parked.remove(&seq).map(|(p, _)| p))
        .collect()
}

/// Hygiene for keys no child ever spent.
pub(crate) fn sweep_children(shared: &Shared) {
    let ttl = shared.tuning.child_ttl;
    let mut recent = shared.recent_children.lock().unwrap();
    recent.retain(|_, queue| {
        queue.retain(|(_, at)| at.elapsed() < ttl);
        !queue.is_empty()
    });
}

/// The body a row is dated from: the parsed body the writer attached,
/// else the file; an index is dated from the child the client pulled.
pub(crate) async fn dated_body(
    shared: &Shared,
    body: &Cached,
    served: Option<&Cached>,
    parsed: Option<Value>,
) -> Result<Value, &'static str> {
    let json = match parsed {
        Some(json) => json,
        None => read_json(shared, body).await.ok_or("failed")?,
    };
    if !is_index(&json) {
        return Ok(json);
    }
    let child = served.ok_or("index-unpulled")?;
    read_json(shared, child).await.ok_or("failed")
}

/// The manifest's own `created` annotation, else its config blob's
/// `created`; anything before 2000-01-01 is a zeroed or forged stamp.
pub(crate) async fn oci_published_at(
    shared: &Shared,
    cfg: &PolicyConfig,
    member: CacheRepo<'_>,
    up: &Upstream,
    name: &str,
    dated: Result<Value, &'static str>,
) -> (Option<DateTime<Utc>>, &'static str) {
    let json = match dated {
        Ok(json) => json,
        Err(source) => return (None, source),
    };
    if let Some(created) = json["annotations"][CREATED].as_str() {
        return created_stamp(created, "annotation");
    }
    let Some(digest) = json["config"]["digest"].as_str() else {
        return (None, "none");
    };
    let (blob, source) = fetch_or_peek(
        shared,
        cfg,
        &OciUpstream,
        up,
        member,
        &oci_blob(up, name, digest),
    )
    .await;
    let Some(blob) = blob else {
        return (None, source);
    };
    match read_json(shared, &blob)
        .await
        .and_then(|c| c["created"].as_str().map(String::from))
    {
        Some(created) => created_stamp(&created, "config-blob"),
        None => (None, "none"),
    }
}

fn created_stamp(created: &str, source: &'static str) -> (Option<DateTime<Utc>>, &'static str) {
    let floor = Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap();
    match parse_time(created) {
        Some(t) if t >= floor => (Some(t), source),
        Some(_) => (None, "unset-created"),
        None => (None, "none"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn oci_created_from_annotation() {
        let (t, source) = created_stamp("2026-09-17T07:10:00Z", "annotation");
        assert_eq!(source, "annotation");
        assert_eq!(t.unwrap().to_rfc3339(), "2026-09-17T07:10:00+00:00");
        assert_eq!(created_stamp("soon", "annotation"), (None, "none"));
    }

    #[test]
    fn oci_created_before_2000_is_none() {
        for zeroed in [
            "1970-01-01T00:00:00Z",
            "1980-01-01T00:00:01Z",
            "1999-12-31T23:59:59Z",
        ] {
            assert_eq!(
                created_stamp(zeroed, "config-blob"),
                (None, "unset-created"),
                "{zeroed}"
            );
        }
        assert!(created_stamp("2000-01-01T00:00:00Z", "config-blob")
            .0
            .is_some());
    }

    #[test]
    fn oci_blob_uses_upstream_name() {
        let hub = Upstream {
            base: reqwest::Url::parse("https://registry-1.docker.io").unwrap(),
            auth: None,
            token_realms: Vec::new(),
            dl_allow_private: false,
        };
        let OciArtifact::Blob { name, digest } = oci_blob(&hub, "nginx", "sha256:ab") else {
            panic!("a blob");
        };
        assert_eq!(
            (name.as_str(), digest.as_str()),
            ("library/nginx", "sha256:ab")
        );
        let OciArtifact::Blob { name, .. } = oci_blob(&hub, "acme/app", "sha256:ab") else {
            panic!("a blob");
        };
        assert_eq!(name, "acme/app");
        let local = Upstream {
            base: reqwest::Url::parse("http://127.0.0.1:1").unwrap(),
            ..hub
        };
        let OciArtifact::Blob { name, .. } = oci_blob(&local, "nginx", "sha256:ab") else {
            panic!("a blob");
        };
        assert_eq!(name, "nginx");
    }

    #[test]
    fn oci_children_of_index_skips_attestations() {
        let index = json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.index.v1+json",
            "manifests": [
                { "digest": "sha256:aa", "platform": { "architecture": "amd64" } },
                { "digest": "sha256:bb", "platform": { "architecture": "arm64" } },
                { "digest": "sha256:cc", "annotations": { "vnd.docker.reference.type": "attestation-manifest" } }
            ]
        });
        assert_eq!(oci_children(index.to_string().as_bytes()), vec!["aa", "bb"]);
        let list = json!({
            "mediaType": "application/vnd.docker.distribution.manifest.list.v2+json",
            "manifests": [{ "digest": "sha256:dd" }]
        });
        assert_eq!(oci_children(list.to_string().as_bytes()), vec!["dd"]);
        let manifest = json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": { "digest": "sha256:ee" },
            "layers": []
        });
        assert!(oci_children(manifest.to_string().as_bytes()).is_empty());
        assert!(oci_children(b"not json").is_empty());
    }
}

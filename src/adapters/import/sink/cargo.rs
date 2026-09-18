//! cargo: the source's sparse index line is the authority on resolution
//! (deps, features, links, MSRV, checksum, yank), the `.crate`'s manifest on
//! description, licence and the rest; a source with no index falls back to
//! the manifest alone.

use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::{BufMut, Bytes, BytesMut};
use reqwest::{Method, StatusCode, Url};
use serde_json::{json, Map, Value};

use crate::adapters::import::http::{FetchError, Gate, Req};
use crate::adapters::import::target::{publish_outcome, Body, TReq, TargetLane};
use crate::domain::import::GapKind;
use crate::domain::Format;
use crate::ports::import::{
    CopyError, Copied, Digests, Gap, Origin, PkgExtra, Planned, Presence, Sink, VersionExtra,
};
use crate::registry::cargo::compute_prefix;

const TARGET_CAP: u64 = 1 << 30;
const MANIFEST_CAP: u64 = 4 << 20;

/// The descriptive fields a publish carries that no index line has.
const DESCRIPTIVE: [&str; 9] = [
    "description", "authors", "license", "license_file", "repository", "homepage", "documentation", "keywords",
    "categories",
];

pub struct CargoSink {
    gate: Arc<Gate>,
    lane: Arc<TargetLane>,
}

impl CargoSink {
    pub fn new(gate: Arc<Gate>, lane: Arc<TargetLane>) -> Self {
        Self { gate, lane }
    }
}

pub fn index_path(name: &str) -> String {
    format!("{}/{}", compute_prefix(name), name.to_lowercase())
}

/// The line of `version` in a sparse index file.
pub fn find_line(file: &str, version: &str) -> Option<Value> {
    file.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|l| l.get("vers").and_then(|v| v.as_str()) == Some(version))
}

/// The inverse of the target's `index_dep`: an index line names the alias
/// `name` and the real `package`, a publish body the real `name` and the
/// alias `explicit_name_in_toml`, and calls the requirement `version_req`.
pub fn publish_dep(index: &Value) -> Value {
    let mut out = Map::new();
    let alias = index.get("name").cloned().unwrap_or(Value::Null);
    match index.get("package").filter(|p| p.is_string()) {
        Some(real) => {
            out.insert("name".into(), real.clone());
            out.insert("explicit_name_in_toml".into(), alias);
        }
        None => {
            out.insert("name".into(), alias);
        }
    }
    out.insert("version_req".into(), index.get("req").cloned().unwrap_or(json!("*")));
    for key in ["features", "optional", "default_features", "target", "kind", "registry"] {
        if let Some(v) = index.get(key).filter(|v| !v.is_null()) {
            out.insert(key.into(), v.clone());
        }
    }
    Value::Object(out)
}

/// `Cargo.toml` and its README out of a `.crate`.
pub fn read_crate(path: &Path, name: &str, version: &str) -> (Option<toml::Value>, Option<String>) {
    let Ok(file) = std::fs::File::open(path) else { return (None, None) };
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let root = format!("{name}-{version}/");
    let (mut manifest, mut readmes) = (None, std::collections::HashMap::new());
    let Ok(entries) = archive.entries() else { return (None, None) };
    for entry in entries.flatten() {
        let Ok(p) = entry.path().map(|p| p.to_string_lossy().to_string()) else { continue };
        let Some(rel) = p.strip_prefix(&root) else { continue };
        let wanted = rel == "Cargo.toml" || (!rel.contains('/') && rel.to_ascii_lowercase().starts_with("readme"));
        if !wanted {
            continue;
        }
        let mut s = String::new();
        if entry.take(MANIFEST_CAP).read_to_string(&mut s).is_err() {
            continue;
        }
        if rel == "Cargo.toml" {
            manifest = toml::from_str::<toml::Value>(&s).ok();
        } else {
            readmes.insert(rel.to_string(), s);
        }
    }
    let named = manifest
        .as_ref()
        .and_then(|m| m.get("package")?.get("readme")?.as_str().map(String::from));
    let readme = named
        .and_then(|n| readmes.get(&n).cloned())
        .or_else(|| readmes.get("README.md").cloned())
        .or_else(|| readmes.values().next().cloned());
    (manifest, readme)
}

fn to_json(v: &toml::Value) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

/// `dep:` and `pkg?/feat` values only an index v2 reader understands.
pub fn split_features(features: &Map<String, Value>) -> (Map<String, Value>, Map<String, Value>) {
    let (mut plain, mut two) = (Map::new(), Map::new());
    for (k, v) in features {
        let modern = v
            .as_array()
            .is_some_and(|vals| vals.iter().any(|x| x.as_str().is_some_and(|s| s.starts_with("dep:") || s.contains("?/"))));
        if modern {
            two.insert(k.clone(), v.clone());
        } else {
            plain.insert(k.clone(), v.clone());
        }
    }
    (plain, two)
}

fn manifest_deps(m: &toml::Value) -> Vec<Value> {
    let mut out = Vec::new();
    let mut table = |t: Option<&toml::Value>, kind: &str, target: Option<&str>| {
        let Some(t) = t.and_then(|t| t.as_table()) else { return };
        for (key, spec) in t {
            let mut d = Map::new();
            let (req, real, fields) = match spec {
                toml::Value::String(r) => (r.clone(), None, None),
                toml::Value::Table(tb) => (
                    tb.get("version").and_then(|v| v.as_str()).unwrap_or("*").to_string(),
                    tb.get("package").and_then(|v| v.as_str()).map(String::from),
                    Some(tb),
                ),
                _ => continue,
            };
            match &real {
                Some(r) => {
                    d.insert("name".into(), json!(r));
                    d.insert("explicit_name_in_toml".into(), json!(key));
                }
                None => {
                    d.insert("name".into(), json!(key));
                }
            }
            d.insert("version_req".into(), json!(req));
            d.insert("kind".into(), json!(kind));
            if let Some(tb) = fields {
                let get = |k: &str| tb.get(k).map(to_json);
                d.insert("features".into(), get("features").unwrap_or(json!([])));
                d.insert("optional".into(), get("optional").unwrap_or(json!(false)));
                d.insert(
                    "default_features".into(),
                    get("default-features").or_else(|| get("default_features")).unwrap_or(json!(true)),
                );
                if let Some(r) = get("registry-index").or_else(|| get("registry")) {
                    d.insert("registry".into(), r);
                }
            } else {
                d.insert("features".into(), json!([]));
                d.insert("optional".into(), json!(false));
                d.insert("default_features".into(), json!(true));
            }
            d.insert("target".into(), target.map_or(Value::Null, |t| json!(t)));
            out.push(Value::Object(d));
        }
    };
    for (section, kind) in [("dependencies", "normal"), ("dev-dependencies", "dev"), ("build-dependencies", "build")] {
        table(m.get(section), kind, None);
    }
    if let Some(targets) = m.get("target").and_then(|t| t.as_table()) {
        for (cfg, t) in targets {
            for (section, kind) in [("dependencies", "normal"), ("dev-dependencies", "dev"), ("build-dependencies", "build")] {
                table(t.get(section), kind, Some(cfg));
            }
        }
    }
    out
}

/// The publish body: resolution from the index line when there is one,
/// description and the rest from the manifest, and which descriptive
/// fields could not be read.
pub fn publish_meta(
    name: &str,
    version: &str,
    line: Option<&Value>,
    manifest: Option<&toml::Value>,
    readme: Option<String>,
) -> Result<(Value, Vec<&'static str>), String> {
    let mut meta = Map::new();
    meta.insert("name".into(), json!(name));
    meta.insert("vers".into(), json!(version));
    let package = manifest.and_then(|m| m.get("package"));
    let mut dropped = Vec::new();
    for key in DESCRIPTIVE {
        match package.and_then(|p| p.get(key).or_else(|| p.get(key.replace('_', "-").as_str()))) {
            Some(v) => {
                meta.insert(key.into(), to_json(v));
            }
            None if manifest.is_none() => dropped.push(key),
            None => {}
        }
    }
    if let Some(r) = readme {
        meta.insert("readme".into(), json!(r));
    }
    match line {
        Some(l) => {
            let deps: Vec<Value> = l.get("deps").and_then(|d| d.as_array()).map(|d| d.iter().map(publish_dep).collect()).unwrap_or_default();
            meta.insert("deps".into(), json!(deps));
            meta.insert("features".into(), l.get("features").cloned().unwrap_or(json!({})));
            for key in ["features2", "v", "links", "rust_version"] {
                if let Some(v) = l.get(key).filter(|v| !v.is_null()) {
                    meta.insert(key.into(), v.clone());
                }
            }
        }
        None => {
            let m = manifest.ok_or_else(|| {
                "the source has no index line and the .crate's Cargo.toml is unreadable: no dependency list to publish".to_string()
            })?;
            meta.insert("deps".into(), json!(manifest_deps(m)));
            let features = m.get("features").map(to_json).and_then(|f| f.as_object().cloned()).unwrap_or_default();
            let (plain, two) = split_features(&features);
            meta.insert("features".into(), Value::Object(plain));
            if !two.is_empty() {
                meta.insert("features2".into(), Value::Object(two));
                meta.insert("v".into(), json!(2));
            }
            if let Some(links) = package.and_then(|p| p.get("links")) {
                meta.insert("links".into(), to_json(links));
            }
            if let Some(msrv) = package.and_then(|p| p.get("rust-version")) {
                meta.insert("rust_version".into(), to_json(msrv));
            }
        }
    }
    Ok((Value::Object(meta), dropped))
}

fn dl_url(template: &str, name: &str, version: &str, cksum: &str) -> Option<Url> {
    let markers = ["{crate}", "{version}", "{prefix}", "{lowerprefix}", "{sha256-checksum}"];
    let raw = if markers.iter().any(|m| template.contains(m)) {
        template
            .replace("{crate}", name)
            .replace("{version}", version)
            .replace("{prefix}", &crate::registry::cargo::prefix_of(name))
            .replace("{lowerprefix}", &compute_prefix(name))
            .replace("{sha256-checksum}", cksum)
    } else {
        format!("{}/{name}/{version}/download", template.trim_end_matches('/'))
    };
    Url::parse(&raw).ok()
}

fn framed_prefix(meta: &Value, crate_len: u64) -> Bytes {
    let json = meta.to_string();
    let mut b = BytesMut::with_capacity(json.len() + 8);
    b.put_u32_le(json.len() as u32);
    b.put_slice(json.as_bytes());
    b.put_u32_le(crate_len as u32);
    b.freeze()
}

#[async_trait]
impl Sink for CargoSink {
    fn format(&self) -> Format {
        Format::Cargo
    }

    async fn present(&self, p: &Planned) -> Result<Presence, CopyError> {
        let url = self.lane.url(&format!("{}/index/{}", p.target_repo, index_path(&p.target_name)));
        let resp = self.lane.send(&TReq::new(Method::GET, url)).await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(Presence::Absent);
        }
        if !resp.status().is_success() {
            return Err(publish_outcome(resp).await.err().unwrap_or(CopyError::Transient("target read".into())));
        }
        let text = resp.text().await.map_err(|e| CopyError::Transient(e.without_url().to_string()))?;
        let Some(line) = find_line(&text, &p.item.coord.version) else { return Ok(Presence::Absent) };
        let have = line.get("cksum").and_then(|c| c.as_str()).unwrap_or_default().to_string();
        Ok(match &p.item.want.sha256 {
            Some(want) if want.eq_ignore_ascii_case(&have) => Presence::Same,
            Some(_) => Presence::Different(format!("cksum {have}")),
            None => Presence::Present,
        })
    }

    async fn copy(&self, p: &Planned) -> Result<Copied, CopyError> {
        let (name, version) = (&p.item.coord.name, &p.item.coord.version);
        let bad = |e: url::ParseError| CopyError::Permanent(e.to_string());
        let (line, crate_url, want) = match &p.item.origin {
            Origin::Cargo { index, .. } => {
                let index = Url::parse(index).map_err(bad)?;
                let config = self.gate.get_json(index.join("config.json").map_err(bad)?).await?;
                let file = match self.gate.bytes(&Req::get(index.join(&index_path(name)).map_err(bad)?), 64 << 20).await {
                    Ok(b) => String::from_utf8_lossy(&b).to_string(),
                    Err(FetchError::NotFound(u)) => return Err(CopyError::Permanent(format!("{u}: the source index has no such crate"))),
                    Err(e) => return Err(e.into()),
                };
                let line = find_line(&file, version)
                    .ok_or_else(|| CopyError::Permanent(format!("the source index lists no {name} {version}")))?;
                let cksum = line.get("cksum").and_then(|c| c.as_str()).unwrap_or_default().to_string();
                let template = config.get("dl").and_then(|d| d.as_str()).unwrap_or_default();
                let url = dl_url(template, name, version, &cksum)
                    .ok_or_else(|| CopyError::Permanent("the source index has no usable dl template".into()))?;
                let want = Digests { sha256: Some(cksum).filter(|c| !c.is_empty()), ..Default::default() };
                (Some(line), url, want)
            }
            Origin::Asset { endpoint, repo, path } => {
                let url = Url::parse(&format!("{endpoint}{repo}/{path}")).map_err(bad)?;
                (None, url, p.item.want.clone())
            }
            _ => return Err(CopyError::Permanent("cargo sink handed a non-cargo origin".into())),
        };
        let spooled = self.gate.spool(crate_url, &want).await?;
        if spooled.size > TARGET_CAP {
            return Err(CopyError::TooLarge { size: spooled.size, limit: TARGET_CAP });
        }
        let (path, n, v) = (spooled.path.clone(), name.clone(), version.clone());
        let (manifest, readme) = tokio::task::spawn_blocking(move || read_crate(&path, &n, &v))
            .await
            .map_err(|e| CopyError::Permanent(e.to_string()))?;
        let (mut meta, dropped) =
            publish_meta(&p.target_name, version, line.as_ref(), manifest.as_ref(), readme).map_err(CopyError::Permanent)?;
        let mut gaps = Vec::new();
        if !dropped.is_empty() {
            gaps.push(Gap::new(
                GapKind::SourceOnlyFeature,
                &p.item.source_ref,
                format!("the .crate's Cargo.toml is unreadable; published without {}", dropped.join(", ")),
            ));
        }
        if line.is_none() {
            gaps.push(Gap::new(
                GapKind::SourceOnlyFeature,
                &p.item.source_ref,
                "the source serves no index: yank state is unknown, published unyanked",
            ));
        }
        if let Some(obj) = meta.as_object_mut() {
            obj.retain(|_, v| !v.is_null());
        }
        let body = Body::Framed {
            prefix: framed_prefix(&meta, spooled.size),
            file: spooled.path.clone(),
            file_len: spooled.size,
            base64: false,
            suffix: Bytes::new(),
        };
        let url = self.lane.url(&format!("{}/api/v1/crates/new", p.target_repo));
        publish_outcome(self.lane.send(&TReq::new(Method::PUT, url).body(body).publish()).await?).await?;
        let note = spooled.unverified.then(|| {
            crate::domain::import::tagged(GapKind::CopiedUnverified, "the source announced no checksum for this crate")
        });
        Ok(Copied { bytes: spooled.size, sha256: spooled.sha256.clone(), note, gaps })
    }

    async fn seal(
        &self,
        repo: &str,
        name: &str,
        _pkg: &PkgExtra,
        versions: &[(String, VersionExtra)],
    ) -> Result<Vec<Gap>, CopyError> {
        for (v, _) in versions.iter().filter(|(_, e)| e.yanked) {
            let url = self.lane.url(&format!("{repo}/api/v1/crates/{name}/{v}/yank"));
            publish_outcome(self.lane.send(&TReq::new(Method::DELETE, url)).await?).await?;
        }
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_dep_inverts_the_index_shape() {
        let index = json!({ "name": "alias", "package": "real", "req": "^1.2", "features": ["x"], "optional": true,
            "default_features": false, "target": null, "kind": "normal", "registry": "https://other/index" });
        let d = publish_dep(&index);
        assert_eq!(d["name"], "real");
        assert_eq!(d["explicit_name_in_toml"], "alias");
        assert_eq!(d["version_req"], "^1.2");
        assert_eq!(d["registry"], "https://other/index");
        assert!(d.get("target").is_none());
        let plain = publish_dep(&json!({ "name": "serde", "req": "1" }));
        assert_eq!(plain["name"], "serde");
        assert!(plain.get("explicit_name_in_toml").is_none());
    }

    #[test]
    fn manifest_fallback_splits_features_and_carries_links_and_msrv() {
        let m: toml::Value = toml::from_str(
            r#"
            [package]
            name = "z-sys"
            version = "1.0.0"
            links = "z"
            rust-version = "1.70"
            description = "zlib"
            license = "MIT"
            [dependencies]
            libc = "0.2"
            aliased = { package = "real", version = "^1.2", optional = true, registry-index = "https://other/index" }
            [target.'cfg(unix)'.dependencies]
            nix = "0.27"
            [features]
            default = ["std"]
            std = []
            extra = ["dep:aliased"]
            "#,
        )
        .unwrap();
        let (meta, dropped) = publish_meta("z-sys", "1.0.0", None, Some(&m), None).unwrap();
        assert!(dropped.is_empty());
        assert_eq!(meta["links"], "z");
        assert_eq!(meta["rust_version"], "1.70");
        assert_eq!(meta["v"], 2);
        assert_eq!(meta["features2"], json!({ "extra": ["dep:aliased"] }));
        assert_eq!(meta["features"]["std"], json!([]));
        assert_eq!(meta["license"], "MIT");
        let deps = meta["deps"].as_array().unwrap();
        let aliased = deps.iter().find(|d| d["name"] == "real").unwrap();
        assert_eq!(aliased["explicit_name_in_toml"], "aliased");
        assert_eq!(aliased["registry"], "https://other/index");
        assert!(deps.iter().any(|d| d["name"] == "nix" && d["target"] == "cfg(unix)"));
        assert!(publish_meta("x", "1.0.0", None, None, None).is_err());
    }

    #[test]
    fn dl_templates_expand() {
        let u = dl_url("https://s/api/v1/crates", "Serde", "1.0.0", "ab").unwrap();
        assert_eq!(u.as_str(), "https://s/api/v1/crates/Serde/1.0.0/download");
        let u = dl_url("https://s/{lowerprefix}/{crate}/{crate}-{version}.crate?c={sha256-checksum}", "Serde", "1.0.0", "ab").unwrap();
        assert_eq!(u.as_str(), "https://s/se/rd/Serde/Serde-1.0.0.crate?c=ab");
    }
}

//! The subset of a registry `server.json` record opencargo reads. Unknown
//! fields are kept in `raw`, and the stored record is always the verbatim
//! envelope: these types decide nothing about what is re-emitted.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{AppError, AppResult};

/// The envelope key our own gate state lives under; never the official one.
pub const MIRROR_META: &str = "eu.opencargo.registry/mirror";
/// The registry-minted block: copied from upstream, minted only on hosted rows.
pub const OFFICIAL_META: &str = "io.modelcontextprotocol.registry/official";

const KNOWN_SCHEMA_DATES: &[&str] = &["2025-07-09", "2025-09-29", "2025-10-17", "2025-12-11"];

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ServerDetail {
    pub name: String,
    pub description: String,
    pub version: String,
    pub title: Option<String>,
    pub repository: Option<Value>,
    pub website_url: Option<String>,
    #[serde(default)]
    pub packages: Vec<Package>,
    #[serde(default)]
    pub remotes: Vec<RemoteTransport>,
    #[serde(default)]
    pub icons: Vec<Icon>,
    #[serde(rename = "$schema")]
    pub schema: Option<String>,
    #[serde(flatten)]
    pub raw: Map<String, Value>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Icon {
    pub src: String,
    pub mime_type: Option<String>,
    pub sizes: Option<Vec<String>>,
    pub theme: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Package {
    pub registry_type: String,
    pub identifier: String,
    pub version: Option<String>,
    pub registry_base_url: Option<String>,
    pub file_sha256: Option<String>,
    pub runtime_hint: Option<String>,
    pub transport: Transport,
    #[serde(default)]
    pub runtime_arguments: Vec<Value>,
    #[serde(default)]
    pub package_arguments: Vec<Value>,
    #[serde(default)]
    pub environment_variables: Vec<Value>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Transport {
    #[serde(rename = "type")]
    pub kind: String,
    pub url: Option<String>,
    #[serde(default)]
    pub headers: Vec<Value>,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteTransport {
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<Value>,
    #[serde(default)]
    pub variables: Map<String, Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TransportKind {
    Stdio,
    StreamableHttp,
    Sse,
}

impl TransportKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "stdio" => Some(TransportKind::Stdio),
            "streamable-http" | "http" => Some(TransportKind::StreamableHttp),
            "sse" => Some(TransportKind::Sse),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            TransportKind::Stdio => "stdio",
            TransportKind::StreamableHttp => "streamable-http",
            TransportKind::Sse => "sse",
        }
    }
}

/// A record offers packages and remotes side by side, each with its own
/// transport: two sets, sorted and deduplicated, never one value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransportSet {
    pub packages: Vec<TransportKind>,
    pub remotes: Vec<TransportKind>,
}

impl TransportSet {
    pub fn of(d: &ServerDetail) -> Self {
        let set = |kinds: Vec<Option<TransportKind>>| {
            let mut v: Vec<TransportKind> = kinds.into_iter().flatten().collect();
            v.sort();
            v.dedup();
            v
        };
        Self {
            packages: set(d.packages.iter().map(|p| TransportKind::parse(&p.transport.kind)).collect()),
            remotes: set(d.remotes.iter().map(|r| TransportKind::parse(&r.kind)).collect()),
        }
    }

    pub fn join(kinds: &[TransportKind]) -> String {
        kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(",")
    }

    pub fn split(stored: &str) -> Vec<TransportKind> {
        stored.split(',').filter_map(TransportKind::parse).collect()
    }
}

/// The dated `$schema` a record declares: known dates validate strictly,
/// an unknown one is ingested and flagged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchemaId {
    Known(&'static str),
    Unknown(String),
    Absent,
}

impl SchemaId {
    pub fn of(url: Option<&str>) -> Self {
        let Some(url) = url else {
            return SchemaId::Absent;
        };
        KNOWN_SCHEMA_DATES
            .iter()
            .find(|date| url.contains(&format!("/schemas/{date}/")))
            .map_or_else(|| SchemaId::Unknown(url.to_string()), |date| SchemaId::Known(date))
    }
}

/// A `ServerDetail` out of a raw record, refusing only what the schema
/// requires: a name, a description and a version, and per package its
/// registry type, identifier and transport.
pub fn parse(raw: &Value) -> AppResult<(ServerDetail, SchemaId)> {
    let detail: ServerDetail = serde_json::from_value(raw.clone())
        .map_err(|e| AppError::BadRequest(format!("invalid server record: {e}")))?;
    let schema = SchemaId::of(detail.schema.as_deref());
    Ok((detail, schema))
}

/// The record an envelope carries, and the envelope with any inbound
/// mirror key stripped: someone else's approval chip is never ours.
pub fn split_envelope(envelope: &Value) -> AppResult<(Value, Value)> {
    let record = envelope
        .get("server")
        .cloned()
        .ok_or_else(|| AppError::BadRequest("server response has no 'server' object".into()))?;
    let mut stored = envelope.clone();
    if let Some(meta) = stored.get_mut("_meta").and_then(Value::as_object_mut) {
        meta.remove(MIRROR_META);
    }
    Ok((record, stored))
}

/// The official block's fields, as stored columns derive them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Official {
    pub status: String,
    pub status_message: Option<String>,
    pub status_changed_at: Option<String>,
    pub published_at: Option<String>,
    pub updated_at: Option<String>,
    pub is_latest: bool,
}

impl Official {
    pub fn of(envelope: &Value) -> Self {
        let block = envelope.get("_meta").and_then(|m| m.get(OFFICIAL_META));
        let text = |key: &str| block.and_then(|b| b.get(key)).and_then(Value::as_str).map(str::to_string);
        let status = text("status")
            .filter(|s| matches!(s.as_str(), "active" | "deprecated" | "deleted"))
            .unwrap_or_else(|| "active".to_string());
        Self {
            status,
            status_message: text("statusMessage"),
            status_changed_at: text("statusChangedAt"),
            published_at: text("publishedAt"),
            updated_at: text("updatedAt"),
            is_latest: block
                .and_then(|b| b.get("isLatest"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_page() -> Value {
        serde_json::from_str(include_str!("../../../tests/fixtures/mcp/live-page.json")).unwrap()
    }

    #[test]
    fn live_page_of_a_hundred_records_parses_with_no_failure_and_keeps_website_url() {
        let page = live_page();
        let servers = page["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 100);
        let mut with_website = 0;
        for envelope in servers {
            let (record, _) = split_envelope(envelope).unwrap();
            let (detail, schema) = parse(&record).unwrap();
            assert!(matches!(schema, SchemaId::Known(_)), "{schema:?}");
            if record.get("websiteUrl").is_some() {
                assert_eq!(detail.website_url.as_deref(), record["websiteUrl"].as_str());
                with_website += 1;
            }
            for (package, raw) in detail.packages.iter().zip(record["packages"].as_array().into_iter().flatten()) {
                assert_eq!(Some(package.registry_type.as_str()), raw["registryType"].as_str());
            }
        }
        assert!(with_website > 0);
    }

    #[test]
    fn required_fields_are_required_and_the_rest_default() {
        assert!(parse(&serde_json::json!({"name": "a/b", "version": "1"})).is_err());
        let (d, schema) = parse(&serde_json::json!({"name": "a/b", "description": "d", "version": "1"})).unwrap();
        assert!(d.packages.is_empty() && d.remotes.is_empty() && d.icons.is_empty());
        assert_eq!(schema, SchemaId::Absent);
        let bad_package = serde_json::json!({"name": "a/b", "description": "d", "version": "1",
            "packages": [{"identifier": "x", "transport": {"type": "stdio"}}]});
        assert!(parse(&bad_package).is_err());
    }

    #[test]
    fn an_unknown_record_field_survives_a_round_trip_through_raw() {
        let record = serde_json::json!({"name": "a/b", "description": "d", "version": "1", "futureField": {"x": 1}});
        let (d, _) = parse(&record).unwrap();
        assert_eq!(d.raw["futureField"], serde_json::json!({"x": 1}));
        assert_eq!(serde_json::to_value(&d).unwrap()["futureField"], serde_json::json!({"x": 1}));
    }

    #[test]
    fn two_schema_dates_are_known_and_a_third_is_flagged() {
        let at = |d: &str| format!("https://static.modelcontextprotocol.io/schemas/{d}/server.schema.json");
        assert_eq!(SchemaId::of(Some(&at("2025-12-11"))), SchemaId::Known("2025-12-11"));
        assert_eq!(SchemaId::of(Some(&at("2025-09-29"))), SchemaId::Known("2025-09-29"));
        assert!(matches!(SchemaId::of(Some(&at("2031-01-01"))), SchemaId::Unknown(_)));
    }

    #[test]
    fn an_inbound_mirror_key_is_stripped_and_the_official_block_read() {
        let envelope = serde_json::json!({"server": {"name": "a/b"}, "_meta": {
            MIRROR_META: {"approval": "approved"},
            OFFICIAL_META: {"status": "deleted", "statusChangedAt": "2026-01-01T00:00:00Z", "isLatest": true}}});
        let (_, stored) = split_envelope(&envelope).unwrap();
        assert!(stored["_meta"].get(MIRROR_META).is_none());
        assert_eq!(stored["_meta"][OFFICIAL_META], envelope["_meta"][OFFICIAL_META]);
        let official = Official::of(&stored);
        assert_eq!((official.status.as_str(), official.is_latest), ("deleted", true));
        assert_eq!(official.status_changed_at.as_deref(), Some("2026-01-01T00:00:00Z"));
    }

    #[test]
    fn transports_are_two_sorted_sets() {
        let (d, _) = parse(&serde_json::json!({"name": "a/b", "description": "d", "version": "1",
            "packages": [{"registryType": "npm", "identifier": "x", "transport": {"type": "stdio"}}],
            "remotes": [{"type": "sse", "url": "https://a"}, {"type": "streamable-http", "url": "https://b"},
                        {"type": "sse", "url": "https://c"}]}))
        .unwrap();
        let set = TransportSet::of(&d);
        assert_eq!(set.packages, vec![TransportKind::Stdio]);
        assert_eq!(set.remotes, vec![TransportKind::StreamableHttp, TransportKind::Sse]);
        assert_eq!(TransportSet::split(&TransportSet::join(&set.remotes)), set.remotes);
    }
}

//! A registry envelope turned into what the store writes: the record
//! verbatim, the columns derived from it, the declared surface and the scan
//! of its prose. The only place a wire record becomes a store command.

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::rules::McpRules;
use super::scan::{scan, texts_of_server, Finding};
use super::schema::{parse, split_envelope, Official, ServerDetail, TransportSet};
use super::surface::{permission_surface, Hashes, Tool};
use crate::domain::FormatRules;
use crate::error::{AppError, AppResult};
use crate::ports::mcp::{NewFinding, NewSurface, RecordWrite, SurfaceSource};

pub fn new_findings(found: Vec<Finding>) -> Vec<NewFinding> {
    found
        .into_iter()
        .map(|f| NewFinding {
            pattern: f.pattern.to_string(),
            high: f.confidence == super::scan::Confidence::High,
            promoted_by: f.promoted_by.map(str::to_string),
            field: f.field.as_string(),
            tool: f.tool.unwrap_or_default(),
            span: (f.span.0 as i64, f.span.1 as i64),
            excerpt: f.excerpt,
        })
        .collect()
}

/// The record's name and version, held to the shape every key is written
/// against.
pub fn validate(d: &ServerDetail) -> AppResult<()> {
    McpRules.validate(&d.name)?;
    McpRules.validate_version(&d.version)?;
    Ok(())
}

/// The declared surface of a record: its permissions, no tools.
pub fn declared_surface(d: &ServerDetail) -> NewSurface {
    let hashes = Hashes::of(&permission_surface(d), None);
    NewSurface {
        source: SurfaceSource::Declared,
        remote_url: String::new(),
        tools_json: None,
        tools_sha256: None,
        permissions_sha256: hashes.permissions_sha256,
        combined_sha256: hashes.combined_sha256,
        captured_by: None,
        findings: new_findings(scan(&texts_of_server(d), &[])),
    }
}

/// A surface one endpoint answered, scanned.
pub fn observed_surface(
    d: &ServerDetail,
    source: SurfaceSource,
    remote_url: &str,
    tools: &[Tool],
    captured_by: Option<String>,
) -> AppResult<NewSurface> {
    let hashes = Hashes::of(&permission_surface(d), Some(tools));
    Ok(NewSurface {
        source,
        remote_url: remote_url.to_string(),
        tools_json: Some(serde_json::to_string(tools)?),
        tools_sha256: hashes.tools_sha256,
        permissions_sha256: hashes.permissions_sha256,
        combined_sha256: hashes.combined_sha256,
        captured_by,
        findings: new_findings(super::scan::scan_tools(tools)),
    })
}

/// A synced or hosted envelope as the store writes it.
pub fn record_write(repository: i64, envelope: &Value, hosted: bool, now: DateTime<Utc>) -> AppResult<RecordWrite> {
    let (record, stored) = split_envelope(envelope)?;
    let (detail, _) = parse(&record)?;
    validate(&detail)?;
    let official = Official::of(&stored);
    let transports = TransportSet::of(&detail);
    Ok(RecordWrite {
        repository,
        name: detail.name.clone(),
        version: detail.version.clone(),
        hosted,
        envelope_json: serde_json::to_string(&stored)?,
        schema_url: detail.schema.clone(),
        status: official.status,
        status_message: official.status_message,
        status_changed_at: official.status_changed_at,
        is_latest: official.is_latest,
        take_latest: false,
        published_at: official.published_at,
        upstream_updated_at: official.updated_at,
        package_transports: TransportSet::join(&transports.packages),
        remote_transports: TransportSet::join(&transports.remotes),
        remote_urls: detail.remotes.iter().map(|r| r.url.clone()).collect(),
        declared: declared_surface(&detail),
        now,
    })
}

/// The record inside a stored envelope.
pub fn detail_of(envelope_json: &str) -> AppResult<ServerDetail> {
    let envelope: Value = serde_json::from_str(envelope_json)?;
    let record = envelope
        .get("server")
        .ok_or_else(|| AppError::Internal("stored envelope has no server".into()))?;
    Ok(parse(record)?.0)
}

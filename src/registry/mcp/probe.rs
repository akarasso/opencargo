//! What the probe use case needs to know of a record: its remotes, and a
//! surface out of what one of them answered.

use serde_json::Value;

use super::ingest::{detail_of, observed_surface};
use super::schema::TransportKind;
use super::surface::Tool;
use crate::app::mcp::probe::Remote;
use crate::ports::mcp::{NewSurface, SurfaceSource};

/// The record's remotes in declaration order; a URL still carrying a
/// template variable cannot be probed as written and is left out.
pub fn remotes(envelope_json: &str) -> Vec<Remote> {
    let Ok(detail) = detail_of(envelope_json) else {
        return Vec::new();
    };
    detail
        .remotes
        .iter()
        .filter(|r| !r.url.contains('{'))
        .map(|r| Remote {
            url: r.url.clone(),
            sse: TransportKind::parse(&r.kind) == Some(TransportKind::Sse),
        })
        .collect()
}

pub fn surface(envelope_json: &str, url: &str, tools: &[Value]) -> Result<NewSurface, String> {
    let detail = detail_of(envelope_json).map_err(|e| e.to_string())?;
    let tools: Vec<Tool> = tools
        .iter()
        .cloned()
        .map(serde_json::from_value)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("the server answered an unreadable tool: {e}"))?;
    observed_surface(&detail, SurfaceSource::Probe, url, &tools, None).map_err(|e| e.to_string())
}

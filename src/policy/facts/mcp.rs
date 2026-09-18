//! What the four MCP rules judge: the addressed repository's view of one
//! served server version, gathered by the read path from rows it already
//! holds, never from the network.

use crate::domain::Drift;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    Approved,
    Pending,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpFinding {
    pub pattern: String,
    pub high: bool,
    pub field: String,
    pub tool: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpFacts {
    /// The repository the client addressed, whose verdicts these are.
    pub addressed: String,
    pub package_transports: Vec<String>,
    pub remote_transports: Vec<String>,
    /// The addressed repository's allow rules let the name through.
    pub allowed: bool,
    pub approval: Approval,
    /// Some decision exists for the version, in the addressed repository
    /// or its member.
    pub reviewed: bool,
    pub drift: Drift,
    pub drifted_remote: Option<String>,
    /// Unsuppressed findings of every live surface of the version.
    pub findings: Vec<McpFinding>,
    pub scan_medium: bool,
    pub tools_observed: bool,
}

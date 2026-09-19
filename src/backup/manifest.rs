//! What a snapshot says about itself: written last, so a directory without
//! one is an interrupted run.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The manifest format this binary writes and the newest it reads.
pub const FORMAT_VERSION: u32 = 1;

pub const MANIFEST: &str = "manifest.json";
pub const DATABASE: &str = "db.sqlite";
/// One `{sha256} {bytes} {key}` line per stored object, in copy order.
pub const KEYS: &str = "keys.sha256";
pub const STORAGE_DIR: &str = "storage";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub taken_at: DateTime<Utc>,
    pub db_sha256: String,
    /// Whether the storage half was copied at all: a database-only snapshot
    /// is not restorable onto an empty tree.
    pub storage: bool,
    pub storage_keys: u64,
    pub storage_bytes: u64,
}

/// One line of `keys.sha256`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyLine {
    pub sha256: String,
    pub bytes: u64,
    pub key: String,
}

impl KeyLine {
    pub fn render(&self) -> String {
        format!("{} {} {}\n", self.sha256, self.bytes, self.key)
    }

    pub fn parse(line: &str) -> Option<KeyLine> {
        let mut parts = line.splitn(3, ' ');
        let sha256 = parts.next()?.to_string();
        let bytes = parts.next()?.parse().ok()?;
        let key = parts.next()?.to_string();
        (sha256.len() == 64 && !key.is_empty()).then_some(KeyLine { sha256, bytes, key })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_line_round_trips_and_keeps_spaces_in_keys() {
        let line = KeyLine {
            sha256: "a".repeat(64),
            bytes: 12,
            key: "npm/with space/x.tgz".to_string(),
        };
        assert_eq!(KeyLine::parse(line.render().trim_end()), Some(line));
        assert_eq!(KeyLine::parse("short 1 k"), None);
    }
}

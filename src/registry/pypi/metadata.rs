//! Core metadata (`METADATA`, `PKG-INFO`): the header block of the artifact
//! itself, the only source of a file's name, version and requirements.

use serde_json::{json, Value};

use crate::domain::DomainError;
use crate::registry::archive::{self, ArchiveError, Budget};

use super::names::{normalize, Filename, Kind};
use crate::domain::Pep440;

const BUDGET: Budget = Budget {
    max_members: 100_000,
    max_member_bytes: 4 * 1024 * 1024,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreMetadata {
    pub name: String,
    pub version: String,
    pub summary: Option<String>,
    pub requires_python: Option<String>,
    pub requires_dist: Vec<String>,
    pub license: Option<String>,
    /// The fields a sdist leaves to build time (PEP 643), lower-cased.
    pub dynamic: Vec<String>,
}

impl CoreMetadata {
    /// Headers until the first blank line; a line opening with whitespace
    /// continues the previous one.
    pub fn parse(text: &str) -> Option<Self> {
        let mut fields: Vec<(String, String)> = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                break;
            }
            if line.starts_with([' ', '\t']) {
                if let Some((_, value)) = fields.last_mut() {
                    value.push('\n');
                    value.push_str(line.trim());
                }
                continue;
            }
            let (key, value) = line.split_once(':')?;
            fields.push((key.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
        let one = |key: &str| {
            fields
                .iter()
                .find(|(k, v)| k == key && !v.is_empty())
                .map(|(_, v)| v.clone())
        };
        Some(Self {
            name: one("name")?,
            version: one("version")?,
            summary: one("summary"),
            requires_python: one("requires-python"),
            requires_dist: fields
                .iter()
                .filter(|(k, _)| k == "requires-dist")
                .map(|(_, v)| v.clone())
                .collect(),
            license: one("license"),
            dynamic: fields
                .iter()
                .filter(|(k, _)| k == "dynamic")
                .map(|(_, v)| v.to_ascii_lowercase())
                .collect(),
        })
    }

    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "version": self.version,
            "summary": self.summary,
            "requires_python": self.requires_python,
            "requires_dist": self.requires_dist,
            "license": self.license,
            "dynamic": self.dynamic,
        })
    }
}

/// What an upload carries besides its bytes, read from the archive.
#[derive(Debug)]
pub struct Inspected {
    pub metadata: CoreMetadata,
    /// The raw `METADATA` of a wheel, served as its `.metadata` (PEP 658).
    pub raw: Option<Vec<u8>>,
}

fn invalid(why: impl std::fmt::Display) -> DomainError {
    DomainError::InvalidName(format!("invalid distribution: {why}"))
}

fn top_level(name: &str, suffix: &str) -> bool {
    name.strip_suffix(suffix)
        .is_some_and(|dir| !dir.is_empty() && !dir.contains('/'))
}

/// The core metadata inside `bytes`, which must name the same project and
/// version as the filename.
pub fn inspect(file: &Filename, bytes: &[u8]) -> Result<Inspected, DomainError> {
    let refused = |e: ArchiveError| invalid(e);
    let (raw, keep) = match file.kind {
        Kind::Wheel => {
            let found = archive::zip_member(bytes, BUDGET, |n| top_level(n, ".dist-info/METADATA"))
                .map_err(refused)?;
            (found.map(|(_, b)| b), true)
        }
        Kind::Sdist if file.canonical.ends_with(".zip") => {
            let found = archive::zip_member(bytes, BUDGET, |n| top_level(n, "/PKG-INFO"))
                .map_err(refused)?;
            (found.map(|(_, b)| b), false)
        }
        Kind::Sdist => {
            let found = archive::tar_gz_member(bytes, BUDGET, |n| top_level(n, "/PKG-INFO"))
                .map_err(refused)?;
            (found.map(|(_, b)| b), false)
        }
    };
    let raw = raw.ok_or_else(|| invalid("no core metadata in the archive"))?;
    let text = String::from_utf8(raw.clone()).map_err(|_| invalid("core metadata is not UTF-8"))?;
    let metadata = CoreMetadata::parse(&text).ok_or_else(|| invalid("core metadata has no Name or Version"))?;
    if normalize(&metadata.name) != file.project {
        return Err(invalid(format!(
            "the archive names project '{}', the filename '{}'",
            metadata.name, file.project
        )));
    }
    let version = Pep440::parse(&metadata.version).ok_or_else(|| invalid("unparseable Version"))?;
    if version.canonical() != file.version.canonical() {
        return Err(invalid(format!(
            "the archive is version '{}', the filename '{}'",
            metadata.version,
            file.version.normalized()
        )));
    }
    Ok(Inspected {
        metadata,
        raw: keep.then_some(raw),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "Metadata-Version: 2.1\nName: Demo_Pkg\nVersion: 1.0.0\nSummary: A demo\nRequires-Python: >=3.8\nRequires-Dist: requests (>=2)\nRequires-Dist: idna\nLicense: MIT\n  continued\n\nName: not-a-header\n";

    #[test]
    fn headers_end_at_the_first_blank_line() {
        let m = CoreMetadata::parse(TEXT).unwrap();
        assert_eq!(m.name, "Demo_Pkg");
        assert_eq!(m.version, "1.0.0");
        assert_eq!(m.requires_python.as_deref(), Some(">=3.8"));
        assert_eq!(m.requires_dist, ["requests (>=2)", "idna"]);
        assert_eq!(m.license.as_deref(), Some("MIT\ncontinued"));
        assert!(m.dynamic.is_empty());
        assert!(CoreMetadata::parse("Name: x\n").is_none(), "no Version");
    }

    #[test]
    fn dynamic_fields_reach_the_json_lower_cased() {
        let m = CoreMetadata::parse(
            "Metadata-Version: 2.2\nName: demo\nVersion: 1.0\nDynamic: Requires-Dist\nDynamic: Requires-Python\n\n",
        )
        .unwrap();
        assert_eq!(m.dynamic, ["requires-dist", "requires-python"]);
        assert_eq!(m.to_json()["dynamic"], serde_json::json!(["requires-dist", "requires-python"]));
    }
}

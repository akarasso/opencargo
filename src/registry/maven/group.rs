//! A group's `maven-metadata.xml`, rendered from its members' documents:
//! versions united and `latest`/`release`/`lastUpdated` recomputed, the
//! newest snapshot build taken whole from the one member announcing it,
//! plugins united by prefix. Its `ETag` digests the members' own.

use bytes::Bytes;
use sha2::{Digest, Sha256};

use super::hosted::Rendered;
use super::leaves::Member;
use super::metadata::{parse, ArtifactLevel, GroupLevel, Parsed};

pub fn merge(dir: &[String], members: &[Member]) -> Option<Rendered> {
    let parsed: Vec<Parsed> = members
        .iter()
        .filter_map(|m| match parse(&m.body) {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::warn!(error = %e, "maven: a member's metadata is unreadable; left out of the merge");
                None
            }
        })
        .collect();
    if parsed.is_empty() {
        return None;
    }
    let body = if super::leaves::snapshot_of(dir).is_some() {
        let newest = parsed.iter().max_by(|a, b| a.snapshot.newer(&b.snapshot))?;
        newest.snapshot.render()
    } else if parsed.iter().any(|p| !p.artifact.versions.is_empty()) {
        let first = parsed.iter().find(|p| !p.artifact.versions.is_empty())?;
        let versions = parsed
            .iter()
            .flat_map(|p| p.artifact.versions.iter().cloned())
            .collect();
        let last = parsed.iter().filter_map(|p| p.artifact.last_updated.clone()).max();
        ArtifactLevel::computed(&first.artifact.group, &first.artifact.artifact, versions, (None, None), last).render()
    } else {
        let mut plugins: Vec<(String, String, String)> = Vec::new();
        for p in &parsed {
            for plugin in &p.plugins {
                if !plugins.iter().any(|known| known.0 == plugin.0) {
                    plugins.push(plugin.clone());
                }
            }
        }
        if plugins.is_empty() {
            return None;
        }
        GroupLevel { plugins }.render()
    };
    let mut hasher = Sha256::new();
    for m in members {
        hasher.update(m.etag.as_bytes());
        hasher.update(b"\n");
    }
    Some(Rendered {
        body: Bytes::from(body),
        etag: format!("\"g{:x}\"", hasher.finalize()),
        last_modified: members.iter().filter_map(|m| m.last_modified).max(),
        stale: members.iter().any(|m| m.stale),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::metadata::{SnapshotEntry, SnapshotLevel};

    fn member(body: String, etag: &str) -> Member {
        Member {
            body: Bytes::from(body),
            etag: etag.to_string(),
            last_modified: None,
            stale: false,
        }
    }

    fn artifact(versions: &[&str], last: &str) -> Member {
        let level = ArtifactLevel::computed(
            "g",
            "a",
            versions.iter().map(|v| v.to_string()).collect(),
            (None, None),
            Some(last.to_string()),
        );
        member(level.render(), last)
    }

    #[test]
    fn versions_are_united_and_the_maxima_recomputed() {
        let dir = vec!["g".to_string(), "a".to_string()];
        let merged = merge(&dir, &[artifact(&["1.0", "1.1"], "2026"), artifact(&["1.10", "2.0-SNAPSHOT"], "2027")]).unwrap();
        let doc = parse(&merged.body).unwrap().artifact;
        assert_eq!(doc.versions, ["1.0", "1.1", "1.10", "2.0-SNAPSHOT"]);
        assert_eq!(doc.latest.as_deref(), Some("2.0-SNAPSHOT"));
        assert_eq!(doc.release.as_deref(), Some("1.10"));
        assert_eq!(doc.last_updated.as_deref(), Some("2027"));
        let again = merge(&dir, &[artifact(&["1.0", "1.1"], "2026"), artifact(&["1.10", "2.0-SNAPSHOT"], "2028")]).unwrap();
        assert_ne!(merged.etag, again.etag, "a member's change is a new ETag");
    }

    #[test]
    fn the_newest_snapshot_is_taken_whole_from_one_member() {
        let snap = |ts: &str, n: u32| {
            let level = SnapshotLevel {
                group: "g".into(),
                artifact: "a".into(),
                version: "1.0-SNAPSHOT".into(),
                timestamp: Some(ts.into()),
                build_number: Some(n),
                last_updated: Some(ts.replace('.', "")),
                entries: vec![SnapshotEntry {
                    classifier: None,
                    extension: "jar".into(),
                    value: format!("1.0-{ts}-{n}"),
                    updated: ts.replace('.', ""),
                }],
            };
            member(level.render(), ts)
        };
        let dir: Vec<String> = ["g", "a", "1.0-SNAPSHOT"].iter().map(|s| s.to_string()).collect();
        let merged = merge(&dir, &[snap("20260918.120000", 7), snap("20260918.130000", 2)]).unwrap();
        let doc = parse(&merged.body).unwrap().snapshot;
        assert_eq!(doc.build_number, Some(2));
        assert!(doc.entries.iter().all(|e| e.value == "1.0-20260918.130000-2"));
    }
}

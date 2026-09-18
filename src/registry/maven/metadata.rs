//! `maven-metadata.xml`: the three documents a repository renders (artifact,
//! snapshot and group level) and what the server reads out of the ones
//! clients and upstreams send.

use std::cmp::Ordering;

use chrono::{DateTime, Utc};

use super::version::compare;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArtifactLevel {
    pub group: String,
    pub artifact: String,
    /// Ascending, in Maven's order.
    pub versions: Vec<String>,
    pub latest: Option<String>,
    pub release: Option<String>,
    pub last_updated: Option<String>,
}

impl ArtifactLevel {
    /// `versions` sorted and deduplicated, `latest`/`release` their maxima
    /// unless a kept hint names one of them.
    pub fn computed(
        group: &str,
        artifact: &str,
        mut versions: Vec<String>,
        hints: (Option<&str>, Option<&str>),
        last_updated: Option<String>,
    ) -> Self {
        versions.sort_by(|a, b| compare(a, b).then_with(|| a.cmp(b)));
        versions.dedup();
        let known = |v: Option<&str>| v.filter(|v| versions.iter().any(|x| x == v)).map(String::from);
        let latest = known(hints.1).or_else(|| versions.last().cloned());
        let release = known(hints.0.filter(|v| !super::path::is_snapshot(v))).or_else(|| {
            versions
                .iter()
                .rev()
                .find(|v| !super::path::is_snapshot(v))
                .cloned()
        });
        Self {
            group: group.to_string(),
            artifact: artifact.to_string(),
            versions,
            latest,
            release,
            last_updated,
        }
    }

    pub fn render(&self) -> String {
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata>\n");
        element(&mut xml, 1, "groupId", &self.group);
        element(&mut xml, 1, "artifactId", &self.artifact);
        xml.push_str("  <versioning>\n");
        if let Some(latest) = &self.latest {
            element(&mut xml, 2, "latest", latest);
        }
        if let Some(release) = &self.release {
            element(&mut xml, 2, "release", release);
        }
        xml.push_str("    <versions>\n");
        for v in &self.versions {
            element(&mut xml, 3, "version", v);
        }
        xml.push_str("    </versions>\n");
        if let Some(at) = &self.last_updated {
            element(&mut xml, 2, "lastUpdated", at);
        }
        xml.push_str("  </versioning>\n</metadata>\n");
        xml
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotEntry {
    pub classifier: Option<String>,
    pub extension: String,
    pub value: String,
    pub updated: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnapshotLevel {
    pub group: String,
    pub artifact: String,
    pub version: String,
    /// `yyyyMMdd.HHmmss`, and the build number that goes with it.
    pub timestamp: Option<String>,
    pub build_number: Option<u32>,
    pub last_updated: Option<String>,
    pub entries: Vec<SnapshotEntry>,
}

impl SnapshotLevel {
    /// Which of two documents announces the newer build.
    pub fn newer(&self, other: &SnapshotLevel) -> Ordering {
        (self.timestamp.as_deref(), self.build_number)
            .cmp(&(other.timestamp.as_deref(), other.build_number))
            .then_with(|| self.last_updated.cmp(&other.last_updated))
    }

    pub fn render(&self) -> String {
        let mut xml = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata modelVersion=\"1.1.0\">\n",
        );
        element(&mut xml, 1, "groupId", &self.group);
        element(&mut xml, 1, "artifactId", &self.artifact);
        element(&mut xml, 1, "version", &self.version);
        xml.push_str("  <versioning>\n");
        if let (Some(timestamp), Some(number)) = (&self.timestamp, self.build_number) {
            xml.push_str("    <snapshot>\n");
            element(&mut xml, 3, "timestamp", timestamp);
            element(&mut xml, 3, "buildNumber", &number.to_string());
            xml.push_str("    </snapshot>\n");
        }
        if let Some(at) = &self.last_updated {
            element(&mut xml, 2, "lastUpdated", at);
        }
        xml.push_str("    <snapshotVersions>\n");
        for e in &self.entries {
            xml.push_str("      <snapshotVersion>\n");
            if let Some(c) = &e.classifier {
                element(&mut xml, 4, "classifier", c);
            }
            element(&mut xml, 4, "extension", &e.extension);
            element(&mut xml, 4, "value", &e.value);
            element(&mut xml, 4, "updated", &e.updated);
            xml.push_str("      </snapshotVersion>\n");
        }
        xml.push_str("    </snapshotVersions>\n  </versioning>\n</metadata>\n");
        xml
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GroupLevel {
    /// `(prefix, artifactId, name)`.
    pub plugins: Vec<(String, String, String)>,
}

impl GroupLevel {
    pub fn render(&self) -> String {
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<metadata>\n  <plugins>\n");
        for (prefix, artifact, name) in &self.plugins {
            xml.push_str("    <plugin>\n");
            element(&mut xml, 3, "name", name);
            element(&mut xml, 3, "prefix", prefix);
            element(&mut xml, 3, "artifactId", artifact);
            xml.push_str("    </plugin>\n");
        }
        xml.push_str("  </plugins>\n</metadata>\n");
        xml
    }
}

/// Whatever a `maven-metadata.xml` says, level by level.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Parsed {
    pub artifact: ArtifactLevel,
    pub snapshot: SnapshotLevel,
    pub plugins: Vec<(String, String, String)>,
}

pub fn parse(body: &[u8]) -> Result<Parsed, String> {
    let text = std::str::from_utf8(body).map_err(|e| format!("not UTF-8: {e}"))?;
    let doc = roxmltree::Document::parse(text).map_err(|e| format!("not XML: {e}"))?;
    let root = doc.root_element();
    if root.tag_name().name() != "metadata" {
        return Err("the root element is not <metadata>".to_string());
    }
    let mut parsed = Parsed::default();
    parsed.artifact.group = text_of(root, "groupId").unwrap_or_default();
    parsed.artifact.artifact = text_of(root, "artifactId").unwrap_or_default();
    parsed.snapshot.group = parsed.artifact.group.clone();
    parsed.snapshot.artifact = parsed.artifact.artifact.clone();
    parsed.snapshot.version = text_of(root, "version").unwrap_or_default();
    if let Some(versioning) = child(root, "versioning") {
        parsed.artifact.latest = text_of(versioning, "latest");
        parsed.artifact.release = text_of(versioning, "release");
        parsed.artifact.last_updated = text_of(versioning, "lastUpdated");
        parsed.snapshot.last_updated = parsed.artifact.last_updated.clone();
        if let Some(versions) = child(versioning, "versions") {
            parsed.artifact.versions = versions
                .children()
                .filter(|c| c.is_element() && c.tag_name().name() == "version")
                .filter_map(|c| c.text().map(|t| t.trim().to_string()))
                .filter(|t| !t.is_empty())
                .collect();
        }
        if let Some(snapshot) = child(versioning, "snapshot") {
            parsed.snapshot.timestamp = text_of(snapshot, "timestamp");
            parsed.snapshot.build_number =
                text_of(snapshot, "buildNumber").and_then(|n| n.parse().ok());
        }
        if let Some(list) = child(versioning, "snapshotVersions") {
            parsed.snapshot.entries = list
                .children()
                .filter(|c| c.is_element() && c.tag_name().name() == "snapshotVersion")
                .filter_map(|c| {
                    Some(SnapshotEntry {
                        classifier: text_of(c, "classifier"),
                        extension: text_of(c, "extension")?,
                        value: text_of(c, "value")?,
                        updated: text_of(c, "updated").unwrap_or_default(),
                    })
                })
                .collect();
        }
    }
    if let Some(plugins) = child(root, "plugins") {
        parsed.plugins = plugins
            .children()
            .filter(|c| c.is_element() && c.tag_name().name() == "plugin")
            .filter_map(|c| {
                Some((
                    text_of(c, "prefix")?,
                    text_of(c, "artifactId")?,
                    text_of(c, "name").unwrap_or_default(),
                ))
            })
            .collect();
    }
    Ok(parsed)
}

fn child<'a, 'i>(node: roxmltree::Node<'a, 'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
    node.children()
        .find(|c| c.is_element() && c.tag_name().name() == name)
}

fn text_of(node: roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    child(node, name)
        .and_then(|c| c.text())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// `yyyyMMddHHmmss`, the `lastUpdated`/`updated` form.
pub fn stamp(at: DateTime<Utc>) -> String {
    at.format("%Y%m%d%H%M%S").to_string()
}

fn element(xml: &mut String, depth: usize, name: &str, value: &str) {
    for _ in 0..depth {
        xml.push_str("  ");
    }
    xml.push('<');
    xml.push_str(name);
    xml.push('>');
    for c in value.chars() {
        match c {
            '&' => xml.push_str("&amp;"),
            '<' => xml.push_str("&lt;"),
            '>' => xml.push_str("&gt;"),
            '"' => xml.push_str("&quot;"),
            '\'' => xml.push_str("&apos;"),
            _ => xml.push(c),
        }
    }
    xml.push_str("</");
    xml.push_str(name);
    xml.push_str(">\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_artifact_document_round_trips_and_recomputes_its_maxima() {
        let level = ArtifactLevel::computed(
            "org.example",
            "lib",
            vec!["1.10".into(), "1.2".into(), "2.0-SNAPSHOT".into(), "1.2".into()],
            (None, None),
            Some("20260918120000".into()),
        );
        assert_eq!(level.versions, ["1.2", "1.10", "2.0-SNAPSHOT"]);
        assert_eq!(level.latest.as_deref(), Some("2.0-SNAPSHOT"));
        assert_eq!(level.release.as_deref(), Some("1.10"));
        let parsed = parse(level.render().as_bytes()).unwrap();
        assert_eq!(parsed.artifact, level);

        let hinted = ArtifactLevel::computed(
            "g",
            "a",
            vec!["1.0".into(), "2.0".into()],
            (Some("1.0"), Some("9.9")),
            None,
        );
        assert_eq!(hinted.release.as_deref(), Some("1.0"), "a hint naming a listed version is kept");
        assert_eq!(hinted.latest.as_deref(), Some("2.0"), "one naming nothing listed is not");
    }

    #[test]
    fn a_snapshot_document_round_trips() {
        let level = SnapshotLevel {
            group: "g".into(),
            artifact: "a".into(),
            version: "1.0-SNAPSHOT".into(),
            timestamp: Some("20260918.120000".into()),
            build_number: Some(3),
            last_updated: Some("20260918120001".into()),
            entries: vec![SnapshotEntry {
                classifier: Some("sources".into()),
                extension: "jar".into(),
                value: "1.0-20260918.120000-3".into(),
                updated: "20260918120000".into(),
            }],
        };
        assert_eq!(parse(level.render().as_bytes()).unwrap().snapshot, level);
    }

    #[test]
    fn plugins_are_read_and_values_escaped() {
        let group = GroupLevel {
            plugins: vec![("ex".into(), "ex-maven-plugin".into(), "A & B".into())],
        };
        let xml = group.render();
        assert!(xml.contains("A &amp; B"));
        assert_eq!(parse(xml.as_bytes()).unwrap().plugins, group.plugins);
        assert!(parse(b"<nope/>").is_err());
        assert!(parse(b"not xml").is_err());
    }
}

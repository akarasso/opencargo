//! A repository-relative Maven path, parsed before any key or URL is built:
//! `group/artifact/version/file`, a `maven-metadata.xml` in a directory, and
//! the checksum files beside either.

use crate::domain::Format;
use crate::error::{AppError, AppResult};
use crate::ports::maven::SumAlgorithm;
use crate::registry::rules::rules_of;

pub const METADATA: &str = "maven-metadata.xml";
pub const SNAPSHOT: &str = "SNAPSHOT";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gav {
    pub group: String,
    pub artifact: String,
    pub version: String,
}

impl Gav {
    pub fn ga(&self) -> String {
        format!("{}:{}", self.group, self.artifact)
    }

    pub fn is_snapshot(&self) -> bool {
        is_snapshot(&self.version)
    }

    /// `group/as/path/artifact`, the directory of the artifact.
    pub fn artifact_dir(&self) -> String {
        format!("{}/{}", self.group.replace('.', "/"), self.artifact)
    }

    pub fn version_dir(&self) -> String {
        format!("{}/{}", self.artifact_dir(), self.version)
    }
}

pub fn is_snapshot(version: &str) -> bool {
    version.ends_with("-SNAPSHOT")
}

/// One file of a version: its build is the `yyyyMMdd.HHmmss-N` a snapshot
/// deposit is stamped with, empty otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactFile {
    pub gav: Gav,
    pub filename: String,
    pub build: String,
    pub classifier: Option<String>,
    pub extension: String,
}

impl ArtifactFile {
    pub fn is_pom(&self) -> bool {
        self.classifier.is_none() && self.extension == "pom"
    }

    pub fn path(&self) -> String {
        format!("{}/{}", self.gav.version_dir(), self.filename)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    File(ArtifactFile),
    /// A `maven-metadata.xml`, by its directory's segments.
    Metadata(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MavenPath {
    pub target: Target,
    pub sum: Option<SumAlgorithm>,
}

/// What a metadata directory can be: its last segment is either a snapshot
/// version under an artifact, or an artifact, or a group (for plugins).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataLevel {
    Snapshot(Gav),
    Artifact { group: String, artifact: String },
}

impl MetadataLevel {
    pub fn of(dir: &[String]) -> Option<Self> {
        match dir {
            [group @ .., artifact, version] if is_snapshot(version) && !group.is_empty() => {
                Some(MetadataLevel::Snapshot(Gav {
                    group: group.join("."),
                    artifact: artifact.clone(),
                    version: version.clone(),
                }))
            }
            [group @ .., artifact] if !group.is_empty() => Some(MetadataLevel::Artifact {
                group: group.join("."),
                artifact: artifact.clone(),
            }),
            _ => None,
        }
    }
}

/// `yyyyMMdd.HHmmss-N`, the timestamped build of a snapshot file.
pub fn parse_build(s: &str) -> Option<(&str, u32, usize)> {
    let bytes = s.as_bytes();
    if bytes.len() < 17 || bytes[8] != b'.' || bytes[15] != b'-' {
        return None;
    }
    let digits = |r: std::ops::Range<usize>| bytes[r].iter().all(u8::is_ascii_digit);
    if !digits(0..8) || !digits(9..15) {
        return None;
    }
    let n_len = bytes[16..].iter().take_while(|b| b.is_ascii_digit()).count();
    if n_len == 0 {
        return None;
    }
    let number: u32 = s[16..16 + n_len].parse().ok()?;
    Some((&s[..15], number, 16 + n_len))
}

fn invalid(path: &str) -> AppError {
    AppError::BadRequest(format!("invalid maven path: '{path}'"))
}

fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'))
}

impl MavenPath {
    pub fn parse(path: &str) -> AppResult<Self> {
        let segments: Vec<&str> = path.split('/').collect();
        if path.len() > 1024 || !segments.iter().all(|s| valid_segment(s)) {
            return Err(invalid(path));
        }
        let (dirs, name) = segments.split_at(segments.len() - 1);
        let (name, sum) = split_sum(name[0]);
        if name == METADATA {
            if dirs.is_empty() {
                return Err(invalid(path));
            }
            return Ok(Self {
                target: Target::Metadata(dirs.iter().map(|s| s.to_string()).collect()),
                sum,
            });
        }
        let file = artifact_file(dirs, name).ok_or_else(|| invalid(path))?;
        let rules = rules_of(Format::Maven)?;
        rules.validate(&file.gav.ga()).map_err(|_| invalid(path))?;
        rules.validate_version(&file.gav.version).map_err(|_| invalid(path))?;
        Ok(Self {
            target: Target::File(file),
            sum,
        })
    }
}

fn split_sum(name: &str) -> (&str, Option<SumAlgorithm>) {
    for algorithm in SumAlgorithm::ALL {
        if let Some(base) = name.strip_suffix(&format!(".{}", algorithm.as_str())) {
            if !base.is_empty() {
                return (base, Some(algorithm));
            }
        }
    }
    (name, None)
}

fn artifact_file(dirs: &[&str], name: &str) -> Option<ArtifactFile> {
    let [group @ .., artifact, version] = dirs else {
        return None;
    };
    if group.is_empty() {
        return None;
    }
    let gav = Gav {
        group: group.join("."),
        artifact: artifact.to_string(),
        version: version.to_string(),
    };
    let rest = name.strip_prefix(*artifact)?.strip_prefix('-')?;
    let (build, tail) = match version.strip_suffix("-SNAPSHOT") {
        Some(base) => {
            let after = rest.strip_prefix(base)?.strip_prefix('-')?;
            match after.strip_prefix(SNAPSHOT) {
                Some(tail) => (String::new(), tail),
                None => {
                    let (_, _, used) = parse_build(after)?;
                    (after[..used].to_string(), &after[used..])
                }
            }
        }
        None => (String::new(), rest.strip_prefix(*version)?),
    };
    let (classifier, extension) = match tail.as_bytes().first()? {
        b'.' => (None, &tail[1..]),
        b'-' => {
            let (classifier, extension) = tail[1..].split_once('.')?;
            if classifier.is_empty() {
                return None;
            }
            (Some(classifier.to_string()), extension)
        }
        _ => return None,
    };
    if extension.is_empty() {
        return None;
    }
    Some(ArtifactFile {
        gav,
        filename: name.to_string(),
        build,
        classifier,
        extension: extension.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str) -> ArtifactFile {
        match MavenPath::parse(path).unwrap().target {
            Target::File(f) => f,
            other => panic!("{path}: {other:?}"),
        }
    }

    #[test]
    fn release_files_split_into_coordinates_classifier_and_extension() {
        let jar = file("org/example/lib/1.0/lib-1.0.jar");
        assert_eq!(jar.gav.ga(), "org.example:lib");
        assert_eq!((jar.build.as_str(), jar.classifier.as_deref(), jar.extension.as_str()), ("", None, "jar"));
        let sources = file("org/example/lib/1.0/lib-1.0-sources.jar");
        assert_eq!(sources.classifier.as_deref(), Some("sources"));
        assert!(file("org/example/lib/1.0/lib-1.0.pom").is_pom());
        assert_eq!(file("org/example/lib/1.0/lib-1.0.tar.gz").extension, "tar.gz");
        assert_eq!(file("org/example/lib/1.0/lib-1.0.jar.asc").extension, "jar.asc");
    }

    #[test]
    fn snapshot_files_carry_their_build() {
        let jar = file("org/example/lib/1.0-SNAPSHOT/lib-1.0-20260918.120000-3.jar");
        assert_eq!(jar.build, "20260918.120000-3");
        assert_eq!(jar.gav.version, "1.0-SNAPSHOT");
        let tests = file("org/example/lib/1.0-SNAPSHOT/lib-1.0-20260918.120000-12-tests.jar");
        assert_eq!((tests.build.as_str(), tests.classifier.as_deref()), ("20260918.120000-12", Some("tests")));
        let plain = file("org/example/lib/1.0-SNAPSHOT/lib-1.0-SNAPSHOT.pom");
        assert_eq!(plain.build, "");
        assert!(plain.is_pom());
    }

    #[test]
    fn sums_and_metadata_are_recognized() {
        let sum = MavenPath::parse("org/example/lib/1.0/lib-1.0.jar.sha512").unwrap();
        assert_eq!(sum.sum, Some(SumAlgorithm::Sha512));
        let meta = MavenPath::parse("org/example/lib/maven-metadata.xml.md5").unwrap();
        assert_eq!(meta.sum, Some(SumAlgorithm::Md5));
        assert_eq!(meta.target, Target::Metadata(vec!["org".into(), "example".into(), "lib".into()]));
        assert_eq!(
            MetadataLevel::of(&["org".into(), "lib".into(), "1.0-SNAPSHOT".into()]),
            Some(MetadataLevel::Snapshot(Gav {
                group: "org".into(),
                artifact: "lib".into(),
                version: "1.0-SNAPSHOT".into()
            }))
        );
        assert_eq!(MetadataLevel::of(&["lib".into()]), None);
    }

    #[test]
    fn anything_else_is_refused_before_a_key_is_built() {
        for bad in [
            "lib/1.0/lib-1.0.jar",
            "org/example/lib/1.0/other-1.0.jar",
            "org/example/lib/1.0/lib-1.1.jar",
            "org/example/lib/1.0/lib-1.0",
            "org/example/lib/1.0/lib-1.0-.jar",
            "org/example/lib/1.0/lib-1.0.",
            "org/../lib/1.0/lib-1.0.jar",
            "org//lib/1.0/lib-1.0.jar",
            "org/example/lib/1.0-SNAPSHOT/lib-1.0-2026.jar",
            "maven-metadata.xml",
            "org/ex ample/lib/1.0/lib-1.0.jar",
        ] {
            assert!(MavenPath::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn builds_parse_timestamp_and_number() {
        assert_eq!(parse_build("20260918.120000-3.jar"), Some(("20260918.120000", 3, 17)));
        assert_eq!(parse_build("20260918.120000-"), None);
        assert_eq!(parse_build("2026091.1200000-1"), None);
    }
}

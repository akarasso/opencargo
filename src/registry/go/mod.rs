pub mod escape;
pub mod leaves;
pub mod publish;
pub mod read;
pub mod routes;
pub mod upstream;

use serde_json::{json, Value};

use crate::domain::Version;
use crate::wire::wire_ts;

/// `v` stripped and parsed as semver (pseudo-versions are pre-releases);
/// unparseable versions rank below every parseable one and compare lexically.
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |v: &str| semver::Version::parse(v.strip_prefix('v').unwrap_or(v)).ok();
    match (parse(a), parse(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (None, None) => a.cmp(b),
    }
}

fn latest_of(versions: &[Version]) -> Option<&Version> {
    versions
        .iter()
        .max_by(|a, b| compare_versions(&a.version, &b.version))
}

/// The `.info` and `@latest` document of a hosted version.
fn info_json(version: &Version) -> Value {
    json!({
        "Version": version.version,
        "Time": wire_ts(version.published_at),
    })
}

#[cfg(test)]
mod tests {
    use super::compare_versions;
    use std::cmp::Ordering;

    #[test]
    fn versions_order_by_semver_then_lexically() {
        assert_eq!(compare_versions("v1.10.0", "v1.9.0"), Ordering::Greater);
        assert_eq!(compare_versions("v1.0.0", "v1.0.0-rc1"), Ordering::Greater);
        assert_eq!(
            compare_versions("v0.0.0-20230101120000-abcdef123456", "v0.1.0"),
            Ordering::Less
        );
        assert_eq!(compare_versions("v1.0.0", "master"), Ordering::Greater);
        assert_eq!(compare_versions("dev", "master"), Ordering::Less);
    }
}

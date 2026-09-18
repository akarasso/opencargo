//! How a domain value is written into an HTTP body. The mirror of the SQLite
//! adapter's `bind_ts` and of `RepoConfig::from_json`: a timestamp reaches a
//! client as RFC 3339, never as the storage format, and a repository with no
//! config document reaches it as `null`, never as an empty member list.

use chrono::{DateTime, SecondsFormat, Utc};

use crate::domain::RepoConfig;

pub fn wire_ts(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn wire_config(config: Option<&RepoConfig>) -> Option<String> {
    config.map(RepoConfig::to_json)
}

/// The dashboard's panels still read raw tuples, so their timestamps are
/// decoded here on the way out rather than at a row conversion; step 8c gives
/// them typed rows and this goes with them. An unreadable column serves
/// nothing rather than a second format.
pub fn wire_stored_ts(stored: &str) -> String {
    match crate::db::parse_ts(stored) {
        Some(at) => wire_ts(at),
        None => {
            tracing::warn!("unreadable stored timestamp: {stored:?}");
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_rfc_3339_to_the_second() {
        let at = DateTime::from_timestamp(1_758_153_863, 0).unwrap();
        assert_eq!(wire_ts(at), "2025-09-18T00:04:23Z");
        assert_eq!(
            wire_stored_ts("2025-09-18 00:04:23"),
            "2025-09-18T00:04:23Z"
        );
        assert_eq!(wire_stored_ts("not a timestamp"), "");
    }

    /// `None` is JSON `null`, which is what every hosted and every proxy
    /// repository serves; an empty member list is a different document.
    #[test]
    fn an_absent_config_is_not_an_empty_one() {
        assert_eq!(wire_config(None), None);
        assert_eq!(
            wire_config(Some(&RepoConfig::default())).as_deref(),
            Some(r#"{"members":[]}"#)
        );
    }
}

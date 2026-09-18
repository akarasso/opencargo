//! What a caching proxy decides, and what it remembers.
//!
//! Every type here would be true of a caching registry proxy written in any
//! language: how long an answer may be reused, how a body travels, what an
//! upstream status means, who chose a URL's host — and the cache row itself,
//! whose instants are values rather than one storage format's rendering of
//! them.

use std::time::Duration;

use chrono::{DateTime, Utc};

/// A repository's identity, and a cache row's, as the stores speak of them.
pub type RepoId = i64;
pub type CacheEntryId = i64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ttl {
    Default,
    Secs(u64),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CachePolicy {
    Immutable,
    Ttl(Ttl),
}

/// Every body is written to disk as it arrives; `Buffered` only bounds the
/// whole transfer, for the small documents a request waits on before
/// answering.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transfer {
    Buffered,
    Streamed,
}

/// `Miss` is an authoritative "does not exist" worth a negative row;
/// `Refused` is a 404 to the client too, but asked again next time, since a
/// credential or rate-limit problem looks the same as an unknown artifact.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Classified {
    Miss,
    Refused,
    Fail,
}

/// Who chose the host of an upstream URL: the admin (the configured
/// upstream, trusted as is) or upstream content (held to the private-range
/// guard unless the member opted in).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UrlSource {
    Admin,
    Content { allow_private: bool },
}

/// One remembered upstream answer: where its body is, what it was, and until
/// when it may be served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheEntry {
    pub id: CacheEntryId,
    pub repository_id: RepoId,
    pub kind: String,
    pub cache_key: String,
    pub status: i64,
    pub storage_path: Option<String>,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub digest: Option<String>,
    pub size: i64,
    pub fetched_at: DateTime<Utc>,
    /// `None` is an immutable answer: nothing to re-ask.
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: DateTime<Utc>,
    /// Whether the row had not expired at the `now` the read was made with.
    /// The reader's clock decides it, never the store's.
    pub fresh: bool,
}

impl CacheEntry {
    /// Whether a row expiring at `expires_at` is still servable at `now`:
    /// the rule spelled once, so no adapter and no fake invents its own.
    pub fn fresh_at(expires_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
        expires_at.is_none_or(|until| until > now)
    }

    /// When an answer written at `now` under `ttl` stops being servable.
    pub fn expiry(ttl: Option<Duration>, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        ttl.map(|ttl| now + ttl)
    }
}

/// An answer to remember: borrowed in, owned out.
pub struct NewEntry<'a> {
    pub repository_id: RepoId,
    pub kind: &'a str,
    pub cache_key: &'a str,
    pub status: i64,
    pub storage_path: Option<&'a str>,
    pub content_type: Option<&'a str>,
    pub etag: Option<&'a str>,
    pub digest: Option<&'a str>,
    pub size: i64,
    /// `None` is immutable: no expiry is written at all.
    pub ttl_secs: Option<u64>,
}

impl NewEntry<'_> {
    pub fn ttl(&self) -> Option<Duration> {
        self.ttl_secs.map(Duration::from_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 18, hour, 0, 0).unwrap()
    }

    #[test]
    fn an_answer_is_stale_from_its_expiry_onwards() {
        let until = Some(at(10));
        assert!(CacheEntry::fresh_at(until, at(9)));
        assert!(
            !CacheEntry::fresh_at(until, at(10)),
            "at the expiry, not after it"
        );
        assert!(!CacheEntry::fresh_at(until, at(11)));
    }

    #[test]
    fn an_answer_with_no_expiry_is_fresh_forever() {
        assert!(CacheEntry::fresh_at(None, at(23)));
        assert_eq!(CacheEntry::expiry(None, at(9)), None);
    }

    #[test]
    fn a_ttl_is_counted_from_the_instant_the_answer_was_written() {
        let hour = Duration::from_secs(3600);
        assert_eq!(CacheEntry::expiry(Some(hour), at(9)), Some(at(10)));
        assert_eq!(
            NewEntry {
                repository_id: 1,
                kind: "npm-metadata",
                cache_key: "lodash",
                status: 200,
                storage_path: None,
                content_type: None,
                etag: None,
                digest: None,
                size: 0,
                ttl_secs: Some(3600),
            }
            .ttl(),
            Some(hour)
        );
    }
}

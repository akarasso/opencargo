//! Accounts and the credentials that stand for them.

use chrono::{DateTime, Utc};

/// An account. The password hash travels with it because checking a login is
/// the only thing anything ever does with the account, and neither the hash
/// nor this type ever reaches a response body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub password_hash: String,
    pub role: String,
    /// While set, the account may only change its password — a forced
    /// rotation that a long-lived token cannot outlive.
    pub must_change_password: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A long-lived credential standing for a user. Only its hash is kept, so a
/// leaked store cannot be replayed against the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiToken {
    pub id: String,
    pub user_id: i64,
    pub name: String,
    /// The first 16 bytes of the raw token: what a lookup is keyed on, since
    /// the hash cannot be searched for.
    pub prefix: String,
    pub token_hash: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl ApiToken {
    /// A token with no expiry never expires; one with an expiry is live up to
    /// and including the instant it names.
    pub fn is_live(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_none_or(|at| at >= now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 18, hour, 30, 0).unwrap()
    }

    fn token(expires_at: Option<DateTime<Utc>>) -> ApiToken {
        ApiToken {
            id: "t-1".to_string(),
            user_id: 1,
            name: "ci-runner".to_string(),
            prefix: "trg_000000000000".to_string(),
            token_hash: "hash".to_string(),
            expires_at,
            last_used_at: None,
            created_at: at(9),
        }
    }

    #[test]
    fn a_token_without_an_expiry_never_expires() {
        assert!(token(None).is_live(at(23)));
    }

    /// The boundary is inclusive, and it is the one an off-by-one would make
    /// a live credential fail an authentication on.
    #[test]
    fn an_expiry_is_live_up_to_the_instant_it_names() {
        let token = token(Some(at(12)));
        assert!(token.is_live(at(11)));
        assert!(token.is_live(at(12)));
        assert!(!token.is_live(at(13)));
    }
}

//! What each provider type says in its ID Token, projected onto the
//! domain's neutral identity: issuer rules, group source, e-mail trust and
//! tenant.

use serde_json::{Map, Value};

use crate::domain::identity::{Authority, EmailTrust, ExternalIdentity, Groups, IdentityKey};

pub const GOOGLE_ISSUER: &str = "https://accounts.google.com";
pub const ENTRA_ISSUER_TEMPLATE: &str = "https://login.microsoftonline.com/{tid}/v2.0";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Google,
    /// `tenant` is a tenant id (pinned) or `common` / `organizations`.
    Entra {
        tenant: String,
    },
    Gitlab,
    Generic {
        groups_claim: Option<String>,
        open: bool,
    },
}

impl Kind {
    pub fn tenant_pinned(&self) -> bool {
        matches!(self, Kind::Entra { tenant } if !matches!(tenant.as_str(), "common" | "organizations" | "consumers"))
    }

    pub fn open(&self, issuer: &str) -> bool {
        match self {
            Kind::Google => true,
            Kind::Entra { .. } => !self.tenant_pinned(),
            Kind::Gitlab => url::Url::parse(issuer)
                .ok()
                .and_then(|u| u.host_str().map(|h| h == "gitlab.com"))
                .unwrap_or(false),
            Kind::Generic { open, .. } => *open,
        }
    }
}

/// Where discovery lives, what issuer it must announce, and the authority
/// identities are keyed under. `issuer` is the configured one: the fixed
/// Google issuer, an Entra template with `{tid}`, or an instance URL.
pub struct Issuers {
    pub discovery_base: String,
    pub discovery_issuer: String,
    pub authority_issuer: String,
}

pub fn issuers(kind: &Kind, issuer: &str) -> Issuers {
    match kind {
        Kind::Entra { tenant } => {
            let concrete = issuer.replace("{tid}", tenant);
            let (discovery_issuer, authority_issuer) = if kind.tenant_pinned() {
                (concrete.clone(), concrete.clone())
            } else {
                (issuer.replace("{tid}", "{tenantid}"), issuer.to_string())
            };
            Issuers {
                discovery_base: concrete,
                discovery_issuer,
                authority_issuer,
            }
        }
        _ => Issuers {
            discovery_base: issuer.to_string(),
            discovery_issuer: issuer.to_string(),
            authority_issuer: issuer.to_string(),
        },
    }
}

fn same_issuer(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

fn string(claims: &Map<String, Value>, name: &str) -> Option<String> {
    claims.get(name).and_then(Value::as_str).map(str::to_string)
}

fn truthy(claims: &Map<String, Value>, name: &str) -> bool {
    match claims.get(name) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true",
        _ => false,
    }
}

fn list(value: Option<&Value>) -> Groups {
    match value {
        None => Groups::Listed(Vec::new()),
        Some(Value::Array(items)) => {
            let names: Option<Vec<String>> = items
                .iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect();
            names.map_or(Groups::Unreadable, Groups::Listed)
        }
        Some(_) => Groups::Unreadable,
    }
}

/// The verified claims of one ID Token, projected. `Err` names the claim
/// that does not hold; the signature, audience, expiry and nonce were
/// checked before this.
pub fn project(
    kind: &Kind,
    provider: &str,
    configured_issuer: &str,
    issuers: &Issuers,
    claims: &Map<String, Value>,
) -> Result<ExternalIdentity, String> {
    let iss = string(claims, "iss").ok_or("no iss")?;
    let sub = string(claims, "sub").ok_or("no sub")?;
    let email = string(claims, "email");
    let (subject, tenant, email_trust, groups) = match kind {
        Kind::Entra { tenant } => {
            let tid = string(claims, "tid").ok_or("no tid")?;
            if !same_issuer(&iss, &configured_issuer.replace("{tid}", &tid)) {
                return Err(format!("iss {iss} does not match the tenant {tid}"));
            }
            let pinned = kind.tenant_pinned();
            if pinned && &tid != tenant {
                return Err(format!("tenant {tid} is not the configured one"));
            }
            let overage = claims
                .get("_claim_names")
                .and_then(|n| n.get("groups"))
                .is_some()
                || claims.contains_key("hasgroups");
            let groups = if overage {
                Groups::Unreadable
            } else {
                list(claims.get("groups"))
            };
            let trust = if pinned {
                EmailTrust::Verified
            } else {
                EmailTrust::Unverified
            };
            let subject = if pinned { sub } else { format!("{tid}/{sub}") };
            (subject, Some(tid), trust, groups)
        }
        _ => {
            if !same_issuer(&iss, &issuers.discovery_issuer) {
                return Err(format!("iss {iss} is not {}", issuers.discovery_issuer));
            }
            let trust = if truthy(claims, "email_verified") {
                EmailTrust::Verified
            } else {
                EmailTrust::Unverified
            };
            let (tenant, groups) = match kind {
                Kind::Google => (string(claims, "hd"), Groups::NotProvided),
                Kind::Gitlab => (None, list(claims.get("groups_direct"))),
                Kind::Generic {
                    groups_claim: Some(name),
                    ..
                } => (None, list(claims.get(name))),
                _ => (None, Groups::NotProvided),
            };
            (sub, tenant, trust, groups)
        }
    };
    Ok(ExternalIdentity {
        key: IdentityKey {
            authority: Authority::new(provider, &issuers.authority_issuer),
            subject,
        },
        email,
        email_trust,
        tenant,
        groups,
        preferred_name: string(claims, "preferred_username").or_else(|| string(claims, "nickname")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn claims(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn entra_issuer_is_per_tenant_and_a_pinned_tenant_is_enforced() {
        let pinned = Kind::Entra {
            tenant: "t1".into(),
        };
        let i = issuers(&pinned, ENTRA_ISSUER_TEMPLATE);
        assert_eq!(
            i.discovery_issuer,
            "https://login.microsoftonline.com/t1/v2.0"
        );
        let ok = claims(
            json!({"iss": "https://login.microsoftonline.com/t1/v2.0", "sub": "s", "tid": "t1",
                                "email": "a@corp.example", "groups": ["g"]}),
        );
        let id = project(&pinned, "entra", ENTRA_ISSUER_TEMPLATE, &i, &ok).unwrap();
        assert_eq!(id.email_trust, EmailTrust::Verified);
        assert_eq!(id.groups, Groups::Listed(vec!["g".into()]));
        let other = claims(
            json!({"iss": "https://login.microsoftonline.com/t2/v2.0", "sub": "s", "tid": "t2"}),
        );
        assert!(project(&pinned, "entra", ENTRA_ISSUER_TEMPLATE, &i, &other).is_err());
        let forged = claims(
            json!({"iss": "https://login.microsoftonline.com/t2/v2.0", "sub": "s", "tid": "t1"}),
        );
        assert!(project(&pinned, "entra", ENTRA_ISSUER_TEMPLATE, &i, &forged).is_err());
    }

    #[test]
    fn entra_common_never_trusts_email_and_overage_is_unreadable() {
        let common = Kind::Entra {
            tenant: "common".into(),
        };
        let i = issuers(&common, ENTRA_ISSUER_TEMPLATE);
        assert_eq!(
            i.discovery_issuer,
            "https://login.microsoftonline.com/{tenantid}/v2.0"
        );
        let c = claims(
            json!({"iss": "https://login.microsoftonline.com/t9/v2.0", "sub": "s", "tid": "t9",
                               "email_verified": true, "email": "a@x.example",
                               "_claim_names": {"groups": "src1"}}),
        );
        let id = project(&common, "entra", ENTRA_ISSUER_TEMPLATE, &i, &c).unwrap();
        assert_eq!(id.email_trust, EmailTrust::Unverified);
        assert_eq!(id.groups, Groups::Unreadable);
        assert_eq!(id.key.subject, "t9/s");
        assert!(common.open(ENTRA_ISSUER_TEMPLATE));
    }

    #[test]
    fn google_and_gitlab_project_their_own_claims() {
        let i = issuers(&Kind::Google, GOOGLE_ISSUER);
        let g = claims(json!({"iss": "accounts.google.com", "sub": "s"}));
        assert!(
            project(&Kind::Google, "g", GOOGLE_ISSUER, &i, &g).is_err(),
            "the bare host form is not the issuer"
        );
        let g = claims(
            json!({"iss": GOOGLE_ISSUER, "sub": "s", "hd": "corp.example",
                               "email": "a@corp.example", "email_verified": true}),
        );
        let id = project(&Kind::Google, "g", GOOGLE_ISSUER, &i, &g).unwrap();
        assert_eq!(id.tenant.as_deref(), Some("corp.example"));
        assert_eq!(id.groups, Groups::NotProvided);
        assert_eq!(id.email_trust, EmailTrust::Verified);

        let gl = "https://gitlab.example";
        let i = issuers(&Kind::Gitlab, gl);
        let c = claims(json!({"iss": gl, "sub": "7", "groups_direct": ["a/b"]}));
        let id = project(&Kind::Gitlab, "gl", gl, &i, &c).unwrap();
        assert_eq!(id.groups, Groups::Listed(vec!["a/b".into()]));
        assert!(!Kind::Gitlab.open(gl));
        assert!(Kind::Gitlab.open("https://gitlab.com"));
        let bad = claims(json!({"iss": gl, "sub": "7", "groups_direct": "a/b"}));
        assert_eq!(
            project(&Kind::Gitlab, "gl", gl, &i, &bad).unwrap().groups,
            Groups::Unreadable
        );
    }
}

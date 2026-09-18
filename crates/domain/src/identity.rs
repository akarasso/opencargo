//! External identities, once an adapter has verified them. The domain never
//! learns which protocol or which provider produced one: it reasons on an
//! authority, a subject, an e-mail with its trust, an opaque tenant and a
//! group list.

use chrono::{DateTime, Duration, Utc};

use crate::permission::Rights;

/// An issuer with its trailing `/` dropped: two spellings of one issuer are
/// one authority.
pub fn normalize_issuer(issuer: &str) -> &str {
    issuer.trim_end_matches('/')
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Authority {
    pub provider: String,
    pub issuer: String,
}

impl Authority {
    pub fn new(provider: &str, issuer: &str) -> Self {
        Self {
            provider: provider.to_string(),
            issuer: normalize_issuer(issuer).to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct IdentityKey {
    pub authority: Authority,
    pub subject: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmailTrust {
    Unverified,
    Verified,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Groups {
    /// The provider carries no group source; rules on groups cannot match.
    NotProvided,
    Listed(Vec<String>),
    /// The source exists but its value could not be read (overage included).
    Unreadable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalIdentity {
    pub key: IdentityKey,
    pub email: Option<String>,
    pub email_trust: EmailTrust,
    pub tenant: Option<String>,
    pub groups: Groups,
    pub preferred_name: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantRole {
    Reader,
    Publisher,
}

impl GrantRole {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "reader" => Some(Self::Reader),
            "publisher" => Some(Self::Publisher),
            _ => None,
        }
    }

    pub fn rights(self) -> Rights {
        match self {
            Self::Reader => Rights {
                read: true,
                write: false,
                delete: false,
                admin: false,
            },
            Self::Publisher => Rights {
                read: true,
                write: true,
                delete: false,
                admin: false,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupGrant {
    pub group: String,
    pub repository: String,
    pub role: GrantRole,
}

/// What an operator's rules say about one verified identity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoginPolicy {
    /// Any one of them admits; empty admits everyone the other rules admit.
    pub required_groups: Vec<String>,
    /// E-mail domains a verified address must belong to; empty admits any.
    pub allowed_domains: Vec<String>,
    pub default_role: String,
    pub grants: Vec<GroupGrant>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Access {
        role: String,
        grants: Vec<(String, GrantRole)>,
    },
    Denied(DenyReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DenyReason {
    NotInRequiredGroup,
    DomainNotAllowed,
}

/// The claims could not be read: nothing is changed on its account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    GroupsUnreadable,
}

impl LoginPolicy {
    pub fn groups_of(&self, identity: &ExternalIdentity) -> Result<Vec<String>, Rejected> {
        match &identity.groups {
            Groups::NotProvided => Ok(Vec::new()),
            Groups::Listed(groups) => Ok(groups.clone()),
            Groups::Unreadable => Err(Rejected::GroupsUnreadable),
        }
    }

    pub fn decide(&self, identity: &ExternalIdentity) -> Result<Decision, Rejected> {
        let groups = self.groups_of(identity)?;
        if !self.allowed_domains.is_empty() {
            let ok = identity.email_trust == EmailTrust::Verified
                && identity
                    .email
                    .as_deref()
                    .and_then(email_domain)
                    .is_some_and(|d| {
                        self.allowed_domains
                            .iter()
                            .any(|a| a.eq_ignore_ascii_case(d))
                    });
            if !ok {
                return Ok(Decision::Denied(DenyReason::DomainNotAllowed));
            }
        }
        if !self.required_groups.is_empty()
            && !self.required_groups.iter().any(|g| groups.contains(g))
        {
            return Ok(Decision::Denied(DenyReason::NotInRequiredGroup));
        }
        let grants = self
            .grants
            .iter()
            .filter(|g| groups.contains(&g.group))
            .map(|g| (g.repository.clone(), g.role))
            .collect();
        Ok(Decision::Access {
            role: self.default_role.clone(),
            grants,
        })
    }

    /// A rule that narrows who may log in, which an open issuer requires.
    pub fn is_restrictive(&self) -> bool {
        !self.required_groups.is_empty() || !self.allowed_domains.is_empty()
    }
}

pub fn email_domain(email: &str) -> Option<&str> {
    email
        .rsplit_once('@')
        .map(|(_, d)| d)
        .filter(|d| !d.is_empty())
}

/// One configured provider, in the domain's words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderProfile {
    pub authority: Authority,
    /// Anyone on the internet can hold an account at this issuer.
    pub open: bool,
    /// The operator accepts an open issuer without a restrictive rule.
    pub allow_open: bool,
    /// The tenant is fixed by configuration, so the issuer is not open.
    pub tenant_pinned: bool,
    pub authoritative_domains: Vec<String>,
    pub policy: LoginPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigRefusal {
    AdminGrant(String),
    AdminDefaultRole(String),
    UnknownRole(String),
    DuplicateIssuer(String),
    OpenIssuer(String),
    UndeclaredAuthority(String),
}

impl std::fmt::Display for ConfigRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AdminGrant(p) => write!(f, "provider {p}: grants may only name reader or publisher"),
            Self::AdminDefaultRole(p) => write!(f, "provider {p}: default_role may not be admin"),
            Self::UnknownRole(p) => write!(f, "provider {p}: unknown default_role"),
            Self::DuplicateIssuer(i) => write!(f, "two providers declare the issuer {i}"),
            Self::OpenIssuer(p) => write!(
                f,
                "provider {p}: open issuer without a restrictive rule; add a rule or allow_open = true"
            ),
            Self::UndeclaredAuthority(a) => write!(
                f,
                "known authority {a} is absent from the configuration: declare retired = true or issuer_was"
            ),
        }
    }
}

/// Every refusal the configuration can earn, and the providers accepted
/// open by explicit opt-in, which the caller logs on every start.
pub fn validate_profiles(profiles: &[ProviderProfile]) -> Result<Vec<String>, ConfigRefusal> {
    let mut issuers: Vec<&str> = Vec::new();
    let mut opted_in = Vec::new();
    for p in profiles {
        let name = &p.authority.provider;
        match p.policy.default_role.as_str() {
            "admin" => return Err(ConfigRefusal::AdminDefaultRole(name.clone())),
            "reader" | "publisher" => {}
            _ => return Err(ConfigRefusal::UnknownRole(name.clone())),
        }
        if issuers.contains(&p.authority.issuer.as_str()) {
            return Err(ConfigRefusal::DuplicateIssuer(p.authority.issuer.clone()));
        }
        issuers.push(&p.authority.issuer);
        let open = p.open && !p.tenant_pinned;
        if open && !p.policy.is_restrictive() {
            if !p.allow_open {
                return Err(ConfigRefusal::OpenIssuer(name.clone()));
            }
            opted_in.push(name.clone());
        }
    }
    Ok(opted_in)
}

/// Whether the address may pre-fill a link proposal: verified, and inside a
/// domain the provider is declared authoritative for.
pub fn email_link_eligible(profile: &ProviderProfile, identity: &ExternalIdentity) -> bool {
    identity.email_trust == EmailTrust::Verified
        && identity
            .email
            .as_deref()
            .and_then(email_domain)
            .is_some_and(|d| {
                profile
                    .authoritative_domains
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(d))
            })
}

/// What happens at startup to an authority the store knows and the
/// configuration no longer names the same way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reconcile {
    Keep,
    Revoke(Authority),
    Migrate { from: Authority, to: Authority },
}

/// A declared retirement or issuer migration, never a diff: an authority
/// known to the store that the configuration neither names, retires nor
/// migrates refuses the start.
pub struct Declared<'a> {
    pub active: &'a [Authority],
    pub retired: &'a [Authority],
    /// `(old, new)` issuer migrations.
    pub migrated: &'a [(Authority, Authority)],
}

pub fn reconcile(
    known: &[Authority],
    declared: &Declared<'_>,
) -> Result<Vec<Reconcile>, ConfigRefusal> {
    let mut plan = Vec::new();
    for a in known {
        if declared.active.contains(a) {
            continue;
        }
        if declared.retired.contains(a) {
            plan.push(Reconcile::Revoke(a.clone()));
        } else if let Some((from, to)) = declared.migrated.iter().find(|(from, _)| from == a) {
            plan.push(Reconcile::Migrate {
                from: from.clone(),
                to: to.clone(),
            });
        } else {
            return Err(ConfigRefusal::UndeclaredAuthority(format!(
                "{} ({})",
                a.provider, a.issuer
            )));
        }
    }
    Ok(plan)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialKind {
    Password,
    ApiToken,
    StaticToken,
    RegistryToken,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PasswordMode {
    #[default]
    Enabled,
    AdminsOnly,
    Disabled,
}

impl PasswordMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "enabled" => Some(Self::Enabled),
            "admins_only" => Some(Self::AdminsOnly),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }
}

/// A period during which the provider could not be reached by the server's
/// own probe; `end` is `None` while it lasts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outage {
    pub start: DateTime<Utc>,
    pub end: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkState {
    pub last_login: DateTime<Utc>,
    pub outages: Vec<Outage>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoginState {
    pub disabled: bool,
    pub bootstrap: bool,
    /// The user's live links; empty for an account that never used SSO.
    pub links: Vec<LinkState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GatePolicy {
    pub password_mode: PasswordMode,
    pub reauth_after: Option<Duration>,
    pub grace_max: Duration,
}

impl Default for GatePolicy {
    fn default() -> Self {
        Self {
            password_mode: PasswordMode::Enabled,
            reauth_after: None,
            grace_max: Duration::zero(),
        }
    }
}

/// The reauthentication clock of one link: it only runs while the provider
/// was reachable, and the time it stands still is capped by `grace_max`.
pub fn reauth_expired(
    link: &LinkState,
    now: DateTime<Utc>,
    after: Duration,
    grace_max: Duration,
) -> bool {
    let elapsed = now - link.last_login;
    let mut suspended = Duration::zero();
    for o in &link.outages {
        let start = o.start.max(link.last_login);
        let end = o.end.unwrap_or(now).min(now);
        if end > start {
            suspended += end - start;
        }
    }
    elapsed - suspended.min(grace_max) > after
}

/// Whether a verified credential may still act: the one rule behind the
/// password mode, the disabled state and `reauth_after`.
pub fn login_allowed(
    role: &str,
    state: &LoginState,
    kind: CredentialKind,
    now: DateTime<Utc>,
    policy: &GatePolicy,
) -> bool {
    if state.bootstrap {
        return true;
    }
    if state.disabled {
        return false;
    }
    if kind == CredentialKind::Password {
        match policy.password_mode {
            PasswordMode::Enabled => {}
            PasswordMode::AdminsOnly if crate::permission::can_admin(role) => {}
            PasswordMode::AdminsOnly | PasswordMode::Disabled => return false,
        }
    }
    let Some(after) = policy.reauth_after else {
        return true;
    };
    if state.links.is_empty() || kind == CredentialKind::StaticToken {
        return true;
    }
    state
        .links
        .iter()
        .any(|l| !reauth_expired(l, now, after, policy.grace_max))
}

/// A same-origin path to land on after login; anything else lands on `/`.
pub fn safe_return_to(candidate: &str) -> &str {
    let ok = candidate.starts_with('/')
        && !candidate.starts_with("//")
        && !candidate.starts_with("/\\")
        && candidate.len() <= 512
        && !candidate.chars().any(|c| c.is_control() || c == '\\');
    if ok {
        candidate
    } else {
        "/"
    }
}

/// The account name a new identity is provisioned under, from what the
/// provider says it prefers, else the e-mail's local part.
pub fn provisioned_name(identity: &ExternalIdentity) -> Option<String> {
    let source = identity
        .preferred_name
        .as_deref()
        .or_else(|| identity.email.as_deref().and_then(|e| e.split('@').next()))?;
    let name: String = source
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let name = name
        .trim_matches(|c| matches!(c, '.' | '-' | '_'))
        .to_string();
    (!name.is_empty() && name.len() <= 64).then_some(name)
}

/// Every refusal an SSO flow can answer, named once for audit and clients.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SsoRefusal {
    UnknownProvider,
    StateMismatch,
    HandoffUnknown,
    HandoffConsumed,
    HandoffExpired,
    BindingMismatch,
    NameCollision,
    Rejected,
    Denied,
    Disabled,
    AdminNotLinkable,
    WrongUser,
    AlreadyLinked,
    Unavailable,
    IdpError,
}

impl SsoRefusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnknownProvider => "unknown_provider",
            Self::StateMismatch => "state_mismatch",
            Self::HandoffUnknown => "handoff_unknown",
            Self::HandoffConsumed => "handoff_consumed",
            Self::HandoffExpired => "handoff_expired",
            Self::BindingMismatch => "binding_mismatch",
            Self::NameCollision => "name_collision",
            Self::Rejected => "rejected",
            Self::Denied => "denied",
            Self::Disabled => "disabled",
            Self::AdminNotLinkable => "admin_not_linkable",
            Self::WrongUser => "wrong_user",
            Self::AlreadyLinked => "already_linked",
            Self::Unavailable => "unavailable",
            Self::IdpError => "idp_error",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(h: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap() + Duration::hours(h)
    }

    fn identity(groups: Groups, email: Option<&str>, trust: EmailTrust) -> ExternalIdentity {
        ExternalIdentity {
            key: IdentityKey {
                authority: Authority::new("corp", "https://idp.example/"),
                subject: "s1".into(),
            },
            email: email.map(str::to_string),
            email_trust: trust,
            tenant: None,
            groups,
            preferred_name: None,
        }
    }

    fn profile(open: bool, policy: LoginPolicy) -> ProviderProfile {
        ProviderProfile {
            authority: Authority::new("corp", "https://idp.example"),
            open,
            allow_open: false,
            tenant_pinned: false,
            authoritative_domains: vec!["example.com".into()],
            policy,
        }
    }

    fn reader() -> LoginPolicy {
        LoginPolicy {
            default_role: "reader".into(),
            ..LoginPolicy::default()
        }
    }

    #[test]
    fn unreadable_groups_are_rejected_and_rules_deny() {
        let mut policy = reader();
        policy.required_groups = vec!["dev".into()];
        policy.grants = vec![GroupGrant {
            group: "dev".into(),
            repository: "private".into(),
            role: GrantRole::Publisher,
        }];
        let unreadable = identity(Groups::Unreadable, None, EmailTrust::Unverified);
        assert_eq!(policy.decide(&unreadable), Err(Rejected::GroupsUnreadable));
        let outsider = identity(
            Groups::Listed(vec!["ops".into()]),
            None,
            EmailTrust::Unverified,
        );
        assert_eq!(
            policy.decide(&outsider),
            Ok(Decision::Denied(DenyReason::NotInRequiredGroup))
        );
        let member = identity(
            Groups::Listed(vec!["dev".into()]),
            None,
            EmailTrust::Unverified,
        );
        assert_eq!(
            policy.decide(&member),
            Ok(Decision::Access {
                role: "reader".into(),
                grants: vec![("private".into(), GrantRole::Publisher)]
            })
        );
    }

    #[test]
    fn domain_rule_needs_a_verified_address() {
        let mut policy = reader();
        policy.allowed_domains = vec!["example.com".into()];
        let unverified = identity(
            Groups::NotProvided,
            Some("a@example.com"),
            EmailTrust::Unverified,
        );
        assert!(matches!(
            policy.decide(&unverified),
            Ok(Decision::Denied(_))
        ));
        let verified = identity(
            Groups::NotProvided,
            Some("a@example.com"),
            EmailTrust::Verified,
        );
        assert!(matches!(
            policy.decide(&verified),
            Ok(Decision::Access { .. })
        ));
    }

    #[test]
    fn email_link_eligibility_needs_trust_and_an_authoritative_domain() {
        let p = profile(false, reader());
        let ok = identity(
            Groups::NotProvided,
            Some("a@example.com"),
            EmailTrust::Verified,
        );
        assert!(email_link_eligible(&p, &ok));
        let unverified = identity(
            Groups::NotProvided,
            Some("a@example.com"),
            EmailTrust::Unverified,
        );
        assert!(!email_link_eligible(&p, &unverified));
        let foreign = identity(
            Groups::NotProvided,
            Some("a@other.org"),
            EmailTrust::Verified,
        );
        assert!(!email_link_eligible(&p, &foreign));
    }

    #[test]
    fn profiles_refuse_duplicates_admin_and_open_issuers() {
        let dup = [profile(false, reader()), profile(false, reader())];
        assert!(matches!(
            validate_profiles(&dup),
            Err(ConfigRefusal::DuplicateIssuer(_))
        ));
        let mut admin = reader();
        admin.default_role = "admin".into();
        assert!(matches!(
            validate_profiles(&[profile(false, admin)]),
            Err(ConfigRefusal::AdminDefaultRole(_))
        ));
        assert!(matches!(
            validate_profiles(&[profile(true, reader())]),
            Err(ConfigRefusal::OpenIssuer(_))
        ));
        let mut opted = profile(true, reader());
        opted.allow_open = true;
        assert_eq!(
            validate_profiles(&[opted]).unwrap(),
            vec!["corp".to_string()]
        );
        let mut pinned = profile(true, reader());
        pinned.tenant_pinned = true;
        assert!(validate_profiles(&[pinned]).unwrap().is_empty());
    }

    #[test]
    fn issuers_differing_by_a_trailing_slash_are_one_authority() {
        assert_eq!(
            Authority::new("a", "https://x/"),
            Authority::new("a", "https://x")
        );
    }

    #[test]
    fn an_absent_authority_needs_a_declaration() {
        let a = Authority::new("old", "https://old");
        let b = Authority::new("new", "https://new");
        let none = Declared {
            active: &[],
            retired: &[],
            migrated: &[],
        };
        assert!(matches!(
            reconcile(std::slice::from_ref(&a), &none),
            Err(ConfigRefusal::UndeclaredAuthority(_))
        ));
        let retired = Declared {
            active: &[],
            retired: std::slice::from_ref(&a),
            migrated: &[],
        };
        assert_eq!(
            reconcile(std::slice::from_ref(&a), &retired).unwrap(),
            vec![Reconcile::Revoke(a.clone())]
        );
        let moved = [(a.clone(), b.clone())];
        let migrated = Declared {
            active: std::slice::from_ref(&b),
            retired: &[],
            migrated: &moved,
        };
        assert_eq!(
            reconcile(std::slice::from_ref(&a), &migrated).unwrap(),
            vec![Reconcile::Migrate { from: a, to: b }]
        );
    }

    #[test]
    fn the_reauth_clock_stands_still_during_outages_up_to_the_grace() {
        let after = Duration::hours(10);
        let grace = Duration::hours(4);
        let quiet = LinkState {
            last_login: t(0),
            outages: vec![],
        };
        assert!(!reauth_expired(&quiet, t(10), after, grace));
        assert!(reauth_expired(&quiet, t(11), after, grace));
        let down = LinkState {
            last_login: t(0),
            outages: vec![Outage {
                start: t(5),
                end: Some(t(8)),
            }],
        };
        assert!(!reauth_expired(&down, t(13), after, grace));
        assert!(reauth_expired(&down, t(14), after, grace));
        let long = LinkState {
            last_login: t(0),
            outages: vec![Outage {
                start: t(2),
                end: None,
            }],
        };
        assert!(!reauth_expired(&long, t(14), after, grace));
        assert!(
            reauth_expired(&long, t(15), after, grace),
            "capped by the grace"
        );
    }

    #[test]
    fn login_allowed_applies_mode_disabled_state_and_reauth() {
        let policy = GatePolicy {
            password_mode: PasswordMode::Disabled,
            reauth_after: Some(Duration::hours(1)),
            grace_max: Duration::zero(),
        };
        let local = LoginState::default();
        assert!(!login_allowed(
            "reader",
            &local,
            CredentialKind::Password,
            t(0),
            &policy
        ));
        assert!(login_allowed(
            "reader",
            &local,
            CredentialKind::ApiToken,
            t(0),
            &policy
        ));
        let admins_only = GatePolicy {
            password_mode: PasswordMode::AdminsOnly,
            ..policy
        };
        assert!(login_allowed(
            "admin",
            &local,
            CredentialKind::Password,
            t(0),
            &admins_only
        ));
        assert!(!login_allowed(
            "reader",
            &local,
            CredentialKind::Password,
            t(0),
            &admins_only
        ));
        let disabled = LoginState {
            disabled: true,
            ..LoginState::default()
        };
        assert!(!login_allowed(
            "reader",
            &disabled,
            CredentialKind::ApiToken,
            t(0),
            &policy
        ));
        let bootstrap = LoginState {
            disabled: true,
            bootstrap: true,
            links: vec![],
        };
        assert!(login_allowed(
            "admin",
            &bootstrap,
            CredentialKind::Password,
            t(0),
            &policy
        ));
        let linked = LoginState {
            links: vec![LinkState {
                last_login: t(0),
                outages: vec![],
            }],
            ..LoginState::default()
        };
        assert!(login_allowed(
            "reader",
            &linked,
            CredentialKind::ApiToken,
            t(1),
            &policy
        ));
        assert!(!login_allowed(
            "reader",
            &linked,
            CredentialKind::ApiToken,
            t(2),
            &policy
        ));
        assert!(!login_allowed(
            "reader",
            &linked,
            CredentialKind::RegistryToken,
            t(2),
            &policy
        ));
    }

    #[test]
    fn return_to_stays_on_this_origin() {
        assert_eq!(safe_return_to("/packages/x?y=1"), "/packages/x?y=1");
        for bad in ["//evil.example", "https://evil.example", "/\\evil", "", "x"] {
            assert_eq!(safe_return_to(bad), "/", "{bad}");
        }
    }

    #[test]
    fn provisioned_names_are_sanitized() {
        let mut id = identity(
            Groups::NotProvided,
            Some("Jane.Doe+x@example.com"),
            EmailTrust::Verified,
        );
        assert_eq!(provisioned_name(&id).as_deref(), Some("jane.doe-x"));
        id.preferred_name = Some("J D".into());
        assert_eq!(provisioned_name(&id).as_deref(), Some("j-d"));
        id.preferred_name = Some("---".into());
        assert_eq!(provisioned_name(&id), None);
    }
}

//! Governance of what a repository distributes: allow rules over names,
//! approvals bound to a fingerprinted surface, and the gate that turns both
//! into what is served. Nothing here knows a wire format or a hash function:
//! a surface is a pair of digests the caller computed.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::DomainError;

/// What a repository's rules and approvals do to what it serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateMode {
    /// Serve everything; nothing is flagged.
    Off,
    /// Serve everything, flagging what an approval or a rule would refuse.
    #[default]
    Warn,
    /// Remove what an approval or a rule refuses.
    Hide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    Allow,
    Deny,
}

impl Effect {
    pub const fn as_str(self) -> &'static str {
        match self {
            Effect::Allow => "allow",
            Effect::Deny => "deny",
        }
    }
}

impl FromStr for Effect {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, DomainError> {
        match s {
            "allow" => Ok(Effect::Allow),
            "deny" => Ok(Effect::Deny),
            other => Err(DomainError::InvalidName(format!("invalid rule effect: {other}"))),
        }
    }
}

/// An exact name, or a prefix ending in `*` right after a `/` or a `.`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowRule {
    pub pattern: String,
    pub effect: Effect,
}

impl AllowRule {
    pub fn new(pattern: &str, effect: Effect) -> Result<Self, DomainError> {
        validate_pattern(pattern)?;
        Ok(Self {
            pattern: pattern.to_string(),
            effect,
        })
    }

    /// The prefix a `*` rule matches, `None` for an exact rule.
    pub fn prefix(&self) -> Option<&str> {
        self.pattern.strip_suffix('*')
    }

    pub fn matches(&self, name: &str) -> bool {
        match self.prefix() {
            Some(prefix) => name.starts_with(prefix),
            None => name == self.pattern,
        }
    }
}

/// No general globbing: one trailing `*`, after a boundary, or none.
pub fn validate_pattern(pattern: &str) -> Result<(), DomainError> {
    let refuse = || DomainError::InvalidName(format!("invalid allow rule pattern: '{pattern}'"));
    if pattern.is_empty() || pattern.len() > 256 || pattern.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(refuse());
    }
    match pattern.strip_suffix('*') {
        Some(prefix) if prefix.contains('*') => Err(refuse()),
        Some(prefix) if !(prefix.ends_with('/') || prefix.ends_with('.')) => Err(refuse()),
        Some(_) => Ok(()),
        None if pattern.contains('*') => Err(refuse()),
        None => Ok(()),
    }
}

/// The rule deciding `name`: the longest matching pattern, `deny` winning a
/// tie. `None` when no rule matches.
pub fn allowed(rules: &[AllowRule], name: &str) -> Option<bool> {
    rules
        .iter()
        .filter(|rule| rule.matches(name))
        .max_by_key(|rule| (rule.pattern.len(), rule.effect == Effect::Deny))
        .map(|rule| rule.effect == Effect::Allow)
}

/// Whether a ruleset lets `name` through. With no match, a set holding any
/// `allow` rule is closed and a deny-only set is open, so an empty set is
/// never an accidental lockout.
pub fn admits(rules: &[AllowRule], name: &str) -> bool {
    allowed(rules, name).unwrap_or_else(|| !rules.iter().any(|r| r.effect == Effect::Allow))
}

/// Which half of an approved surface moved, worst last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Drift {
    #[default]
    None,
    Permissions,
    Tools,
    Both,
    /// An endpoint no approval of the version covers.
    NewEndpoint,
}

impl Drift {
    pub const fn as_str(self) -> &'static str {
        match self {
            Drift::None => "none",
            Drift::Permissions => "permissions",
            Drift::Tools => "tools",
            Drift::Both => "both",
            Drift::NewEndpoint => "new_endpoint",
        }
    }
}

impl FromStr for Drift {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, DomainError> {
        [Drift::None, Drift::Permissions, Drift::Tools, Drift::Both, Drift::NewEndpoint]
            .into_iter()
            .find(|d| d.as_str() == s)
            .ok_or_else(|| DomainError::InvalidName(format!("invalid drift: {s}")))
    }
}

/// The two digests a reviewer approves: the declared permissions, and the
/// tools when someone observed them.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Fingerprint {
    pub permissions: String,
    pub tools: Option<String>,
}

/// Each half compared on its own. Tools observed for the first time since
/// the approval is a drift; tools no longer observed is not, so losing a
/// probe never invalidates a review.
pub fn drift(approved: &Fingerprint, current: &Fingerprint) -> Drift {
    let permissions = approved.permissions != current.permissions;
    let tools = match (&approved.tools, &current.tools) {
        (Some(a), Some(c)) => a != c,
        (None, Some(_)) => true,
        (_, None) => false,
    };
    match (permissions, tools) {
        (false, false) => Drift::None,
        (true, false) => Drift::Permissions,
        (false, true) => Drift::Tools,
        (true, true) => Drift::Both,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Approved,
    Blocked,
}

impl Decision {
    pub const fn as_str(self) -> &'static str {
        match self {
            Decision::Approved => "approved",
            Decision::Blocked => "blocked",
        }
    }
}

impl FromStr for Decision {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, DomainError> {
        match s {
            "approved" => Ok(Decision::Approved),
            "blocked" => Ok(Decision::Blocked),
            other => Err(DomainError::InvalidName(format!("invalid decision: {other}"))),
        }
    }
}

/// One endpoint of a version as a verdict sees it: what it currently
/// answers, and what a reviewer decided about it, if anyone did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub url: String,
    pub current: Fingerprint,
    pub approval: Option<(Decision, Fingerprint)>,
}

/// The version's verdict for one repository, over every endpoint the record
/// currently declares.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct EndpointVerdict {
    pub surface_endpoints: i64,
    pub approved_endpoints: i64,
    pub worst_drift: Drift,
    pub drifted_remote: Option<String>,
}

impl EndpointVerdict {
    /// Every endpoint approved and none moved since.
    pub fn approved(&self) -> bool {
        self.surface_endpoints > 0
            && self.approved_endpoints == self.surface_endpoints
            && self.worst_drift == Drift::None
    }
}

/// An endpoint counts as approved when its approval still matches what it
/// answers. An endpoint with no approval, on a version with approvals
/// elsewhere, is a new endpoint rather than an unexplained absence.
pub fn endpoint_verdict(endpoints: &[Endpoint]) -> EndpointVerdict {
    let reviewed = endpoints.iter().any(|e| e.approval.is_some());
    let mut verdict = EndpointVerdict {
        surface_endpoints: endpoints.len() as i64,
        ..EndpointVerdict::default()
    };
    for endpoint in endpoints {
        let moved = match &endpoint.approval {
            Some((decision, approved)) => {
                let moved = drift(approved, &endpoint.current);
                if *decision == Decision::Approved && moved == Drift::None {
                    verdict.approved_endpoints += 1;
                }
                moved
            }
            None if reviewed => Drift::NewEndpoint,
            None => Drift::None,
        };
        if moved > verdict.worst_drift {
            verdict.worst_drift = moved;
            verdict.drifted_remote = Some(endpoint.url.clone());
        }
    }
    verdict
}

/// What a gate does to one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Visibility {
    Serve,
    ServeFlagged(String),
    Hide(String),
}

impl Visibility {
    fn rank(&self) -> u8 {
        match self {
            Visibility::Serve => 0,
            Visibility::ServeFlagged(_) => 1,
            Visibility::Hide(_) => 2,
        }
    }

    /// The stricter of two gates; the first one's reason on a tie.
    pub fn worst(self, other: Visibility) -> Visibility {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }

    pub fn served(&self) -> bool {
        !matches!(self, Visibility::Hide(_))
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Visibility::Serve => None,
            Visibility::ServeFlagged(why) | Visibility::Hide(why) => Some(why),
        }
    }
}

/// `why` removed under `Hide`, flagged under `Warn`, ignored under `Off`.
pub fn by_mode(mode: GateMode, why: String) -> Visibility {
    match mode {
        GateMode::Off => Visibility::Serve,
        GateMode::Warn => Visibility::ServeFlagged(why),
        GateMode::Hide => Visibility::Hide(why),
    }
}

/// Everything the gate knows about one server version in one repository.
#[derive(Debug, Clone)]
pub struct Candidate<'a> {
    pub name: &'a str,
    pub deleted: bool,
    pub verdict: &'a EndpointVerdict,
    pub blocked: bool,
}

/// One repository's gate over a server version: deletion first, then its
/// allow rules, then the approval of every endpoint.
pub fn decide(
    mode: GateMode,
    rules: &[AllowRule],
    candidate: &Candidate<'_>,
    include_deleted: bool,
) -> Visibility {
    if candidate.deleted {
        if !include_deleted {
            return Visibility::Hide("deleted upstream".to_string());
        }
        return Visibility::ServeFlagged("deleted upstream".to_string());
    }
    let mut out = Visibility::Serve;
    if !admits(rules, candidate.name) {
        out = out.worst(by_mode(mode, "matches no allow rule".to_string()));
    }
    let verdict = candidate.verdict;
    if candidate.blocked {
        out = out.worst(by_mode(mode, "blocked by an admin".to_string()));
    } else if verdict.worst_drift != Drift::None {
        let remote = verdict.drifted_remote.as_deref().filter(|r| !r.is_empty());
        let why = match (verdict.worst_drift, remote) {
            (Drift::NewEndpoint, Some(r)) => format!("a new endpoint {r} appeared since approval"),
            (d, Some(r)) => format!("{} changed at {r} since approval", drift_subject(d)),
            (d, None) => format!("{} changed since approval", drift_subject(d)),
        };
        out = out.worst(by_mode(mode, why));
    } else if !verdict.approved() {
        out = out.worst(by_mode(mode, "not approved".to_string()));
    }
    out
}

fn drift_subject(drift: Drift) -> &'static str {
    match drift {
        Drift::Permissions => "declared permissions",
        Drift::Tools => "tool descriptions",
        _ => "permissions and tool descriptions",
    }
}

/// A gate over something with no endpoints: allow rules, then one approval.
pub fn decide_single(mode: GateMode, rules: &[AllowRule], name: &str, decision: Option<Decision>) -> Visibility {
    let mut out = Visibility::Serve;
    if !admits(rules, name) {
        out = out.worst(by_mode(mode, "matches no allow rule".to_string()));
    }
    match decision {
        Some(Decision::Approved) => out,
        Some(Decision::Blocked) => out.worst(by_mode(mode, "blocked by an admin".to_string())),
        None => out.worst(by_mode(mode, "not approved".to_string())),
    }
}

/// The allow rules of every `Hide` repository from which a leaf is
/// reachable, each keeping its own name so a hidden row says who hid it. A
/// floor only ever narrows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Floor {
    pub fragments: Vec<(String, Vec<AllowRule>)>,
}

impl Floor {
    pub fn visibility(&self, name: &str) -> Visibility {
        for (repo, rules) in &self.fragments {
            if !admits(rules, name) {
                return Visibility::Hide(format!("matches no allow rule of {repo}"));
            }
        }
        Visibility::Serve
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(spec: &[(&str, Effect)]) -> Vec<AllowRule> {
        spec.iter().map(|(p, e)| AllowRule::new(p, *e).unwrap()).collect()
    }

    fn fp(permissions: &str, tools: Option<&str>) -> Fingerprint {
        Fingerprint {
            permissions: permissions.to_string(),
            tools: tools.map(str::to_string),
        }
    }

    #[test]
    fn empty_allowlist_is_open_and_the_first_allow_rule_closes_it() {
        assert!(admits(&[], "io.github.acme/x"));
        let deny_only = rules(&[("io.github.evil/*", Effect::Deny)]);
        assert!(admits(&deny_only, "io.github.acme/x"));
        assert!(!admits(&deny_only, "io.github.evil/x"));
        let closed = rules(&[("io.github.acme/*", Effect::Allow)]);
        assert!(admits(&closed, "io.github.acme/x"));
        assert!(!admits(&closed, "com.other/x"));
    }

    #[test]
    fn longest_pattern_wins_and_deny_beats_allow_at_equal_length() {
        let set = rules(&[
            ("io.github.acme/*", Effect::Allow),
            ("io.github.acme/bad", Effect::Deny),
            ("io.github.*", Effect::Deny),
        ]);
        assert_eq!(allowed(&set, "io.github.acme/good"), Some(true));
        assert_eq!(allowed(&set, "io.github.acme/bad"), Some(false));
        assert_eq!(allowed(&set, "io.github.other/x"), Some(false));
        let tie = rules(&[("io.github.acme/x", Effect::Allow), ("io.github.acme/*", Effect::Allow)]);
        assert_eq!(allowed(&tie, "io.github.acme/x"), Some(true));
        let mut equal = rules(&[("io.github.acme/*", Effect::Allow)]);
        equal.push(AllowRule {
            pattern: "io.github.acme/*".to_string(),
            effect: Effect::Deny,
        });
        assert_eq!(allowed(&equal, "io.github.acme/x"), Some(false));
    }

    #[test]
    fn drift_compares_each_half_and_tools_asymmetrically() {
        let approved = fp("p", Some("t"));
        assert_eq!(drift(&approved, &fp("p", Some("t"))), Drift::None);
        assert_eq!(drift(&approved, &fp("q", Some("t"))), Drift::Permissions);
        assert_eq!(drift(&approved, &fp("p", Some("u"))), Drift::Tools);
        assert_eq!(drift(&approved, &fp("q", Some("u"))), Drift::Both);
        assert_eq!(drift(&approved, &fp("p", None)), Drift::None, "a lost probe is not a change");
        assert_eq!(drift(&fp("p", None), &fp("p", Some("t"))), Drift::Tools, "tools seen for the first time");
    }

    #[test]
    fn a_version_is_approved_only_when_every_endpoint_is() {
        let ok = |url: &str| Endpoint {
            url: url.to_string(),
            current: fp("p", Some("t")),
            approval: Some((Decision::Approved, fp("p", Some("t")))),
        };
        let both = endpoint_verdict(&[ok("https://a"), ok("https://b")]);
        assert!(both.approved());
        let second = Endpoint {
            approval: None,
            ..ok("https://b")
        };
        let one = endpoint_verdict(&[ok("https://a"), second.clone()]);
        assert_eq!((one.surface_endpoints, one.approved_endpoints), (2, 1));
        assert_eq!(one.worst_drift, Drift::NewEndpoint);
        assert_eq!(one.drifted_remote.as_deref(), Some("https://b"));
        let unreviewed = endpoint_verdict(&[second]);
        assert_eq!(unreviewed.worst_drift, Drift::None);
        assert!(!unreviewed.approved());
        let moved = Endpoint {
            current: fp("p", Some("u")),
            ..ok("https://b")
        };
        let v = endpoint_verdict(&[ok("https://a"), moved]);
        assert_eq!((v.worst_drift, v.drifted_remote.as_deref()), (Drift::Tools, Some("https://b")));
    }

    #[test]
    fn the_gate_flags_under_warn_hides_under_hide_and_serves_under_off() {
        let pending = EndpointVerdict {
            surface_endpoints: 1,
            ..EndpointVerdict::default()
        };
        let c = Candidate {
            name: "io.github.acme/x",
            deleted: false,
            verdict: &pending,
            blocked: false,
        };
        assert_eq!(decide(GateMode::Off, &[], &c, false), Visibility::Serve);
        assert_eq!(decide(GateMode::Warn, &[], &c, false), Visibility::ServeFlagged("not approved".into()));
        assert!(!decide(GateMode::Hide, &[], &c, false).served());
        let deleted = Candidate { deleted: true, ..c.clone() };
        assert!(!decide(GateMode::Off, &[], &deleted, false).served());
        assert!(decide(GateMode::Off, &[], &deleted, true).served());
        let closed = rules(&[("com.other/*", Effect::Allow)]);
        assert_eq!(
            decide(GateMode::Warn, &closed, &c, false).reason(),
            Some("matches no allow rule")
        );
    }

    #[test]
    fn a_floor_hides_whatever_any_hide_repository_on_any_path_refuses() {
        let floor = Floor {
            fragments: vec![
                ("platform-shared".to_string(), rules(&[("io.github.acme/*", Effect::Allow)])),
                ("mirror".to_string(), Vec::new()),
            ],
        };
        assert_eq!(floor.visibility("io.github.acme/x"), Visibility::Serve);
        assert_eq!(
            floor.visibility("com.other/x"),
            Visibility::Hide("matches no allow rule of platform-shared".into())
        );
        let worst = Visibility::ServeFlagged("a".into()).worst(Visibility::Hide("b".into()));
        assert_eq!(worst, Visibility::Hide("b".into()));
    }

    #[test]
    fn a_pattern_is_exact_or_one_star_after_a_boundary() {
        for good in ["io.github.acme/x", "io.github.acme/*", "com.stripe.*", "deploy-runbook"] {
            assert!(validate_pattern(good).is_ok(), "{good}");
        }
        for bad in ["", "*", "io.github.acme*", "io.*/x", "a/**", "a b/*", "io.github/x*"] {
            assert!(validate_pattern(bad).is_err(), "{bad}");
        }
        let rule = AllowRule::new("io.github.acme/*", Effect::Allow).unwrap();
        assert!(rule.matches("io.github.acme/server"));
        assert!(!rule.matches("io.github.acmecorp/server"));
    }

    #[test]
    fn modes_and_effects_spell_themselves_lowercase() {
        assert_eq!(serde_json::to_string(&GateMode::Hide).unwrap(), "\"hide\"");
        assert_eq!(GateMode::default(), GateMode::Warn);
        assert_eq!("deny".parse::<Effect>().unwrap(), Effect::Deny);
        assert!("maybe".parse::<Effect>().is_err());
    }
}

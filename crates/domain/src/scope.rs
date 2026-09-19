//! What a credential narrows its bearer to.
//!
//! A scope never grants: the effective right is the intersection of what the
//! bearer may do at this instant and what the token allows, so a token can
//! neither outlive a revoked grant nor exceed the role behind it.

use serde::{Deserialize, Serialize};

use super::permission::{Rights, RightsSource};

/// Bounds a hot path pays on every request, and a token nobody can reason
/// about is a token nobody can audit.
pub const MAX_GRANTS: usize = 32;
const MAX_PATTERN_LEN: usize = 200;
const MAX_STARS: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScopeError {
    #[error("empty pattern")]
    EmptyPattern,
    #[error("pattern longer than {MAX_PATTERN_LEN} characters")]
    PatternTooLong,
    #[error("pattern uses more than {MAX_STARS} wildcards")]
    TooManyStars,
    #[error("pattern contains a control character")]
    ControlCharacter,
    #[error("a scope carries more than {MAX_GRANTS} lines")]
    TooManyGrants,
    #[error("a scope line names no action")]
    NoAction,
    #[error("{0} is not an action of this selector")]
    ActionNotOnSelector(&'static str),
    #[error("a scoped credential never writes the {0} domain")]
    WriteForbidden(&'static str),
    #[error("only a repository selector resolves to incarnations")]
    IncarnationsNotOnSelector,
    #[error("unreadable scope: {0}")]
    Unreadable(String),
}

/// A name pattern. `*` is the only metacharacter and it covers any substring,
/// `/` included: four of the seven formats put `/` inside a package name, so a
/// `/` that ended a wildcard would silently match nothing for them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Pattern(String);

impl Pattern {
    pub fn parse(raw: &str) -> Result<Self, ScopeError> {
        if raw.is_empty() {
            return Err(ScopeError::EmptyPattern);
        }
        if raw.chars().count() > MAX_PATTERN_LEN {
            return Err(ScopeError::PatternTooLong);
        }
        if raw.chars().any(char::is_control) {
            return Err(ScopeError::ControlCharacter);
        }
        if raw.chars().filter(|c| *c == '*').count() > MAX_STARS {
            return Err(ScopeError::TooManyStars);
        }
        Ok(Self(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A repository name, in the one explicit form both sides are folded to:
    /// nothing in the schema collates `repositories.name` for us.
    pub fn matches_repo(&self, name: &str) -> bool {
        Self(normalize_repo_name(&self.0)).matches(&normalize_repo_name(name))
    }

    /// An account name, folded the way the login limiter keys one.
    pub fn matches_account(&self, name: &str) -> bool {
        Self(normalize_account(&self.0)).matches(&normalize_account(name))
    }

    /// Linear, with one fallback mark: no pattern makes this backtrack into
    /// an exponential walk.
    pub fn matches(&self, subject: &str) -> bool {
        let pattern: Vec<char> = self.0.chars().collect();
        let subject: Vec<char> = subject.chars().collect();
        let (mut pi, mut si) = (0usize, 0usize);
        let (mut star, mut mark) = (None, 0usize);
        while si < subject.len() {
            if pi < pattern.len() && pattern[pi] == '*' {
                star = Some(pi);
                pi += 1;
                mark = si;
            } else if pi < pattern.len() && pattern[pi] == subject[si] {
                pi += 1;
                si += 1;
            } else if let Some(at) = star {
                pi = at + 1;
                mark += 1;
                si = mark;
            } else {
                return false;
            }
        }
        pattern[pi..].iter().all(|c| *c == '*')
    }
}

impl TryFrom<String> for Pattern {
    type Error = ScopeError;

    fn try_from(raw: String) -> Result<Self, ScopeError> {
        Pattern::parse(&raw)
    }
}

impl From<Pattern> for String {
    fn from(pattern: Pattern) -> String {
        pattern.0
    }
}

/// Repository names are compared in one explicit form on both sides, because
/// no collation on `repositories.name` decides it for us.
pub fn normalize_repo_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

/// Usernames are compared the way the login limiter keys them.
pub fn normalize_account(name: &str) -> String {
    name.to_lowercase()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminDomain {
    Repos,
    Users,
    Permissions,
    Tokens,
    Webhooks,
    Policy,
    Audit,
    Storage,
    Sso,
}

impl AdminDomain {
    pub fn as_str(self) -> &'static str {
        match self {
            AdminDomain::Repos => "repos",
            AdminDomain::Users => "users",
            AdminDomain::Permissions => "permissions",
            AdminDomain::Tokens => "tokens",
            AdminDomain::Webhooks => "webhooks",
            AdminDomain::Policy => "policy",
            AdminDomain::Audit => "audit",
            AdminDomain::Storage => "storage",
            AdminDomain::Sso => "sso",
        }
    }

    /// The domains a scoped credential may only read: four through which a
    /// restricted token would mint itself a new one, and `webhooks`, whose
    /// write is a standing subscription to every repository — an exfiltration
    /// channel no repository selector can narrow, because the route that
    /// creates it names no repository at all.
    pub fn read_only_under_scope(self) -> bool {
        matches!(
            self,
            AdminDomain::Users
                | AdminDomain::Tokens
                | AdminDomain::Permissions
                | AdminDomain::Sso
                | AdminDomain::Webhooks
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeAction {
    Read,
    Write,
    Delete,
    Admin,
}

impl ScopeAction {
    pub fn as_str(self) -> &'static str {
        match self {
            ScopeAction::Read => "read",
            ScopeAction::Write => "write",
            ScopeAction::Delete => "delete",
            ScopeAction::Admin => "admin",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "on", rename_all = "snake_case", deny_unknown_fields)]
pub enum Selector {
    Repo {
        repo: Pattern,
    },
    /// Two fields, never one string: `/` is an ordinary character, so a joined
    /// `<repo>/<package>` would have no readable boundary.
    Package {
        repo: Pattern,
        package: Pattern,
    },
    Admin {
        domain: AdminDomain,
    },
    Account {
        account: Pattern,
    },
}

/// What a request is being judged against. `Repository` and `Package` carry
/// the repository's incarnation, never only its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject<'a> {
    Repository {
        incarnation: &'a str,
    },
    /// `package` is the name its format already normalized.
    Package {
        incarnation: &'a str,
        package: &'a str,
    },
    Admin {
        domain: AdminDomain,
    },
    Account {
        username: &'a str,
    },
}

/// One line of a scope: what it selects, what it allows there, and — for the
/// two repository families — the incarnations the pattern resolved to when
/// the token was issued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    #[serde(flatten)]
    pub selector: Selector,
    pub actions: Vec<ScopeAction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incarnations: Vec<String>,
}

impl Grant {
    pub fn rights(&self) -> Rights {
        let mut rights = Rights::NONE;
        for action in &self.actions {
            match action {
                ScopeAction::Read => rights.read = true,
                ScopeAction::Write => rights.write = true,
                ScopeAction::Delete => rights.delete = true,
                ScopeAction::Admin => rights.admin = true,
            }
        }
        rights
    }

    /// Closed in both directions: a line the current vocabulary cannot hold
    /// is refused at creation rather than stored and reinterpreted later.
    pub fn validate(&self) -> Result<(), ScopeError> {
        if self.actions.is_empty() {
            return Err(ScopeError::NoAction);
        }
        match &self.selector {
            Selector::Repo { .. } | Selector::Package { .. } => Ok(()),
            Selector::Admin { domain } => {
                if !self.incarnations.is_empty() {
                    return Err(ScopeError::IncarnationsNotOnSelector);
                }
                for action in &self.actions {
                    match action {
                        ScopeAction::Read => {}
                        ScopeAction::Write if !domain.read_only_under_scope() => {}
                        ScopeAction::Write => {
                            return Err(ScopeError::WriteForbidden(domain.as_str()))
                        }
                        other => return Err(ScopeError::ActionNotOnSelector(other.as_str())),
                    }
                }
                Ok(())
            }
            Selector::Account { .. } => {
                if !self.incarnations.is_empty() {
                    return Err(ScopeError::IncarnationsNotOnSelector);
                }
                match self.actions.as_slice() {
                    [ScopeAction::Read] => Ok(()),
                    _ => Err(ScopeError::ActionNotOnSelector("write")),
                }
            }
        }
    }

    fn covers(&self, subject: &Subject<'_>) -> bool {
        match (&self.selector, subject) {
            (Selector::Repo { .. }, Subject::Repository { incarnation })
            | (Selector::Repo { .. }, Subject::Package { incarnation, .. }) => {
                self.holds(incarnation)
            }
            (
                Selector::Package { package, .. },
                Subject::Package {
                    incarnation,
                    package: name,
                },
            ) => self.holds(incarnation) && package.matches(name),
            (Selector::Admin { domain }, Subject::Admin { domain: asked }) => domain == asked,
            (Selector::Account { account }, Subject::Account { username }) => {
                account.matches_account(username)
            }
            _ => false,
        }
    }

    /// A name retired and recreated is a new incarnation, which no scope
    /// issued before it can name.
    fn holds(&self, incarnation: &str) -> bool {
        self.incarnations.iter().any(|known| known == incarnation)
    }
}

/// `Inherit` is every right its bearer holds — what every token was before
/// scopes existed, and what a login session still is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TokenScope {
    Inherit,
    Limited { grants: Vec<Grant> },
}

impl TokenScope {
    pub fn is_inherit(&self) -> bool {
        matches!(self, TokenScope::Inherit)
    }

    pub fn validate(&self) -> Result<(), ScopeError> {
        let TokenScope::Limited { grants } = self else {
            return Ok(());
        };
        if grants.len() > MAX_GRANTS {
            return Err(ScopeError::TooManyGrants);
        }
        grants.iter().try_for_each(Grant::validate)
    }

    /// An unreadable scope refuses the credential; it is never degraded into
    /// `Inherit`, which would turn a corrupt row into a full-powered token.
    pub fn parse(raw: &str) -> Result<Self, ScopeError> {
        let scope: TokenScope =
            serde_json::from_str(raw).map_err(|e| ScopeError::Unreadable(e.to_string()))?;
        scope.validate()?;
        Ok(scope)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("a scope is always serializable")
    }

    pub fn grants(&self) -> &[Grant] {
        match self {
            TokenScope::Inherit => &[],
            TokenScope::Limited { grants } => grants,
        }
    }

    /// The scope as it is frozen at issue: every repository pattern replaced
    /// by the incarnations it named at that instant. A pattern never reaches
    /// a repository created afterwards, and a name retired and recreated is
    /// another incarnation, which no scope issued before it can hold.
    pub fn resolved(&self, repositories: &[Incarnation<'_>]) -> TokenScope {
        let TokenScope::Limited { grants } = self else {
            return TokenScope::Inherit;
        };
        let resolved = grants
            .iter()
            .map(|grant| {
                let pattern = match &grant.selector {
                    Selector::Repo { repo } | Selector::Package { repo, .. } => repo,
                    Selector::Admin { .. } | Selector::Account { .. } => return grant.clone(),
                };
                Grant {
                    incarnations: repositories
                        .iter()
                        .filter(|r| pattern.matches_repo(r.name))
                        .map(|r| r.incarnation.to_string())
                        .collect(),
                    ..grant.clone()
                }
            })
            .collect();
        TokenScope::Limited { grants: resolved }
    }
}

/// A repository as the resolution sees it: the name a pattern is compared to
/// and the incarnation the scope will actually hold.
#[derive(Debug, Clone, Copy)]
pub struct Incarnation<'a> {
    pub name: &'a str,
    pub incarnation: &'a str,
}

/// The effective right: what the bearer holds, intersected with what the
/// credential allows on this subject. Nothing here can turn a `false` into a
/// `true`, which is the whole of the feature's safety argument.
pub fn narrow(held: Rights, scope: &TokenScope, subject: &Subject<'_>) -> Rights {
    let TokenScope::Limited { grants } = scope else {
        return held;
    };
    let mut allowed = Rights::NONE;
    for grant in grants.iter().filter(|g| g.covers(subject)) {
        let rights = grant.rights();
        allowed.read |= rights.read;
        allowed.write |= rights.write;
        allowed.delete |= rights.delete;
        allowed.admin |= rights.admin;
    }
    Rights {
        read: held.read && allowed.read,
        write: held.write && allowed.write,
        delete: held.delete && allowed.delete,
        admin: held.admin && allowed.admin,
    }
}

/// Which rung answered once the scope has had its say: the scope, when it is
/// what removed something the bearer otherwise held.
pub fn narrowed_source(held: Rights, effective: Rights, source: RightsSource) -> RightsSource {
    if effective == held {
        source
    } else {
        RightsSource::Scope
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pat(raw: &str) -> Pattern {
        Pattern::parse(raw).unwrap()
    }

    fn repo_grant(pattern: &str, incarnations: &[&str], actions: &[ScopeAction]) -> Grant {
        Grant {
            selector: Selector::Repo { repo: pat(pattern) },
            actions: actions.to_vec(),
            incarnations: incarnations.iter().map(|i| i.to_string()).collect(),
        }
    }

    fn limited(grants: Vec<Grant>) -> TokenScope {
        TokenScope::Limited { grants }
    }

    /// `/` is an ordinary character: the four formats whose names carry one
    /// would otherwise never match a wildcard written the obvious way.
    #[test]
    fn a_wildcard_covers_a_slash_like_any_other_character() {
        assert!(pat("libs/*").matches("libs/deep/nested"));
        assert!(pat("*").matches("github.com/org/mod"));
        assert!(pat("@acme/*").matches("@acme/httpclient"));
        assert!(pat("com.acme:*").matches("com.acme:widgets"));
        assert!(pat("team/*").matches("team/app"));
        assert!(!pat("libs/*").matches("other/thing"));
    }

    #[test]
    fn a_pattern_without_a_wildcard_is_an_exact_name() {
        assert!(pat("builds").matches("builds"));
        assert!(!pat("builds").matches("builds-2"));
        assert!(!pat("builds").matches("build"));
    }

    #[test]
    fn several_wildcards_match_without_backtracking_into_a_wrong_answer() {
        assert!(pat("a*b*c").matches("axxbyyc"));
        assert!(!pat("a*b*c").matches("axxbyy"));
        assert!(pat("*x*").matches("x"));
        assert!(pat("**").matches("anything"));
    }

    #[test]
    fn the_vocabulary_refuses_what_it_cannot_hold() {
        assert_eq!(Pattern::parse(""), Err(ScopeError::EmptyPattern));
        assert_eq!(Pattern::parse("a\nb"), Err(ScopeError::ControlCharacter));
        assert_eq!(Pattern::parse("*****"), Err(ScopeError::TooManyStars));
        assert_eq!(
            Pattern::parse(&"a".repeat(MAX_PATTERN_LEN + 1)),
            Err(ScopeError::PatternTooLong)
        );
        let line = |body: &str| format!(r#"{{"kind":"limited","grants":[{body}]}}"#);
        assert!(TokenScope::parse(r#"{"kind":"other"}"#).is_err());
        assert!(
            TokenScope::parse(&line(r#"{"on":"repo","repo":"a","actions":["frobnicate"]}"#))
                .is_err()
        );
        assert!(TokenScope::parse(&line(r#"{"on":"lunar","actions":["read"]}"#)).is_err());
        assert!(
            TokenScope::parse(&line(
                r#"{"on":"repo","repo":"a","actions":["read"],"but":"also"}"#
            ))
            .is_err(),
            "a field the vocabulary does not know is refused, never ignored"
        );
    }

    #[test]
    fn a_repository_name_is_compared_in_one_explicit_form() {
        assert_eq!(normalize_repo_name("NPM-Prod"), "npm-prod");
        assert_eq!(normalize_account("Alice"), "alice");
    }

    #[test]
    fn a_scope_never_adds_a_right_its_bearer_does_not_hold() {
        let scope = limited(vec![repo_grant(
            "builds",
            &["inc-1"],
            &[ScopeAction::Read, ScopeAction::Write],
        )]);
        let subject = Subject::Repository { incarnation: "inc-1" };
        let reader = Rights { read: true, ..Rights::NONE };
        assert_eq!(narrow(reader, &scope, &subject), reader);
        assert_eq!(
            narrow(Rights::NONE, &scope, &subject),
            Rights::NONE,
            "a scope on a bearer with nothing stays nothing"
        );
    }

    /// The admin short circuit runs before the scope; if the scope did not
    /// apply after it, an admin's scoped token would not be scoped at all.
    #[test]
    fn an_admin_is_narrowed_like_anyone_else() {
        let scope = limited(vec![repo_grant("builds", &["inc-1"], &[ScopeAction::Read])]);
        let narrowed = narrow(
            Rights::FULL,
            &scope,
            &Subject::Repository { incarnation: "inc-1" },
        );
        assert_eq!(narrowed, Rights { read: true, ..Rights::NONE });
        assert_eq!(
            narrow(Rights::FULL, &scope, &Subject::Repository { incarnation: "inc-2" }),
            Rights::NONE
        );
    }

    #[test]
    fn inherit_reproduces_the_rights_of_the_bearer_exactly() {
        for held in [Rights::NONE, Rights::FULL, Rights { read: true, ..Rights::NONE }] {
            for subject in [
                Subject::Repository { incarnation: "inc-1" },
                Subject::Admin { domain: AdminDomain::Repos },
                Subject::Account { username: "alice" },
            ] {
                assert_eq!(narrow(held, &TokenScope::Inherit, &subject), held);
            }
        }
    }

    #[test]
    fn an_empty_scope_refuses_everything() {
        let scope = limited(Vec::new());
        assert_eq!(
            narrow(Rights::FULL, &scope, &Subject::Repository { incarnation: "inc-1" }),
            Rights::NONE
        );
        assert_eq!(
            narrow(Rights::FULL, &scope, &Subject::Admin { domain: AdminDomain::Audit }),
            Rights::NONE
        );
    }

    /// A pattern is resolved to incarnations when the token is issued, so a
    /// name retired and recreated is out of every scope issued before it.
    #[test]
    fn a_recreated_name_is_a_new_incarnation_no_earlier_scope_names() {
        let scope = limited(vec![repo_grant("builds", &["inc-old"], &[ScopeAction::Read])]);
        let at = |incarnation| Subject::Repository { incarnation };
        assert!(narrow(Rights::FULL, &scope, &at("inc-old")).read);
        assert!(!narrow(Rights::FULL, &scope, &at("inc-new")).read);
    }

    #[test]
    fn a_repository_line_covers_the_packages_of_that_repository() {
        let scope = limited(vec![repo_grant("builds", &["inc-1"], &[ScopeAction::Read])]);
        let subject = Subject::Package {
            incarnation: "inc-1",
            package: "@acme/httpclient",
        };
        assert!(narrow(Rights::FULL, &scope, &subject).read);
    }

    /// A package line is not a repository line: an admin route that names a
    /// repository is not satisfied by a scope that only names packages.
    #[test]
    fn a_package_line_does_not_answer_for_its_repository() {
        let scope = limited(vec![Grant {
            selector: Selector::Package {
                repo: pat("builds"),
                package: pat("@acme/*"),
            },
            actions: vec![ScopeAction::Read, ScopeAction::Write],
            incarnations: vec!["inc-1".to_string()],
        }]);
        assert!(!narrow(
            Rights::FULL,
            &scope,
            &Subject::Repository { incarnation: "inc-1" }
        )
        .read);
        assert!(narrow(
            Rights::FULL,
            &scope,
            &Subject::Package { incarnation: "inc-1", package: "@acme/httpclient" }
        )
        .read);
        assert!(!narrow(
            Rights::FULL,
            &scope,
            &Subject::Package { incarnation: "inc-1", package: "@other/thing" }
        )
        .read);
    }

    #[test]
    fn an_account_line_matches_its_own_account_only_and_only_to_read() {
        let grant = Grant {
            selector: Selector::Account { account: pat("alice") },
            actions: vec![ScopeAction::Read],
            incarnations: Vec::new(),
        };
        assert_eq!(grant.validate(), Ok(()));
        let scope = limited(vec![grant]);
        assert!(narrow(Rights::FULL, &scope, &Subject::Account { username: "Alice" }).read);
        assert!(!narrow(Rights::FULL, &scope, &Subject::Account { username: "bob" }).read);

        let writer = Grant {
            selector: Selector::Account { account: pat("alice") },
            actions: vec![ScopeAction::Read, ScopeAction::Write],
            incarnations: Vec::new(),
        };
        assert!(writer.validate().is_err(), "an account line never writes");
    }

    /// The domains through which a restricted token would mint itself a new
    /// credential, or subscribe to what it may not read, exist in read only.
    #[test]
    fn the_escalation_domains_exist_in_read_only() {
        for domain in [
            AdminDomain::Users,
            AdminDomain::Tokens,
            AdminDomain::Permissions,
            AdminDomain::Sso,
            AdminDomain::Webhooks,
        ] {
            let grant = Grant {
                selector: Selector::Admin { domain },
                actions: vec![ScopeAction::Write],
                incarnations: Vec::new(),
            };
            assert_eq!(grant.validate(), Err(ScopeError::WriteForbidden(domain.as_str())));
        }
        for domain in [
            AdminDomain::Repos,
            AdminDomain::Policy,
            AdminDomain::Storage,
            AdminDomain::Audit,
        ] {
            let grant = Grant {
                selector: Selector::Admin { domain },
                actions: vec![ScopeAction::Write],
                incarnations: Vec::new(),
            };
            assert_eq!(grant.validate(), Ok(()), "{}", domain.as_str());
        }
    }

    #[test]
    fn an_admin_line_never_carries_a_repository_action_or_an_incarnation() {
        let grant = Grant {
            selector: Selector::Admin { domain: AdminDomain::Repos },
            actions: vec![ScopeAction::Delete],
            incarnations: Vec::new(),
        };
        assert_eq!(grant.validate(), Err(ScopeError::ActionNotOnSelector("delete")));
        let grant = Grant {
            selector: Selector::Admin { domain: AdminDomain::Repos },
            actions: vec![ScopeAction::Read],
            incarnations: vec!["inc-1".to_string()],
        };
        assert_eq!(grant.validate(), Err(ScopeError::IncarnationsNotOnSelector));
    }

    #[test]
    fn a_scope_is_bounded_in_lines_and_a_line_names_an_action() {
        let many = (0..MAX_GRANTS + 1)
            .map(|_| repo_grant("a", &["inc-1"], &[ScopeAction::Read]))
            .collect();
        assert_eq!(limited(many).validate(), Err(ScopeError::TooManyGrants));
        assert_eq!(repo_grant("a", &["inc-1"], &[]).validate(), Err(ScopeError::NoAction));
    }

    #[test]
    fn a_scope_survives_a_round_trip_through_its_stored_form() {
        let scope = limited(vec![
            repo_grant("builds", &["inc-1"], &[ScopeAction::Read, ScopeAction::Write]),
            Grant {
                selector: Selector::Package {
                    repo: pat("libs-*"),
                    package: pat("@acme/*"),
                },
                actions: vec![ScopeAction::Write, ScopeAction::Delete],
                incarnations: vec!["inc-2".to_string()],
            },
            Grant {
                selector: Selector::Admin { domain: AdminDomain::Audit },
                actions: vec![ScopeAction::Read],
                incarnations: Vec::new(),
            },
        ]);
        assert_eq!(TokenScope::parse(&scope.to_json()), Ok(scope));
        assert_eq!(
            TokenScope::parse(&TokenScope::Inherit.to_json()),
            Ok(TokenScope::Inherit)
        );
    }

    #[test]
    fn resolution_freezes_the_incarnations_a_pattern_named_at_issue() {
        let scope = limited(vec![
            repo_grant("libs-*", &[], &[ScopeAction::Read]),
            Grant {
                selector: Selector::Package {
                    repo: Pattern::parse("NPM").unwrap(),
                    package: pat("@acme/*"),
                },
                actions: vec![ScopeAction::Write],
                incarnations: Vec::new(),
            },
            Grant {
                selector: Selector::Admin { domain: AdminDomain::Audit },
                actions: vec![ScopeAction::Read],
                incarnations: Vec::new(),
            },
        ]);
        let world = [
            Incarnation { name: "libs-a", incarnation: "inc-a" },
            Incarnation { name: "Libs-B", incarnation: "inc-b" },
            Incarnation { name: "prod", incarnation: "inc-p" },
            Incarnation { name: "npm", incarnation: "inc-n" },
        ];

        let resolved = scope.resolved(&world);
        let grants = resolved.grants();
        assert_eq!(grants[0].incarnations, ["inc-a", "inc-b"], "case folds on both sides");
        assert_eq!(grants[1].incarnations, ["inc-n"]);
        assert!(grants[2].incarnations.is_empty(), "an admin line names no repository");

        // The repository created after the issue is outside every scope
        // already written, and so is a name retired and recreated.
        let later = [
            Incarnation { name: "libs-a", incarnation: "inc-a2" },
            Incarnation { name: "libs-c", incarnation: "inc-c" },
        ];
        for repo in later {
            assert!(
                !grants[0].covers(&Subject::Repository { incarnation: repo.incarnation }),
                "{}",
                repo.incarnation
            );
        }
    }

    #[test]
    fn a_pattern_that_names_nothing_resolves_to_an_inert_line() {
        let scope = limited(vec![repo_grant("gone-*", &[], &[ScopeAction::Read])]);
        let resolved = scope.resolved(&[Incarnation { name: "libs", incarnation: "inc" }]);
        assert!(resolved.grants()[0].incarnations.is_empty());
        assert_eq!(
            narrow(Rights::FULL, &resolved, &Subject::Repository { incarnation: "inc" }),
            Rights::NONE
        );
    }

    #[test]
    fn the_source_names_the_scope_when_the_scope_is_what_removed_something() {
        let held = Rights::FULL;
        assert_eq!(
            narrowed_source(held, held, RightsSource::Admin),
            RightsSource::Admin
        );
        assert_eq!(
            narrowed_source(held, Rights { read: true, ..Rights::NONE }, RightsSource::Admin),
            RightsSource::Scope
        );
    }
}

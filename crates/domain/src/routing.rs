//! Group routing rules: which members of a group are allowed to answer for a
//! name. The defence against dependency confusion, decided here and nowhere
//! else.
//!
//! A rule only ever *removes* members from a walk. There is no effect that
//! routes a name somewhere the walk would not have gone, which is what makes
//! adding a rule safe and reviewing one local.
//!
//! Two keys, never one. `patterns` compare on the format's `match_key`, a
//! coarsening of store identity: coarsening refuses more, which is
//! conservative. `except` compares on `ident_key`, store identity itself:
//! coarsening there would reopen more, which is not.

use std::fmt;

use super::{DomainError, Format, RepoKind};

/// The metacharacter a pattern is cut on, and the placeholder that stands in
/// for it while the format canonicalizes the rest. A name can never contain
/// the placeholder: every format's read validator refuses a control byte.
const STAR: char = '*';
const PLACEHOLDER: char = '\u{0}';

/// What a pattern may cost to write and to evaluate, bounded without
/// analysis: no backtracking is possible, but an unbounded number of
/// segments would still be an unbounded number of substring searches.
const MAX_PATTERN_LEN: usize = 255;
const MAX_STARS: usize = 8;

/// A compiled glob over the image of a format's `match_key`.
///
/// `literals` holds the pieces the author wrote between the stars, already
/// canonicalized *in context*: `literals.len()` is the number of stars plus
/// one, so a pattern with no star is one literal and an exact comparison.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    literals: Vec<String>,
    source: String,
}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

impl Pattern {
    /// The pattern as the author wrote it, for display and for round-tripping
    /// through the store.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The canonical form, stars included: what `explain` shows beside the
    /// computed key so a coarsening is visible rather than surprising.
    pub fn canonical(&self) -> String {
        self.literals.join("*")
    }

    /// The literal a pattern starts with, before its first star: the class of
    /// keys it can possibly filter, as far as a prefix can tell.
    pub fn head(&self) -> &str {
        &self.literals[0]
    }

    /// True when the pattern filters every key of its format.
    pub fn is_catch_all(&self) -> bool {
        self.literals.len() == 2 && self.literals.iter().all(String::is_empty)
    }

    /// Anchored on the whole key; `*` spans any run of bytes, separators
    /// included. No allocation, one pass forward.
    pub fn matches(&self, key: &str) -> bool {
        let (first, tail) = self.literals.split_first().expect("a pattern has a literal");
        let Some(mut rest) = key.strip_prefix(first.as_str()) else {
            return false;
        };
        let Some((last, middles)) = tail.split_last() else {
            return rest.is_empty();
        };
        for middle in middles {
            match rest.find(middle.as_str()) {
                Some(at) => rest = &rest[at + middle.len()..],
                None => return false,
            }
        }
        rest.len() >= last.len() && rest.ends_with(last.as_str())
    }
}

fn invalid(pattern: &str, why: &str) -> DomainError {
    DomainError::InvalidName(format!("invalid routing pattern '{pattern}': {why}"))
}

/// Compile one pattern against a format's canonicalization.
///
/// The stars are replaced by a placeholder, the *whole* string is
/// canonicalized, and the pieces are cut back out. Canonicalizing each piece
/// on its own is the seam that lies: PyPI folds a run of `-_.` and Maven has
/// a `:` between two halves, so a piece taken out of context does not
/// canonicalize the way it does in place.
///
/// `canonicalize` is the format's `match_key`, and nothing else: a pattern
/// and a key that disagree on their canonicalization is exactly the
/// asymmetry a spelling escapes through.
pub fn compile_pattern(
    pattern: &str,
    canonicalize: impl Fn(&str) -> String,
) -> Result<Pattern, DomainError> {
    if pattern.is_empty() {
        return Err(invalid(pattern, "a pattern is never empty"));
    }
    if pattern.len() > MAX_PATTERN_LEN {
        return Err(invalid(
            pattern,
            &format!("longer than {MAX_PATTERN_LEN} bytes"),
        ));
    }
    if pattern.contains('\\') {
        return Err(invalid(
            pattern,
            "the language has no escape, so it cannot express a literal '*'; \
             name such a package in except[] instead",
        ));
    }
    if pattern.contains(PLACEHOLDER) {
        return Err(invalid(pattern, "a control byte is not part of any name"));
    }
    let stars = pattern.matches(STAR).count();
    if stars > MAX_STARS {
        return Err(invalid(pattern, &format!("more than {MAX_STARS} stars")));
    }
    let canonical = canonicalize(&pattern.replace(STAR, &PLACEHOLDER.to_string()));
    if canonicalize(&canonical) != canonical {
        return Err(invalid(
            pattern,
            "does not canonicalize to a stable form in this format",
        ));
    }
    if canonical.matches(PLACEHOLDER).count() != stars {
        return Err(invalid(
            pattern,
            "canonicalization moved or dropped a '*', so the pattern cannot be trusted",
        ));
    }
    let literals: Vec<String> = canonical.split(PLACEHOLDER).map(str::to_string).collect();
    if literals.iter().any(|l| l.contains(STAR)) {
        return Err(invalid(pattern, "'*' is a metacharacter, never a literal"));
    }
    Ok(Pattern {
        literals,
        source: pattern.to_string(),
    })
}

/// Which members a rule leaves able to answer. There is no "route to": every
/// effect is a restriction of a set the walk already had (I1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Every hosted member, **present and future**: a hosted repository added
    /// to the group later widens the rule without the rule changing.
    AnyHosted,
    /// Exactly these repository incarnations. An incarnation is never reused,
    /// so deleting a repository and recreating its name never re-targets the
    /// rule (I11); a target that is gone simply matches nothing, which
    /// refuses.
    Members(Vec<String>),
    /// A name no member serves.
    Deny,
}

impl Effect {
    pub fn as_str(&self) -> &'static str {
        match self {
            Effect::AnyHosted => "allow_hosted",
            Effect::Members(_) => "allow_members",
            Effect::Deny => "deny",
        }
    }
}

/// The member a decision is about, as the walk knows it: never a name.
#[derive(Clone, Copy, Debug)]
pub struct MemberRef<'a> {
    pub kind: RepoKind,
    pub incarnation: &'a str,
}

/// One rule: a named object, per format, total over its format.
///
/// There is no scope field. A rule applies to every repository of its format,
/// present and future, addressed directly or reached through any group: a
/// protection that a URL change walks around is not a protection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoutingRule {
    pub name: String,
    pub format: Format,
    pub patterns: Vec<Pattern>,
    /// Compared on `ident_key`, never globbed: an exception reopens exactly
    /// the spelling class the store itself serves as one row.
    pub except: Vec<String>,
    pub effect: Effect,
}

impl RoutingRule {
    /// Whether this rule has anything to say about this name.
    pub fn covers(&self, match_key: &str, ident_key: &str) -> bool {
        self.patterns.iter().any(|p| p.matches(match_key))
            && !self.except.iter().any(|e| e == ident_key)
    }

    /// Whether this rule has anything to say about this name, in this
    /// format, and refuses this member for it.
    fn refuses(
        &self,
        format: Format,
        match_key: &str,
        ident_key: &str,
        member: MemberRef<'_>,
    ) -> bool {
        self.format == format && self.covers(match_key, ident_key) && !self.admits(member)
    }

    /// D7: whether this rule has anything to say about an enumeration *term*.
    ///
    /// A term is not a name, it is a prefix of the names that will come back,
    /// so the comparison runs both ways: `acme.internal` under `acme.*` — the
    /// term is inside the covered class — and `acme` under `acme.*` — the
    /// term completes into it. Only the first would stop a whole internal
    /// name being handed to a public search; only the second stops it being
    /// reconstructed by completion.
    fn touches_term(&self, term_key: &str) -> bool {
        self.patterns.iter().any(|p| {
            let head = p.head();
            head.starts_with(term_key) || term_key.starts_with(head)
        })
    }

    fn admits(&self, member: MemberRef<'_>) -> bool {
        match &self.effect {
            Effect::Deny => false,
            Effect::AnyHosted => member.kind == RepoKind::Hosted,
            Effect::Members(targets) => {
                member.kind == RepoKind::Hosted
                    && targets.iter().any(|t| t == member.incarnation)
            }
        }
    }
}

/// Every rule that refuses one member for one name.
///
/// A refusal is imputable to a *set* (I8): the allowed set is an
/// intersection, so several rules can refuse the same member for the same
/// name, and deleting the one a message happened to name would not lift the
/// refusal. Held as the query that produced it rather than as a list, so the
/// admission path allocates nothing and the set is walked only when something
/// — `explain`, the audit, the UI — actually reads it.
#[derive(Clone, Copy, Debug)]
pub struct RefusalSet<'a> {
    rules: &'a [RoutingRule],
    format: Format,
    match_key: &'a str,
    ident_key: &'a str,
    member: MemberRef<'a>,
}

impl<'a> RefusalSet<'a> {
    /// Non-empty by construction, in name order.
    pub fn rules(&self) -> impl Iterator<Item = &'a RoutingRule> + '_ {
        let (format, match_key, ident_key, member) =
            (self.format, self.match_key, self.ident_key, self.member);
        self.rules
            .iter()
            .filter(move |r| r.refuses(format, match_key, ident_key, member))
    }

    pub fn names(&self) -> impl Iterator<Item = &'a str> + '_ {
        self.rules().map(|r| r.name.as_str())
    }

    /// The first name in the set, for the one line a UI has room for. A
    /// label, never an identity: `rules()` is what a decision is about.
    pub fn label(&self) -> &'a str {
        self.names().next().expect("a refusal names at least one rule")
    }
}

/// What a rule set said about one member.
#[derive(Clone, Copy, Debug)]
pub enum Decision<'a> {
    Admit,
    Refuse(RefusalSet<'a>),
}

impl<'a> Decision<'a> {
    pub fn admitted(&self) -> bool {
        matches!(self, Decision::Admit)
    }

    pub fn refused_by(&self) -> Option<RefusalSet<'a>> {
        match self {
            Decision::Admit => None,
            Decision::Refuse(set) => Some(*set),
        }
    }
}

/// The compiled snapshot the walk decides against, and the monotonic version
/// every memo keyed on a group's view must also key on (I9).
#[derive(Clone, Debug, Default)]
pub struct RouteSet {
    rules: Vec<RoutingRule>,
    version: u64,
}

impl<'a> RouteSet {
    /// Rules are held in name order: the order a refusal is imputed in is
    /// total and deterministic, so `explain` and the walk name the same rule.
    pub fn new(mut rules: Vec<RoutingRule>, version: u64) -> Self {
        rules.sort_by(|a, b| a.name.cmp(&b.name));
        Self { rules, version }
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn rules(&self) -> &[RoutingRule] {
        &self.rules
    }

    /// Whether any rule at all speaks for this format: the short-circuit that
    /// keeps a deployment with no rule paying for none of this.
    pub fn governs(&self, format: Format) -> bool {
        self.rules.iter().any(|r| r.format == format)
    }

    /// The rules covering this name, in name order — what a repository page
    /// and `explain` list.
    pub fn covering(&self, format: Format, match_key: &str, ident_key: &str) -> Vec<&RoutingRule> {
        self.rules
            .iter()
            .filter(|r| r.format == format && r.covers(match_key, ident_key))
            .collect()
    }

    /// Whether any rule of this format speaks about this enumeration term at
    /// all: the caller asks before paying for what a decision costs.
    pub fn touches_term(&self, format: Format, term_key: &str) -> bool {
        self.rules
            .iter()
            .any(|r| r.format == format && r.touches_term(term_key))
    }

    /// The rules that refuse this member for an enumeration *term* (D7),
    /// before any upstream call is made for it.
    ///
    /// `except[]` is not consulted: it names exact spellings, and no exact
    /// spelling can reopen a prefix. That is the conservative side — a term
    /// is refused a little more often than a name would be, never less.
    pub fn term_refusers(
        &'a self,
        format: Format,
        term_key: &'a str,
        member: MemberRef<'a>,
    ) -> impl Iterator<Item = &'a RoutingRule> + 'a {
        self.rules.iter().filter(move |r| {
            r.format == format && r.touches_term(term_key) && !r.admits(member)
        })
    }

    /// The whole decision: every covering rule applies, and the allowed set is
    /// their intersection, so deny wins by construction and adding a rule can
    /// never open a path. No allocation, no clock, no caller.
    pub fn decide(
        &'a self,
        format: Format,
        match_key: &'a str,
        ident_key: &'a str,
        member: MemberRef<'a>,
    ) -> Decision<'a> {
        let refused = self
            .rules
            .iter()
            .any(|r| r.refuses(format, match_key, ident_key, member));
        if refused {
            Decision::Refuse(RefusalSet {
                rules: &self.rules,
                format,
                match_key,
                ident_key,
                member,
            })
        } else {
            Decision::Admit
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lower(s: &str) -> String {
        s.to_ascii_lowercase()
    }

    fn pat(source: &str) -> Pattern {
        compile_pattern(source, lower).expect("a valid pattern")
    }

    fn hosted(incarnation: &str) -> MemberRef<'_> {
        MemberRef {
            kind: RepoKind::Hosted,
            incarnation,
        }
    }

    fn proxy(incarnation: &str) -> MemberRef<'_> {
        MemberRef {
            kind: RepoKind::Proxy,
            incarnation,
        }
    }

    /// Every rule a decision refuses by, in name order.
    fn refusers(decision: Decision<'_>) -> Vec<&str> {
        match decision.refused_by() {
            Some(set) => set.names().collect(),
            None => Vec::new(),
        }
    }

    fn rule(name: &str, patterns: &[&str], effect: Effect) -> RoutingRule {
        RoutingRule {
            name: name.to_string(),
            format: Format::Npm,
            patterns: patterns.iter().copied().map(pat).collect(),
            except: Vec::new(),
            effect,
        }
    }

    #[test]
    fn a_star_spans_separators_and_the_pattern_is_anchored() {
        assert!(pat("@acme/*").matches("@acme/foo"));
        assert!(pat("@acme/*").matches("@acme/foo/bar"), "S1: a star spans '/'");
        assert!(!pat("@acme/*").matches("@acmex/foo"));
        assert!(!pat("@acme/*").matches("x@acme/foo"), "anchored at the start");
        assert!(!pat("acme").matches("acme-tools"), "anchored at the end");
        assert!(pat("*-internal").matches("acme-internal"));
        assert!(!pat("*-internal").matches("acme-internal-x"));
        assert!(pat("a*b*c").matches("axxbyyc"));
        assert!(!pat("a*b*c").matches("axxcyyb"));
        assert!(pat("*").matches(""), "the catch-all takes everything");
        assert!(pat("ab*ab").matches("abab"), "prefix and suffix may not overlap");
        assert!(!pat("ab*ab").matches("aba"));
    }

    #[test]
    fn a_pattern_is_canonicalized_whole_and_refused_when_it_cannot_be() {
        assert_eq!(pat("@ACME/*").canonical(), "@acme/*");
        assert!(pat("@ACME/*").matches("@acme/foo"), "the key is lowercase too");
        assert_eq!(pat("@acme/*").source(), "@acme/*");
        assert!(pat("*").is_catch_all());
        assert!(!pat("a*").is_catch_all());
        // PEP 503 collapses a run of separators, so the seam is exercised in
        // context rather than piece by piece.
        let pep503 = |s: &str| {
            let mut out = String::new();
            let mut run = false;
            for c in s.chars() {
                if matches!(c, '-' | '_' | '.') {
                    run = true;
                    continue;
                }
                if run && !out.is_empty() {
                    out.push('-');
                }
                run = false;
                out.push(c.to_ascii_lowercase());
            }
            out
        };
        let p = compile_pattern("Acme_*", pep503).unwrap();
        assert_eq!(p.canonical(), "acme-*");
        assert!(p.matches(&pep503("Acme.Lib")));
        assert!(!p.matches(&pep503("acmelib")), "the separator survived the cut");
    }

    #[test]
    fn the_write_side_refuses_what_the_language_cannot_express() {
        for bad in ["", "a\\*b", "a\0b", "*********"] {
            assert!(compile_pattern(bad, lower).is_err(), "{bad:?}");
        }
        let too_long = "a".repeat(MAX_PATTERN_LEN + 1);
        assert!(compile_pattern(&too_long, lower).is_err());
        assert!(
            compile_pattern("a*b", |_| "zzz".to_string()).is_err(),
            "a canonicalization that eats a star is refused, not trusted"
        );
    }

    #[test]
    fn a_rule_only_ever_removes_members() {
        let set = RouteSet::new(
            vec![rule("internal", &["@acme/*"], Effect::Members(vec!["i1".into()]))],
            1,
        );
        let key = "@acme/foo";
        assert!(set.decide(Format::Npm, key, key, hosted("i1")).admitted());
        assert_eq!(
            refusers(set.decide(Format::Npm, key, key, proxy("i2"))),
            ["internal"],
            "a proxy member is never a hosted target"
        );
        assert_eq!(
            refusers(set.decide(Format::Npm, key, key, hosted("i2"))),
            ["internal"],
            "a hosted member that is not the target is refused too"
        );
        let other = "left-pad";
        assert!(
            set.decide(Format::Npm, other, other, proxy("i2")).admitted(),
            "a name outside every pattern is untouched"
        );
        assert!(
            set.decide(Format::Cargo, key, key, proxy("i2")).admitted(),
            "a rule speaks for its own format only"
        );
    }

    #[test]
    fn restrictions_intersect_and_deny_wins() {
        let set = RouteSet::new(
            vec![
                rule("a-wide", &["@acme/*"], Effect::AnyHosted),
                rule("b-narrow", &["@acme/secret*"], Effect::Members(vec!["i1".into()])),
                rule("c-deny", &["@acme/banned"], Effect::Deny),
            ],
            1,
        );
        let plain = "@acme/tool";
        assert!(set.decide(Format::Npm, plain, plain, hosted("i2")).admitted());
        let secret = "@acme/secret-x";
        assert_eq!(
            refusers(set.decide(Format::Npm, secret, secret, hosted("i2"))),
            ["b-narrow"],
            "the intersection of the two rules, not the widest"
        );
        let banned = "@acme/banned";
        assert_eq!(
            refusers(set.decide(Format::Npm, banned, banned, hosted("i1"))),
            ["c-deny"],
            "deny is allow(nothing)"
        );
    }

    #[test]
    fn a_refusal_is_imputable_to_every_rule_that_refuses_it() {
        let set = RouteSet::new(
            vec![
                rule("zebra", &["@acme/*"], Effect::Deny),
                rule("alpha", &["@acme/*"], Effect::Deny),
                rule("middle", &["@other/*"], Effect::Deny),
            ],
            7,
        );
        let key = "@acme/foo";
        let decision = set.decide(Format::Npm, key, key, hosted("i1"));
        assert_eq!(
            refusers(decision),
            ["alpha", "zebra"],
            "both, in name order: deleting the first would not lift the refusal"
        );
        assert_eq!(
            decision.refused_by().unwrap().label(),
            "alpha",
            "a label for one line of UI, never an identity"
        );
        assert_eq!(set.version(), 7);
        assert_eq!(set.rules().len(), 3);
    }

    #[test]
    fn an_exception_reopens_one_identity_class_and_no_spelling_around_it() {
        let mut r = rule("internal", &["@acme/*"], Effect::AnyHosted);
        r.except = vec!["@acme/public-ui".to_string()];
        let set = RouteSet::new(vec![r], 1);
        // npm identity is the exact spelling: the coarsened key matches the
        // pattern, but the exception only holds for the exact name.
        assert!(set
            .decide(Format::Npm, "@acme/public-ui", "@acme/public-ui", proxy("i9"))
            .admitted());
        assert_eq!(
            refusers(set.decide(Format::Npm, "@acme/public-ui", "@ACME/public-ui", proxy("i9"))),
            ["internal"],
            "a coarsening never reopens"
        );
    }

    /// D7: a search term never leaves for an upstream that the rule would
    /// refuse the answer from, and a prefix of the covered class is refused
    /// too — otherwise the internal name comes back by completion.
    #[test]
    fn an_enumeration_term_is_decided_before_the_upstream_call() {
        let set = RouteSet::new(
            vec![rule("internal", &["acme.*"], Effect::AnyHosted)],
            1,
        );
        let refused = |term: &str| {
            set.term_refusers(Format::Npm, term, proxy("i2")).count() > 0
        };
        assert!(refused("acme.internal"), "the term is inside the covered class");
        assert!(refused("acme"), "and a prefix of it, which would complete into it");
        assert!(refused("a"), "as would any prefix, however short");
        assert!(!refused("other"), "an unrelated term leaves as it always did");
        assert!(
            set.term_refusers(Format::Npm, "acme.internal", hosted("i1")).count() == 0,
            "a hosted member is what the rule allows"
        );
        assert!(
            set.term_refusers(Format::Cargo, "acme.internal", proxy("i2")).count() == 0,
            "a rule speaks for its own format only"
        );
    }

    #[test]
    fn a_format_with_no_rule_pays_for_none_of_this() {
        let set = RouteSet::new(vec![rule("internal", &["@acme/*"], Effect::Deny)], 1);
        assert!(set.governs(Format::Npm));
        assert!(!set.governs(Format::Cargo));
        assert!(!RouteSet::default().governs(Format::Npm));
        assert_eq!(RouteSet::default().version(), 0);
        assert_eq!(set.covering(Format::Npm, "@acme/x", "@acme/x").len(), 1);
        assert!(set.covering(Format::Npm, "other", "other").is_empty());
        assert_eq!(Effect::Deny.as_str(), "deny");
        assert_eq!(Effect::AnyHosted.as_str(), "allow_hosted");
        assert_eq!(Effect::Members(vec![]).as_str(), "allow_members");
    }
}

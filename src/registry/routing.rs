//! The routing snapshot a request decides against, and the gate every site
//! that enumerates members goes through.
//!
//! The rules live in the domain; their patterns are compiled by the format's
//! `FormatRules`, which lives here. This module is the seam between the two,
//! and the only place a stored rule becomes a decidable one.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::domain::{
    DomainError, Format, MemberRef, RepoKind, Repository, RouteSet, RoutingRule,
};
use crate::ports::repositories::RepositoryStore;
use crate::ports::routing::{RoutingRuleStore, StoredRule};
use crate::registry::resolve::{ResolveError, Subject};
use crate::registry::rules::rules_of;

/// Compile what the store holds into what the walk decides against. An
/// unreadable rule set is an error and never an empty one: a rule nobody can
/// parse must stop a boot, not quietly stop protecting.
pub fn compile(stored: &[StoredRule], version: u64) -> Result<RouteSet, DomainError> {
    let mut rules = Vec::with_capacity(stored.len());
    for rule in stored {
        let format_rules = rules_of(rule.format)?;
        let patterns = rule
            .patterns
            .iter()
            .map(|p| format_rules.canonical_pattern(p))
            .collect::<Result<Vec<_>, _>>()?;
        rules.push(RoutingRule {
            name: rule.name.clone(),
            format: rule.format,
            patterns,
            except: rule.except.iter().map(|e| format_rules.ident_key(e)).collect(),
            effect: rule.effect.clone(),
        });
    }
    Ok(RouteSet::new(rules, version))
}

/// What one request decides against.
#[derive(Clone)]
pub struct Snapshot {
    pub set: Arc<RouteSet>,
    /// Nothing has refreshed the set for longer than `max_snapshot_age`.
    pub stale: bool,
}

struct Held {
    set: Arc<RouteSet>,
    refreshed: Instant,
}

/// The composition root's holder: one compiled snapshot, refreshed on a local
/// write and on a timer, with a bound on how long an unrefreshed one is
/// served.
pub struct RoutingRegistry {
    store: Option<Arc<dyn RoutingRuleStore>>,
    max_age: Duration,
    held: RwLock<Held>,
}

impl RoutingRegistry {
    /// A registry with nothing behind it: a fixture, and the tests that
    /// script a rule set directly.
    pub fn fixed(set: RouteSet) -> Self {
        Self {
            store: None,
            max_age: Duration::MAX,
            held: RwLock::new(Held {
                set: Arc::new(set),
                refreshed: Instant::now(),
            }),
        }
    }

    /// Read and compile the rules once. An error aborts the boot, like the
    /// policy startup guard: a rule set that cannot be read is not an absence
    /// of rules.
    pub async fn load(
        store: Arc<dyn RoutingRuleStore>,
        max_age: Duration,
    ) -> Result<Self, ResolveError> {
        let registry = Self {
            store: Some(store),
            max_age,
            held: RwLock::new(Held {
                set: Arc::new(RouteSet::default()),
                refreshed: Instant::now(),
            }),
        };
        registry.refresh().await?;
        Ok(registry)
    }

    /// Re-read and re-compile. A failure keeps the last valid snapshot and
    /// leaves its age running, so a node cut off from the database ends up
    /// stale rather than wrong.
    pub async fn refresh(&self) -> Result<u64, ResolveError> {
        let Some(store) = &self.store else {
            return Ok(self.version());
        };
        let version = store.version().await?;
        let set = compile(&store.all().await?, version)?;
        let mut held = self.held.write().expect("the routing snapshot lock");
        held.set = Arc::new(set);
        held.refreshed = Instant::now();
        Ok(version)
    }

    pub fn version(&self) -> u64 {
        self.held.read().expect("the routing snapshot lock").set.version()
    }

    pub fn snapshot(&self) -> Snapshot {
        let held = self.held.read().expect("the routing snapshot lock");
        Snapshot {
            set: held.set.clone(),
            stale: held.refreshed.elapsed() > self.max_age,
        }
    }
}

impl Default for RoutingRegistry {
    fn default() -> Self {
        Self::fixed(RouteSet::default())
    }
}

/// Why a member was left out, for the audit and for `explain` — never for the
/// wire, where a refusal names no rule (I6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Admitted,
    /// Every rule that refuses, in name order, and the identity key they
    /// refused: a set, not one rule chosen out of it (I8).
    Refused { rules: Vec<String>, ident_key: String },
    /// This node has not refreshed its rules within `max_snapshot_age`, so
    /// the surface the rules protect closes rather than serve the state from
    /// before a rule it may not have seen (D11). Hosted members are untouched.
    Stale,
}

impl Verdict {
    pub fn admitted(&self) -> bool {
        matches!(self, Verdict::Admitted)
    }

    /// The rules to name in an audit line; empty when the refusal is the
    /// staleness bound rather than a rule.
    pub fn rules(&self) -> &[String] {
        match self {
            Verdict::Refused { rules, .. } => rules,
            _ => &[],
        }
    }
}

/// One request's routing decision: opened once, asked once per member.
///
/// The two keys are computed here, at the entry of the resolution, and not
/// per member — `decide` then takes two `&str` and allocates nothing.
pub struct Gate {
    snapshot: Snapshot,
    format: Format,
    keys: Option<Keys>,
    /// An enumeration's term in `match_key` form; `None` on a named subject
    /// and on an enumeration the client did not narrow.
    term: Option<String>,
}

struct Keys {
    match_key: String,
    ident_key: String,
}

impl Gate {
    pub fn open(
        snapshot: Snapshot,
        format: Format,
        subject: &Subject<'_>,
    ) -> Result<Self, DomainError> {
        let rules = rules_of(format)?;
        let (keys, term) = match subject {
            Subject::Named(name) => (
                Some(Keys {
                    match_key: rules.match_key(name),
                    ident_key: rules.ident_key(name),
                }),
                None,
            ),
            Subject::Enumeration { term } => (None, term.map(|t| rules.match_key(t))),
        };
        Ok(Self { snapshot, format, keys, term })
    }

    pub fn version(&self) -> u64 {
        self.snapshot.set.version()
    }

    /// The two keys as `explain` shows them beside the name the caller typed,
    /// so a coarsening is visible rather than surprising (R8).
    pub fn keys(&self) -> Option<(&str, &str)> {
        self.keys.as_ref().map(|k| (k.match_key.as_str(), k.ident_key.as_str()))
    }

    /// Whether any rule at all speaks for this format: the short circuit that
    /// keeps a deployment without rules paying for none of this.
    fn idle(&self) -> bool {
        !self.snapshot.set.governs(self.format)
    }

    /// The verdict on one member, and the one store read it can cost — the
    /// member's incarnation, read only when a rule has something to say about
    /// the subject, and never a read that depends on the name itself (I3,
    /// I13).
    pub async fn admits(
        &self,
        repos: &dyn RepositoryStore,
        member: &Repository,
    ) -> Result<Verdict, ResolveError> {
        if self.idle() {
            return Ok(Verdict::Admitted);
        }
        let kind = member.kind()?;
        if self.snapshot.stale && kind == RepoKind::Proxy {
            return Ok(Verdict::Stale);
        }
        let set = &self.snapshot.set;
        let concerned = match (&self.keys, &self.term) {
            (Some(keys), _) => !set.covering(self.format, &keys.match_key, &keys.ident_key).is_empty(),
            (None, Some(term)) => set.touches_term(self.format, term),
            // An enumeration nobody narrowed: no member can be ruled out in
            // advance, and the merge is what filters (D7).
            (None, None) => false,
        };
        if !concerned {
            return Ok(Verdict::Admitted);
        }
        let incarnation = repos.incarnation(member.id).await?.unwrap_or_default();
        let at = MemberRef { kind, incarnation: &incarnation };
        Ok(match (&self.keys, &self.term) {
            (Some(keys), _) => match set
                .decide(self.format, &keys.match_key, &keys.ident_key, at)
                .refused_by()
            {
                None => Verdict::Admitted,
                Some(refusals) => Verdict::Refused {
                    rules: refusals.names().map(str::to_string).collect(),
                    ident_key: keys.ident_key.clone(),
                },
            },
            (None, Some(term)) => self.decide_term(at, term),
            (None, None) => Verdict::Admitted,
        })
    }

    /// A term is not a name: a rule that covers the class the term names, or
    /// that the term would complete into, refuses this member before any
    /// upstream call and before any cache entry (I12).
    fn decide_term(&self, at: MemberRef<'_>, term: &str) -> Verdict {
        let rules: Vec<String> = self
            .snapshot
            .set
            .term_refusers(self.format, term, at)
            .map(|r| r.name.clone())
            .collect();
        if rules.is_empty() {
            Verdict::Admitted
        } else {
            Verdict::Refused { rules, ident_key: term.to_string() }
        }
    }
}

#[cfg(test)]
mod tests;

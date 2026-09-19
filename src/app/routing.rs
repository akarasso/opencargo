//! Administering the routing rules, and trying one out before activating it.
//!
//! A rule is written by name and read back by incarnation: the administrator
//! names hosted repositories, the store keeps the opaque incarnations they
//! had at that moment, and deleting a repository then recreating its name
//! never re-targets the rule (I11).
//!
//! Every write ends the same way: the node's compiled snapshot is refreshed
//! before the call returns, so the administrator who wrote the rule is never
//! served by a node that has not seen it.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::app::audit::{self, Actor};
use crate::domain::{
    Audience, DomainEvent, RouteEffect as Effect, Format, RepoKind, Repository, RouteSet, RoutingRule,
};
use crate::error::{AppError, AppResult};
use crate::ports::audit::AuditStore;
use crate::ports::events::Events;
use crate::ports::repositories::RepositoryStore;
use crate::ports::routing::{NewRule, RoutingRuleStore, StoredRule};
use crate::registry::resolve::Subject;
use crate::registry::routing::{compile, Gate, RoutingRegistry, Verdict};
use crate::registry::rules::rules_of;

/// A rule as an administrator writes it: targets are repository **names**,
/// which is the only place a name is ever used to designate one.
pub struct RuleDraft<'a> {
    pub name: &'a str,
    pub format: Format,
    pub patterns: &'a [String],
    pub except: &'a [String],
    pub effect: &'a str,
    pub targets: &'a [String],
    /// A pattern that filters every name of its format cuts all of that
    /// format's proxy traffic, so it is written on purpose or not at all (R1).
    pub confirm_catch_all: bool,
}

pub struct RoutingRules {
    store: Arc<dyn RoutingRuleStore>,
    repos: Arc<dyn RepositoryStore>,
    registry: Arc<RoutingRegistry>,
    audit: Arc<dyn AuditStore>,
    events: Arc<dyn Events>,
}

impl RoutingRules {
    pub fn new(
        store: Arc<dyn RoutingRuleStore>,
        repos: Arc<dyn RepositoryStore>,
        registry: Arc<RoutingRegistry>,
        audit: Arc<dyn AuditStore>,
        events: Arc<dyn Events>,
    ) -> Self {
        Self { store, repos, registry, audit, events }
    }

    pub async fn all(&self) -> AppResult<Vec<StoredRule>> {
        Ok(self.store.all().await?)
    }

    pub async fn by_name(&self, name: &str) -> AppResult<StoredRule> {
        self.store
            .by_name(name)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("routing rule not found: {name}")))
    }

    pub async fn create(
        &self,
        draft: &RuleDraft<'_>,
        by: &Actor<'_>,
        now: DateTime<Utc>,
    ) -> AppResult<StoredRule> {
        let effect = self.effect_of(draft).await?;
        let written = self
            .store
            .create(&self.new_rule(draft, &effect), now)
            .await?;
        self.settled("routing.rule.create", draft.name, by, now).await?;
        Ok(written)
    }

    pub async fn update(
        &self,
        draft: &RuleDraft<'_>,
        by: &Actor<'_>,
        now: DateTime<Utc>,
    ) -> AppResult<StoredRule> {
        let effect = self.effect_of(draft).await?;
        let written = self
            .store
            .update(&self.new_rule(draft, &effect), now)
            .await?;
        self.settled("routing.rule.update", draft.name, by, now).await?;
        Ok(written)
    }

    pub async fn delete(
        &self,
        name: &str,
        by: &Actor<'_>,
        now: DateTime<Utc>,
    ) -> AppResult<()> {
        self.store.delete(name).await?;
        self.settled("routing.rule.delete", name, by, now).await?;
        Ok(())
    }

    /// Seed an empty table from the configuration file, and say when the file
    /// and the table have drifted apart.
    ///
    /// The file seeds a deployment once; it does not own it afterwards, so
    /// hardening a pattern here has no effect on a live one. That silence is
    /// what the note is for: it names the rules the file declares that the
    /// table does not have, or has differently.
    pub async fn seed(
        &self,
        drafts: &[RuleDraft<'_>],
        now: DateTime<Utc>,
    ) -> AppResult<Seeded> {
        let mut effects = Vec::with_capacity(drafts.len());
        for draft in drafts {
            effects.push(self.effect_of(draft).await?);
        }
        let rules: Vec<NewRule<'_>> = drafts
            .iter()
            .zip(&effects)
            .map(|(draft, effect)| self.new_rule(draft, effect))
            .collect();
        let written = self.store.ensure_seeded(&rules, now).await?;
        let stored = self.store.all().await?;
        let drifted = if written > 0 {
            Vec::new()
        } else {
            drafts
                .iter()
                .zip(&effects)
                .filter(|(draft, effect)| {
                    !stored.iter().any(|s| {
                        s.name == draft.name
                            && s.patterns == draft.patterns
                            && s.except == draft.except
                            && s.effect == **effect
                    })
                })
                .map(|(draft, _)| draft.name.to_string())
                .collect()
        };
        self.registry.refresh().await?;
        Ok(Seeded { written, drifted })
    }

    /// What the walk would do with this name in this repository, on the same
    /// snapshot and through the same `decide` the walk uses (I10).
    ///
    /// `candidate` is a rule that is *not* stored: reviewing one must not
    /// require publishing it.
    pub async fn explain(
        &self,
        repository: &str,
        name: &str,
        candidate: Option<&RuleDraft<'_>>,
    ) -> AppResult<Explanation> {
        let repo = crate::registry::load_repo(self.repos.as_ref(), repository).await?;
        let format = repo.fmt()?;
        let mut snapshot = self.registry.snapshot();
        if let Some(draft) = candidate {
            let effect = self.effect_of(draft).await?;
            let mut rules: Vec<RoutingRule> = snapshot.set.rules().to_vec();
            rules.push(compile_one(draft, &effect)?);
            snapshot.set = Arc::new(RouteSet::new(rules, snapshot.set.version()));
        }
        let subject = Subject::of(name);
        let gate = Gate::open(snapshot, format, &subject)?;
        let (match_key, ident_key) = gate
            .keys()
            .map(|(m, i)| (m.to_string(), i.to_string()))
            .expect("a named subject has both keys");

        let mut members = Vec::new();
        for member in self.terminals(&repo).await? {
            let verdict = gate.admits(self.repos.as_ref(), &member).await?;
            let kind = member.kind()?;
            members.push(Member {
                admitted: verdict.admitted(),
                refused_by: verdict.rules().to_vec(),
                stale: matches!(verdict, Verdict::Stale),
                name: member.name,
                kind,
            });
        }
        Ok(Explanation {
            match_key,
            ident_key,
            version: gate.version(),
            members,
        })
    }

    /// The terminal members a walk would visit, in walk order, with the
    /// permission ladder left out: `explain` is an admin tool and answers
    /// about the configuration, not about one caller's view of it.
    async fn terminals(&self, repo: &Repository) -> AppResult<Vec<Repository>> {
        let mut seen = vec![repo.id];
        let mut out = Vec::new();
        self.walk_terminals(repo, 0, &mut seen, &mut out).await?;
        Ok(out)
    }

    fn walk_terminals<'a>(
        &'a self,
        repo: &'a Repository,
        depth: u32,
        seen: &'a mut Vec<i64>,
        out: &'a mut Vec<Repository>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AppResult<()>> + Send + 'a>> {
        Box::pin(async move {
            if repo.kind()? != RepoKind::Group {
                out.push(repo.clone());
                return Ok(());
            }
            if depth >= crate::domain::MAX_GROUP_DEPTH {
                return Ok(());
            }
            let format = repo.fmt()?;
            for name in repo.members() {
                let Some(member) = self.repos.by_name(&name).await? else {
                    continue;
                };
                if member.fmt()? != format || seen.contains(&member.id) {
                    continue;
                }
                seen.push(member.id);
                self.walk_terminals(&member, depth + 1, seen, out).await?;
            }
            Ok(())
        })
    }

    /// Resolve target names to the incarnations the rule keeps, refusing a
    /// target that is not a hosted repository at write time rather than at
    /// resolution time (D10).
    async fn effect_of(&self, draft: &RuleDraft<'_>) -> AppResult<Effect> {
        let effect = match draft.effect {
            "deny" => Effect::Deny,
            "allow_hosted" => Effect::AnyHosted,
            "allow_members" => {
                if draft.targets.is_empty() {
                    return Err(AppError::BadRequest(
                        "allow_members names no repository; use deny to allow none".to_string(),
                    ));
                }
                let mut incarnations = Vec::with_capacity(draft.targets.len());
                for target in draft.targets {
                    let repo = crate::registry::load_repo(self.repos.as_ref(), target).await?;
                    if repo.kind()? != RepoKind::Hosted {
                        return Err(AppError::BadRequest(format!(
                            "routing target '{target}' is not a hosted repository"
                        )));
                    }
                    if repo.fmt()? != draft.format {
                        return Err(AppError::BadRequest(format!(
                            "routing target '{target}' is not a {} repository",
                            draft.format.as_str()
                        )));
                    }
                    incarnations.push(self.repos.incarnation(repo.id).await?.ok_or_else(|| {
                        AppError::Internal(format!("repository '{target}' has no incarnation"))
                    })?);
                }
                Effect::Members(incarnations)
            }
            other => {
                return Err(AppError::BadRequest(format!(
                    "unknown routing effect '{other}': deny, allow_hosted or allow_members"
                )))
            }
        };
        validate(draft)?;
        Ok(effect)
    }

    fn new_rule<'a>(&self, draft: &'a RuleDraft<'a>, effect: &'a Effect) -> NewRule<'a> {
        NewRule {
            name: draft.name,
            format: draft.format,
            patterns: draft.patterns,
            except: draft.except,
            effect,
        }
    }

    /// One write, one refreshed snapshot, one audit line, one announcement.
    async fn settled(
        &self,
        action: &str,
        name: &str,
        by: &Actor<'_>,
        now: DateTime<Utc>,
    ) -> AppResult<()> {
        self.registry.refresh().await?;
        audit::record(self.audit.as_ref(), self.events.as_ref(), by, action, Some(name), now).await;
        self.events
            .emit(DomainEvent::RepositoriesChanged, Audience::Admin);
        Ok(())
    }
}

/// What seeding did, and what it deliberately did not do.
pub struct Seeded {
    pub written: usize,
    /// Rules the file declares that the table does not carry, or carries
    /// differently. A hardened pattern lands here and nowhere else.
    pub drifted: Vec<String>,
}

pub struct Explanation {
    pub match_key: String,
    pub ident_key: String,
    pub version: u64,
    pub members: Vec<Member>,
}

pub struct Member {
    pub name: String,
    pub kind: RepoKind,
    pub admitted: bool,
    /// Every rule that refuses, never one chosen out of them (I8).
    pub refused_by: Vec<String>,
    pub stale: bool,
}

/// Everything a rule has to satisfy to be written at all.
fn validate(draft: &RuleDraft<'_>) -> AppResult<()> {
    if draft.name.trim().is_empty() {
        return Err(AppError::BadRequest("a routing rule needs a name".to_string()));
    }
    if draft.patterns.is_empty() {
        return Err(AppError::BadRequest(
            "a routing rule with no pattern covers nothing".to_string(),
        ));
    }
    let rules = rules_of(draft.format)?;
    for pattern in draft.patterns {
        let compiled = rules.canonical_pattern(pattern)?;
        if compiled.is_catch_all() && !draft.confirm_catch_all {
            return Err(AppError::BadRequest(format!(
                "pattern '{pattern}' filters every {} name: confirm it explicitly",
                draft.format.as_str()
            )));
        }
    }
    for except in draft.except {
        if except.contains('*') {
            return Err(AppError::BadRequest(format!(
                "exception '{except}' is a name, not a pattern: except[] is compared on store \
                 identity and is never globbed"
            )));
        }
        let key = rules.ident_key(except);
        if key.is_empty() || rules.ident_key(&key) != key {
            return Err(AppError::BadRequest(format!(
                "exception '{except}' has no stable identity in this format"
            )));
        }
    }
    Ok(())
}

fn compile_one(draft: &RuleDraft<'_>, effect: &Effect) -> AppResult<RoutingRule> {
    let stored = StoredRule {
        name: draft.name.to_string(),
        format: draft.format,
        patterns: draft.patterns.to_vec(),
        except: draft.except.to_vec(),
        effect: effect.clone(),
        created_at: DateTime::UNIX_EPOCH,
        updated_at: DateTime::UNIX_EPOCH,
    };
    let set = compile(std::slice::from_ref(&stored), 0)?;
    Ok(set.rules().first().expect("one rule in, one rule out").clone())
}


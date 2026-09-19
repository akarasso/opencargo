//! The gate at the two sites the walk owns: `walk` and `view`. What a format
//! does with a hit is not under test here; which members are asked at all is.

use std::time::Duration;

use chrono::DateTime;

use super::*;
use crate::domain::{Effect, RepoSpec, Visibility};
use crate::ports::routing::StoredRule;
use crate::registry::resolve::{collect, first_hit, view, Collected, Leaf, Upstream};
use crate::testing::fakes::FakeDb;
use crate::testing::resolver::Resolver;
use crate::domain::{CacheRepo, Outcome};

/// A leaf that answers with the member's name and remembers who was asked:
/// "zero request to the upstream" is an assertion about this list.
struct Probe {
    subject: String,
    /// `None` is a named subject; `Some` is an enumeration carrying that term.
    term: Option<Option<String>>,
    asked: std::sync::Mutex<Vec<String>>,
}

impl Probe {
    fn named(name: &str) -> Self {
        Self {
            subject: name.to_string(),
            term: None,
            asked: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn searching(term: &str) -> Self {
        Self {
            subject: String::new(),
            term: Some(Some(term.to_string())),
            asked: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn listing() -> Self {
        Self {
            subject: String::new(),
            term: Some(None),
            asked: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }

    fn answer(&self, member: &Repository) -> Outcome<String> {
        self.asked.lock().unwrap().push(member.name.clone());
        Outcome::Found(member.name.clone())
    }
}

#[async_trait::async_trait]
impl Leaf for Probe {
    type Out = String;

    fn subject(&self) -> Subject<'_> {
        match &self.term {
            None => Subject::of(&self.subject),
            Some(None) => Subject::listing(),
            Some(Some(term)) => Subject::searching(term),
        }
    }

    async fn hosted(
        &self,
        _cx: &Cx<'_>,
        m: CacheRepo<'_>,
    ) -> Result<Outcome<String>, ResolveError> {
        Ok(self.answer(m.0))
    }

    async fn proxy(
        &self,
        _cx: &Cx<'_>,
        m: CacheRepo<'_>,
        _up: &Upstream,
    ) -> Result<Outcome<String>, ResolveError> {
        Ok(self.answer(m.0))
    }
}

fn spec<'a>(name: &'a str, kind: RepoKind, members: &'a [String]) -> RepoSpec<'a> {
    RepoSpec {
        name,
        kind,
        format: Format::Npm,
        visibility: Visibility::Public,
        upstream: (kind == RepoKind::Proxy).then_some("http://127.0.0.1:1"),
        members,
    }
}

async fn add(db: &FakeDb, name: &str, kind: RepoKind, members: &[&str]) {
    let members: Vec<String> = members.iter().map(|m| m.to_string()).collect();
    db.repositories()
        .create(&spec(name, kind, &members), DateTime::UNIX_EPOCH)
        .await
        .unwrap();
}

/// `internal` and `sandbox` hosted, `public` proxying an upstream, grouped in
/// that order; plus a nested group whose own membership is wider.
async fn fixture() -> Resolver {
    let db = FakeDb::new();
    add(&db, "internal", RepoKind::Hosted, &[]).await;
    add(&db, "sandbox", RepoKind::Hosted, &[]).await;
    add(&db, "public", RepoKind::Proxy, &[]).await;
    add(&db, "inner", RepoKind::Group, &["sandbox", "public"]).await;
    add(&db, "all", RepoKind::Group, &["internal", "public"]).await;
    add(&db, "outer", RepoKind::Group, &["internal", "inner"]).await;
    Resolver::new(db, Default::default())
}

async fn repo(fx: &Resolver, name: &str) -> Repository {
    fx.fakes
        .repositories()
        .by_name(name)
        .await
        .unwrap()
        .expect("the fixture declares it")
}

async fn incarnation_of(fx: &Resolver, name: &str) -> String {
    let id = repo(fx, name).await.id;
    fx.fakes
        .repositories()
        .incarnation(id)
        .await
        .unwrap()
        .expect("every repository gets one at creation")
}

fn stored(name: &str, patterns: &[&str], effect: Effect) -> StoredRule {
    StoredRule {
        name: name.to_string(),
        format: Format::Npm,
        patterns: patterns.iter().map(|p| p.to_string()).collect(),
        except: Vec::new(),
        effect,
        created_at: DateTime::UNIX_EPOCH,
        updated_at: DateTime::UNIX_EPOCH,
    }
}

fn install(fx: &mut Resolver, rules: &[StoredRule], version: u64) {
    fx.routing = RoutingRegistry::fixed(compile(rules, version).unwrap());
}

/// Whoever the walk actually asked, in order.
async fn asked(fx: &Resolver, group: &str, probe: &Probe) -> Vec<String> {
    let Collected { hits, .. } = collect(&fx.cx(None, group), &repo(fx, group).await, probe)
        .await
        .unwrap();
    assert_eq!(hits, probe.asked(), "a hit comes from a member that was asked");
    probe.asked()
}

/// T1, T3, T7: a scope pinned to one hosted member never reaches the proxy —
/// through the group or through the proxy's own URL — and a name outside the
/// patterns is untouched.
#[tokio::test]
async fn a_pinned_scope_never_leaves_for_an_upstream() {
    let mut fx = fixture().await;
    let internal = incarnation_of(&fx, "internal").await;
    install(
        &mut fx,
        &[stored("acme", &["@acme/*"], Effect::Members(vec![internal]))],
        1,
    );

    let probe = Probe::named("@acme/foo");
    assert_eq!(asked(&fx, "all", &probe).await, ["internal"]);

    let elsewhere = Probe::named("left-pad");
    assert_eq!(asked(&fx, "all", &elsewhere).await, ["internal", "public"]);

    // The proxy's own URL is a walk of one member, and the rule is total over
    // its format, so it decides there too (D5).
    let direct = Probe::named("@acme/foo");
    let err = first_hit(&fx.cx(None, "public"), &repo(&fx, "public").await, &direct)
        .await
        .unwrap_err();
    assert!(matches!(err, ResolveError::NotFound(_)), "{err}");
    assert!(direct.asked().is_empty(), "no member was asked at all");
}

/// T4: the npm spelling that motivated the whole feature. A capital letter
/// reaches the read path and used to walk straight past a rule keyed on
/// `normalize`, which is the identity for npm.
#[tokio::test]
async fn a_capital_letter_does_not_walk_around_a_rule() {
    let mut fx = fixture().await;
    install(&mut fx, &[stored("acme", &["@acme/*"], Effect::AnyHosted)], 1);
    for spelling in ["@acme/foo", "@ACME/foo", "@Acme/Foo"] {
        let probe = Probe::named(spelling);
        assert_eq!(asked(&fx, "all", &probe).await, ["internal"], "{spelling}");
    }
}

/// T5: a nested group cannot loosen the root's restriction — every terminal
/// member is decided once, so the intersection holds down the tree.
#[tokio::test]
async fn a_nested_group_inherits_the_restriction() {
    let mut fx = fixture().await;
    let internal = incarnation_of(&fx, "internal").await;
    install(
        &mut fx,
        &[stored("acme", &["@acme/*"], Effect::Members(vec![internal]))],
        1,
    );
    let probe = Probe::named("@acme/foo");
    assert_eq!(
        asked(&fx, "outer", &probe).await,
        ["internal"],
        "neither the inner group's hosted member nor its proxy answers"
    );
}

/// T6: two rules intersect, and an exception reopens exactly the spelling the
/// administrator wrote — not the one a coarsening would let in (D0bis).
#[tokio::test]
async fn rules_intersect_and_an_exception_reopens_one_spelling() {
    let mut fx = fixture().await;
    let mut rule = stored("acme", &["@acme/*"], Effect::AnyHosted);
    rule.except = vec!["@acme/public-ui".to_string()];
    install(
        &mut fx,
        &[rule, stored("secrets", &["@acme/secret*"], Effect::Deny)],
        1,
    );

    let reopened = Probe::named("@acme/public-ui");
    assert_eq!(asked(&fx, "all", &reopened).await, ["internal", "public"]);

    let around = Probe::named("@ACME/public-ui");
    assert_eq!(
        asked(&fx, "all", &around).await,
        ["internal"],
        "the exception is store identity, and npm's is the exact spelling"
    );

    let denied = Probe::named("@acme/secret-x");
    assert!(asked(&fx, "all", &denied).await.is_empty(), "deny is allow(nothing)");
}

/// T24, I12: a search term is decided before any upstream call, both when it
/// is inside the covered class and when it would complete into it.
#[tokio::test]
async fn a_search_term_is_decided_before_the_call() {
    let mut fx = fixture().await;
    install(&mut fx, &[stored("acme", &["@acme/*"], Effect::AnyHosted)], 1);

    for term in ["@acme/internal", "@acme", "@"] {
        let probe = Probe::searching(term);
        assert_eq!(asked(&fx, "all", &probe).await, ["internal"], "{term}");
    }

    let unrelated = Probe::searching("left-pad");
    assert_eq!(asked(&fx, "all", &unrelated).await, ["internal", "public"]);

    // T13: nothing narrows an untermed enumeration, so every member is asked
    // and the merge is what filters.
    let listing = Probe::listing();
    assert_eq!(asked(&fx, "all", &listing).await, ["internal", "public"]);
}

/// T18, D6bis: the view is filtered by the same gate, so a member the walk
/// refuses is not stamped, not read and not counted as an upstream source.
#[tokio::test]
async fn the_view_and_the_walk_see_the_same_members() {
    let mut fx = fixture().await;
    let internal = incarnation_of(&fx, "internal").await;
    install(
        &mut fx,
        &[stored("acme", &["@acme/*"], Effect::Members(vec![internal]))],
        1,
    );
    let group = repo(&fx, "all").await;
    let cx = fx.cx(None, "all");

    let pinned = view(&cx, &group, &Subject::of("@acme/foo")).await.unwrap();
    assert_eq!(
        pinned.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
        ["internal"]
    );

    let free = view(&cx, &group, &Subject::of("left-pad")).await.unwrap();
    assert_eq!(
        free.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
        ["internal", "public"]
    );

    let listing = view(&cx, &group, &Subject::listing()).await.unwrap();
    assert_eq!(listing.len(), 2, "an untermed enumeration filters nothing here");
}

/// T28, D11: past `max_snapshot_age` a node stops serving the state from
/// before a rule it may not have seen. The proxy members of a governed format
/// close; the hosted ones are untouched.
#[tokio::test]
async fn a_snapshot_nobody_refreshed_closes_the_protected_surface() {
    let mut fx = fixture().await;
    let rules = [stored("acme", &["@acme/*"], Effect::AnyHosted)];
    install(&mut fx, &rules, 1);
    let fresh = Probe::named("left-pad");
    assert_eq!(asked(&fx, "all", &fresh).await, ["internal", "public"]);

    fx.routing = RoutingRegistry::load(Arc::new(Rules(rules.to_vec())), Duration::ZERO)
        .await
        .unwrap();

    let stale = Probe::named("left-pad");
    assert_eq!(
        asked(&fx, "all", &stale).await,
        ["internal"],
        "a name no rule covers still closes: the surface is the format"
    );
}

/// T12: a rule set that cannot be compiled stops the boot rather than
/// silently protecting nothing (I4).
#[tokio::test]
async fn an_unreadable_rule_set_refuses_to_load() {
    let broken = stored("bad", &["a\\*b"], Effect::Deny);
    assert!(compile(std::slice::from_ref(&broken), 1).is_err());
    assert!(
        RoutingRegistry::load(Arc::new(Rules(Vec::new())), Duration::MAX)
            .await
            .is_ok(),
        "an empty set is a set"
    );
    assert!(
        RoutingRegistry::load(Arc::new(Rules(vec![broken])), Duration::MAX)
            .await
            .is_err()
    );
}

/// A store that answers one fixed rule set, so a registry can be built
/// without a database. Its writes are never reached.
struct Rules(Vec<StoredRule>);

#[async_trait::async_trait]
impl RoutingRuleStore for Rules {
    async fn all(&self) -> Result<Vec<StoredRule>, crate::error::StoreError> {
        Ok(self.0.clone())
    }

    async fn by_name(&self, _n: &str) -> Result<Option<StoredRule>, crate::error::StoreError> {
        Ok(None)
    }

    async fn create(
        &self,
        _r: &crate::ports::routing::NewRule<'_>,
        _now: chrono::DateTime<chrono::Utc>,
    ) -> Result<StoredRule, crate::error::StoreError> {
        Err(crate::error::StoreError::Conflict)
    }

    async fn update(
        &self,
        _r: &crate::ports::routing::NewRule<'_>,
        _now: chrono::DateTime<chrono::Utc>,
    ) -> Result<StoredRule, crate::error::StoreError> {
        Err(crate::error::StoreError::NotFound)
    }

    async fn delete(&self, _n: &str) -> Result<(), crate::error::StoreError> {
        Err(crate::error::StoreError::NotFound)
    }

    async fn version(&self) -> Result<u64, crate::error::StoreError> {
        Ok(1)
    }

    async fn ensure_seeded(
        &self,
        _r: &[crate::ports::routing::NewRule<'_>],
        _now: chrono::DateTime<chrono::Utc>,
    ) -> Result<usize, crate::error::StoreError> {
        Ok(0)
    }
}

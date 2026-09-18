//! Resolving a name through a group: the walk's state machine and the two
//! newtypes that keep a requested repository apart from the member that
//! answered.
//!
//! What is here is the bookkeeping — member order, the dedup, the depth cap
//! and the verdict when nothing was found. Ordering the I/O that fills it is
//! the application's, which is why [`Walk`] never sees a port, a leaf or a
//! request context.

use super::Repository;

/// How many groups may nest before a read refuses. Write-time validation
/// already refuses a deeper stack, so a walk that reaches it is reading a row
/// that should never have been written.
pub const MAX_GROUP_DEPTH: u32 = 5;

/// The repository the client addressed: the only name in URLs it sees.
#[derive(Clone, Copy, Debug)]
pub struct UrlRepo<'a>(pub &'a str);

/// The member repository that owns cached bytes: the only name allowed in
/// cache keys, storage paths and `repository_id`.
#[derive(Clone, Copy, Debug)]
pub struct CacheRepo<'a>(pub &'a Repository);

/// Whether a lookup had the thing asked for.
#[derive(Debug)]
pub enum Outcome<T> {
    Found(T),
    NotFound,
}

/// What one member contributed to a walk.
///
/// A member that failed contributes the reason as text: the domain orders the
/// walk, and naming the failure is the application's, which is the only layer
/// that knows what a failure is made of.
#[derive(Debug)]
pub enum Visit<T> {
    Hit(T),
    Nothing,
    Failed(String),
}

/// Why a walk ended with no hit: some member was unreachable, or no member
/// had it. The application turns this into a status and names the repository
/// the client asked for — a name the walk never sees.
#[derive(Debug, PartialEq, Eq)]
pub enum Miss {
    Degraded(String),
    Nothing,
}

/// One pass over a group and its members, in member order.
pub struct Walk<T> {
    hits: Vec<T>,
    first_only: bool,
    seen: std::collections::HashSet<i64>,
    failure: Option<String>,
}

impl<T> Walk<T> {
    /// The root counts as visited, so a group listing itself is not a cycle
    /// the walk follows.
    pub fn new(root: &Repository, first_only: bool) -> Self {
        Self {
            hits: Vec::new(),
            first_only,
            seen: std::collections::HashSet::from([root.id]),
            failure: None,
        }
    }

    /// Whether the walk has what it was asked for and may stop.
    pub fn done(&self) -> bool {
        self.first_only && !self.hits.is_empty()
    }

    pub fn found(&self) -> bool {
        !self.hits.is_empty()
    }

    /// First failure wins: the caller is told what broke first, not last.
    pub fn record(&mut self, visit: Visit<T>) {
        match visit {
            Visit::Hit(hit) => self.hits.push(hit),
            Visit::Nothing => {}
            Visit::Failed(why) => {
                self.failure.get_or_insert(why);
            }
        }
    }

    /// Whether this member is new to the walk; a member reached twice through
    /// two groups is answered once.
    pub fn first_visit(&mut self, member: i64) -> bool {
        self.seen.insert(member)
    }

    pub fn miss(&self) -> Miss {
        match &self.failure {
            Some(why) => Miss::Degraded(why.clone()),
            None => Miss::Nothing,
        }
    }

    /// The hits in member order, and the reason the answer is partial.
    pub fn finish(self) -> (Vec<T>, Option<String>) {
        (self.hits, self.failure)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{RepoConfig, Visibility};

    fn repo(id: i64) -> Repository {
        Repository {
            id,
            name: format!("r{id}"),
            repo_type: "group".to_string(),
            format: "npm".to_string(),
            visibility: Visibility::Public,
            upstream_url: None,
            config: Some(RepoConfig::default()),
            created_at: chrono::DateTime::UNIX_EPOCH,
            updated_at: chrono::DateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_first_only_walk_stops_at_its_first_hit_and_a_collecting_one_does_not() {
        let mut first = Walk::new(&repo(1), true);
        assert!(!first.done());
        first.record(Visit::Nothing);
        assert!(!first.done(), "nothing found is not done");
        first.record(Visit::Hit("a"));
        assert!(first.done());

        let mut every = Walk::new(&repo(1), false);
        every.record(Visit::Hit("a"));
        every.record(Visit::Hit("b"));
        assert!(!every.done());
        assert_eq!(every.finish().0, vec!["a", "b"], "in member order");
    }

    #[test]
    fn the_first_failure_is_the_one_reported_and_hits_after_it_are_degraded() {
        let mut walk = Walk::new(&repo(1), false);
        walk.record(Visit::Failed("first broke".to_string()));
        walk.record(Visit::Failed("second broke".to_string()));
        assert_eq!(walk.miss(), Miss::Degraded("first broke".to_string()));
        walk.record(Visit::Hit("late"));
        assert!(walk.found());
        let (hits, degraded) = walk.finish();
        assert_eq!(hits, vec!["late"]);
        assert_eq!(degraded, Some("first broke".to_string()));
    }

    #[test]
    fn nothing_anywhere_is_not_a_degraded_group() {
        let mut walk = Walk::<&str>::new(&repo(1), true);
        walk.record(Visit::Nothing);
        assert_eq!(walk.miss(), Miss::Nothing);
        assert!(!walk.found());
    }

    #[test]
    fn the_root_is_already_seen_and_a_member_is_answered_once() {
        let mut walk = Walk::<&str>::new(&repo(1), true);
        assert!(!walk.first_visit(1), "the root is not walked twice");
        assert!(walk.first_visit(2));
        assert!(!walk.first_visit(2));
    }
}

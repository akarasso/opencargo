//! `Authorize`: what the caller may do here, decided in one place.
//!
//! The effective right is an intersection, computed at the request: what the
//! bearer holds at this instant, narrowed by what the credential allows. A
//! scope never adds, so nothing here can turn a refusal into an allowance —
//! including for the admin role, whose short circuit runs inside the ladder
//! and is narrowed like any other standing.
//!
//! The verdict distinguishes "nothing was presented" from "what was presented
//! is not enough": the first is the 401 that sends a real client back to fetch
//! credentials, and confusing them stops the OCI token dance dead.

use crate::app::authenticate::AuthUser;
use crate::domain::{
    effective_rights, narrow, RepoAction, Repository, Rights, Subject, TokenScope, Visibility,
};
use crate::error::{AppError, AppResult};
use crate::ports::permissions::PermissionStore;
use crate::ports::repositories::RepositoryStore;
use crate::registry::rules::rules_of;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    /// No credential at all: the adapter answers 401 with its own challenge.
    Unauthenticated,
    /// A valid credential whose bearer does not hold the right.
    Forbidden,
    /// A valid credential whose scope removed a right its bearer holds.
    OutOfScope,
    /// A store could not answer; never a permissive fallback.
    Unavailable,
}

pub struct Authorize<'a> {
    pub perms: &'a dyn PermissionStore,
    pub repos: &'a dyn RepositoryStore,
    /// With anonymous reads on, a scoped credential reads a public repository
    /// the way sending no credential at all would: no client knows how to
    /// stay silent on some repositories and not others, so the alternative
    /// breaks every CI on repositories a bare `curl` serves.
    pub anonymous_read: bool,
}

impl Authorize<'_> {
    /// What the caller may do to this repository, scope included, on a
    /// route that names no package: an enumeration, or content of the
    /// repository itself. A package line of a scope does not cover it.
    pub async fn repository(
        &self,
        caller: Option<&AuthUser>,
        repo: &Repository,
        action: RepoAction,
    ) -> Verdict {
        let scope = caller.map_or(&TokenScope::Inherit, |c| &c.scope);
        self.decide(caller, repo, None, action, scope).await
    }

    /// The same on a route that names a package: a package line covers it
    /// when its pattern reaches the name under the format's own key.
    pub async fn package(
        &self,
        caller: Option<&AuthUser>,
        repo: &Repository,
        package: &str,
        action: RepoAction,
    ) -> Verdict {
        let scope = caller.map_or(&TokenScope::Inherit, |c| &c.scope);
        self.decide(caller, repo, Some(package), action, scope).await
    }

    /// Inside a group's walk the scope is not evaluated: it was judged at the
    /// entry, on the repository the client named, where a single response can
    /// still carry its reason. What is refused here is a right, and the walk
    /// skips such a member silently, as it always has.
    pub async fn member(
        &self,
        caller: Option<&AuthUser>,
        repo: &Repository,
        action: RepoAction,
    ) -> Verdict {
        self.decide(caller, repo, None, action, &TokenScope::Inherit).await
    }

    async fn decide(
        &self,
        caller: Option<&AuthUser>,
        repo: &Repository,
        package: Option<&str>,
        action: RepoAction,
        scope: &TokenScope,
    ) -> Verdict {
        // A public repository is readable as it always was. A scope narrows
        // that only where sending no credential at all would not have served
        // it: with anonymous reads on, staying silent is a move every client
        // could make and none knows how to make per repository.
        if repo.visibility == Visibility::Public
            && action == RepoAction::Read
            && (scope.is_inherit() || self.anonymous_read)
        {
            return Verdict::Ok;
        }
        let Some(caller) = caller else {
            return Verdict::Unauthenticated;
        };
        let grant = match caller.user_id {
            Some(id) => match self.perms.rights(id, repo.id).await {
                Ok(grant) => grant,
                Err(e) => {
                    tracing::warn!(error = %e, "store error during permission check");
                    return Verdict::Unavailable;
                }
            },
            None => None,
        };
        let held = effective_rights(&caller.role, grant).0;
        if scope.is_inherit() {
            return verdict(held, held, action);
        }
        let Some(incarnation) = (match self.repos.incarnation(repo.id).await {
            Ok(found) => found,
            Err(e) => {
                tracing::warn!(error = %e, "store error while reading an incarnation");
                return Verdict::Unavailable;
            }
        }) else {
            // A repository with no incarnation is one no scope can name, and a
            // scope that cannot name it allows nothing on it.
            return Verdict::OutOfScope;
        };
        let subject = match package {
            None => Subject::Repository {
                incarnation: &incarnation,
            },
            Some(package) => match repo.fmt().ok().and_then(|format| rules_of(format).ok()) {
                Some(rules) => Subject::Package {
                    incarnation: &incarnation,
                    package,
                    rules,
                },
                // A name no format keys is one no package line can be
                // compared on, and a line that cannot be compared allows
                // nothing.
                None => return verdict(held, Rights::NONE, action),
            },
        };
        verdict(held, narrow(held, scope, &subject), action)
    }
}

fn verdict(held: Rights, effective: Rights, action: RepoAction) -> Verdict {
    if effective.allows(action) {
        Verdict::Ok
    } else if held.allows(action) {
        Verdict::OutOfScope
    } else {
        Verdict::Forbidden
    }
}

impl Verdict {
    /// The refusal an adapter answers with, or nothing when the verdict is
    /// `Ok`. `denied` is what the route says about a plain refusal of right;
    /// the other three answers are the same everywhere, because they are
    /// about the credential and not about the route.
    pub fn into_result(self, denied: impl FnOnce() -> String, unauthenticated: &str) -> AppResult<()> {
        match self {
            Verdict::Ok => Ok(()),
            Verdict::Unauthenticated => Err(AppError::Unauthorized(unauthenticated.to_string())),
            Verdict::Forbidden => Err(AppError::Forbidden(denied())),
            Verdict::OutOfScope => Err(AppError::InsufficientScope(
                "the presented token is not scoped to this repository or package".to_string(),
            )),
            Verdict::Unavailable => Err(AppError::ServiceUnavailable(
                "permission check temporarily unavailable, try again".to_string(),
            )),
        }
    }
}

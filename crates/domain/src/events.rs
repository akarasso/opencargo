//! What happened, as the registry says it, and who is allowed to hear it.
//!
//! The vocabulary is the registry's, never the wire's: a dotted name and the
//! values behind it. How that becomes JSON is the WebSocket adapter's job;
//! *who receives it* is neither the adapter's nor the bus's, because deciding
//! it needs the repository the event is about — see [`announce`].

use super::kinds::{Format, Visibility};

/// Who may receive an event. Ordered, and a subscriber receives everything at
/// or below its own level, which is the one rule the adapter still applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Audience {
    /// Safe for anonymous readers: public-repository activity.
    Public = 0,
    /// Any signed-in user; private-repository *hints*, never their names.
    Authenticated = 1,
    /// Admin role only: the audit trail and user, token and webhook changes.
    Admin = 2,
}

/// A version that has just been served for the first time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageRelease {
    pub package: String,
    pub version: String,
    pub repository: String,
    pub format: Format,
    pub published_by: String,
}

/// A version copied from one repository into another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackagePromotion {
    pub package: String,
    pub version: String,
    /// Where it landed, which is the repository the event is about.
    pub to: String,
    pub promoted_by: String,
    /// Where it came from — carried only when naming it leaks nothing.
    pub from: Option<String>,
}

/// One coalesced policy-resolution count per requested/member pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolutionCounts {
    pub repo: String,
    pub member: String,
    pub count: u64,
    pub would_block: u64,
    pub unknown: u64,
}

/// Everything the registry announces, one variant per dotted name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DomainEvent {
    PackagePublished(PackageRelease),
    PackagePromoted(PackagePromotion),
    /// The anonymized hint a private repository sends instead of a name.
    RegistryChanged { repository: String },
    RepositoriesChanged,
    PermissionsChanged { username: String },
    AuditEntry {
        username: String,
        action: String,
        target: Option<String>,
    },
    PolicyResolution(ResolutionCounts),
}

impl DomainEvent {
    /// The dotted name this event has always gone out under.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::PackagePublished(_) => "package.published",
            Self::PackagePromoted(_) => "package.promoted",
            Self::RegistryChanged { .. } => "registry.changed",
            Self::RepositoriesChanged => "repositories.changed",
            Self::PermissionsChanged { .. } => "permissions.changed",
            Self::AuditEntry { .. } => "audit.entry",
            Self::PolicyResolution(_) => "policy.resolution",
        }
    }
}

/// How a package event reaches the bus, given the visibility of the
/// repository it is about.
///
/// A public repository's activity is everyone's. A private one's is the
/// admins', plus an anonymized `registry.changed` so other signed-in users
/// refetch what they display without learning a package name they may not
/// read. Callers that could not establish the visibility pass
/// [`Visibility::Private`]: the quiet answer is the safe one.
pub fn announce(
    event: DomainEvent,
    repository: &str,
    vis: Visibility,
) -> Vec<(DomainEvent, Audience)> {
    if vis == Visibility::Public {
        return vec![(event, Audience::Public)];
    }
    vec![
        (event, Audience::Admin),
        (
            DomainEvent::RegistryChanged {
                repository: repository.to_string(),
            },
            Audience::Authenticated,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release() -> DomainEvent {
        DomainEvent::PackagePublished(PackageRelease {
            package: "@sec/hidden".to_string(),
            version: "1.0.0".to_string(),
            repository: "npm-secret".to_string(),
            format: Format::Npm,
            published_by: "alice".to_string(),
        })
    }

    #[test]
    fn a_public_repository_announces_once_to_everyone() {
        let sent = announce(release(), "npm-pub", Visibility::Public);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1, Audience::Public);
        assert_eq!(sent[0].0.kind(), "package.published");
    }

    /// The disclosure this function exists to prevent: the package name goes
    /// to admins, and everyone else gets the repository and nothing else.
    #[test]
    fn a_private_repository_names_the_package_only_to_admins() {
        let sent = announce(release(), "npm-secret", Visibility::Private);
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].1, Audience::Admin);
        assert_eq!(
            sent[1],
            (
                DomainEvent::RegistryChanged {
                    repository: "npm-secret".to_string()
                },
                Audience::Authenticated
            )
        );
    }

    #[test]
    fn a_subscriber_hears_everything_at_or_below_its_level() {
        assert!(Audience::Public < Audience::Authenticated);
        assert!(Audience::Authenticated < Audience::Admin);
    }
}

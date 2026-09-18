//! Who may do what to a repository.
//!
//! The ladder is a rule, not a query: an admin is unrestricted, an explicit
//! grant beats the role it belongs to in both directions, and a role default
//! answers when there is no grant. Looking the grant up is a port call the
//! layer above makes; deciding with it happens here.

/// The four things a caller may do to a repository. Anything else is not an
/// action this registry has, which is why reading one is fallible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoAction {
    Read,
    Write,
    Delete,
    Admin,
}

impl RepoAction {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "delete" => Some(Self::Delete),
            "admin" => Some(Self::Admin),
            _ => None,
        }
    }
}

/// What a caller may do, as a set: one grant's four columns, and equally the
/// answer the ladder produces for a caller that holds none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rights {
    pub read: bool,
    pub write: bool,
    pub delete: bool,
    pub admin: bool,
}

impl Rights {
    pub const NONE: Rights = Rights {
        read: false,
        write: false,
        delete: false,
        admin: false,
    };

    pub const FULL: Rights = Rights {
        read: true,
        write: true,
        delete: true,
        admin: true,
    };

    pub fn allows(self, action: RepoAction) -> bool {
        match action {
            RepoAction::Read => self.read,
            RepoAction::Write => self.write,
            RepoAction::Delete => self.delete,
            RepoAction::Admin => self.admin,
        }
    }
}

/// Which rung answered. `/api/v1/me/permissions` reports it so a caller can
/// tell a grant made for them from the default their role carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RightsSource {
    Admin,
    Grant,
    Role,
}

impl RightsSource {
    pub fn as_str(self) -> &'static str {
        match self {
            RightsSource::Admin => "admin",
            RightsSource::Grant => "grant",
            RightsSource::Role => "role",
        }
    }
}

const ADMIN: &str = "admin";

/// Whether a role is the unrestricted one.
pub fn can_admin(role: &str) -> bool {
    role == ADMIN
}

/// The ladder, with the rung it stopped on.
pub fn effective_rights(role: &str, grant: Option<Rights>) -> (Rights, RightsSource) {
    if can_admin(role) {
        return (Rights::FULL, RightsSource::Admin);
    }
    match grant {
        Some(rights) => (rights, RightsSource::Grant),
        None => (role_default(role), RightsSource::Role),
    }
}

fn role_default(role: &str) -> Rights {
    match role {
        "publisher" => Rights {
            read: true,
            write: true,
            ..Rights::NONE
        },
        "reader" => Rights {
            read: true,
            ..Rights::NONE
        },
        _ => Rights::NONE,
    }
}

/// Whether `role`, holding `grant`, may take the action named `action`.
///
/// An admin is unrestricted by an action this registry does not have either:
/// the role answers before the name is read, which is what keeps a future
/// verb from being denied to admins until someone adds it here.
pub fn allows(role: &str, grant: Option<Rights>, action: &str) -> bool {
    if can_admin(role) {
        return true;
    }
    match RepoAction::parse(action) {
        Some(action) => effective_rights(role, grant).0.allows(action),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DENY_ALL: Rights = Rights::NONE;

    #[test]
    fn the_admin_role_short_circuits_everything() {
        for action in ["read", "write", "delete", "admin", "frobnicate"] {
            assert!(
                allows("admin", None, action),
                "admin must be allowed {action:?}"
            );
            assert!(
                allows("admin", Some(DENY_ALL), action),
                "a deny-all grant cannot restrict an admin"
            );
        }
        assert_eq!(
            effective_rights("admin", Some(DENY_ALL)),
            (Rights::FULL, RightsSource::Admin)
        );
    }

    #[test]
    fn an_explicit_grant_beats_the_role_default_in_both_directions() {
        let grant = Rights {
            read: false,
            write: true,
            ..Rights::NONE
        };
        assert!(
            !allows("reader", Some(grant), "read"),
            "an explicit can_read=0 must beat the reader default"
        );
        assert!(
            allows("reader", Some(grant), "write"),
            "an explicit can_write=1 must beat the reader default"
        );
        assert!(!allows("reader", Some(grant), "delete"));
        assert!(!allows("reader", Some(grant), "admin"));
        assert_eq!(effective_rights("reader", Some(grant)).1, RightsSource::Grant);
    }

    #[test]
    fn role_defaults_apply_when_no_grant_does() {
        assert_eq!(
            effective_rights("publisher", None),
            (
                Rights {
                    read: true,
                    write: true,
                    ..Rights::NONE
                },
                RightsSource::Role
            )
        );
        assert_eq!(
            effective_rights("reader", None),
            (
                Rights {
                    read: true,
                    ..Rights::NONE
                },
                RightsSource::Role
            )
        );
        for unknown in ["ghost", "", "anonymous"] {
            assert_eq!(
                effective_rights(unknown, None),
                (Rights::NONE, RightsSource::Role),
                "{unknown:?} carries no default"
            );
        }
    }

    /// An action nobody defined is denied whether or not a grant allows
    /// everything: the name is read, not guessed at.
    #[test]
    fn an_unknown_action_is_denied_with_and_without_a_grant() {
        assert!(!allows("reader", Some(Rights::FULL), "frobnicate"));
        assert!(!allows("publisher", None, "frobnicate"));
        assert_eq!(RepoAction::parse("frobnicate"), None);
        assert_eq!(RepoAction::parse("read"), Some(RepoAction::Read));
    }

    #[test]
    fn a_source_names_the_rung_it_answered_from() {
        assert_eq!(RightsSource::Admin.as_str(), "admin");
        assert_eq!(RightsSource::Grant.as_str(), "grant");
        assert_eq!(RightsSource::Role.as_str(), "role");
    }
}

use std::fmt;

/// What an error is about: a kind of thing (`"repository"`, `"package"`) and
/// which one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub kind: &'static str,
    pub id: String,
}

impl fmt::Display for Resource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} '{}'", self.kind, self.id)
    }
}

/// What a caller tried to do to a resource: the subject of a refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub verb: &'static str,
    pub on: Resource,
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.verb, self.on)
    }
}

/// How registry semantics refuse. The layer above turns these into statuses;
/// the domain never knows a status exists.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    #[error("{0} not found")]
    NotFound(Resource),

    #[error("{0} already exists")]
    Conflict(Resource),

    #[error("not allowed to {0}")]
    Forbidden(Action),

    #[error("{0}")]
    InvalidName(String),

    /// A stored value the schema is supposed to constrain came back
    /// unreadable, e.g. a `repo_type` outside its CHECK list.
    #[error("repository '{repo}' has a corrupt {column} column: '{value}'")]
    CorruptColumn {
        repo: String,
        column: &'static str,
        value: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_name_the_subject() {
        let repo = Resource {
            kind: "repository",
            id: "npm-hosted".to_string(),
        };
        assert_eq!(
            DomainError::NotFound(repo.clone()).to_string(),
            "repository 'npm-hosted' not found"
        );
        assert_eq!(
            DomainError::Conflict(repo.clone()).to_string(),
            "repository 'npm-hosted' already exists"
        );
        assert_eq!(
            DomainError::Forbidden(Action {
                verb: "publish to",
                on: repo,
            })
            .to_string(),
            "not allowed to publish to repository 'npm-hosted'"
        );
    }

    #[test]
    fn corrupt_column_quotes_the_value_it_could_not_read() {
        let err = DomainError::CorruptColumn {
            repo: "r".to_string(),
            column: "repo_type",
            value: "bogus".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "repository 'r' has a corrupt repo_type column: 'bogus'"
        );
    }
}

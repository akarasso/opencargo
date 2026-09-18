use chrono::{DateTime, Utc};

/// Which events a webhook asked for: every one, or a named list.
///
/// The `"*"`-or-comma-separated spelling is the one the stored column, the
/// config file and the admin API all carry, so parsing and rendering it lives
/// here once rather than in each of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subscription {
    All,
    Only(Vec<String>),
}

impl Subscription {
    pub fn parse(raw: &str) -> Self {
        if raw == "*" {
            Subscription::All
        } else {
            Subscription::Only(raw.split(',').map(|name| name.trim().to_string()).collect())
        }
    }

    /// An empty list is "everything": what a config entry and an API request
    /// that names no event both mean.
    pub fn of_names(names: &[String]) -> Self {
        if names.is_empty() {
            Subscription::All
        } else {
            Subscription::Only(names.to_vec())
        }
    }

    pub fn matches(&self, event: &str) -> bool {
        match self {
            Subscription::All => true,
            Subscription::Only(names) => names.iter().any(|name| name == event),
        }
    }

    /// The list a reader sees, `["*"]` included.
    pub fn names(&self) -> Vec<String> {
        match self {
            Subscription::All => vec!["*".to_string()],
            Subscription::Only(names) => names.clone(),
        }
    }

    pub fn encoded(&self) -> String {
        match self {
            Subscription::All => "*".to_string(),
            Subscription::Only(names) => names.join(","),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Webhook {
    pub id: i64,
    pub url: String,
    pub events: Subscription,
    pub secret: Option<String>,
    pub active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The selection a dispatch makes, as a value: the same three cases the
    /// stored column used to be matched on inside a query.
    #[test]
    fn a_subscription_selects_the_events_it_names() {
        assert!(Subscription::parse("*").matches("package.published"));

        let some = Subscription::parse("package.published, package.promoted");
        assert!(some.matches("package.published"));
        assert!(some.matches("package.promoted"));
        assert!(!some.matches("user.delete"));

        assert!(!Subscription::parse("").matches("package.published"));
    }

    #[test]
    fn a_subscription_round_trips_through_its_stored_spelling() {
        for raw in ["*", "package.published", "package.published,user.delete"] {
            assert_eq!(Subscription::parse(raw).encoded(), raw);
        }
        assert_eq!(Subscription::parse("a, b").encoded(), "a,b");
    }

    /// Naming no event at all is a subscription to everything, whether it
    /// comes from the config file or from an admin request.
    #[test]
    fn naming_no_event_subscribes_to_everything() {
        assert_eq!(Subscription::of_names(&[]), Subscription::All);
        assert_eq!(Subscription::of_names(&[]).names(), vec!["*".to_string()]);
        assert_eq!(
            Subscription::of_names(&["package.published".to_string()]),
            Subscription::Only(vec!["package.published".to_string()])
        );
    }
}

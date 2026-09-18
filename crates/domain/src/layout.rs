//! Where a repository's bytes live, as keys: pure functions of opaque ids.
//! Every new key of a repository lies under its incarnation's prefix, so a
//! retired name and its recreation share nothing.

/// The prefix every new key of an incarnation lies under.
pub fn incarnation_prefix(incarnation: &str) -> String {
    format!("r/{incarnation}")
}

/// The name-keyed prefixes a repository's bytes were written under before
/// incarnations: its format's tree and its proxy cache.
pub fn name_keyed_prefixes(format: &str, name: &str) -> Vec<String> {
    vec![format!("{format}/{name}"), format!("_proxy_cache/{name}")]
}

/// A logical key placed at one generation: the only key a row records.
pub fn physical_key(logical: &str, generation: &str) -> String {
    format!("{logical}~{generation}")
}

/// Whether `key` is `prefix` itself or lies under it, segment by segment:
/// `r/ab` never contains `r/abc`.
pub fn under(key: &str, prefix: &str) -> bool {
    key == prefix
        || key
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_compare_as_segments() {
        assert!(under("r/ab/x", "r/ab"));
        assert!(under("r/ab", "r/ab"));
        assert!(!under("r/abc/x", "r/ab"), "a string-prefix sibling is another prefix");
    }

    #[test]
    fn a_physical_key_names_its_logical_key_and_generation() {
        assert_eq!(physical_key("r/i/p/f", "g1"), "r/i/p/f~g1");
        assert_eq!(incarnation_prefix("i"), "r/i");
        assert_eq!(
            name_keyed_prefixes("npm", "web"),
            vec!["npm/web".to_string(), "_proxy_cache/web".to_string()]
        );
    }
}

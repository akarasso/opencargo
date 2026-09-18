pub mod auth_rules;
pub mod download;
pub mod index;
pub mod leaves;
pub mod publish;
pub mod routes;
pub mod upstream;

/// The sparse-index prefix cargo derives from a lowercased crate name:
/// `1`, `2`, `3/{first}` or `{first_two}/{next_two}`.
pub fn compute_prefix(name: &str) -> String {
    prefix_of(&name.to_lowercase())
}

/// The same prefix with the name's case kept (cargo's `{prefix}` dl marker).
pub fn prefix_of(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    match chars.len() {
        1 => "1".to_string(),
        2 => "2".to_string(),
        3 => format!("3/{}", chars[0]),
        _ => format!("{}{}/{}{}", chars[0], chars[1], chars[2], chars[3]),
    }
}

/// A string field of one JSON index line, `None` for anything else.
pub fn line_field(line: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()?
        .get(key)?
        .as_str()
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes() {
        assert_eq!(compute_prefix("a"), "1");
        assert_eq!(compute_prefix("ab"), "2");
        assert_eq!(compute_prefix("Abc"), "3/a");
        assert_eq!(compute_prefix("Serde"), "se/rd");
        assert_eq!(prefix_of("Serde"), "Se/rd");
        assert_eq!(compute_prefix("éàç"), "3/é");
    }

    #[test]
    fn line_fields() {
        assert_eq!(
            line_field(r#"{"vers":"1.0.0"}"#, "vers").as_deref(),
            Some("1.0.0")
        );
        assert_eq!(line_field(r#"{"yanked":true}"#, "yanked"), None);
        assert_eq!(line_field("junk", "vers"), None);
    }
}

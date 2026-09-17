use serde_json::Value;

/// Dependency name/version pairs of a published version: npm and cargo store
/// JSON metadata, Go stores the raw go.mod.
pub fn extract_dependencies(metadata: &str, ecosystem: &str) -> Vec<(String, String)> {
    if ecosystem == "Go" {
        return parse_go_mod(metadata);
    }
    let Ok(meta) = serde_json::from_str::<Value>(metadata) else {
        return Vec::new();
    };
    match ecosystem {
        "npm" => npm_dependencies(&meta),
        "crates.io" => cargo_dependencies(&meta),
        _ => Vec::new(),
    }
}

/// npm metadata stores dependencies as `{"name": "version_req"}` maps.
fn npm_dependencies(meta: &Value) -> Vec<(String, String)> {
    const FIELDS: [&str; 4] = [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ];
    FIELDS
        .iter()
        .filter_map(|field| meta.get(*field).and_then(|v| v.as_object()))
        .flatten()
        .filter_map(|(name, version)| {
            let clean = clean_version_string(version.as_str().unwrap_or("*"));
            (!clean.is_empty()).then(|| (name.clone(), clean))
        })
        .collect()
}

/// Cargo metadata stores deps as an array of `{"name", "version_req"}` objects.
fn cargo_dependencies(meta: &Value) -> Vec<(String, String)> {
    meta.get("deps")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|dep| {
            let name = dep.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let req = dep
                .get("version_req")
                .and_then(|v| v.as_str())
                .unwrap_or("*");
            let clean = clean_version_string(req);
            (!name.is_empty() && !clean.is_empty()).then(|| (name.to_string(), clean))
        })
        .collect()
}

/// `require` directives of a go.mod, single-line and parenthesised blocks;
/// `replace`/`exclude`/`retract` blocks are skipped.
fn parse_go_mod(go_mod: &str) -> Vec<(String, String)> {
    let mut deps = Vec::new();
    let mut block: Option<&str> = None;
    for raw in go_mod.lines() {
        let line = raw.split("//").next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        match block {
            Some(_) if line == ")" => block = None,
            Some("require") => deps.extend(go_requirement(line)),
            Some(_) => {}
            None => match line.split_once(char::is_whitespace) {
                Some((directive, "(")) => block = Some(directive),
                Some(("require", rest)) => deps.extend(go_requirement(rest.trim())),
                _ => {}
            },
        }
    }
    deps
}

fn go_requirement(line: &str) -> Option<(String, String)> {
    let mut parts = line.split_whitespace();
    let (path, version) = (parts.next()?, parts.next()?);
    version
        .starts_with('v')
        .then(|| (path.to_string(), version.to_string()))
}

/// Clean a version string by removing common range prefixes.
/// OSV.dev needs exact versions, not ranges.
fn clean_version_string(version: &str) -> String {
    let v = version.trim();
    // Strip ^, ~, >=, <=, >, <, = prefixes
    let v = v.trim_start_matches('^');
    let v = v.trim_start_matches('~');
    let v = v.trim_start_matches(">=");
    let v = v.trim_start_matches("<=");
    let v = v.trim_start_matches('>');
    let v = v.trim_start_matches('<');
    let v = v.trim_start_matches('=');
    let v = v.trim();

    // Skip wildcards and complex ranges
    if v == "*" || v.contains("||") || v.contains(' ') {
        return String::new();
    }

    v.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_npm_dependencies() {
        let meta = r#"{
            "name": "test-pkg",
            "version": "1.0.0",
            "dependencies": {
                "lodash": "^4.17.20",
                "axios": "~0.21.0"
            },
            "devDependencies": {
                "jest": "^27.0.0"
            }
        }"#;

        let deps = extract_dependencies(meta, "npm");
        assert_eq!(deps.len(), 3);
        assert!(deps.iter().any(|(n, v)| n == "lodash" && v == "4.17.20"));
        assert!(deps.iter().any(|(n, v)| n == "axios" && v == "0.21.0"));
        assert!(deps.iter().any(|(n, v)| n == "jest" && v == "27.0.0"));
    }

    #[test]
    fn test_extract_cargo_dependencies() {
        let meta = r#"{
            "name": "my-crate",
            "vers": "0.1.0",
            "deps": [
                {"name": "serde", "version_req": "^1.0"},
                {"name": "tokio", "version_req": ">=1.0"}
            ]
        }"#;

        let deps = extract_dependencies(meta, "crates.io");
        assert_eq!(deps.len(), 2);
        assert!(deps.iter().any(|(n, v)| n == "serde" && v == "1.0"));
        assert!(deps.iter().any(|(n, v)| n == "tokio" && v == "1.0"));
    }

    #[test]
    fn test_extract_go_mod_require_lines_and_blocks() {
        let go_mod = "module example.com/app\n\ngo 1.22\n\n\
            require github.com/one/single v1.0.0\n\n\
            require (\n\tgithub.com/two/block v2.1.0 // indirect\n\t// a comment line\n\
            \tgolang.org/x/text v0.3.7\n)\n\n\
            replace (\n\tgithub.com/two/block => ../local\n)\n\
            exclude github.com/bad/one v0.9.0\n";
        let deps = extract_dependencies(go_mod, "Go");
        assert_eq!(
            deps,
            vec![
                ("github.com/one/single".to_string(), "v1.0.0".to_string()),
                ("github.com/two/block".to_string(), "v2.1.0".to_string()),
                ("golang.org/x/text".to_string(), "v0.3.7".to_string()),
            ]
        );
    }

    #[test]
    fn test_clean_version_string() {
        assert_eq!(clean_version_string("^4.17.20"), "4.17.20");
        assert_eq!(clean_version_string("~0.21.0"), "0.21.0");
        assert_eq!(clean_version_string(">=1.0.0"), "1.0.0");
        assert_eq!(clean_version_string("*"), "");
        assert_eq!(clean_version_string("1.0.0 || 2.0.0"), "");
    }

    #[test]
    fn test_extract_no_deps() {
        let meta = r#"{"name": "empty", "version": "1.0.0"}"#;
        let deps = extract_dependencies(meta, "npm");
        assert!(deps.is_empty());
        assert!(extract_dependencies("module x\n\ngo 1.22\n", "Go").is_empty());
    }
}

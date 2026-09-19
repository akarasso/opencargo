use serde_json::Value;

use crate::domain::Format;
use crate::registry::pypi::names::normalize;

/// Dependency name/version pairs of a published version. Exhaustive on
/// `Format`: a format that declares itself scannable reads its own document,
/// and one that does not says so in its row rather than falling through a
/// default arm that answers "no dependencies". `Err` names why a document
/// cannot say what it depends on.
pub fn extract_dependencies(metadata: &str, format: Format) -> Result<Vec<(String, String)>, String> {
    if matches!(format, Format::Go) {
        return Ok(parse_go_mod(metadata));
    }
    let Ok(meta) = serde_json::from_str::<Value>(metadata) else {
        return Ok(Vec::new());
    };
    Ok(match format {
        Format::Npm => npm_dependencies(&meta),
        Format::Cargo => cargo_dependencies(&meta),
        Format::Maven => maven_dependencies(&meta),
        Format::Nuget => nuget_dependencies(&meta),
        Format::Pypi => pypi_dependencies(&meta)?,
        Format::Go => unreachable!("read above, before the document is parsed as JSON"),
        Format::Oci | Format::Mcp | Format::Raw => Vec::new(),
    })
}

/// Core metadata's `Requires-Dist` lines (PEP 508), each read as its
/// lower bound, the PEP 503 name; a marker still names a dependency, a URL
/// requirement names no version. A sdist whose `Requires-Dist` is
/// `Dynamic` (PEP 643) carries no dependency set at all.
fn pypi_dependencies(meta: &Value) -> Result<Vec<(String, String)>, String> {
    let dynamic = meta
        .get("dynamic")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .any(|field| field.eq_ignore_ascii_case("requires-dist"));
    if dynamic {
        return Err("the archive declares Requires-Dist dynamic: its dependencies are only known at build time".to_string());
    }
    let mut deps: Vec<(String, String)> = Vec::new();
    for req in meta
        .get("requires_dist")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
    {
        let Some((name, version)) = pypi_requirement(req) else {
            continue;
        };
        if !deps.iter().any(|(n, v)| *n == name && *v == version) {
            deps.push((name, version));
        }
    }
    Ok(deps)
}

fn pypi_requirement(req: &str) -> Option<(String, String)> {
    let spec = req.split(';').next()?.trim();
    if spec.contains('@') {
        return None;
    }
    let name_end = spec
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
        .unwrap_or(spec.len());
    let (name, rest) = spec.split_at(name_end);
    if name.is_empty() {
        return None;
    }
    let rest = match rest.trim_start().strip_prefix('[') {
        Some(after) => after.split_once(']')?.1,
        None => rest,
    };
    let specifiers = rest.trim().trim_start_matches('(').trim_end_matches(')');
    let clauses: Vec<&str> = specifiers.split(',').map(str::trim).filter(|c| !c.is_empty()).collect();
    let bound = |ops: &[&str]| {
        clauses.iter().find_map(|c| {
            ops.iter()
                .find_map(|op| c.strip_prefix(op))
                .map(|v| v.trim().trim_end_matches(".*").to_string())
        })
    };
    let version = bound(&["===", "=="]).or_else(|| bound(&["~=", ">=", ">"]))?;
    (!version.is_empty()).then(|| (normalize(name), version))
}

/// The POM's `groupId:artifactId` to requirement map, a range read as its
/// lower bound; an unresolved property names no version.
fn maven_dependencies(meta: &Value) -> Vec<(String, String)> {
    meta.get("dependencies")
        .and_then(|v| v.as_object())
        .into_iter()
        .flatten()
        .filter_map(|(name, req)| {
            let req = req.as_str().unwrap_or("*");
            if req.contains("${") {
                return None;
            }
            let lower = req
                .trim_start_matches(['[', '('])
                .split(',')
                .next()
                .unwrap_or("")
                .trim_end_matches([']', ')']);
            let clean = clean_version_string(lower);
            (!clean.is_empty()).then(|| (name.clone(), clean))
        })
        .collect()
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

/// The nuspec's dependency groups, each range read as its lower bound.
fn nuget_dependencies(meta: &Value) -> Vec<(String, String)> {
    let groups = meta
        .pointer("/nuspec/dependency_groups")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten();
    let mut deps: Vec<(String, String)> = Vec::new();
    for dep in groups
        .filter_map(|g| g.get("dependencies").and_then(|d| d.as_array()))
        .flatten()
    {
        let name = dep.get("id").and_then(|n| n.as_str()).unwrap_or("");
        let range = dep.get("range").and_then(|r| r.as_str()).unwrap_or("");
        let lower = range
            .trim_start_matches(['[', '('])
            .split(',')
            .next()
            .unwrap_or("")
            .trim_end_matches([']', ')']);
        let clean = clean_version_string(lower);
        if !name.is_empty() && !clean.is_empty() && !deps.iter().any(|(n, v)| n == name && *v == clean) {
            deps.push((name.to_string(), clean));
        }
    }
    deps
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
    fn nuget_dependencies_are_the_lower_bounds_of_every_group() {
        let meta = r#"{"nuspec": {"dependency_groups": [
            {"target_framework": "net8.0", "dependencies": [{"id": "Newtonsoft.Json", "range": "[13.0.1, )"}]},
            {"target_framework": "netstandard2.0", "dependencies": [{"id": "Newtonsoft.Json", "range": "[13.0.1, )"}, {"id": "A", "range": "1.2.3"}, {"id": "B"}]}
        ]}}"#;
        assert_eq!(
            extract_dependencies(meta, Format::Nuget).unwrap(),
            vec![
                ("Newtonsoft.Json".to_string(), "13.0.1".to_string()),
                ("A".to_string(), "1.2.3".to_string())
            ]
        );
    }

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

        let deps = extract_dependencies(meta, Format::Npm).unwrap();
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

        let deps = extract_dependencies(meta, Format::Cargo).unwrap();
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
        let deps = extract_dependencies(go_mod, Format::Go).unwrap();
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
    fn maven_dependencies_are_the_pom_map() {
        let meta = r#"{"groupId": "g", "dependencies": {"com.x:y": "2.1", "a:b": "[1.0,2.0)", "c:d": "${lib.version}", "e:f": "*"}}"#;
        let mut deps = extract_dependencies(meta, Format::Maven).unwrap();
        deps.sort();
        assert_eq!(
            deps,
            vec![
                ("a:b".to_string(), "1.0".to_string()),
                ("com.x:y".to_string(), "2.1".to_string())
            ],
            "a range is its lower bound, a property and a wildcard name no version"
        );
    }

    #[test]
    fn pypi_requires_dist_reads_lower_bounds_and_pep_503_names() {
        let meta = r#"{"name": "demo", "requires_dist": [
            "requests (>=2.31)",
            "Django_Rest.Framework>=3.14,<4; python_version >= \"3.8\"",
            "cryptography[ssh]==41.0.*",
            "idna",
            "numpy!=1.25.0",
            "tool @ https://example.com/tool.zip",
            "pyyaml~=6.0",
            "requests (>=2.31)"
        ]}"#;
        assert_eq!(
            extract_dependencies(meta, Format::Pypi).unwrap(),
            vec![
                ("requests".to_string(), "2.31".to_string()),
                ("django-rest-framework".to_string(), "3.14".to_string()),
                ("cryptography".to_string(), "41.0".to_string()),
                ("pyyaml".to_string(), "6.0".to_string()),
            ]
        );
        assert!(extract_dependencies(r#"{"name": "bare"}"#, Format::Pypi).unwrap().is_empty());
    }

    #[test]
    fn pypi_dynamic_requires_dist_is_unscannable_not_clean() {
        let meta = r#"{"name": "demo", "requires_dist": [], "dynamic": ["Requires-Dist", "Requires-Python"]}"#;
        let why = extract_dependencies(meta, Format::Pypi).unwrap_err();
        assert!(why.contains("Requires-Dist dynamic"), "{why}");
        let other = r#"{"name": "demo", "requires_dist": ["idna==3.4"], "dynamic": ["Description"]}"#;
        assert_eq!(
            extract_dependencies(other, Format::Pypi).unwrap(),
            vec![("idna".to_string(), "3.4".to_string())]
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
        let deps = extract_dependencies(meta, Format::Npm).unwrap();
        assert!(deps.is_empty());
        assert!(extract_dependencies("module x\n\ngo 1.22\n", Format::Go).unwrap().is_empty());
    }
}

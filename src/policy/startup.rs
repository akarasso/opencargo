use crate::config::Config;
use crate::domain::{Format, RepoKind};

use super::rules::{install_scripts, min_release_age, typosquat, PolicyConfig};

/// What `build_state` says about `[policy.*]` before serving: `recording`
/// names the members whose downloads are recorded, `unknown` the keys
/// naming no configured repository, `inapplicable` the members with a
/// rule that can only answer `not_applicable` on their format.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StartupNotes {
    pub recording: Vec<String>,
    pub unknown: Vec<String>,
    pub inapplicable: Vec<(String, &'static str)>,
}

/// Pure over the config; `Err` names a hosted or group key.
pub fn startup_notes(config: &Config) -> Result<StartupNotes, String> {
    let mut keys: Vec<&String> = config.policy.keys().collect();
    keys.sort();
    let mut notes = StartupNotes::default();
    for key in keys {
        let cfg = &config.policy[key];
        let Some(repo) = config.repositories.iter().find(|r| &r.name == key) else {
            notes.unknown.push(key.clone());
            continue;
        };
        if repo.repo_type != RepoKind::Proxy {
            return Err(format!(
                "[policy.{key}] names a {} repository; rules apply to proxy members only",
                repo.repo_type.as_str()
            ));
        }
        if cfg.is_empty() {
            continue;
        }
        notes.recording.push(key.clone());
        for rule in inapplicable(repo.format, cfg) {
            notes.inapplicable.push((key.clone(), rule));
        }
    }
    Ok(notes)
}

/// The enabled rules that can only answer `not_applicable` on `format`,
/// from what each rule says it can evaluate.
pub fn inapplicable(format: Format, cfg: &PolicyConfig) -> Vec<&'static str> {
    let mcp = format == Format::Mcp;
    [
        (cfg.typosquat && !typosquat::has_lists(format), "typosquat"),
        (cfg.osv_severity.is_some() && format.osv_ecosystem().is_none(), "osv_severity"),
        (cfg.min_release_age.is_some() && min_release_age::undated(format), "min_release_age"),
        (cfg.install_scripts && !install_scripts::applies(format), "install_scripts"),
        (cfg.mcp_allowlist && !mcp, "mcp_allowlist"),
        (cfg.mcp_injection && !mcp, "mcp_injection"),
        (cfg.mcp_transport && !mcp, "mcp_transport"),
        (cfg.mcp_drift && !mcp, "mcp_drift"),
    ]
    .into_iter()
    .filter_map(|(on, rule)| on.then_some(rule))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPOS: &str = r#"
[[repositories]]
name = "npm-proxy"
type = "proxy"
format = "npm"
upstream = "https://registry.npmjs.org"

[[repositories]]
name = "oci-proxy"
type = "proxy"
format = "oci"
upstream = "https://docker.io"

[[repositories]]
name = "npm-hosted"
type = "hosted"
format = "npm"

[[repositories]]
name = "npm-all"
type = "group"
format = "npm"
members = ["npm-hosted", "npm-proxy"]
"#;

    #[test]
    fn notes_name_recording_and_inapplicable_members() {
        let config: Config = toml::from_str(&format!(
            r#"{REPOS}
[policy.npm-proxy]
min_release_age = "48h"

[policy.oci-proxy]
typosquat = true
osv_severity = "high"

[policy.later]
install_scripts = true

[policy.quiet-proxy]
"#
        ))
        .unwrap();
        let notes = startup_notes(&config).unwrap();
        assert_eq!(notes.recording, vec!["npm-proxy", "oci-proxy"]);
        assert_eq!(notes.unknown, vec!["later", "quiet-proxy"]);
        assert_eq!(
            notes.inapplicable,
            vec![
                ("oci-proxy".to_string(), "typosquat"),
                ("oci-proxy".to_string(), "osv_severity")
            ]
        );
        let empty: Config = toml::from_str(&format!("{REPOS}\n[policy.npm-proxy]\n")).unwrap();
        assert_eq!(startup_notes(&empty).unwrap(), StartupNotes::default());
    }

    /// Maven: the ecosystem is scanned, nothing else can fire, and the
    /// operator hears it before the first row instead of never.
    #[test]
    fn maven_rules_that_cannot_fire_are_named_at_startup() {
        let config: Config = toml::from_str(&format!(
            r#"{REPOS}
[[repositories]]
name = "maven-proxy"
type = "proxy"
format = "maven"
upstream = "https://repo1.maven.org/maven2"

[policy.maven-proxy]
min_release_age = "48h"
osv_severity = "high"
install_scripts = true
typosquat = true
"#
        ))
        .unwrap();
        let notes = startup_notes(&config).unwrap();
        assert_eq!(notes.recording, vec!["maven-proxy"]);
        assert_eq!(
            notes.inapplicable,
            vec![
                ("maven-proxy".to_string(), "typosquat"),
                ("maven-proxy".to_string(), "min_release_age"),
                ("maven-proxy".to_string(), "install_scripts")
            ]
        );
        let all_on = PolicyConfig {
            min_release_age: Some("1h".parse().unwrap()),
            osv_severity: Some(crate::domain::Severity::High),
            install_scripts: true,
            typosquat: true,
            ..Default::default()
        };
        assert!(inapplicable(Format::Npm, &all_on).is_empty());
        assert_eq!(inapplicable(Format::Nuget, &all_on), ["typosquat"]);
        assert_eq!(inapplicable(Format::Raw, &all_on), ["typosquat", "osv_severity", "install_scripts"]);
    }

    #[test]
    fn group_key_is_err() {
        for (key, kind) in [("npm-all", "group"), ("npm-hosted", "hosted")] {
            let config: Config =
                toml::from_str(&format!("{REPOS}\n[policy.{key}]\ntyposquat = true\n")).unwrap();
            let err = startup_notes(&config).unwrap_err();
            assert!(err.contains(key) && err.contains(kind), "{err}");
        }
    }
}

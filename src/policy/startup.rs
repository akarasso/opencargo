use crate::config::Config;
use crate::db::kinds::{Format, RepoKind};

/// What `build_state` says about `[policy.*]` before serving: `recording`
/// names the members whose downloads are recorded, `unknown` the keys
/// naming no configured repository, `inapplicable` the OCI members with a
/// rule that can only answer `not_applicable` there.
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
        if repo.format == Format::Oci {
            if cfg.typosquat {
                notes.inapplicable.push((key.clone(), "typosquat"));
            }
            if cfg.osv_severity.is_some() {
                notes.inapplicable.push((key.clone(), "osv_severity"));
            }
        }
    }
    Ok(notes)
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

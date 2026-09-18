//! The deployment manifests as a cluster would receive them: the chart
//! through a real `helm template`, the kustomize base as written. Relations
//! are asserted on rendered text, never on a template.

use std::path::{Path, PathBuf};

pub fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// A config block the way a real install passes one: its own URL, bind and
/// database, replacing the chart's default string wholesale.
pub const OVERRIDE: &str = "[server]\nbind = \"0.0.0.0:6789\"\nbase_url = \"https://registry.corp.example\"\nstorage_path = \"/data/storage\"\n\n[database]\nurl = \"sqlite:/data/db/opencargo.db?mode=rwc\"\n";

/// The chart rendered at its default config and under [`OVERRIDE`], `None`
/// when helm is absent (a failure under `OPENCARGO_E2E_REQUIRE=1`).
pub fn helm_renders(extra: &[&str]) -> Option<Vec<String>> {
    let helm = super::client_bin("HELM_BIN")?;
    let tmp = tempfile::TempDir::new().unwrap();
    let override_file = tmp.path().join("config.toml");
    std::fs::write(&override_file, OVERRIDE).unwrap();
    let set_file = format!("config={}", override_file.display());
    let mut renders = Vec::new();
    for with_override in [false, true] {
        let mut args = vec!["template", "r", "helm/opencargo", "--set", "auth.adminPassword=x"];
        if with_override {
            args.extend(["--set-file", set_file.as_str()]);
        }
        args.extend(extra);
        let out = std::process::Command::new(&helm)
            .args(&args)
            .current_dir(repo())
            .output()
            .expect("helm runs");
        assert!(out.status.success(), "helm template: {}", String::from_utf8_lossy(&out.stderr));
        renders.push(String::from_utf8(out.stdout).unwrap());
    }
    Some(renders)
}

pub fn kustomize_deployment() -> String {
    std::fs::read_to_string(repo().join("k8s/base/deployment.yaml")).unwrap()
}

pub fn read(path: &Path) -> String {
    std::fs::read_to_string(repo().join(path)).unwrap()
}

/// The YAML document of `kind` in a multi-document render.
pub fn document(render: &str, kind: &str) -> Option<String> {
    render
        .split("\n---")
        .find(|doc| doc.lines().any(|l| l.trim() == format!("kind: {kind}")))
        .map(str::to_string)
}

/// The lines nested under the first `key:` line, dedented to that key.
pub fn block(doc: &str, key: &str) -> String {
    let lines: Vec<&str> = doc.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.trim_start().trim_start_matches("- ") == format!("{key}:"))
        .unwrap_or_else(|| panic!("no {key}: in\n{doc}"));
    let indent = indent_of(lines[start]);
    lines[start + 1..]
        .iter()
        .take_while(|l| l.trim().is_empty() || indent_of(l) > indent)
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The scalar of the first `key: value` line in `doc`.
pub fn scalar(doc: &str, key: &str) -> Option<String> {
    doc.lines().find_map(|l| {
        let l = l.trim_start().trim_start_matches("- ");
        l.strip_prefix(&format!("{key}:"))
            .map(|v| v.trim().trim_matches('"').to_string())
            .filter(|v| !v.is_empty())
    })
}

pub fn number(doc: &str, key: &str) -> u64 {
    scalar(doc, key)
        .unwrap_or_else(|| panic!("no {key} in\n{doc}"))
        .parse()
        .unwrap_or_else(|_| panic!("{key} is not a number in\n{doc}"))
}

/// The items of a block sequence.
pub fn items(block: &str) -> Vec<String> {
    block
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("- "))
        .map(|v| v.trim().trim_matches('"').to_string())
        .collect()
}

/// The value of container env var `name`.
pub fn env_value(doc: &str, name: &str) -> Option<String> {
    let lines: Vec<&str> = doc.lines().collect();
    let at = lines.iter().position(|l| l.trim() == format!("- name: {name}"))?;
    lines[at + 1..]
        .iter()
        .take(2)
        .find_map(|l| l.trim().strip_prefix("value:"))
        .map(|v| v.trim().trim_matches('"').to_string())
}

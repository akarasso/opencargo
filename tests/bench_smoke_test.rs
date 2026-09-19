//! The measurement harness runs one tiny scenario here, so that a change to
//! the server, the wire recipes or the report shape breaks a test instead of
//! rotting silently until someone runs `make bench`.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn missing_tool() -> Option<&'static str> {
    for tool in ["jq", "python3", "curl"] {
        if Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {tool}"))
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            return Some(tool);
        }
    }
    None
}

fn number(value: &Value, key: &str) -> f64 {
    value
        .get(key)
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("{key} missing or not a number in {value}"))
}

fn run(out: &Path) -> Value {
    let output = Command::new(root().join("scripts/bench.sh"))
        .current_dir(root())
        .args([
            "--smoke",
            "--binary",
            env!("CARGO_BIN_EXE_opencargo"),
            "--publishes",
            "2",
            "--out",
        ])
        .arg(out)
        .output()
        .expect("failed to run scripts/bench.sh");
    assert!(
        output.status.success(),
        "bench.sh exited {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let run_dir = std::fs::read_dir(out)
        .expect("no output directory")
        .map(|e| e.expect("unreadable entry").path())
        .next()
        .expect("the run wrote no directory");
    for name in ["results.json", "results.csv", "report.md"] {
        assert!(run_dir.join(name).is_file(), "{name} was not written");
    }

    let csv = std::fs::read_to_string(run_dir.join("results.csv")).unwrap();
    assert!(csv.starts_with("\"name\",\"storage\",\"status\""), "csv header: {csv:.60}");
    let report = std::fs::read_to_string(run_dir.join("report.md")).unwrap();
    assert!(report.contains("| scenario |"), "report.md has no table:\n{report}");
    assert!(report.contains("peak RSS"), "report.md has no peak RSS column");

    let results: Value =
        serde_json::from_str(&std::fs::read_to_string(run_dir.join("results.json")).unwrap())
            .expect("results.json is not JSON");
    let rows = csv.lines().count() - 1;
    assert_eq!(
        rows,
        results["scenarios"].as_array().unwrap().len(),
        "the csv and the json disagree on how many scenarios ran"
    );
    results
}

#[test]
fn the_harness_writes_a_well_formed_run() {
    if let Some(tool) = missing_tool() {
        if std::env::var("OPENCARGO_E2E_REQUIRE").is_err() {
            eprintln!("skipping: {tool} is not installed");
            return;
        }
        panic!("{tool} is required with OPENCARGO_E2E_REQUIRE set");
    }
    let out = tempfile::tempdir().expect("tempdir");
    let results = run(out.path());

    assert_eq!(results["schema"], 1);
    assert_eq!(
        results["commit"].as_str().unwrap_or_default().len(),
        40,
        "the run must name the exact commit"
    );
    assert!(results["version"]
        .as_str()
        .unwrap_or_default()
        .starts_with("opencargo "));
    assert!(number(&results["binary"], "size_bytes") > 0.0);
    assert!(number(&results["machine"], "cpu_threads") >= 1.0);
    assert!(number(&results["machine"], "mem_total_bytes") > 0.0);
    assert!(!results["machine"]["kernel"].as_str().unwrap().is_empty());
    assert_eq!(results["settings"]["smoke"], true);

    let scenarios = results["scenarios"].as_array().expect("scenarios array");
    assert_eq!(scenarios.len(), 2, "smoke runs idle and one npm publish: {scenarios:?}");

    let idle = &scenarios[0];
    assert_eq!(idle["name"], "idle");
    assert_eq!(idle["status"], "ok", "{idle}");
    assert!(number(idle, "rss_peak_bytes") > 1_000_000.0, "an idle server has an RSS");
    assert!(number(idle, "rss_steady_bytes") > 0.0, "the settle window produced no sample");
    assert!(number(idle, "samples") > 1.0);
    assert!(number(idle, "cpu_avg_pct") >= 0.0);
    assert!(number(idle, "db_bytes") > 0.0, "the database was created");

    let publish = &scenarios[1];
    assert_eq!(publish["name"], "publish-npm");
    assert_eq!(publish["status"], "ok", "{publish}");
    assert_eq!(number(publish, "requests"), 2.0);
    assert_eq!(number(publish, "errors"), 0.0, "a publish was refused: {publish}");
    assert!(number(publish, "p50_ms") > 0.0);
    assert!(number(publish, "p95_ms") >= number(publish, "p50_ms"));
    assert!(number(publish, "wall_ms") > 0.0);
    assert!(
        number(publish, "storage_bytes") > number(publish, "requests"),
        "two published tarballs left nothing in storage: {publish}"
    );
    assert!(publish["notes"].as_str().is_some_and(|n| !n.is_empty()));
}

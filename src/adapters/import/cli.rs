//! `opencargo import ...`: parses, composes the adapters, calls the use case
//! and renders. Credentials come from the environment, never from `argv`.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use clap::{Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

use super::http::{admin_url, Credential, Gate, GateConfig, HostSet, Secret};
use super::sink::cargo::CargoSink;
use super::sink::go::GoSink;
use super::sink::npm::NpmSink;
use super::sink::oci::OciSink;
use super::source::artifactory::Artifactory;
use super::source::distribution::Distribution;
use super::source::github::Github;
use super::source::nexus::Nexus;
use super::source::verdaccio::Verdaccio;
use super::target::{HttpTargetAdmin, LaneConfig, TargetLane};
use crate::adapters::sqlite::import_journal::SqliteImportJournal;
use crate::adapters::system::SystemClock;
use crate::app::import::permissions::{self, ProposePermissions};
use crate::app::import::plan::PlanRules;
use crate::app::import::report::{self, Report};
use crate::app::import::run::{Finished, RunImport, RunOpts};
use crate::domain::import::{EXIT_CLEAN, EXIT_NOT_STARTED};
use crate::domain::Format;
use crate::ports::import::{redact, ImportJournal, RunHeader, Sink, Source, SourceFilter};

#[derive(Debug, Subcommand)]
pub enum Import {
    /// Copy a source registry into this registry's hosted repositories
    Run(Box<RunArgs>),
    /// Carry on with a run's state file, re-reading its options
    Resume(ResumeArgs),
    /// Counts by status of a run's state file
    Status(StateArgs),
    /// Re-render the report of a state file without touching the network
    Report(ReportArgs),
    /// Delete a state file: the only command that does
    Forget(StateArgs),
    /// Map the source's user rights onto the target; writes only with --apply
    Permissions(PermissionsArgs),
}

#[derive(Debug, Args)]
pub struct PermissionsArgs {
    #[arg(long)]
    pub state: PathBuf,
    /// Create the users and grants, with OPENCARGO_IMPORT_TARGET_ADMIN_TOKEN
    #[arg(long)]
    pub apply: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Nexus,
    Artifactory,
    Verdaccio,
    Github,
    Distribution,
}

impl SourceKind {
    fn as_str(self) -> &'static str {
        match self {
            SourceKind::Nexus => "nexus",
            SourceKind::Artifactory => "artifactory",
            SourceKind::Verdaccio => "verdaccio",
            SourceKind::Github => "github",
            SourceKind::Distribution => "distribution",
        }
    }
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct Tuning {
    /// File-lane workers (npm, cargo, go, ...)
    #[arg(long, default_value_t = 4)]
    pub concurrency: usize,
    /// Blob-lane workers (OCI)
    #[arg(long, default_value_t = 1)]
    pub oci_concurrency: usize,
    /// Source requests per second, per host
    #[arg(long, default_value_t = 5.0)]
    pub rate: f64,
    /// npm publishes per minute on the target
    #[arg(long, default_value_t = 25)]
    pub target_rate: u32,
    #[arg(long, default_value_t = 3)]
    pub retries: u32,
    #[arg(long, default_value = "60s")]
    pub max_backoff: String,
    /// How long a target 429 or 503 parks the lane when it names no delay
    #[arg(long, default_value = "60s", hide = true)]
    pub target_cooldown: String,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct RunArgs {
    #[arg(long, value_enum)]
    pub source: SourceKind,
    /// The source's base URL; credentials go in OPENCARGO_IMPORT_SOURCE_*
    #[arg(long)]
    pub from: String,
    /// The target opencargo's base URL
    #[arg(long)]
    pub to: String,
    #[arg(long = "source-repo")]
    pub source_repos: Vec<String>,
    /// `SRC=DST`: route a source repository (a glob) to a target one
    #[arg(long = "map")]
    pub maps: Vec<String>,
    #[arg(long)]
    pub target_repo: Option<String>,
    #[arg(long)]
    pub create_repos: bool,
    #[arg(long)]
    pub flatten_names: bool,
    #[arg(long)]
    pub include: Vec<String>,
    #[arg(long)]
    pub exclude: Vec<String>,
    /// `30d` or `2026-01-01`
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long)]
    pub until: Option<String>,
    #[arg(long)]
    pub latest_only: bool,
    #[arg(long)]
    pub max_versions: Option<usize>,
    #[command(flatten)]
    pub tuning: Tuning,
    #[arg(long)]
    pub no_retag: bool,
    /// OCI upload chunk, raised to the target's advertised minimum
    #[arg(long, default_value = "32MiB")]
    pub oci_chunk: String,
    #[arg(long, default_value = "https://npm.pkg.github.com/", hide = true)]
    pub github_npm_url: String,
    #[arg(long, default_value = "https://ghcr.io/", hide = true)]
    pub github_registry_url: String,
    /// Reach this host when the source names it; never sends it credentials
    #[arg(long = "allow-source-host")]
    pub allow_source_hosts: Vec<String>,
    #[arg(long, default_value = "2GiB")]
    pub max_artifact_size: String,
    #[arg(long, default_value = "100MiB")]
    pub max_npm_body: String,
    #[arg(long, default_value_t = 1000)]
    pub max_pages: u32,
    #[arg(long)]
    pub spool_dir: Option<PathBuf>,
    #[arg(long)]
    pub state: Option<PathBuf>,
    #[arg(long)]
    pub report: Option<PathBuf>,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub fail_fast: bool,
    #[arg(long)]
    pub allow_anonymous: bool,
    #[arg(long)]
    pub allow_incomplete: bool,
    #[arg(long)]
    pub include_proxy_caches: bool,
    #[arg(long)]
    pub json: bool,
    #[arg(long, default_value_t = 20)]
    pub report_collapse: usize,
    /// Read the source token from this variable instead
    #[arg(long)]
    pub source_token_env: Option<String>,
    /// Read the target token from this variable instead
    #[arg(long)]
    pub target_token_env: Option<String>,
    #[arg(long, default_value_t = 250, hide = true)]
    pub page_size: usize,
}

#[derive(Debug, Args)]
pub struct ResumeArgs {
    #[arg(long)]
    pub state: PathBuf,
    #[arg(long)]
    pub concurrency: Option<usize>,
    #[arg(long)]
    pub oci_concurrency: Option<usize>,
    #[arg(long)]
    pub rate: Option<f64>,
    #[arg(long)]
    pub retries: Option<u32>,
    #[arg(long)]
    pub max_artifact_size: Option<String>,
    #[arg(long)]
    pub allow_incomplete: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct StateArgs {
    #[arg(long)]
    pub state: PathBuf,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ReportArgs {
    #[arg(long)]
    pub state: PathBuf,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub allow_incomplete: bool,
    #[arg(long, default_value_t = 20)]
    pub report_collapse: usize,
}

/// What a command printed and how it exits.
#[derive(Debug, Default)]
pub struct Ran {
    pub code: u8,
    pub stdout: String,
}

fn fail(msg: impl std::fmt::Display) -> Ran {
    Ran { code: EXIT_NOT_STARTED, stdout: format!("error: {msg}\n") }
}

pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let n: u64 = n.parse().map_err(|_| format!("invalid size: {s}"))?;
    let mult: u64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        _ => return Err(format!("invalid size unit: {s}")),
    };
    Ok(n.saturating_mul(mult))
}

pub fn parse_duration(s: &str) -> Result<Duration, String> {
    crate::config::parse_chrono_duration(s)
        .ok()
        .and_then(|d| d.to_std().ok())
        .or_else(|| s.trim().parse::<u64>().ok().map(Duration::from_secs))
        .ok_or_else(|| format!("invalid duration: {s}"))
}

pub fn parse_when(s: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(d.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc());
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }
    crate::config::parse_chrono_duration(s)
        .map(|d| now - d)
        .map_err(|_| format!("invalid date: {s} (use 30d or 2026-01-01)"))
}

/// The environment a command reads credentials from.
pub type Env<'a> = &'a (dyn Fn(&str) -> Option<String> + Sync);

fn non_empty(env: Env<'_>, key: &str) -> Option<String> {
    env(key).filter(|v| !v.is_empty())
}

fn source_credential(a: &RunArgs, env: Env<'_>) -> Credential {
    let token = match &a.source_token_env {
        Some(var) => non_empty(env, var),
        None => non_empty(env, "OPENCARGO_IMPORT_SOURCE_TOKEN"),
    };
    if let Some(t) = token {
        return Credential::Bearer(Secret::new(t));
    }
    match (non_empty(env, "OPENCARGO_IMPORT_SOURCE_USER"), non_empty(env, "OPENCARGO_IMPORT_SOURCE_PASSWORD")) {
        (Some(user), Some(pw)) => Credential::Basic { user, password: Secret::new(pw) },
        _ => Credential::Anonymous,
    }
}

fn owner() -> String {
    let host = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "localhost".into());
    format!("{host}:{}", std::process::id())
}

fn default_state(a: &RunArgs, to: &reqwest::Url) -> PathBuf {
    let host = to.host_str().unwrap_or("target").replace(':', "_");
    PathBuf::from(".opencargo-import").join(format!("{}-{host}.db", a.source.as_str()))
}

struct Composed {
    import: RunImport<'static>,
    opts: RunOpts,
    report_dir: PathBuf,
    json: bool,
}

fn compose(
    a: &RunArgs,
    env: Env<'_>,
    journal: Arc<dyn ImportJournal>,
    state: &Path,
    fresh: bool,
) -> Result<Composed, String> {
    let from = admin_url(&a.from, "--from", "OPENCARGO_IMPORT_SOURCE_USER/_PASSWORD or _TOKEN")?;
    let to = admin_url(&a.to, "--to", "OPENCARGO_IMPORT_TARGET_TOKEN")?;
    let target_var = a.target_token_env.as_deref().unwrap_or("OPENCARGO_IMPORT_TARGET_TOKEN");
    let target_token = non_empty(env, target_var)
        .ok_or_else(|| format!("no target token: set {target_var}"))?;
    let credential = source_credential(a, env);
    let state_dir = state.parent().map(Path::to_path_buf).unwrap_or_default();
    let spool_dir = a.spool_dir.clone().unwrap_or_else(|| state_dir.join("spool"));
    let now = Utc::now();
    let gate = Arc::new(Gate::new(GateConfig {
        from: from.clone(),
        credential,
        allow_hosts: HostSet::new(a.allow_source_hosts.clone()),
        rate: a.tuning.rate,
        retries: a.tuning.retries,
        max_backoff: parse_duration(&a.tuning.max_backoff)?,
        max_artifact_size: parse_size(&a.max_artifact_size)?,
        spool_dir,
        timeout: Duration::from_secs(600),
    })?);
    let lane_cfg = |token: String| LaneConfig {
        base: to.clone(),
        token: Secret::new(token),
        publish_rate: a.tuning.target_rate,
        cooldown: parse_duration(&a.tuning.target_cooldown).unwrap_or(Duration::from_secs(60)),
        stall_after: 3,
        timeout: Duration::from_secs(600),
    };
    let lane = Arc::new(TargetLane::new(lane_cfg(target_token))?);
    let admin_lane = match non_empty(env, "OPENCARGO_IMPORT_TARGET_ADMIN_TOKEN") {
        Some(t) => Some(Arc::new(TargetLane::new(lane_cfg(t))?)),
        None => None,
    };
    let source: Arc<dyn Source> = match a.source {
        SourceKind::Verdaccio => Arc::new(Verdaccio::new(gate.clone()).with_page(a.page_size)),
        SourceKind::Nexus => Arc::new(Nexus::new(gate.clone())),
        SourceKind::Artifactory => Arc::new(Artifactory::new(gate.clone()).with_page(a.page_size.min(1000))),
        SourceKind::Distribution => Arc::new(Distribution::new(gate.clone())),
        SourceKind::Github => Arc::new(
            Github::new(
                gate.clone(),
                admin_url(&a.github_npm_url, "--github-npm-url", "")?,
                admin_url(&a.github_registry_url, "--github-registry-url", "")?,
            )
            .with_page(a.page_size.min(100) as u32),
        ),
    };
    let mut sinks: HashMap<Format, Arc<dyn Sink>> = HashMap::new();
    sinks.insert(Format::Npm, Arc::new(NpmSink::new(gate.clone(), lane.clone(), parse_size(&a.max_npm_body)?)));
    sinks.insert(Format::Cargo, Arc::new(CargoSink::new(gate.clone(), lane.clone())));
    sinks.insert(Format::Go, Arc::new(GoSink::new(gate.clone(), lane.clone())));
    sinks.insert(Format::Oci, Arc::new(OciSink::new(gate.clone(), lane.clone(), parse_size(&a.oci_chunk)?)));
    let mut maps = Vec::new();
    for m in &a.maps {
        maps.push(PlanRules::parse_map(m)?);
    }
    let plan = PlanRules {
        maps,
        target_repo: a.target_repo.clone(),
        include: a.include.clone(),
        exclude: a.exclude.clone(),
        since: a.since.as_deref().map(|s| parse_when(s, now)).transpose()?,
        until: a.until.as_deref().map(|s| parse_when(s, now)).transpose()?,
        latest_only: a.latest_only,
        max_versions: a.max_versions,
        flatten_names: a.flatten_names,
    };
    let header = RunHeader {
        source: a.source.as_str().to_string(),
        source_url: redact(&from),
        target_url: redact(&to),
        opts_json: serde_json::to_string(a).map_err(|e| e.to_string())?,
        started_at: now,
        finished_at: None,
        phase: "running".into(),
        owner: None,
    };
    let opts = RunOpts {
        plan,
        filter: SourceFilter {
            source_repos: a.source_repos.clone(),
            include_proxy_caches: a.include_proxy_caches,
            max_pages: a.max_pages,
        },
        concurrency: a.tuning.concurrency,
        oci_concurrency: a.tuning.oci_concurrency,
        dry_run: a.dry_run,
        fail_fast: a.fail_fast,
        allow_incomplete: a.allow_incomplete,
        create_repos: a.create_repos,
        no_retag: a.no_retag,
        retries: a.tuning.retries,
        report_collapse: a.report_collapse,
        fresh,
        owner: owner(),
        header,
        heartbeat: Duration::from_secs(15),
    };
    let import = RunImport {
        journal,
        source,
        sinks,
        admin: Arc::new(HttpTargetAdmin::new(lane, admin_lane)),
        clock: Arc::new(SystemClock),
        names: &crate::registry::rules::rules,
    };
    let report_dir = a.report.clone().unwrap_or(state_dir);
    Ok(Composed { import, opts, report_dir, json: a.json })
}

fn write_reports(dir: &Path, r: &Report) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    std::fs::write(dir.join("report.json"), report::render_json(r)).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("report.md"), report::render_table(r)).map_err(|e| e.to_string())?;
    Ok(())
}

fn render(f: &Finished, json: bool) -> String {
    if json {
        return serde_json::json!({
            "exit_code": f.code,
            "message": f.message,
            "notes": f.notes,
            "report": f.report,
        })
        .to_string()
            + "\n";
    }
    let mut out = String::new();
    if let Some(p) = &f.probe {
        let version = p.version.clone().unwrap_or_else(|| "(version unknown)".into());
        out.push_str(&format!("source: {} {version}", p.product));
        if let Some(who) = &p.authenticated_as {
            out.push_str(&format!(" as {who}"));
        }
        if !p.capabilities.is_empty() {
            out.push_str(&format!(" [{}]", p.capabilities.join(", ")));
        }
        out.push('\n');
    }
    for n in &f.notes {
        out.push_str(&format!("{n}\n"));
    }
    if let Some(r) = &f.report {
        out.push_str(&report::render_table(r));
    }
    if let Some(m) = &f.message {
        out.push_str(&format!("error: {m}\n"));
    }
    out
}

async fn run_composed(c: Composed, cancel: impl Future<Output = ()>) -> Ran {
    let finished = c.import.run(&c.opts, cancel).await;
    if let Some(r) = &finished.report {
        if let Err(e) = write_reports(&c.report_dir, r) {
            tracing::warn!("writing the report: {e}");
        }
    }
    Ran { code: finished.code, stdout: render(&finished, c.json) }
}

async fn open_existing(path: &Path) -> Result<SqliteImportJournal, Ran> {
    SqliteImportJournal::open(path, false).await.map_err(fail)
}

/// Runs one import command. `cancel` resolving stops a run as aborted,
/// state saved.
pub async fn execute(cmd: Import, env: Env<'_>, cancel: impl Future<Output = ()>) -> Ran {
    match cmd {
        Import::Run(a) => {
            let to = match admin_url(&a.to, "--to", "OPENCARGO_IMPORT_TARGET_TOKEN") {
                Ok(u) => u,
                Err(e) => return fail(e),
            };
            let state = a.state.clone().unwrap_or_else(|| default_state(&a, &to));
            let journal = match SqliteImportJournal::open(&state, true).await {
                Ok(j) => Arc::new(j),
                Err(e) => return fail(e),
            };
            match compose(&a, env, journal, &state, true) {
                Ok(c) => run_composed(c, cancel).await,
                Err(e) => fail(e),
            }
        }
        Import::Resume(r) => {
            let journal = match open_existing(&r.state).await {
                Ok(j) => Arc::new(j),
                Err(ran) => return ran,
            };
            let header = match journal.header().await {
                Ok(Some(h)) => h,
                Ok(None) => return fail("the state file holds no run to resume"),
                Err(e) => return fail(e),
            };
            let mut a: RunArgs = match serde_json::from_str(&header.opts_json) {
                Ok(a) => a,
                Err(e) => return fail(format!("the state file's options are unreadable: {e}")),
            };
            if let Some(v) = r.concurrency {
                a.tuning.concurrency = v;
            }
            if let Some(v) = r.oci_concurrency {
                a.tuning.oci_concurrency = v;
            }
            if let Some(v) = r.rate {
                a.tuning.rate = v;
            }
            if let Some(v) = r.retries {
                a.tuning.retries = v;
            }
            if let Some(v) = r.max_artifact_size {
                a.max_artifact_size = v;
            }
            a.allow_incomplete |= r.allow_incomplete;
            a.json |= r.json;
            match compose(&a, env, journal, &r.state, false) {
                Ok(c) => run_composed(c, cancel).await,
                Err(e) => fail(e),
            }
        }
        Import::Permissions(p) => {
            let journal = match open_existing(&p.state).await {
                Ok(j) => Arc::new(j),
                Err(ran) => return ran,
            };
            let header = match journal.header().await {
                Ok(Some(h)) => h,
                Ok(None) => return fail("the state file holds no run: run an import first"),
                Err(e) => return fail(e),
            };
            let a: RunArgs = match serde_json::from_str(&header.opts_json) {
                Ok(a) => a,
                Err(e) => return fail(format!("the state file's options are unreadable: {e}")),
            };
            let c = match compose(&a, env, journal.clone(), &p.state, false) {
                Ok(c) => c,
                Err(e) => return fail(e),
            };
            let propose = ProposePermissions {
                source: c.import.source.as_ref(),
                admin: c.import.admin.as_ref(),
                rules: &c.opts.plan,
            };
            match propose.run(p.apply).await {
                Ok(proposal) => {
                    if let Err(e) = journal.record("permissions", true, &[], &proposal.gaps, &None, true).await {
                        return fail(e);
                    }
                    let stdout = if p.json {
                        serde_json::to_string_pretty(&proposal).unwrap_or_default() + "\n"
                    } else {
                        permissions::render(&proposal, p.apply)
                    };
                    Ran { code: EXIT_CLEAN, stdout }
                }
                Err(e) => fail(e),
            }
        }
        Import::Status(a) => {
            let j = match open_existing(&a.state).await {
                Ok(j) => j,
                Err(r) => return r,
            };
            match report::from_journal(&j, false, 0).await {
                Ok(r) if a.json => Ran {
                    code: EXIT_CLEAN,
                    stdout: serde_json::json!({ "phase": r.phase, "counts": r.counts }).to_string() + "\n",
                },
                Ok(r) => {
                    let counts: Vec<String> = r.counts.iter().map(|(k, v)| format!("{k} {v}")).collect();
                    Ran {
                        code: EXIT_CLEAN,
                        stdout: format!(
                            "phase: {}\nitems: {}\n",
                            r.phase.unwrap_or_else(|| "never started".into()),
                            if counts.is_empty() { "none".into() } else { counts.join(", ") }
                        ),
                    }
                }
                Err(e) => fail(e),
            }
        }
        Import::Report(a) => {
            let j = match open_existing(&a.state).await {
                Ok(j) => j,
                Err(r) => return r,
            };
            match report::from_journal(&j, a.allow_incomplete, a.report_collapse).await {
                Ok(r) => Ran {
                    code: r.exit_code,
                    stdout: if a.json { report::render_json(&r) + "\n" } else { report::render_table(&r) },
                },
                Err(e) => fail(e),
            }
        }
        Import::Forget(a) => {
            if let Err(r) = open_existing(&a.state).await.map(drop) {
                return r;
            }
            let mut removed = Vec::new();
            for suffix in ["", "-wal", "-shm"] {
                let p = PathBuf::from(format!("{}{suffix}", a.state.display()));
                if std::fs::remove_file(&p).is_ok() {
                    removed.push(p.display().to_string());
                }
            }
            Ran { code: EXIT_CLEAN, stdout: format!("removed {}\n", removed.join(", ")) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_durations_and_dates_parse() {
        assert_eq!(parse_size("2GiB").unwrap(), 2 << 30);
        assert_eq!(parse_size("100MiB").unwrap(), 100 << 20);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert!(parse_size("2XB").is_err());
        assert_eq!(parse_duration("60s").unwrap(), Duration::from_secs(60));
        let now = Utc::now();
        assert_eq!(parse_when("30d", now).unwrap(), now - chrono::Duration::days(30));
        assert_eq!(parse_when("2026-01-01", now).unwrap().to_rfc3339(), "2026-01-01T00:00:00+00:00");
    }
}

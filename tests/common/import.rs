//! Drives `opencargo import` in process, with an explicit environment.

use std::path::{Path, PathBuf};

use clap::Parser;
use opencargo::adapters::import::cli::{execute, Import, Ran};
use serde_json::Value;

#[derive(Parser)]
struct Cmd {
    #[command(subcommand)]
    import: Import,
}

pub struct Run {
    pub code: u8,
    pub stdout: String,
    pub report: Option<Value>,
}

/// A working directory for one importer: its state file, spool and report.
pub struct Importer {
    pub dir: tempfile::TempDir,
    pub env: Vec<(String, String)>,
}

impl Default for Importer {
    fn default() -> Self {
        Self::new()
    }
}

impl Importer {
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            env: vec![("OPENCARGO_IMPORT_TARGET_TOKEN".into(), super::STATIC_TOKEN.into())],
        }
    }

    pub fn env(mut self, k: &str, v: &str) -> Self {
        self.env.retain(|(key, _)| key != k);
        self.env.push((k.into(), v.into()));
        self
    }

    pub fn without(mut self, k: &str) -> Self {
        self.env.retain(|(key, _)| key != k);
        self
    }

    pub fn state(&self) -> PathBuf {
        self.dir.path().join("state.db")
    }

    pub fn spool(&self) -> PathBuf {
        self.dir.path().join("spool")
    }

    pub fn report_path(&self) -> PathBuf {
        self.dir.path().join("report.json")
    }

    /// `import run` with the state, spool and report under this directory.
    pub async fn run(&self, args: &[&str]) -> Run {
        let state = self.state();
        let spool = self.spool();
        let mut full: Vec<String> = vec!["import".into(), "run".into()];
        full.extend(args.iter().map(|s| s.to_string()));
        full.extend(["--state".into(), state.display().to_string(), "--spool-dir".into(), spool.display().to_string()]);
        full.extend(["--target-cooldown".into(), "1s".into(), "--max-backoff".into(), "1s".into()]);
        self.exec(full).await
    }

    pub async fn sub(&self, sub: &str, args: &[&str]) -> Run {
        let mut full: Vec<String> = vec!["import".into(), sub.into(), "--state".into(), self.state().display().to_string()];
        full.extend(args.iter().map(|s| s.to_string()));
        self.exec(full).await
    }

    pub async fn exec(&self, argv: Vec<String>) -> Run {
        let cmd = match Cmd::try_parse_from(argv) {
            Ok(c) => c,
            Err(e) => panic!("bad import arguments: {e}"),
        };
        let env = self.env.clone();
        let lookup = move |k: &str| env.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        let Ran { code, stdout } = execute(cmd.import, &lookup, std::future::pending()).await;
        let report = std::fs::read_to_string(self.report_path()).ok().and_then(|s| serde_json::from_str(&s).ok());
        Run { code, stdout, report }
    }
}

impl Run {
    pub fn gaps(&self) -> Vec<(String, String, String)> {
        self.report
            .as_ref()
            .and_then(|r| r.get("gaps"))
            .and_then(|g| g.as_array())
            .map(|g| {
                g.iter()
                    .map(|g| {
                        let s = |k: &str| g.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_string();
                        (s("kind"), s("source_ref"), s("detail"))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn kinds(&self) -> Vec<String> {
        self.gaps().into_iter().map(|g| g.0).collect()
    }

    pub fn count(&self, status: &str) -> u64 {
        self.report
            .as_ref()
            .and_then(|r| r.pointer(&format!("/counts/{status}")))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    }
}

pub fn files_in(dir: &Path) -> usize {
    std::fs::read_dir(dir).map(|d| d.count()).unwrap_or(0)
}

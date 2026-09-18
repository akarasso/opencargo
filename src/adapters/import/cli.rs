//! `opencargo import ...`: parses, composes the adapters, calls the use case
//! and renders. Credentials never come from `argv`.

use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};

use crate::adapters::sqlite::import_journal::SqliteImportJournal;
use crate::app::import::report;
use crate::domain::import::{EXIT_CLEAN, EXIT_NOT_STARTED};

#[derive(Debug, Subcommand)]
pub enum Import {
    /// Counts by status of a run's state file
    Status(StateArgs),
    /// Re-render the report of a state file without touching the network
    Report(ReportArgs),
    /// Delete a state file: the only command that does
    Forget(StateArgs),
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

async fn open_existing(path: &Path) -> Result<SqliteImportJournal, Ran> {
    SqliteImportJournal::open(path, false).await.map_err(fail)
}

pub async fn execute(cmd: Import) -> Ran {
    match cmd {
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
            if let Err(r) = open_existing(&a.state).await.map(|_| ()) {
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

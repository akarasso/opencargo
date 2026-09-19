use clap::Parser;
use std::path::PathBuf;
use tracing::info;

use axum::ServiceExt as _;
use opencargo::{config, server};
use tower::ServiceExt as _;

#[derive(Parser)]
#[command(name = "opencargo", version, about = "Lightweight universal package registry")]
struct Cli {
    /// Path to config file
    #[arg(short, long, env = "OPENCARGO_CONFIG", global = true)]
    config: Option<PathBuf>,

    /// Bind address (overrides config)
    #[arg(short, long, global = true)]
    bind: Option<String>,

    /// Public URL clients use to reach this server (overrides config)
    #[arg(long, env = "OPENCARGO_BASE_URL", global = true)]
    base_url: Option<String>,

    /// OSV API base URL for vulnerability scanning (overrides config)
    #[arg(long, env = "OPENCARGO_OSV_BASE_URL", global = true)]
    osv_base_url: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(clap::Subcommand)]
enum Commands {
    /// Start the registry server (default)
    Serve,
    /// Validate a config file
    ValidateConfig {
        /// Path to config file to validate
        path: PathBuf,
    },
    /// Run database migrations, under the writer lease
    Migrate {
        /// Skip the writer lease, for a holder known to be dead
        #[arg(long)]
        force: bool,
    },
    /// Snapshot the database, and with --storage every object, into a
    /// local directory; or verify a snapshot with --check
    Backup {
        #[arg(long, required_unless_present = "check")]
        to: Option<PathBuf>,
        #[arg(long)]
        storage: bool,
        #[arg(long)]
        force: bool,
        #[arg(long, default_value_t = 7)]
        keep: usize,
        /// Verify a snapshot without restoring it
        #[arg(long, conflicts_with = "to")]
        check: Option<PathBuf>,
    },
    /// Restore a snapshot, storage first; run with the server stopped. An
    /// interrupted restore is finished by running the same command again
    Restore {
        #[arg(long)]
        from: PathBuf,
        /// Replace an existing database, or restore a database-only snapshot
        #[arg(long)]
        force: bool,
    },
    /// Operate on the artifact store; the server must be stopped for
    /// `migrate` and `reclaim`
    Storage {
        #[command(subcommand)]
        command: StorageCommand,
    },
    /// Copy another registry into this one's hosted repositories, and report
    /// what could not be copied
    Import {
        #[command(subcommand)]
        command: opencargo::adapters::import::cli::Import,
    },
    /// Operate on MCP mirrors against the configured database, then exit
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
}

#[derive(clap::Subcommand)]
enum McpCommand {
    /// Sync one mirror, or every mirror
    Sync {
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        full: bool,
    },
}

#[derive(clap::Subcommand)]
enum StorageCommand {
    /// Probe the store and exercise every operation on its reserved tree
    Check,
    /// List keys rows reference with no object, and with --orphans the
    /// objects nothing references; --repair puts back the last noncurrent
    /// version of a missing key, on a store that keeps them
    Verify {
        #[arg(long)]
        orphans: bool,
        #[arg(long)]
        repair: bool,
    },
    /// Copy every object into the store another config file declares
    Migrate {
        #[arg(long)]
        to: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
    /// Run one reclamation pass, or empty a prefix no repository names
    Reclaim {
        #[arg(long)]
        prefix: Option<String>,
    },
}

async fn storage(cfg: &config::Config, command: StorageCommand) -> anyhow::Result<()> {
    let now = chrono::Utc::now();
    match command {
        StorageCommand::Check => {
            let report = server::storage_check(cfg).await?;
            for step in &report.steps {
                match &step.outcome {
                    Ok(()) => println!("ok    {}", step.operation),
                    Err(e) => println!("FAIL  {}: {e}", step.operation),
                }
            }
            if !report.ok() {
                anyhow::bail!("storage check failed");
            }
        }
        StorageCommand::Verify { orphans, repair } => {
            let options = opencargo::app::storage_ops::Verify {
                orphans,
                repair,
                enqueue: false,
            };
            let report = server::storage_verify(cfg, options, now).await?;
            for key in &report.repaired {
                println!("repaired {key}");
            }
            for key in &report.missing {
                println!("missing  {key}");
            }
            for key in &report.orphans {
                println!("orphan   {key}");
            }
            for key in &report.enqueued {
                println!("queued   {key}");
            }
            println!(
                "{} objects, {} missing, {} orphans, {} queued, {} repaired",
                report.objects,
                report.missing.len(),
                report.orphans.len(),
                report.enqueued.len(),
                report.repaired.len(),
            );
            if !report.missing.is_empty() {
                if !repair {
                    println!(
                        "a store that keeps noncurrent versions puts these back with \
                         `opencargo storage verify --repair`; otherwise delete the version, \
                         manifest or file that references each key, and publish it again"
                    );
                }
                anyhow::bail!("rows reference missing objects");
            }
        }
        StorageCommand::Migrate { to, dry_run } => {
            let target = config::load_config(Some(&to))?.config;
            let report = server::storage_migrate(cfg, &target, dry_run).await?;
            println!(
                "{} {} objects ({} bytes), {} already there",
                if dry_run { "would copy" } else { "copied" },
                report.copied,
                report.bytes,
                report.skipped
            );
        }
        StorageCommand::Reclaim { prefix } => {
            let report = server::storage_reclaim(cfg, prefix.as_deref(), now).await?;
            println!("{report:?}");
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    opencargo::telemetry::logging::init();

    let cli = Cli::parse();

    if let Some(Commands::Import { command }) = cli.command {
        let env = |k: &str| std::env::var(k).ok();
        let interrupted = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        let ran = opencargo::adapters::import::cli::execute(command, &env, interrupted).await;
        print!("{}", ran.stdout);
        std::process::exit(i32::from(ran.code));
    }

    let config::Loaded { config: mut cfg, problems } = config::load_config(cli.config.as_deref())?;
    if let Some(base_url) = cli.base_url {
        cfg.server.base_url = base_url.trim_end_matches('/').to_string();
    }
    if let Some(bind) = &cli.bind {
        cfg.server.bind = bind.clone();
    }
    if let Some(osv_base_url) = cli.osv_base_url {
        cfg.vuln_scan.osv_base_url = osv_base_url.trim_end_matches('/').to_string();
    }

    let command = cli.command.unwrap_or(Commands::Serve);
    if !problems.is_empty() {
        for problem in &problems {
            eprintln!("config: {problem}");
        }
        if matches!(command, Commands::Serve | Commands::Migrate { .. }) {
            anyhow::bail!("invalid configuration ({} problems)", problems.len());
        }
    }

    match command {
        Commands::Serve => {
            let bind = cli.bind.as_deref().unwrap_or(&cfg.server.bind);
            info!("Starting opencargo on {}", bind);
            let loopback = bind.starts_with("127.") || bind.starts_with("localhost") || bind.starts_with("[::1]");
            if !loopback && (cfg.server.base_url.contains("localhost") || cfg.server.base_url.contains("127.0.0.1")) {
                tracing::warn!(
                    base_url = %cfg.server.base_url,
                    "base_url points to localhost while listening on {bind}: tarball URLs will not work from other machines, set OPENCARGO_BASE_URL or [server].base_url"
                );
            }

            let srv = server::shutdown::ServerHandle::new();
            let shutdown = server::shutdown::Shutdown::new();
            let endpoint_drain = cfg.server.endpoint_drain()?;
            let grace = cfg.server.shutdown_grace()?;
            let server::Started { state: app_state, lease, lock } =
                server::build_state(&cfg, srv.clone(), shutdown.clone()).await?;
            let settling = app_state.settle_epoch();
            tokio::spawn(async move {
                match settling.run(chrono::Utc::now()).await {
                    Ok(Some(report)) => info!(
                        missing = report.missing.len(),
                        queued = report.enqueued.len(),
                        "the artifact store was verified after a rollback"
                    ),
                    Ok(None) => {}
                    Err(e) => tracing::warn!(error = %e, "the high-water mark is unsettled: reclamation stays refused"),
                }
            });
            if let Some(schedule) = server::backup_schedule(&cfg, &app_state)? {
                tokio::spawn(schedule);
            }
            let runner = lease
                .as_ref()
                .map_or_else(opencargo::app::lease::LeaseHandle::disabled, |l| l.handle());
            let probe_every = config::parse_chrono_duration(&cfg.auth.sso.probe_interval)?
                .to_std()
                .unwrap_or(std::time::Duration::from_secs(60));
            tokio::spawn(server::start_sso_probe(app_state.sso.clone(), probe_every));

            // Spawn the periodic cleanup/GC task before the router consumes
            // app_state: the pre-release sweep needs cleanup.enabled, the proxy
            // cache sweep runs whenever proxy_cache_older_than_days is set.
            tokio::spawn(opencargo::telemetry::cleanup::start_cleanup_task(
                app_state.packages.clone(),
                app_state.cache.clone(),
                app_state.policy_store.clone(),
                app_state.reclaim.clone(),
                app_state.clock.clone(),
                cfg.cleanup.clone(),
                app_state.reconcilers(),
                runner.clone(),
                app_state.server_state.clone(),
            ));
            tokio::spawn(app_state.sync_supervisor()?.run());
            tokio::spawn(opencargo::telemetry::cleanup::start_reconcile_task(
                app_state.reconcilers(),
                app_state.clock.clone(),
                runner.clone(),
            ));

            tokio::spawn(opencargo::app::sweep_storage::start_storage_sweep(
                opencargo::app::sweep_storage::SweepStorage::new(app_state.storage.clone())
                    .settling(app_state.settle_epoch())
                    .reclaiming(app_state.reclaim_orphans())
                    .reaping_uploads(app_state.oci.clone()),
                app_state.clock.clone(),
                runner,
            ));

            // Decode %2f in scoped package names before routing: this must
            // wrap the Router, since Router::layer runs after route matching.
            let app = server::build_router(app_state)
                .map_request(server::decode_percent_encoded_slashes)
                .into_make_service_with_connect_info::<std::net::SocketAddr>();
            let listener = std::net::TcpListener::bind(bind)?;
            listener.set_nonblocking(true)?;
            let mut serving = if !cfg.server.tls.cert_path.is_empty() && !cfg.server.tls.key_path.is_empty() {
                let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(
                    &cfg.server.tls.cert_path,
                    &cfg.server.tls.key_path,
                )
                .await?;
                info!("Listening with TLS on {}", listener.local_addr()?);
                tokio::spawn(
                    axum_server::from_tcp_rustls(listener, tls_config)?
                        .handle(srv.clone())
                        .serve(app),
                )
            } else {
                info!("Listening on {}", listener.local_addr()?);
                tokio::spawn(axum_server::from_tcp(listener)?.handle(srv.clone()).serve(app))
            };

            tokio::select! {
                served = &mut serving => {
                    served??;
                }
                () = server::shutdown::signal() => {
                    shutdown.drain(&srv, endpoint_drain, grace).await;
                    serving.await??;
                }
            }
            if let Some(lease) = lease {
                lease.release().await;
            }
            drop(lock);
            info!("stopped");
        }
        Commands::ValidateConfig { path } => {
            let checked = config::load_config(Some(&path))?;
            if !checked.problems.is_empty() {
                for problem in &checked.problems {
                    println!("{}: {problem}", path.display());
                }
                anyhow::bail!("{} problems in {}", checked.problems.len(), path.display());
            }
            println!("Config is valid.");
        }
        Commands::Migrate { force } => {
            server::run_migrations(&cfg, force).await?;
            println!("Migrations applied successfully.");
        }
        Commands::Backup { check: Some(dir), .. } => {
            let manifest = server::check_backup(&dir).await?;
            println!(
                "{}: ok, taken {}, {} objects{}",
                dir.display(),
                manifest.taken_at.to_rfc3339(),
                manifest.storage_keys,
                if manifest.storage { "" } else { " (storage: false: database only, not restorable onto an empty tree)" }
            );
        }
        Commands::Backup { to, storage, force, keep, check: None } => {
            let snapshot = server::run_backup(
                &cfg,
                server::BackupArgs {
                    to: to.expect("clap requires --to without --check"),
                    storage,
                    force,
                    keep,
                    space: std::sync::Arc::new(opencargo::backup::StatvfsProbe),
                },
            )
            .await?;
            println!("{}", snapshot.dir.display());
        }
        Commands::Restore { from, force } => {
            let db_path = server::database_path(&cfg)
                .ok_or_else(|| anyhow::anyhow!("a restore needs a database file"))?;
            let lock = opencargo::backup::lock::restore_lock_guard(
                &db_path,
                opencargo::backup::lock::Lock::Exclusive(&from),
            )?;
            let report = server::run_restore(&cfg, &from, force, lock).await?;
            println!(
                "restored {} objects and the database of {}; now run `{}`",
                report.objects,
                report.manifest.taken_at.to_rfc3339(),
                report.gate
            );
        }
        Commands::Storage { command } => storage(&cfg, command).await?,
        Commands::Mcp {
            command: McpCommand::Sync { repo, full },
        } => {
            for (name, report) in server::mcp_sync(&cfg, repo.as_deref(), full).await? {
                println!("{name}: {report}");
            }
        }
        Commands::Import { .. } => unreachable!("dispatched before the config is loaded"),
    }

    Ok(())
}

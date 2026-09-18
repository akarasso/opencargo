use clap::Parser;
use std::path::PathBuf;
use tracing::info;

use axum::ServiceExt as _;
use tower::ServiceExt as _;
use opencargo::{config, server};

#[derive(Parser)]
#[command(name = "opencargo", version, about = "Lightweight universal package registry")]
struct Cli {
    /// Path to config file
    #[arg(short, long, env = "OPENCARGO_CONFIG")]
    config: Option<PathBuf>,

    /// Bind address (overrides config)
    #[arg(short, long)]
    bind: Option<String>,

    /// Public URL clients use to reach this server (overrides config)
    #[arg(long, env = "OPENCARGO_BASE_URL")]
    base_url: Option<String>,

    /// OSV API base URL for vulnerability scanning (overrides config)
    #[arg(long, env = "OPENCARGO_OSV_BASE_URL")]
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
    /// Run database migrations
    Migrate,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "opencargo=info,tower_http=info".into()),
        )
        .init();

    let cli = Cli::parse();

    let mut cfg = config::load_config(cli.config.as_deref())?;
    if let Some(base_url) = cli.base_url {
        cfg.server.base_url = base_url.trim_end_matches('/').to_string();
    }
    if let Some(osv_base_url) = cli.osv_base_url {
        cfg.vuln_scan.osv_base_url = osv_base_url.trim_end_matches('/').to_string();
    }

    match cli.command.unwrap_or(Commands::Serve) {
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

            let app_state = server::build_state(&cfg).await?;

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
            ));

            tokio::spawn(opencargo::app::sweep_storage::start_storage_sweep(
                opencargo::app::sweep_storage::SweepStorage::new(app_state.storage.clone())
                    .reclaiming(app_state.reclaim_orphans())
                    .reaping_uploads(app_state.oci.clone()),
                app_state.clock.clone(),
            ));

            let router = server::build_router(app_state);

            // Decode percent-encoded slashes (%2f) before routing.
            // npm/pnpm clients encode scoped package names this way.
            // This must wrap the Router externally (not via Router::layer)
            // because Router::layer runs after route matching.
            // Decode %2f in scoped package names before routing, on BOTH the
            // TLS and plaintext paths (Router::layer runs after route matching,
            // so this must wrap the Router externally). Previously the decoder
            // was only applied on the plaintext branch, so scoped npm packages
            // (`@scope%2fname`) returned 404 over HTTPS — the normal mode for a
            // private registry. The map_request is built inside each branch
            // because the body type differs (axum_server hands the service a
            // `Request<Incoming>`, axum::serve a `Request<Body>`); the decoder
            // is generic over the body type and only rewrites the URI.
            if !cfg.server.tls.cert_path.is_empty() && !cfg.server.tls.key_path.is_empty() {
                let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(
                    &cfg.server.tls.cert_path,
                    &cfg.server.tls.key_path,
                )
                .await?;
                let bind_addr: std::net::SocketAddr = bind.parse()?;
                info!("Listening with TLS on {}", bind_addr);
                axum_server::bind_rustls(bind_addr, tls_config)
                    .serve(
                        router
                            .map_request(server::decode_percent_encoded_slashes)
                            .into_make_service_with_connect_info::<std::net::SocketAddr>(),
                    )
                    .await?;
            } else {
                let app = router
                    .map_request(server::decode_percent_encoded_slashes)
                    .into_make_service_with_connect_info::<std::net::SocketAddr>();
                let listener = tokio::net::TcpListener::bind(bind).await?;
                info!("Listening on {}", listener.local_addr()?);
                axum::serve(listener, app).await?;
            }
        }
        Commands::ValidateConfig { path } => {
            let _cfg = config::load_config(Some(&path))?;
            println!("Config is valid.");
        }
        Commands::Migrate => {
            server::run_migrations(&cfg).await?;
            println!("Migrations applied successfully.");
        }
    }

    Ok(())
}

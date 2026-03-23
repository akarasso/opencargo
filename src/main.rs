use clap::Parser;
use std::path::PathBuf;
use tracing::info;

use axum::ServiceExt as _;
use tower::ServiceExt as _;
use opencargo::{config, db, server};

#[derive(Parser)]
#[command(name = "opencargo", version, about = "Lightweight universal package registry")]
struct Cli {
    /// Path to config file
    #[arg(short, long, env = "OPENCARGO_CONFIG")]
    config: Option<PathBuf>,

    /// Bind address (overrides config)
    #[arg(short, long)]
    bind: Option<String>,

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

    let cfg = config::load_config(cli.config.as_deref())?;

    match cli.command.unwrap_or(Commands::Serve) {
        Commands::Serve => {
            let bind = cli.bind.as_deref().unwrap_or(&cfg.server.bind);
            info!("Starting opencargo on {}", bind);

            let app_state = server::build_state(&cfg).await?;
            let router = server::build_router(app_state);

            // Decode percent-encoded slashes (%2f) before routing.
            // npm/pnpm clients encode scoped package names this way.
            // This must wrap the Router externally (not via Router::layer)
            // because Router::layer runs after route matching.
            if !cfg.server.tls.cert_path.is_empty() && !cfg.server.tls.key_path.is_empty() {
                let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem_file(
                    &cfg.server.tls.cert_path,
                    &cfg.server.tls.key_path,
                )
                .await?;
                let bind_addr: std::net::SocketAddr = bind.parse()?;
                info!("Listening with TLS on {}", bind_addr);
                axum_server::bind_rustls(bind_addr, tls_config)
                    .serve(router.into_make_service())
                    .await?;
            } else {
                let app = router
                    .map_request(server::decode_percent_encoded_slashes)
                    .into_make_service();
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
            let pool = db::connect(&cfg.database.url).await?;
            db::migrate(&pool).await?;
            println!("Migrations applied successfully.");
        }
    }

    Ok(())
}

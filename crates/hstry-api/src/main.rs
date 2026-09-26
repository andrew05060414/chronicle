use std::io::{self, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Args, Parser};
use log::info;

use hstry_api::{AppState, create_router};
use hstry_core::{Config, Database};

fn main() {
    if let Err(err) = try_main() {
        let _ = writeln!(io::stderr(), "{err:?}");
        std::process::exit(1);
    }
}

#[tokio::main]
async fn try_main() -> Result<()> {
    env_logger::init();

    let cli = Cli::parse();
    let config_path = cli
        .common
        .config
        .unwrap_or_else(Config::default_config_path);
    let config = Config::ensure_at(&config_path)?;

    let db = Database::open(&config.database).await?;

    let ingest_token = cli
        .common
        .token
        .clone()
        .or_else(|| std::env::var("HSTRY_API_TOKEN").ok())
        .filter(|t| !t.is_empty());
    let has_token = ingest_token.is_some();
    if !has_token {
        info!(
            "No ingest token configured (set --token or HSTRY_API_TOKEN); /ingest accepts any loopback client"
        );
    }

    let state = AppState::new(Arc::new(config), Arc::new(db), ingest_token);
    let app = create_router(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], cli.common.port));
    info!("Starting API server on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    // Unconditional banner: env_logger is silent without RUST_LOG, which makes
    // a healthy server look hung. Print one line so the user sees it is up.
    let _ = writeln!(
        io::stderr(),
        "hstry-api listening on http://{addr}  (ingest auth: {}, set RUST_LOG=info,tower_http=debug for request logs)",
        if has_token {
            "token required"
        } else {
            "open on loopback"
        }
    );
    axum::serve(listener, app).await?;

    Ok(())
}

#[derive(Debug, Parser)]
#[command(author, version, about = "HTTP API server for rust-workspace")]
struct Cli {
    #[command(flatten)]
    common: CommonOpts,
}

#[derive(Debug, Clone, Args)]
struct CommonOpts {
    /// Override the config file path
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Port to listen on
    #[arg(short, long, default_value = "3000")]
    port: u16,

    /// Bearer token required for /ingest (falls back to HSTRY_API_TOKEN)
    #[arg(long, value_name = "TOKEN")]
    token: Option<String>,
}

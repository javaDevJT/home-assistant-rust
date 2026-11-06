//! Independent native Rust implementation of a subset of Home Assistant Core.

mod config;
mod helpers;

use anyhow::{Context as _, Result, bail};
use clap::Parser;
use config::Config;
use hass_core::{Context, Runtime};
use hass_storage::SqliteStore;
use serde_json::Map;
use std::{
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
};
use tracing::{info, warn};

#[derive(Parser)]
#[command(
    name = "hass",
    version,
    about = "Native Rust Home Assistant Core rewrite (partial compatibility)"
)]
struct Args {
    /// Read the supported YAML configuration subset.
    #[arg(short, long, default_value = "configuration.yaml")]
    config: PathBuf,
    /// Validate configuration without opening a database or binding a listener.
    #[arg(long)]
    check: bool,
    /// Override the configured listening IP address.
    #[arg(long)]
    host: Option<IpAddr>,
    /// Override the configured port (0 selects an available port).
    #[arg(long)]
    port: Option<u16>,
    /// Override the database path (relative to the current directory).
    #[arg(long)]
    database: Option<PathBuf>,
    /// Read a Bearer token from this file; alternatively set HASS_TOKEN.
    #[arg(long, env = "HASS_TOKEN_FILE")]
    token_file: Option<PathBuf>,
}

fn read_token(path: Option<&Path>) -> Result<String> {
    let value = if let Some(path) = path {
        std::fs::read_to_string(path).context("cannot read token file")?
    } else {
        std::env::var("HASS_TOKEN")
            .context("set HASS_TOKEN or supply --token-file before starting the server")?
    };
    let token = value.trim().to_owned();
    if token.len() < 32 || token.len() > 4096 || !token.bytes().all(|c| c.is_ascii_graphic()) {
        bail!("access token must contain 32 to 4096 non-whitespace printable bytes");
    }
    Ok(token)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hass=info,hass_automation=info".into()),
        )
        .with_target(false)
        .init();
    let args = Args::parse();
    let config = Config::load(&args.config)?;
    if args.check {
        println!("Configuration valid (supported Rust subset)");
        return Ok(());
    }
    let access_token = read_token(args.token_file.as_deref())?;
    let address = SocketAddr::new(
        args.host.unwrap_or(config.http.server_host),
        args.port.unwrap_or(config.http.server_port),
    );
    // Bind before mutating persistent data: an occupied port cannot partially
    // initialize an otherwise unused configuration directory.
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("cannot bind {address}"))?;
    let bound_address = listener.local_addr()?;
    let db_path = args.database.unwrap_or_else(|| {
        args.config
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(&config.recorder.db_path)
    });
    let helper_config = config.helpers.clone();
    let runtime = tokio::task::spawn_blocking(move || -> Result<Runtime> {
        if let Some(parent) = db_path.parent().filter(|path| !path.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).context("cannot create database directory")?;
        }
        let store = SqliteStore::open(&db_path)?;
        let runtime = Runtime::new(Arc::new(store))?;
        helpers::setup(&runtime, &helper_config)?;
        Ok(runtime)
    })
    .await
    .context("runtime initialization failed")??;
    let api_config = config.api_config();
    let automations = hass_automation::start(runtime.clone(), config.automation)?;
    let started_runtime = runtime.clone();
    tokio::task::spawn_blocking(move || {
        started_runtime.fire_event("homeassistant_started", Map::new(), Context::default())
    })
    .await??;
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let app = hass_api::router_with_shutdown(
        hass_api::ApiState {
            runtime: runtime.clone(),
            access_token,
            config: api_config,
        },
        shutdown_rx,
    );
    info!(%bound_address, version = hass_core::VERSION, "Rust runtime listening");
    warn!(
        "Partial compatibility: native helpers only; upstream integrations and frontend are not ported"
    );
    let result = axum_serve(listener, app, shutdown_tx).await;
    let automation_result = automations.shutdown().await;
    let stop_result = tokio::task::spawn_blocking(move || {
        runtime.fire_event("homeassistant_stop", Map::new(), Context::default())
    })
    .await?;
    result?;
    automation_result?;
    stop_result?;
    Ok(())
}

async fn axum_serve(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    shutdown: tokio::sync::watch::Sender<bool>,
) -> Result<()> {
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            let _ = shutdown.send(true);
        })
        .await
        .context("HTTP server failed")
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {},
                    _ = term.recv() => {},
                }
            }
            Err(error) => {
                warn!(%error, "cannot listen for SIGTERM; using Ctrl-C");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    info!("Stopping Rust runtime");
}

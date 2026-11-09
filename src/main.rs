mod admin;
mod child;
mod cli;
mod config;
mod http;
mod server;
mod status;
mod tui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use config::{Config, Paths, ensure_private_dir};
use server::{Globals, Server};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Instant};

#[derive(Parser)]
#[command(version, about = "Share local stdio MCP servers across agent sessions")]
struct Cli {
    /// Config file [default: ~/.config/mcpmux/config.toml]
    #[arg(long, global = true, env = "MCPMUX_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon in the foreground (what launchd runs)
    Serve,
    /// Create the token and LaunchAgent, then load it
    Install,
    /// Unload and remove the LaunchAgent (keeps config, token, cache)
    Uninstall,
    /// Daemon and per-server status
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Start a server now instead of on first call
    Start { server: Option<String> },
    /// Stop a server (it restarts on the next call)
    Stop { server: Option<String> },
    /// Stop then start a server
    Restart { server: Option<String> },
    /// Follow logs: the daemon's, or one server's stderr
    Logs { server: Option<String> },
    /// Print client configuration for the shared servers
    Clients {
        #[arg(value_enum)]
        client: Client,
    },
    /// Validate config, secret files, commands and tunnels; --live also handshakes each server
    Check {
        #[arg(long)]
        live: bool,
    },
    /// Print the Authorization header as JSON (for Claude Code's headersHelper)
    Headers,
    /// Dashboard
    Tui,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Client {
    Claude,
    Pi,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::new(cli.config);
    match cli.cmd {
        Cmd::Serve => serve(paths),
        Cmd::Install => cli::install(&paths),
        Cmd::Uninstall => cli::uninstall(&paths),
        Cmd::Status { json } => cli::status(&paths, json),
        Cmd::Start { server } => cli::control(&paths, "start", server),
        Cmd::Stop { server } => cli::control(&paths, "stop", server),
        Cmd::Restart { server } => cli::control(&paths, "restart", server),
        Cmd::Logs { server } => cli::logs(&paths, server),
        Cmd::Clients { client } => cli::clients(&paths, client),
        Cmd::Check { live } => cli::check(&paths, live),
        Cmd::Headers => {
            let t = paths.read_token()?;
            println!("{}", serde_json::json!({"Authorization": format!("Bearer {t}")}));
            Ok(())
        }
        Cmd::Tui => tui::run(&paths),
    }
}

/// Build the per-server managers. Shared by `serve` and `check --live`.
pub fn build_servers(cfg: &Config, paths: &Paths) -> Result<BTreeMap<String, Arc<Server>>> {
    ensure_private_dir(&paths.cache)?;
    ensure_private_dir(&paths.logs)?;
    let globals = Arc::new(Globals::new(cfg, paths.cache.clone(), paths.logs.clone())?);
    Ok(cfg
        .servers
        .iter()
        .map(|(name, sc)| {
            (
                name.clone(),
                Server::new(name.clone(), sc.clone(), globals.clone()),
            )
        })
        .collect())
}

#[tokio::main]
async fn serve(paths: Paths) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mcpmux=info".into()),
        )
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .init();

    let cfg = Config::load(&paths.config)?;
    let token = paths.read_token()?;
    ensure_private_dir(&paths.state)?;
    let servers = build_servers(&cfg, &paths)?;
    for s in servers.values() {
        tokio::spawn(s.clone().reaper());
    }

    let addr = cfg.listen_addr()?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;

    let admin = Arc::new(admin::Admin {
        servers: servers.clone(),
        started: Instant::now(),
        listen: cfg.listen.clone(),
    });
    admin::serve(admin, &paths.socket()).await?;

    let app = http::router(Arc::new(http::AppState {
        servers: servers.clone().into_iter().collect(),
        token,
    }));
    tracing::info!(%addr, servers = ?servers.keys().collect::<Vec<_>>(), "listening");

    // Not graceful: GET notification streams never close on their own.
    tokio::spawn(async move { axum::serve(listener, app).await });
    shutdown_signal().await;

    tracing::info!("shutting down; stopping children");
    let stops: Vec<_> = servers
        .values()
        .cloned()
        .map(|s| tokio::spawn(async move { s.stop("daemon exit").await }))
        .collect();
    for s in stops {
        let _ = s.await;
    }
    let _ = std::fs::remove_file(paths.socket());
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

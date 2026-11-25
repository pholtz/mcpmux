mod admin;
mod child;
mod cli;
mod config;
mod http;
mod logs;
mod server;
mod status;
mod tui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use config::{Config, Paths, ensure_private_dir};
use server::{Globals, Server, SharedGlobals};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

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
    /// Re-read the config; running servers whose command/args/env changed restart when idle
    Reload,
    /// Keep a server running (skip the idle stop) until unpinned or the daemon restarts
    Pin { server: Option<String> },
    /// Undo `pin`
    Unpin { server: Option<String> },
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
        Cmd::Reload => cli::reload(&paths),
        Cmd::Pin { server } => cli::control(&paths, "pin", server),
        Cmd::Unpin { server } => cli::control(&paths, "unpin", server),
        Cmd::Logs { server } => cli::logs(&paths, server),
        Cmd::Clients { client } => cli::clients(&paths, client),
        Cmd::Check { live } => cli::check(&paths, live),
        Cmd::Headers => {
            let t = paths.read_token()?;
            println!(
                "{}",
                serde_json::json!({"Authorization": format!("Bearer {t}")})
            );
            Ok(())
        }
        Cmd::Tui => tui::run(&paths),
    }
}

/// Build the per-server managers. Shared by `serve` and `check --live`.
pub fn build_servers(
    cfg: &Config,
    paths: &Paths,
) -> Result<(SharedGlobals, BTreeMap<String, Arc<Server>>)> {
    ensure_private_dir(&paths.cache)?;
    ensure_private_dir(&paths.logs)?;
    let globals = Arc::new(RwLock::new(Globals::new(
        cfg,
        paths.cache.clone(),
        paths.logs.clone(),
    )?));
    let servers = cfg
        .servers
        .iter()
        .map(|(name, sc)| {
            (
                name.clone(),
                Server::new(name.clone(), sc.clone(), globals.clone()),
            )
        })
        .collect();
    Ok((globals, servers))
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
    let (globals, built) = build_servers(&cfg, &paths)?;
    for s in built.values() {
        tokio::spawn(s.clone().reaper());
    }
    let servers: admin::Servers = Arc::new(RwLock::new(built));

    let addr = cfg.listen_addr()?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;

    let names: Vec<String> = servers.read().unwrap().keys().cloned().collect();
    let admin = Arc::new(admin::Admin {
        servers: servers.clone(),
        globals,
        config: Mutex::new(cfg),
        config_path: paths.config.clone(),
        cache_dir: paths.cache.clone(),
        log_dir: paths.logs.clone(),
        started: Instant::now(),
    });
    admin::serve(admin.clone(), &paths.socket()).await?;
    tokio::spawn(monitor(admin.clone(), paths.logs.clone()));
    tokio::spawn(reload_on_hup(admin.clone()));

    let app = http::router(Arc::new(http::AppState {
        servers: servers.clone(),
        token,
    }));
    tracing::info!(%addr, servers = ?names, "listening");

    // Not graceful: GET notification streams never close on their own.
    tokio::spawn(async move { axum::serve(listener, app).await });
    shutdown_signal().await;

    tracing::info!("shutting down; stopping children");
    let stops: Vec<_> = admin
        .snapshot()
        .into_iter()
        .map(|s| tokio::spawn(async move { s.stop("daemon exit").await }))
        .collect();
    for s in stops {
        let _ = s.await;
    }
    let _ = std::fs::remove_file(paths.socket());
    Ok(())
}

/// Background sampling: tunnel probes every 5s, RSS every 5s (one `ps` for all servers),
/// log size limits every minute (policy re-read, so reload applies it).
async fn monitor(admin: Arc<admin::Admin>, log_dir: PathBuf) {
    let daemon_log = log_dir.join("mcpmux.log");
    let own_stderr = logs::stderr_is(&daemon_log);
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    for n in 0u64.. {
        tick.tick().await;
        let servers = admin.snapshot();
        let mut probes = tokio::task::JoinSet::new();
        for s in &servers {
            let s = s.clone();
            probes.spawn(async move { s.probe_tunnel().await });
        }
        probes.join_all().await;
        if let Ok(rss) = tokio::task::spawn_blocking(server::group_rss_kb).await {
            for s in &servers {
                s.sample_rss(&rss);
            }
        }
        if n % 12 == 0 {
            let policy = admin.globals.read().unwrap().log;
            if own_stderr && let Err(e) = logs::limit_stderr(&daemon_log, policy) {
                tracing::warn!("rotating {}: {e}", daemon_log.display());
            }
            for s in &servers {
                let p = log_dir.join(format!("{}.log", s.name));
                if let Err(e) = logs::limit_open(&p, policy) {
                    tracing::warn!("rotating {}: {e}", p.display());
                }
            }
        }
    }
}

async fn reload_on_hup(admin: Arc<admin::Admin>) {
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(mut hup) = signal(SignalKind::hangup()) else {
        return;
    };
    while hup.recv().await.is_some() {
        if let Err(e) = admin.reload().await {
            tracing::warn!("reload on SIGHUP failed: {e:#}");
        }
    }
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

use crate::{
    Client, admin, build_servers,
    config::{Config, Paths, default_label, ensure_private_dir, expand_tilde},
    status::human_secs,
};
use anyhow::{Context, Result, bail};
use std::{
    io::Write,
    net::{SocketAddr, TcpStream},
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

fn uid() -> u32 {
    // SAFETY: getuid cannot fail.
    unsafe { libc::getuid() }
}

fn launchctl(args: &[&str]) -> Result<std::process::Output> {
    Ok(Command::new("/bin/launchctl").args(args).output()?)
}

pub fn install(paths: &Paths) -> Result<()> {
    let cfg_dir = paths.token.parent().unwrap();
    ensure_private_dir(cfg_dir)?;
    for d in [&paths.state, &paths.cache, &paths.logs] {
        ensure_private_dir(d)?;
    }

    if !paths.token.exists() {
        let mut b = [0u8; 32];
        crate::http::getrandom(&mut b);
        let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&paths.token)?;
        writeln!(f, "{hex}")?;
        println!("created {}", paths.token.display());
    }

    if !paths.config.exists() {
        bail!(
            "{} not found; copy config.example.toml there and edit it",
            paths.config.display()
        );
    }
    let cfg = Config::load(&paths.config)?;
    let addr = cfg.listen_addr()?;

    let running = std::os::unix::net::UnixStream::connect(paths.socket()).is_ok();
    if !running && std::net::TcpListener::bind(addr).is_err() {
        bail!("{addr} is already in use by another process");
    }

    let exe = std::env::current_exe()?.canonicalize()?;
    if exe.components().any(|c| c.as_os_str() == "target") {
        eprintln!(
            "warning: installing a binary from target/; `cargo install --path .` gives launchd a stable path"
        );
    }
    let log = paths.logs.join("mcpmux.log");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key><array>
    <string>{exe}</string><string>--config</string><string>{config}</string><string>serve</string>
  </array>
  <key>WorkingDirectory</key><string>{home}</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        label = cfg.label,
        exe = exe.display(),
        config = paths.config.display(),
        home = crate::config::home().display(),
        log = log.display(),
    );
    let plist_path = paths.plist(&cfg.label);
    std::fs::create_dir_all(plist_path.parent().unwrap())?;
    std::fs::write(&plist_path, plist)?;

    let domain = format!("gui/{}", uid());
    let _ = launchctl(&["bootout", &format!("{domain}/{}", cfg.label)]);
    let out = launchctl(&["bootstrap", &domain, &plist_path.display().to_string()])?;
    if !out.status.success() {
        bail!(
            "launchctl bootstrap failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    println!("loaded {}; listening on {addr}", cfg.label);
    println!("next: `mcpmux clients claude` / `mcpmux clients pi`");
    Ok(())
}

pub fn uninstall(paths: &Paths) -> Result<()> {
    let label = match Config::load(&paths.config) {
        Ok(cfg) => cfg.label,
        Err(e) => {
            eprintln!("warning: {e:#}; using the default label");
            default_label()
        }
    };
    let _ = launchctl(&["bootout", &format!("gui/{}/{label}", uid())]);
    let plist = paths.plist(&label);
    if plist.exists() {
        std::fs::remove_file(&plist)?;
    }
    println!("unloaded {label}; config, token and cache left in place");
    Ok(())
}

fn mb(kb: Option<u64>) -> String {
    kb.map(|k| format!("{}M", k / 1024)).unwrap_or_else(|| "-".into())
}

fn opt<T: ToString>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "-".into())
}

pub fn status(paths: &Paths, json: bool) -> Result<()> {
    let st = admin::status(&paths.socket())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&st)?);
        return Ok(());
    }
    println!(
        "mcpmux pid {} · up {} · {}",
        st.pid,
        human_secs(st.uptime_s),
        st.listen
    );
    println!(
        "{:<18} {:<9} {:>6} {:>6} {:>8} {:>9} {:>4} {:>6} {:>4} {:>6} {:>6}",
        "SERVER", "STATE", "PID", "RSS", "UP", "IDLE", "SESS", "CALLS", "ERR", "P50", "P95"
    );
    for s in &st.servers {
        let idle = s
            .idle_s
            .map(|i| {
                if s.idle_timeout_s == 0 {
                    human_secs(i)
                } else {
                    format!("{}/{}", human_secs(i), human_secs(s.idle_timeout_s))
                }
            })
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<18} {:<9} {:>6} {:>6} {:>8} {:>9} {:>4} {:>6} {:>4} {:>6} {:>6}",
            s.name,
            s.state,
            opt(s.pid),
            mb(s.rss_kb),
            s.uptime_s.map(human_secs).unwrap_or_else(|| "-".into()),
            idle,
            s.sessions,
            s.calls_1h,
            s.errors_1h,
            s.p50_ms.map(|m| format!("{m}ms")).unwrap_or_else(|| "-".into()),
            s.p95_ms.map(|m| format!("{m}ms")).unwrap_or_else(|| "-".into()),
        );
        if let Some(e) = &s.last_error {
            let retry = s
                .retry_in_s
                .map(|r| format!(" (retry in {r}s)"))
                .unwrap_or_default();
            println!("  ↳ {e}{retry}");
        }
    }
    Ok(())
}

pub fn control(paths: &Paths, cmd: &str, server: Option<String>) -> Result<()> {
    admin::call(&paths.socket(), cmd, server.as_deref())?;
    println!("{cmd}: ok");
    Ok(())
}

pub fn logs(paths: &Paths, server: Option<String>) -> Result<()> {
    let file = match &server {
        Some(s) => paths.logs.join(format!("{s}.log")),
        None => paths.logs.join("mcpmux.log"),
    };
    if !file.exists() {
        bail!("{} does not exist yet", file.display());
    }
    let err = Command::new("tail").args(["-n", "100", "-F"]).arg(&file).exec();
    Err(err).context("running tail")
}

pub fn clients(paths: &Paths, client: Client) -> Result<()> {
    let cfg = Config::load(&paths.config)?;
    let base = format!("http://{}", cfg.listen);
    let token = paths.token.display().to_string().replace(
        &crate::config::home().display().to_string(),
        "~",
    );
    match client {
        Client::Claude => {
            println!("# Back up first: cp ~/.claude.json ~/.claude.json.pre-mcpmux");
            println!("# Then restart open Claude sessions.");
            for name in cfg.servers.keys() {
                println!("claude mcp remove -s user {name}");
                println!(
                    "claude mcp add -s user --transport http {name} {base}/{name}/mcp \\\n  --header \"Authorization: Bearer $(cat {token})\""
                );
            }
        }
        Client::Pi => {
            let servers: serde_json::Map<String, serde_json::Value> = cfg
                .servers
                .keys()
                .map(|name| {
                    (
                        name.clone(),
                        serde_json::json!({
                            "url": format!("{base}/{name}/mcp"),
                            "headers": {"Authorization": format!("!echo Bearer $(cat {token})")},
                        }),
                    )
                })
                .collect();
            println!(
                "// ~/.pi/agent/mcp.json: replace each stdio entry's command/args/env with these."
            );
            println!("// Existing keys can stay as they are; the URL picks the server.");
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({ "mcpServers": servers }))?
            );
        }
    }
    Ok(())
}

fn which(cmd: &str, path: &str) -> Option<String> {
    if cmd.contains('/') {
        return Path::new(&expand_tilde(cmd))
            .exists()
            .then(|| cmd.to_string());
    }
    path.split(':')
        .map(|d| Path::new(d).join(cmd))
        .find(|p| p.is_file())
        .map(|p| p.display().to_string())
}

pub fn check(paths: &Paths, live: bool) -> Result<()> {
    let cfg = Config::load(&paths.config)?;
    println!("config  ok   {}", paths.config.display());
    match paths.read_token() {
        Ok(_) => println!("token   ok   {}", paths.token.display()),
        Err(e) => println!("token   FAIL {e:#}"),
    }
    let path = cfg
        .path_env()
        .unwrap_or_else(|| std::env::var("PATH").unwrap_or_default());
    let mut failed = false;
    for (name, sc) in &cfg.servers {
        let mut problems = vec![];
        match which(&sc.command, &path) {
            Some(_) => {}
            None => problems.push(format!("{} not found on PATH", sc.command)),
        }
        if let Err(e) = sc.resolve_env() {
            problems.push(format!("{e:#}"));
        }
        if let Some(addr) = &sc.requires_tcp {
            let a: SocketAddr = addr.parse()?;
            if TcpStream::connect_timeout(&a, Duration::from_secs(2)).is_err() {
                problems.push(format!("nothing listening on {addr} (tunnel down?)"));
            }
        }
        if problems.is_empty() {
            println!("{name:<18} ok");
        } else {
            failed = true;
            for p in problems {
                println!("{name:<18} FAIL {p}");
            }
        }
    }
    if live {
        failed |= check_live(&cfg, paths)?;
    }
    if failed {
        bail!("check failed");
    }
    Ok(())
}

#[tokio::main]
async fn check_live(cfg: &Config, paths: &Paths) -> Result<bool> {
    println!("\nlive handshake (spawns each server once, then stops it; refreshes the cache):");
    let servers = build_servers(cfg, paths)?;
    let mut failed = false;
    for (name, s) in &servers {
        let t = Instant::now();
        match s.ensure_child().await {
            Ok(_) => {
                let st = s.status(&Default::default());
                println!(
                    "{name:<18} ok   {} tools in {:.1}s",
                    opt(st.cached_tools),
                    t.elapsed().as_secs_f32()
                );
            }
            Err(e) => {
                failed = true;
                println!("{name:<18} FAIL {e}");
            }
        }
        s.stop("check").await;
    }
    Ok(failed)
}

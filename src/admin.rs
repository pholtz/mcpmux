//! Admin unix socket: one JSON request line in, one JSON reply line out.

use crate::{
    config::Config,
    server::{Globals, Server, SharedGlobals},
    status::{Status, ToolView},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    net::UnixListener,
};

#[derive(Serialize, Deserialize, Debug)]
pub struct Request {
    pub cmd: String,
    #[serde(default)]
    pub server: Option<String>,
}

/// The live server set. Reload swaps entries; HTTP and admin both read through it.
pub type Servers = Arc<RwLock<BTreeMap<String, Arc<Server>>>>;

pub struct Admin {
    pub servers: Servers,
    pub globals: SharedGlobals,
    pub config: Mutex<Config>,
    pub config_path: PathBuf,
    pub cache_dir: PathBuf,
    pub log_dir: PathBuf,
    pub started: Instant,
}

impl Admin {
    pub fn snapshot(&self) -> Vec<Arc<Server>> {
        self.servers.read().unwrap().values().cloned().collect()
    }

    /// RSS comes from the monitor's samples (every 5s), so status stays cheap.
    pub fn status(&self) -> Status {
        let rss = Default::default();
        Status {
            pid: std::process::id(),
            uptime_s: self.started.elapsed().as_secs(),
            listen: self.config.lock().unwrap().listen.clone(),
            config: self.config_path.display().to_string(),
            servers: self.snapshot().iter().map(|s| s.status(&rss)).collect(),
        }
    }

    /// Re-read the config file and apply it. Live settings change at once; spawn-affecting
    /// ones restart running servers once their in-flight calls drain.
    pub async fn reload(&self) -> Result<Vec<String>> {
        let new = Config::load(&self.config_path)?;
        let old = self.config.lock().unwrap().clone();
        let mut changes = vec![];
        if new.listen != old.listen {
            changes.push(format!(
                "listen {} → {}: needs a daemon restart (launchctl kickstart -k gui/$UID/{})",
                old.listen, new.listen, old.label
            ));
        }
        let respawn_all = new.path != old.path || new.cwd != old.cwd;
        let globals = Globals::new(&new, self.cache_dir.clone(), self.log_dir.clone())?;
        *self.globals.write().unwrap() = globals;
        for (key, a, b) in [
            ("idle_timeout", &old.idle_timeout, &new.idle_timeout),
            (
                "startup_timeout",
                &old.startup_timeout,
                &new.startup_timeout,
            ),
        ] {
            if a != b {
                changes.push(format!("{key} {a} → {b}"));
            }
        }
        if (&new.log_max_size, new.log_rollover) != (&old.log_max_size, old.log_rollover) {
            changes.push(format!(
                "logs {} rollover={} → {} rollover={}",
                old.log_max_size, old.log_rollover, new.log_max_size, new.log_rollover
            ));
        }
        if new.notify != old.notify {
            changes.push(format!("notify {} → {}", old.notify, new.notify));
        }

        let removed: Vec<Arc<Server>> = {
            let mut map = self.servers.write().unwrap();
            let gone: Vec<String> = map
                .keys()
                .filter(|k| !new.servers.contains_key(*k))
                .cloned()
                .collect();
            gone.iter().filter_map(|k| map.remove(k)).collect()
        };
        for s in removed {
            s.retire();
            s.stop("removed from config").await;
            changes.push(format!("{}: removed", s.name));
        }
        for (name, sc) in &new.servers {
            let existing = self.servers.read().unwrap().get(name).cloned();
            match existing {
                Some(s) => changes.extend(s.apply(sc.clone(), respawn_all)),
                None => {
                    let s = Server::new(name.clone(), sc.clone(), self.globals.clone());
                    tokio::spawn(s.clone().reaper());
                    self.servers.write().unwrap().insert(name.clone(), s);
                    changes.push(format!("{name}: added"));
                }
            }
        }
        *self.config.lock().unwrap() = new;
        tracing::info!(?changes, "config reloaded");
        Ok(changes)
    }

    async fn handle(&self, req: Request) -> Value {
        let target = |name: &Option<String>| -> Result<Vec<Arc<Server>>, String> {
            match name.as_deref() {
                None | Some("all") => Ok(self.snapshot()),
                Some(n) => self
                    .servers
                    .read()
                    .unwrap()
                    .get(n)
                    .cloned()
                    .map(|s| vec![s])
                    .ok_or_else(|| format!("unknown server {n:?}")),
            }
        };
        let one = |name: &Option<String>| -> Result<Arc<Server>, String> {
            match name.as_deref() {
                None | Some("all") => Err("name a server".into()),
                Some(_) => target(name).map(|mut v| v.remove(0)),
            }
        };
        let fail = |e: String| json!({"ok": false, "error": e});
        match req.cmd.as_str() {
            "status" => json!({"ok": true, "status": self.status()}),
            "reload" => match self.reload().await {
                Ok(changes) => json!({"ok": true, "changes": changes}),
                Err(e) => fail(format!("{e:#}")),
            },
            "tools" => match one(&req.server) {
                Ok(s) => {
                    let tools: Option<Vec<ToolView>> = s.tools();
                    json!({"ok": true, "tools": tools})
                }
                Err(e) => fail(e),
            },
            "tunnel" => match one(&req.server) {
                Ok(s) => match s.tunnel_up().await {
                    Ok(true) => json!({"ok": true}),
                    Ok(false) => fail(format!("{}: tunnel still down after 15s", s.name)),
                    Err(e) => fail(e),
                },
                Err(e) => fail(e),
            },
            "pin" | "unpin" => match target(&req.server) {
                Ok(servers) => {
                    let mut errors = vec![];
                    for s in servers {
                        s.set_pinned(req.cmd == "pin");
                        if req.cmd == "pin"
                            && let Err(e) = s.ensure_child().await
                        {
                            errors.push(format!("{}: {e}", s.name));
                        }
                    }
                    if errors.is_empty() {
                        json!({"ok": true})
                    } else {
                        fail(errors.join("\n"))
                    }
                }
                Err(e) => fail(e),
            },
            "start" | "stop" | "restart" => {
                let servers = match target(&req.server) {
                    Ok(s) => s,
                    Err(e) => return fail(e),
                };
                let mut errors = vec![];
                for s in servers {
                    if req.cmd != "start" {
                        s.stop("admin").await;
                    }
                    if req.cmd != "stop"
                        && let Err(e) = s.ensure_child().await
                    {
                        errors.push(format!("{}: {e}", s.name));
                    }
                }
                if errors.is_empty() {
                    json!({"ok": true})
                } else {
                    fail(errors.join("\n"))
                }
            }
            other => fail(format!("unknown command {other:?}")),
        }
    }
}

pub async fn serve(admin: Arc<Admin>, path: &Path) -> Result<()> {
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            bail!("another mcpmux is already serving {}", path.display());
        }
        std::fs::remove_file(path)?;
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let admin = admin.clone();
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut lines = tokio::io::BufReader::new(r).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let reply = match serde_json::from_str::<Request>(&line) {
                        Ok(req) => admin.handle(req).await,
                        Err(e) => json!({"ok": false, "error": format!("bad request: {e}")}),
                    };
                    let mut out = reply.to_string();
                    out.push('\n');
                    if w.write_all(out.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    Ok(())
}

/// Blocking client used by the CLI and TUI. `start` can wait out a cold npx/uvx install.
pub fn call(socket: &Path, cmd: &str, server: Option<&str>) -> Result<Value> {
    call_timeout(socket, cmd, server, Duration::from_secs(180))
}

pub fn call_timeout(
    socket: &Path,
    cmd: &str,
    server: Option<&str>,
    timeout: Duration,
) -> Result<Value> {
    let mut stream = std::os::unix::net::UnixStream::connect(socket)
        .with_context(|| format!("mcpmux daemon not running ({})", socket.display()))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let req = Request {
        cmd: cmd.into(),
        server: server.map(str::to_string),
    };
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .with_context(|| format!("daemon did not answer `{cmd}` within {timeout:?}"))?;
    let v: Value = serde_json::from_str(&reply).context("bad reply from daemon")?;
    if v["ok"] != json!(true) {
        bail!("{}", v["error"].as_str().unwrap_or("request failed"));
    }
    Ok(v)
}

/// Status with a short timeout, so a wedged daemon can't freeze the TUI.
pub fn status(socket: &Path) -> Result<Status> {
    let v = call_timeout(socket, "status", None, Duration::from_secs(3))?;
    Ok(serde_json::from_value(v["status"].clone())?)
}

pub fn tools(socket: &Path, server: &str) -> Result<Option<Vec<ToolView>>> {
    let v = call_timeout(socket, "tools", Some(server), Duration::from_secs(3))?;
    Ok(serde_json::from_value(v["tools"].clone())?)
}

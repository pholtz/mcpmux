//! Admin unix socket: one JSON request line in, one JSON reply line out.

use crate::{
    server::{Server, group_rss_kb},
    status::Status,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::Arc,
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

pub struct Admin {
    pub servers: BTreeMap<String, Arc<Server>>,
    pub started: Instant,
    pub listen: String,
}

impl Admin {
    pub fn status(&self) -> Status {
        let rss = group_rss_kb();
        Status {
            pid: std::process::id(),
            uptime_s: self.started.elapsed().as_secs(),
            listen: self.listen.clone(),
            servers: self.servers.values().map(|s| s.status(&rss)).collect(),
        }
    }

    async fn handle(&self, req: Request) -> Value {
        let target = |name: &Option<String>| -> Result<Vec<Arc<Server>>, String> {
            match name.as_deref() {
                None | Some("all") => Ok(self.servers.values().cloned().collect()),
                Some(n) => self
                    .servers
                    .get(n)
                    .cloned()
                    .map(|s| vec![s])
                    .ok_or_else(|| format!("unknown server {n:?}")),
            }
        };
        match req.cmd.as_str() {
            "status" => json!({"ok": true, "status": self.status()}),
            "start" | "stop" | "restart" => {
                let servers = match target(&req.server) {
                    Ok(s) => s,
                    Err(e) => return json!({"ok": false, "error": e}),
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
                    json!({"ok": false, "error": errors.join("\n")})
                }
            }
            other => json!({"ok": false, "error": format!("unknown command {other:?}")}),
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

/// Blocking client used by the CLI and TUI.
pub fn call(socket: &Path, cmd: &str, server: Option<&str>) -> Result<Value> {
    let mut stream = std::os::unix::net::UnixStream::connect(socket)
        .with_context(|| format!("mcpmux daemon not running ({})", socket.display()))?;
    // `start` can wait out a cold npx/uvx install.
    stream.set_read_timeout(Some(Duration::from_secs(180)))?;
    let req = Request {
        cmd: cmd.into(),
        server: server.map(str::to_string),
    };
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    let v: Value = serde_json::from_str(&reply).context("bad reply from daemon")?;
    if v["ok"] != json!(true) {
        bail!("{}", v["error"].as_str().unwrap_or("request failed"));
    }
    Ok(v)
}

pub fn status(socket: &Path) -> Result<Status> {
    let v = call(socket, "status", None)?;
    Ok(serde_json::from_value(v["status"].clone())?)
}

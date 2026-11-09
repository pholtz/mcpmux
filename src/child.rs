//! One stdio MCP server process, multiplexed: requests get fresh upstream IDs so
//! concurrent callers from different sessions never collide.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{mpsc, oneshot},
};

pub enum ChildEvent {
    /// A server-initiated notification other than progress.
    Notification(Value),
    Exited(String),
}

struct ProgressRoute {
    token: Value,
    sink: mpsc::Sender<Value>,
}

pub struct Child {
    pub pid: i32,
    pub generation: u64,
    tx: mpsc::UnboundedSender<String>,
    pending: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    progress: Mutex<HashMap<String, ProgressRoute>>,
    next_id: AtomicI64,
    alive: AtomicBool,
}

pub struct SpawnSpec<'a> {
    pub command: &'a str,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub path: Option<String>,
    pub cwd: String,
    pub stderr_log: &'a Path,
    pub generation: u64,
}

const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

impl Child {
    pub fn spawn(
        spec: SpawnSpec<'_>,
        events: mpsc::UnboundedSender<(u64, ChildEvent)>,
    ) -> Result<Arc<Child>> {
        if let Ok(m) = std::fs::metadata(spec.stderr_log)
            && m.len() > MAX_LOG_BYTES
        {
            let _ = std::fs::rename(spec.stderr_log, spec.stderr_log.with_extension("log.1"));
        }
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(spec.stderr_log)
            .with_context(|| format!("opening {}", spec.stderr_log.display()))?;

        let mut cmd = Command::new(spec.command);
        cmd.args(&spec.args)
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log))
            // Own process group, so stop() can kill npx/uvx grandchildren too.
            .process_group(0);
        if let Some(p) = &spec.path {
            cmd.env("PATH", p);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        let mut proc = cmd
            .spawn()
            .with_context(|| format!("spawning {}", spec.command))?;
        let pid = proc.id().context("child has no pid")? as i32;
        let mut stdin = proc.stdin.take().unwrap();
        let stdout = proc.stdout.take().unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let child = Arc::new(Child {
            pid,
            generation: spec.generation,
            tx,
            pending: Mutex::default(),
            progress: Mutex::default(),
            next_id: AtomicI64::new(1),
            alive: AtomicBool::new(true),
        });

        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                    || stdin.flush().await.is_err()
                {
                    break;
                }
            }
        });

        let reader = child.clone();
        let ev = events.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    tracing::debug!(pid, "non-JSON stdout line ignored");
                    continue;
                };
                reader.on_message(msg, &ev);
            }
            reader.alive.store(false, Ordering::SeqCst);
            // Dropping the senders fails every in-flight call.
            reader.pending.lock().unwrap().clear();
            reader.progress.lock().unwrap().clear();
        });

        let generation = spec.generation;
        tokio::spawn(async move {
            let status = proc.wait().await;
            let why = match status {
                Ok(s) => match s.code() {
                    Some(c) => format!("exited with code {c}"),
                    None => format!("killed ({s})"),
                },
                Err(e) => format!("wait failed: {e}"),
            };
            let _ = events.send((generation, ChildEvent::Exited(why)));
        });

        Ok(child)
    }

    fn on_message(&self, msg: Value, events: &mpsc::UnboundedSender<(u64, ChildEvent)>) {
        let method = msg.get("method").and_then(Value::as_str);
        match (method, msg.get("id")) {
            (None, Some(id)) => {
                if let Some(id) = id.as_i64()
                    && let Some(tx) = self.pending.lock().unwrap().remove(&id)
                {
                    let _ = tx.send(msg);
                }
            }
            (Some(m), Some(id)) => {
                // Server→client request. With one shared child there's no single client to
                // ask, so answer ping and refuse the rest (sampling, roots, elicitation).
                let reply = if m == "ping" {
                    json!({"jsonrpc": "2.0", "id": id, "result": {}})
                } else {
                    json!({"jsonrpc": "2.0", "id": id,
                           "error": {"code": -32601, "message": format!("{m} is not supported through mcpmux")}})
                };
                let _ = self.tx.send(reply.to_string());
            }
            (Some("notifications/progress"), None) => {
                let token = msg.pointer("/params/progressToken").map(token_key);
                let routes = self.progress.lock().unwrap();
                if let Some(route) = token.and_then(|t| routes.get(&t)) {
                    let mut out = msg.clone();
                    out["params"]["progressToken"] = route.token.clone();
                    let _ = route.sink.try_send(out);
                }
            }
            (Some(_), None) => {
                let _ = events.send((self.generation, ChildEvent::Notification(msg)));
            }
            (None, None) => {}
        }
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    /// Send a request with a fresh upstream ID. The caller awaits the receiver; it errors
    /// if the child dies.
    pub fn start_request(&self, mut msg: Value) -> (i64, oneshot::Receiver<Value>) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        msg["id"] = json!(id);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let _ = self.tx.send(msg.to_string());
        (id, rx)
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let (_, rx) =
            self.start_request(json!({"jsonrpc": "2.0", "method": method, "params": params}));
        let resp = rx.await.context("server exited")?;
        if let Some(err) = resp.get("error") {
            anyhow::bail!("{method}: {err}");
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    pub fn notify(&self, msg: Value) {
        let _ = self.tx.send(msg.to_string());
    }

    pub fn cancel(&self, id: i64, reason: &str) {
        if self.pending.lock().unwrap().remove(&id).is_some() {
            self.notify(json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                               "params": {"requestId": id, "reason": reason}}));
        }
    }

    pub fn add_progress_route(&self, upstream: String, token: Value, sink: mpsc::Sender<Value>) {
        self.progress
            .lock()
            .unwrap()
            .insert(upstream, ProgressRoute { token, sink });
    }

    pub fn remove_progress_route(&self, upstream: &str) {
        self.progress.lock().unwrap().remove(upstream);
    }

    pub fn signal_group(&self, sig: i32) -> bool {
        // SAFETY: plain syscall; pid is our own child's process group.
        unsafe { libc::killpg(self.pid, sig) == 0 }
    }

    pub fn group_exists(&self) -> bool {
        self.signal_group(0)
    }
}

pub fn token_key(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

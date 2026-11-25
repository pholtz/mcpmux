//! Per-server lifecycle (lazy start, idle stop, backoff), handshake cache, and request
//! forwarding. HTTP-agnostic: `http.rs` and `admin.rs` sit on top.

use crate::{
    child::{Child, ChildEvent, SpawnSpec, token_key},
    config::{
        Config, DEFAULT_PROTOCOL, ServerConfig, expand_tilde, home, in_window, parse_duration,
    },
    status::{CallView, History, ServerStatus, ToolView, Tunnel},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{broadcast, mpsc, watch};

/// List methods answered from cache when called without a cursor.
const CACHED_LISTS: [(&str, &str); 4] = [
    ("tools/list", "tools"),
    ("prompts/list", "prompts"),
    ("resources/list", "resources"),
    ("resources/templates/list", "resources"),
];

/// Daemon-wide settings. Replaced in place on reload.
#[derive(Clone)]
pub struct Globals {
    pub path: Option<String>,
    pub cwd: String,
    pub idle_timeout: Duration,
    pub startup_timeout: Duration,
    pub cache_dir: PathBuf,
    pub log_dir: PathBuf,
    pub notify: bool,
    pub log: crate::logs::Policy,
}

pub type SharedGlobals = Arc<RwLock<Globals>>;

impl Globals {
    pub fn new(cfg: &Config, cache_dir: PathBuf, log_dir: PathBuf) -> anyhow::Result<Self> {
        Ok(Globals {
            path: cfg.path_env(),
            cwd: cfg
                .cwd
                .as_deref()
                .map(expand_tilde)
                .unwrap_or_else(|| home().display().to_string()),
            idle_timeout: parse_duration(&cfg.idle_timeout)?,
            startup_timeout: parse_duration(&cfg.startup_timeout)?,
            cache_dir,
            log_dir,
            notify: cfg.notify,
            log: crate::logs::Policy::new(cfg)?,
        })
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct Cache {
    fingerprint: String,
    initialize: Value,
    lists: BTreeMap<String, Value>,
    updated: u64,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    Stopped,
    Starting,
    Ready,
    Stopping,
    Failed,
}

struct CallRecord {
    at: Instant,
    tool: String,
    session: String,
    ms: u64,
    ok: bool,
}

struct SessionInfo {
    client: String,
    last_seen: Instant,
}

struct Inner {
    state: State,
    child: Option<Arc<Child>>,
    generation: u64,
    started_at: Option<Instant>,
    last_activity: Instant,
    inflight: usize,
    last_error: Option<String>,
    failures: u32,
    retry_at: Option<Instant>,
    calls: VecDeque<CallRecord>,
    sessions: HashMap<String, SessionInfo>,
    /// (session, client request id) → upstream id, for client cancellation.
    client_reqs: HashMap<(String, String), i64>,
    /// Held warm by the user: no idle stop (the memory guard still applies).
    pinned: bool,
    /// Spawn-affecting config changed while running; restart once in-flight calls drain.
    restart_pending: bool,
    tunnel_up: Option<bool>,
    /// Latest RSS sample of the running child, and of the last run once stopped.
    rss_kb: Option<u64>,
    last_rss_kb: Option<u64>,
    /// One RSS sample (MB) per minute, newest last, with the minute it was taken.
    rss_hist: VecDeque<(u64, u64)>,
    cache_note: Option<String>,
    last_notified: Option<Instant>,
}

pub struct Server {
    pub name: String,
    cfg: RwLock<ServerConfig>,
    globals: SharedGlobals,
    inner: Mutex<Inner>,
    start_lock: tokio::sync::Mutex<()>,
    cache: Mutex<Option<Cache>>,
    /// Server-initiated notifications for GET streams.
    pub broadcast: broadcast::Sender<Value>,
    events: mpsc::UnboundedSender<(u64, ChildEvent)>,
    progress_seq: AtomicU64,
    /// Set when reload removes the server; ends its background tasks.
    retire: watch::Sender<bool>,
}

pub type Reply = Value;

fn rpc_error(id: &Value, code: i64, message: impl Into<String>) -> Reply {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

fn rpc_result(id: &Value, result: Value) -> Reply {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn minute_now() -> u64 {
    now_unix() / 60
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Server {
    pub fn new(name: String, cfg: ServerConfig, globals: SharedGlobals) -> Arc<Self> {
        let (events, events_rx) = mpsc::unbounded_channel();
        let server = Arc::new(Server {
            cache: Mutex::new(None),
            name,
            cfg: RwLock::new(cfg),
            globals,
            inner: Mutex::new(Inner {
                state: State::Stopped,
                child: None,
                generation: 0,
                started_at: None,
                last_activity: Instant::now(),
                inflight: 0,
                last_error: None,
                failures: 0,
                retry_at: None,
                calls: VecDeque::new(),
                sessions: HashMap::new(),
                client_reqs: HashMap::new(),
                pinned: false,
                restart_pending: false,
                tunnel_up: None,
                rss_kb: None,
                last_rss_kb: None,
                rss_hist: VecDeque::new(),
                cache_note: None,
                last_notified: None,
            }),
            start_lock: tokio::sync::Mutex::new(()),
            broadcast: broadcast::channel(64).0,
            events,
            progress_seq: AtomicU64::new(1),
            retire: watch::channel(false).0,
        });
        server.load_cache();
        tokio::spawn(server.clone().event_loop(events_rx));
        server
    }

    fn cfg(&self) -> std::sync::RwLockReadGuard<'_, ServerConfig> {
        self.cfg.read().unwrap()
    }

    fn idle_timeout(&self) -> Duration {
        self.cfg()
            .idle_timeout
            .as_deref()
            .and_then(|t| parse_duration(t).ok())
            .unwrap_or_else(|| self.globals.read().unwrap().idle_timeout)
    }

    fn log_path(&self) -> PathBuf {
        let dir = self.globals.read().unwrap().log_dir.clone();
        dir.join(format!("{}.log", self.name))
    }

    /// Apply a reloaded config. Returns a description of what changed, if anything.
    /// `respawn` forces a restart (daemon-wide PATH or cwd changed).
    pub fn apply(&self, new: ServerConfig, respawn: bool) -> Option<String> {
        let old = self.cfg().clone();
        let (a, b) = (
            serde_json::to_value(&old).unwrap_or_default(),
            serde_json::to_value(&new).unwrap_or_default(),
        );
        let keys: Vec<&str> = b
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(k, v)| a.get(k.as_str()) != Some(v))
            .map(|(k, _)| k.as_str())
            .collect();
        if keys.is_empty() && !respawn {
            return None;
        }
        let spawn_keys = ["command", "args", "env", "cwd", "protocol_version"];
        let respawn = respawn || keys.iter().any(|k| spawn_keys.contains(k));
        let deny_changed = keys.contains(&"deny_tools");
        *self.cfg.write().unwrap() = new;
        let what = if keys.is_empty() {
            "PATH/cwd".to_string()
        } else {
            keys.join(", ")
        };
        if deny_changed {
            // Clients re-fetch tools/list and see the new filter.
            let _ = self
                .broadcast
                .send(json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}));
        }
        if !respawn {
            return Some(format!("{}: {what} updated", self.name));
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.child.is_some() {
            inner.restart_pending = true;
            Some(format!("{}: {what} changed; restarts when idle", self.name))
        } else {
            drop(inner);
            *self.cache.lock().unwrap() = None;
            self.load_cache(); // drops a cache whose fingerprint no longer matches
            Some(format!(
                "{}: {what} changed; applies on next start",
                self.name
            ))
        }
    }

    /// Stop background tasks; the server has been removed from the config.
    pub fn retire(&self) {
        let _ = self.retire.send(true);
    }

    pub fn set_pinned(&self, on: bool) {
        self.inner.lock().unwrap().pinned = on;
    }

    // ---------- cache ----------

    fn cache_path(&self) -> PathBuf {
        let dir = self.globals.read().unwrap().cache_dir.clone();
        dir.join(format!("{}.json", self.name))
    }

    fn load_cache(&self) {
        let Ok(text) = std::fs::read_to_string(self.cache_path()) else {
            return;
        };
        match serde_json::from_str::<Cache>(&text) {
            Ok(c) if c.fingerprint == self.cfg().fingerprint() => {
                *self.cache.lock().unwrap() = Some(c);
            }
            Ok(_) => {
                tracing::info!(server = %self.name, "config changed; ignoring cached handshake")
            }
            Err(e) => tracing::warn!(server = %self.name, "bad cache file: {e}"),
        }
    }

    fn save_cache(&self, cache: &Cache) {
        let path = self.cache_path();
        let tmp = path.with_extension("json.tmp");
        let res = serde_json::to_vec_pretty(cache)
            .map_err(anyhow::Error::from)
            .and_then(|b| Ok(std::fs::write(&tmp, b)?))
            .and_then(|_| Ok(std::fs::rename(&tmp, &path)?));
        if let Err(e) = res {
            tracing::warn!(server = %self.name, "writing cache: {e}");
        }
    }

    /// Re-fetch the list endpoints and save the cache. `fresh_spawn` compares against the
    /// previous cache: clients that already fetched a stale list are told to re-fetch.
    async fn refresh_lists(&self, child: &Child, initialize: Value, fresh_spawn: bool) {
        let caps = initialize.get("capabilities").cloned().unwrap_or_default();
        let mut lists = BTreeMap::new();
        for (method, cap) in CACHED_LISTS {
            if caps.get(cap).is_none() {
                continue;
            }
            match tokio::time::timeout(Duration::from_secs(30), child.request(method, json!({})))
                .await
            {
                Ok(Ok(result)) => {
                    lists.insert(method.to_string(), result);
                }
                Ok(Err(e)) => tracing::warn!(server = %self.name, "{method}: {e}"),
                Err(_) => tracing::warn!(server = %self.name, "{method}: timed out"),
            }
        }
        let cache = Cache {
            fingerprint: self.cfg().fingerprint(),
            initialize,
            lists,
            updated: now_unix(),
        };
        self.save_cache(&cache);
        let old = self.cache.lock().unwrap().replace(cache.clone());
        if fresh_spawn
            && let Some(old) = old
            && let Some(note) = cache_diff(&old, &cache)
        {
            tracing::info!(server = %self.name, "cache was stale: {note}");
            self.inner.lock().unwrap().cache_note = Some(note);
            let _ = self
                .broadcast
                .send(json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}));
        }
    }

    fn cached_initialize(&self) -> Option<Value> {
        self.cache
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.initialize.clone())
    }

    fn cached_list(&self, method: &str) -> Option<Value> {
        self.cache
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|c| c.lists.get(method).cloned())
    }

    pub fn has_cache(&self) -> bool {
        self.cache.lock().unwrap().is_some()
    }

    fn filter_tools(&self, method: &str, mut result: Value) -> Value {
        let cfg = self.cfg();
        if method == "tools/list"
            && !cfg.deny_tools.is_empty()
            && let Some(tools) = result.get_mut("tools").and_then(Value::as_array_mut)
        {
            tools.retain(|t| {
                t.get("name")
                    .and_then(Value::as_str)
                    .is_none_or(|n| !cfg.deny_tools.iter().any(|d| d == n))
            });
        }
        result
    }

    /// Every cached tool, denied ones included, with a rough token cost.
    pub fn tools(&self) -> Option<Vec<ToolView>> {
        let deny = self.cfg().deny_tools.clone();
        let cache = self.cache.lock().unwrap();
        let tools = cache
            .as_ref()?
            .lists
            .get("tools/list")?
            .get("tools")?
            .as_array()?;
        Some(
            tools
                .iter()
                .map(|t| {
                    let name = t.get("name").and_then(Value::as_str).unwrap_or("?");
                    ToolView {
                        name: name.to_string(),
                        description: t
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        tokens: est_tokens(t),
                        denied: deny.iter().any(|d| d == name),
                    }
                })
                .collect(),
        )
    }

    // ---------- lifecycle ----------

    fn ready_child(&self) -> Option<Arc<Child>> {
        let inner = self.inner.lock().unwrap();
        match (&inner.state, &inner.child) {
            (State::Ready, Some(c)) if c.alive() => Some(c.clone()),
            _ => None,
        }
    }

    /// Return the running child, starting it if needed. Startup runs in its own task so a
    /// caller hanging up mid-start doesn't abort it.
    pub async fn ensure_child(self: &Arc<Self>) -> Result<Arc<Child>, String> {
        if let Some(c) = self.ready_child() {
            return Ok(c);
        }
        let this = self.clone();
        tokio::spawn(async move { this.start_locked().await })
            .await
            .map_err(|e| format!("start task failed: {e}"))?
    }

    async fn start_locked(self: &Arc<Self>) -> Result<Arc<Child>, String> {
        let _guard = self.start_lock.lock().await;
        if let Some(c) = self.ready_child() {
            return Ok(c);
        }
        let generation = {
            let mut inner = self.inner.lock().unwrap();
            inner.restart_pending = false;
            if let Some(at) = inner.retry_at
                && Instant::now() < at
            {
                let secs = (at - Instant::now()).as_secs() + 1;
                return Err(format!(
                    "{} failed to start: {}; next attempt allowed in {secs}s",
                    self.name,
                    inner.last_error.as_deref().unwrap_or("unknown error")
                ));
            }
            inner.state = State::Starting;
            inner.generation += 1;
            inner.generation
        };
        tracing::info!(server = %self.name, "starting");
        let started = Instant::now();
        match self.spawn_and_handshake(generation).await {
            Ok(child) => {
                let mut inner = self.inner.lock().unwrap();
                inner.state = State::Ready;
                inner.child = Some(child.clone());
                inner.started_at = Some(Instant::now());
                inner.last_activity = Instant::now();
                inner.last_error = None;
                inner.retry_at = None;
                tracing::info!(server = %self.name, pid = child.pid,
                    ms = started.elapsed().as_millis() as u64, "ready");
                Ok(child)
            }
            Err(e) => {
                let mut inner = self.inner.lock().unwrap();
                inner.state = State::Failed;
                self.record_failure(&mut inner, e.clone());
                tracing::warn!(server = %self.name, "start failed: {e}");
                let body = if e.starts_with("nothing listening") {
                    "tunnel is down"
                } else {
                    "failed to start"
                };
                self.alert(&mut inner, body);
                Err(e)
            }
        }
    }

    fn record_failure(&self, inner: &mut Inner, err: String) {
        inner.failures += 1;
        // First failure: retry on the next call. Repeats back off 1s, 2s, 4s … 60s.
        let backoff = match inner.failures {
            0 | 1 => 0,
            n => (1u64 << (n - 2).min(6)).min(60),
        };
        inner.retry_at = (backoff > 0).then(|| Instant::now() + Duration::from_secs(backoff));
        inner.last_error = Some(err);
    }

    /// macOS notification, at most once per server per 10 minutes. The body never carries
    /// error text: it can echo server output.
    fn alert(&self, inner: &mut Inner, body: &str) {
        if !self.globals.read().unwrap().notify
            || inner
                .last_notified
                .is_some_and(|t| t.elapsed() < Duration::from_secs(600))
        {
            return;
        }
        inner.last_notified = Some(Instant::now());
        notify(&format!("mcpmux: {}", self.name), body);
    }

    async fn spawn_and_handshake(&self, generation: u64) -> Result<Arc<Child>, String> {
        let cfg = self.cfg().clone();
        let globals = self.globals.read().unwrap().clone();
        let log = self.log_path();
        if let Some(addr) = &cfg.requires_tcp {
            let mut ok = probe(addr, Duration::from_secs(2)).await;
            let mut ran = String::new();
            if !ok && let Some(cmd) = &cfg.requires_tcp_cmd {
                ok = self.run_tunnel_cmd(cmd, addr).await;
                ran = format!("; ran `{cmd}`");
            }
            self.inner.lock().unwrap().tunnel_up = Some(ok);
            if !ok {
                let hint = cfg
                    .requires_tcp_hint
                    .as_ref()
                    .map(|h| format!(" ({h})"))
                    .unwrap_or_default();
                return Err(format!(
                    "nothing listening on {addr}{ran}; is the tunnel up?{hint}"
                ));
            }
        }
        let env = cfg.resolve_env().map_err(|e| format!("{e:#}"))?;
        let cwd = cfg
            .cwd
            .as_deref()
            .map(expand_tilde)
            .unwrap_or_else(|| globals.cwd.clone());
        let child = Child::spawn(
            SpawnSpec {
                command: &cfg.command,
                args: cfg.args_expanded(),
                env,
                path: globals.path.clone(),
                cwd,
                stderr_log: &log,
                log_policy: globals.log,
                generation,
            },
            self.events.clone(),
        )
        .map_err(|e| format!("{e:#}"))?;

        let protocol = cfg.protocol_version.as_deref().unwrap_or(DEFAULT_PROTOCOL);
        let init = tokio::time::timeout(
            globals.startup_timeout,
            child.request(
                "initialize",
                json!({
                    "protocolVersion": protocol,
                    "capabilities": {},
                    "clientInfo": {"name": "mcpmux", "version": env!("CARGO_PKG_VERSION")},
                }),
            ),
        )
        .await;
        let init = match init {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                kill_group(&child).await;
                return Err(format!("initialize failed: {e:#} (see {})", log.display()));
            }
            Err(_) => {
                kill_group(&child).await;
                return Err(format!(
                    "no initialize response within {}s (see {})",
                    globals.startup_timeout.as_secs(),
                    log.display()
                ));
            }
        };
        child.notify(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        self.refresh_lists(&child, init, true).await;
        Ok(child)
    }

    /// Run `requires_tcp_cmd` and wait up to 15s for the port. The command is left running
    /// (a foreground tunnel must outlive this call); its output goes to the server's log.
    async fn run_tunnel_cmd(&self, cmd: &str, addr: &str) -> bool {
        let globals = self.globals.read().unwrap().clone();
        let log = self.log_path();
        tracing::info!(server = %self.name, "{addr} closed; running requires_tcp_cmd");
        let out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log);
        let mut c = tokio::process::Command::new("/bin/sh");
        c.arg("-c")
            .arg(cmd)
            .current_dir(&globals.cwd)
            .stdin(std::process::Stdio::null());
        if let Some(p) = &globals.path {
            c.env("PATH", p);
        }
        if let Ok(f) = out {
            use std::io::Write;
            let mut f = f;
            let _ = writeln!(f, "[mcpmux] {addr} closed; running `{cmd}`");
            if let Ok(f2) = f.try_clone() {
                c.stdout(f2);
            }
            c.stderr(f);
        }
        match c.spawn() {
            Ok(mut child) => {
                tokio::spawn(async move {
                    let _ = child.wait().await;
                });
            }
            Err(e) => {
                tracing::warn!(server = %self.name, "requires_tcp_cmd: {e}");
                return false;
            }
        }
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if probe(addr, Duration::from_millis(500)).await {
                return true;
            }
        }
        false
    }

    /// Run the tunnel command now (TUI `T`). Errors if none is configured.
    pub async fn tunnel_up(&self) -> Result<bool, String> {
        let (addr, cmd) = {
            let cfg = self.cfg();
            match (&cfg.requires_tcp, &cfg.requires_tcp_cmd) {
                (Some(a), Some(c)) => (a.clone(), c.clone()),
                _ => return Err(format!("{} has no requires_tcp_cmd", self.name)),
            }
        };
        let ok = probe(&addr, Duration::from_millis(500)).await
            || self.run_tunnel_cmd(&cmd, &addr).await;
        self.inner.lock().unwrap().tunnel_up = Some(ok);
        Ok(ok)
    }

    /// Monitor hook: refresh the tunnel state.
    pub async fn probe_tunnel(&self) {
        let Some(addr) = self.cfg().requires_tcp.clone() else {
            return;
        };
        let up = probe(&addr, Duration::from_millis(500)).await;
        self.inner.lock().unwrap().tunnel_up = Some(up);
    }

    /// Monitor hook: record this server's RSS from a `group_rss_kb` sample.
    pub fn sample_rss(&self, rss: &HashMap<i32, u64>) {
        let mut inner = self.inner.lock().unwrap();
        let kb = inner.child.as_ref().and_then(|c| rss.get(&c.pid).copied());
        inner.rss_kb = kb;
        if kb.is_some() {
            inner.last_rss_kb = kb;
        }
        let minute = minute_now();
        if inner.rss_hist.back().is_none_or(|(m, _)| *m < minute) {
            inner.rss_hist.push_back((minute, kb.unwrap_or(0) / 1024));
            while inner.rss_hist.len() > 60 {
                inner.rss_hist.pop_front();
            }
        }
    }

    pub async fn stop(&self, reason: &str) -> bool {
        let _guard = self.start_lock.lock().await;
        let child = {
            let mut inner = self.inner.lock().unwrap();
            let Some(child) = inner.child.take() else {
                if inner.state == State::Failed {
                    // Explicit stop clears the backoff so `start` retries immediately.
                    inner.state = State::Stopped;
                    inner.retry_at = None;
                    inner.failures = 0;
                }
                return false;
            };
            inner.state = State::Stopping;
            child
        };
        tracing::info!(server = %self.name, pid = child.pid, "stopping ({reason})");
        kill_group(&child).await;
        let mut inner = self.inner.lock().unwrap();
        inner.state = State::Stopped;
        inner.started_at = None;
        inner.rss_kb = None;
        inner.failures = 0;
        inner.retry_at = None;
        inner.client_reqs.clear();
        true
    }

    async fn event_loop(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<(u64, ChildEvent)>) {
        let mut retire = self.retire.subscribe();
        loop {
            let (generation, event) = tokio::select! {
                e = rx.recv() => match e { Some(e) => e, None => return },
                _ = retire.changed() => return,
            };
            let current = {
                let inner = self.inner.lock().unwrap();
                inner
                    .child
                    .as_ref()
                    .filter(|c| c.generation == generation)
                    .cloned()
            };
            match event {
                ChildEvent::Exited(why) => {
                    let Some(child) = current else { continue };
                    // Unexpected exit. Clear out any stragglers left in the group.
                    child.signal_group(libc::SIGKILL);
                    let mut inner = self.inner.lock().unwrap();
                    if inner
                        .started_at
                        .is_some_and(|t| t.elapsed() > Duration::from_secs(60))
                    {
                        inner.failures = 0;
                    }
                    inner.child = None;
                    inner.started_at = None;
                    inner.state = State::Failed;
                    inner.rss_kb = None;
                    inner.client_reqs.clear();
                    tracing::warn!(server = %self.name, "child {why}");
                    self.record_failure(&mut inner, format!("child {why}"));
                    self.alert(&mut inner, "crashed");
                }
                ChildEvent::Notification(msg) => {
                    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                    if method.ends_with("/list_changed")
                        && let Some(child) = &current
                    {
                        let init = self.cached_initialize().unwrap_or_default();
                        self.refresh_lists(child, init, false).await;
                    }
                    if method == "notifications/message" {
                        continue; // server logs; would interleave across sessions
                    }
                    let _ = self.broadcast.send(msg);
                }
            }
        }
    }

    /// Idle stop, memory guard and pending restarts. Runs until the server is retired.
    pub async fn reaper(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        let mut retire = self.retire.subscribe();
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = retire.changed() => return,
            }
            let (idle, rss_kb, pinned, restart) = {
                let mut inner = self.inner.lock().unwrap();
                inner
                    .sessions
                    .retain(|_, s| s.last_seen.elapsed() < Duration::from_secs(24 * 3600));
                let hour = Duration::from_secs(3600);
                while inner.calls.front().is_some_and(|c| c.at.elapsed() > hour) {
                    inner.calls.pop_front();
                }
                if inner.state != State::Ready || inner.inflight > 0 {
                    continue;
                }
                (
                    inner.last_activity.elapsed(),
                    inner.rss_kb,
                    inner.pinned,
                    inner.restart_pending,
                )
            };
            if restart {
                self.stop("config changed").await;
                if pinned {
                    let _ = self.ensure_child().await;
                }
                continue;
            }
            let idle_timeout = self.idle_timeout();
            let warm = pinned || self.cfg().keep_warm.as_deref().is_some_and(in_window);
            if !warm && !idle_timeout.is_zero() && idle >= idle_timeout {
                self.stop("idle").await;
                continue;
            }
            let limit = self.cfg().max_rss_mb;
            if let (Some(limit), Some(rss_kb)) = (limit, rss_kb)
                && rss_kb / 1024 > limit
            {
                tracing::warn!(server = %self.name, rss_mb = rss_kb / 1024, limit, "over memory limit");
                self.stop("memory limit").await;
            }
        }
    }

    // ---------- client traffic ----------

    pub fn touch_session(&self, session: &str, client: Option<&str>) {
        let mut inner = self.inner.lock().unwrap();
        let entry = inner
            .sessions
            .entry(session.to_string())
            .or_insert_with(|| SessionInfo {
                client: String::new(),
                last_seen: Instant::now(),
            });
        entry.last_seen = Instant::now();
        if let Some(c) = client {
            entry.client = c.to_string();
        }
    }

    pub fn end_session(&self, session: &str) {
        self.inner.lock().unwrap().sessions.remove(session);
    }

    pub async fn initialize(self: &Arc<Self>, id: &Value) -> Reply {
        let init = match self.cached_initialize() {
            Some(i) => i,
            None => match self.ensure_child().await {
                Ok(_) => self.cached_initialize().unwrap_or_default(),
                Err(e) => return rpc_error(id, -32603, e),
            },
        };
        let mut init = init;
        // Subscriptions are per client; one shared child can't scope them.
        if let Some(r) = init.pointer_mut("/capabilities/resources")
            && let Some(o) = r.as_object_mut()
        {
            o.remove("subscribe");
        }
        rpc_result(id, init)
    }

    pub fn client_notification(&self, session: &str, msg: &Value) {
        if msg.get("method").and_then(Value::as_str) != Some("notifications/cancelled") {
            return; // initialized, roots/list_changed: handled by mcpmux's own handshake
        }
        let Some(req) = msg.pointer("/params/requestId") else {
            return;
        };
        let key = (session.to_string(), token_key(req));
        let (upstream, child) = {
            let inner = self.inner.lock().unwrap();
            (inner.client_reqs.get(&key).copied(), inner.child.clone())
        };
        if let (Some(id), Some(child)) = (upstream, child) {
            child.cancel(id, "cancelled by client");
        }
    }

    /// Answer a client request: locally when possible, otherwise forwarded to the child.
    /// `progress` receives progress notifications when the client asked for them.
    pub async fn request(
        self: &Arc<Self>,
        session: &str,
        msg: Value,
        progress: Option<mpsc::Sender<Value>>,
    ) -> Reply {
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let has_cursor = msg.pointer("/params/cursor").is_some_and(|c| !c.is_null());
        match method {
            "initialize" => return self.initialize(&id).await,
            "ping" | "logging/setLevel" => return rpc_result(&id, json!({})),
            m if !has_cursor && CACHED_LISTS.iter().any(|(l, _)| *l == m) => {
                if let Some(r) = self.cached_list(m) {
                    return rpc_result(&id, self.filter_tools(m, r));
                }
                if self.has_cache() {
                    // Handshake cached but the server lacks this capability.
                    return rpc_error(&id, -32601, format!("{m} not supported"));
                }
            }
            "tools/call" => {
                let tool = msg.pointer("/params/name").and_then(Value::as_str);
                if tool.is_some_and(|t| self.cfg().deny_tools.iter().any(|d| d == t)) {
                    return rpc_error(&id, -32602, "tool is disabled by mcpmux");
                }
            }
            _ => {}
        }
        self.forward(session, msg, progress).await
    }

    async fn forward(
        self: &Arc<Self>,
        session: &str,
        mut msg: Value,
        progress: Option<mpsc::Sender<Value>>,
    ) -> Reply {
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let tool = (method == "tools/call")
            .then(|| msg.pointer("/params/name").and_then(Value::as_str))
            .flatten()
            .map(str::to_string);

        let child = match self.ensure_child().await {
            Ok(c) => c,
            Err(e) => {
                if let Some(t) = &tool {
                    self.record_call(t, session, 0, false);
                }
                return rpc_error(&id, -32603, e);
            }
        };

        let mut route = None;
        if let Some(token) = msg.pointer("/params/_meta/progressToken").cloned() {
            if let Some(sink) = progress {
                let upstream = format!("mux-{}", self.progress_seq.fetch_add(1, Ordering::SeqCst));
                child.add_progress_route(upstream.clone(), token, sink);
                msg["params"]["_meta"]["progressToken"] = json!(upstream);
                route = Some(upstream);
            } else if let Some(meta) = msg
                .pointer_mut("/params/_meta")
                .and_then(Value::as_object_mut)
            {
                meta.remove("progressToken");
            }
        }

        let (upstream_id, rx) = child.start_request(msg);
        let mut guard = InFlight::new(self.clone(), child, upstream_id, session, &id, tool, route);
        match rx.await {
            Ok(mut r) => {
                r["id"] = id.clone();
                let failed = r.get("error").is_some()
                    || r.pointer("/result/isError").and_then(Value::as_bool) == Some(true);
                guard.finish(!failed);
                r
            }
            Err(_) => {
                guard.finish(false);
                rpc_error(
                    &id,
                    -32603,
                    format!("{} exited during the request", self.name),
                )
            }
        }
    }

    fn record_call(&self, tool: &str, session: &str, ms: u64, ok: bool) {
        let mut inner = self.inner.lock().unwrap();
        inner.calls.push_back(CallRecord {
            at: Instant::now(),
            tool: tool.to_string(),
            session: session.chars().take(8).collect(),
            ms,
            ok,
        });
        while inner.calls.len() > 2000 {
            inner.calls.pop_front();
        }
        tracing::info!(server = %self.name, tool, ms, ok, "call");
    }

    // ---------- status ----------

    pub fn status(&self, rss: &HashMap<i32, u64>) -> ServerStatus {
        // Taken before the locks below: std's RwLock must not be read-locked twice per thread.
        let idle_timeout = self.idle_timeout();
        let log = self.log_path();
        let inner = self.inner.lock().unwrap();
        let cache = self.cache.lock().unwrap();
        let pid = inner.child.as_ref().map(|c| c.pid);
        let mut ms: Vec<u64> = inner.calls.iter().map(|c| c.ms).collect();
        ms.sort_unstable();
        let pct = |p: f64| (!ms.is_empty()).then(|| ms[((ms.len() - 1) as f64 * p) as usize]);
        let active = Duration::from_secs(30 * 60);
        let cfg = self.cfg();
        ServerStatus {
            name: self.name.clone(),
            state: format!("{:?}", inner.state).to_lowercase(),
            pid,
            rss_kb: pid.and_then(|p| rss.get(&p).copied()).or(inner.rss_kb),
            uptime_s: inner.started_at.map(|t| t.elapsed().as_secs()),
            idle_s: (inner.state == State::Ready).then(|| inner.last_activity.elapsed().as_secs()),
            idle_timeout_s: idle_timeout.as_secs(),
            inflight: inner.inflight,
            sessions: inner
                .sessions
                .values()
                .filter(|s| s.last_seen.elapsed() < active)
                .count(),
            clients: {
                let mut c: Vec<String> = inner
                    .sessions
                    .values()
                    .filter(|s| s.last_seen.elapsed() < active && !s.client.is_empty())
                    .map(|s| s.client.clone())
                    .collect();
                c.sort();
                c.dedup();
                c
            },
            calls_1h: inner.calls.len(),
            errors_1h: inner.calls.iter().filter(|c| !c.ok).count(),
            p50_ms: pct(0.5),
            p95_ms: pct(0.95),
            last_error: inner.last_error.clone(),
            retry_in_s: inner
                .retry_at
                .filter(|t| *t > Instant::now())
                .map(|t| (t - Instant::now()).as_secs() + 1),
            cached_tools: cache.as_ref().and_then(|c| {
                c.lists
                    .get("tools/list")
                    .and_then(|l| l.get("tools"))
                    .and_then(Value::as_array)
                    .map(Vec::len)
            }),
            recent: inner
                .calls
                .iter()
                .rev()
                .take(50)
                .map(|c| CallView {
                    ago_s: c.at.elapsed().as_secs(),
                    tool: c.tool.clone(),
                    session: c.session.clone(),
                    ms: c.ms,
                    ok: c.ok,
                })
                .collect(),
            log: log.display().to_string(),
            pinned: inner.pinned,
            keep_warm: cfg.keep_warm.clone(),
            restart_pending: inner.restart_pending,
            tunnel: cfg.requires_tcp.as_ref().map(|addr| Tunnel {
                addr: addr.clone(),
                up: inner.tunnel_up,
                hint: cfg.requires_tcp_hint.clone(),
                cmd: cfg.requires_tcp_cmd.clone(),
            }),
            last_rss_kb: inner.last_rss_kb,
            tool_tokens: cache.as_ref().and_then(|c| {
                let tools = c.lists.get("tools/list")?.get("tools")?.as_array()?;
                Some(
                    tools
                        .iter()
                        .filter(|t| {
                            t.get("name")
                                .and_then(Value::as_str)
                                .is_none_or(|n| !cfg.deny_tools.iter().any(|d| d == n))
                        })
                        .map(est_tokens)
                        .sum(),
                )
            }),
            cache_note: inner.cache_note.clone(),
            history: history(&inner),
        }
    }
}

/// Per-minute series for the last hour, oldest first.
fn history(inner: &Inner) -> History {
    let now = minute_now();
    let ago = |t: Instant| (t.elapsed().as_secs() / 60) as usize;
    let mut h = History {
        calls: vec![0; 60],
        errors: vec![0; 60],
        p95_ms: vec![0; 60],
        rss_mb: vec![0; 60],
    };
    let mut lat: Vec<Vec<u64>> = vec![vec![]; 60];
    for c in &inner.calls {
        let a = ago(c.at);
        if a < 60 {
            h.calls[59 - a] += 1;
            if !c.ok {
                h.errors[59 - a] += 1;
            }
            lat[59 - a].push(c.ms);
        }
    }
    for (i, mut l) in lat.into_iter().enumerate() {
        if !l.is_empty() {
            l.sort_unstable();
            h.p95_ms[i] = l[((l.len() - 1) as f64 * 0.95) as usize];
        }
    }
    for (m, mb) in &inner.rss_hist {
        let a = now.saturating_sub(*m) as usize;
        if a < 60 {
            h.rss_mb[59 - a] = *mb;
        }
    }
    h
}

/// Rough LLM token count of a tool definition: its JSON length / 4.
fn est_tokens(tool: &Value) -> u64 {
    (tool.to_string().len() as u64).div_ceil(4)
}

/// What changed between two cached handshakes, if the version or tool set did.
fn cache_diff(old: &Cache, new: &Cache) -> Option<String> {
    let ver = |c: &Cache| {
        c.initialize
            .pointer("/serverInfo/version")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string()
    };
    let names = |c: &Cache| -> std::collections::BTreeSet<String> {
        c.lists
            .get("tools/list")
            .and_then(|l| l.get("tools"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_string))
            .collect()
    };
    let (ov, nv) = (ver(old), ver(new));
    let (on, nn) = (names(old), names(new));
    let mut parts = vec![];
    if ov != nv {
        parts.push(format!("version {ov} → {nv}"));
    }
    let added = nn.difference(&on).count();
    let removed = on.difference(&nn).count();
    if added + removed > 0 {
        parts.push(format!("tools +{added} −{removed}"));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

async fn probe(addr: &str, timeout: Duration) -> bool {
    tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr))
        .await
        .is_ok_and(|r| r.is_ok())
}

/// Post a macOS notification. Title and body go through argv, never into the script.
fn notify(title: &str, body: &str) {
    let r = std::process::Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "on run argv",
            "-e",
            "display notification (item 2 of argv) with title (item 1 of argv)",
            "-e",
            "end run",
            title,
            body,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Ok(mut c) = r {
        std::thread::spawn(move || c.wait());
    }
}

/// Tracks one forwarded request. Dropping it before `finish` (client hung up) cancels the
/// upstream call.
struct InFlight {
    server: Arc<Server>,
    child: Arc<Child>,
    upstream_id: i64,
    key: (String, String),
    tool: Option<String>,
    route: Option<String>,
    started: Instant,
    outcome: Option<bool>,
}

impl InFlight {
    fn new(
        server: Arc<Server>,
        child: Arc<Child>,
        upstream_id: i64,
        session: &str,
        client_id: &Value,
        tool: Option<String>,
        route: Option<String>,
    ) -> Self {
        let key = (session.to_string(), token_key(client_id));
        {
            let mut inner = server.inner.lock().unwrap();
            inner.inflight += 1;
            inner.last_activity = Instant::now();
            inner.client_reqs.insert(key.clone(), upstream_id);
        }
        InFlight {
            server,
            child,
            upstream_id,
            key,
            tool,
            route,
            started: Instant::now(),
            outcome: None,
        }
    }

    fn finish(&mut self, ok: bool) {
        self.outcome = Some(ok);
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if self.outcome.is_none() {
            self.child.cancel(self.upstream_id, "client disconnected");
        }
        if let Some(r) = &self.route {
            self.child.remove_progress_route(r);
        }
        {
            let mut inner = self.server.inner.lock().unwrap();
            inner.inflight = inner.inflight.saturating_sub(1);
            inner.last_activity = Instant::now();
            inner.client_reqs.remove(&self.key);
        }
        if let Some(t) = &self.tool {
            let ms = self.started.elapsed().as_millis() as u64;
            self.server
                .record_call(t, &self.key.0, ms, self.outcome.unwrap_or(false));
        }
    }
}

/// SIGTERM the process group, then SIGKILL whatever is left after 5s.
pub async fn kill_group(child: &Child) {
    if !child.signal_group(libc::SIGTERM) {
        return;
    }
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if !child.group_exists() {
            return;
        }
    }
    tracing::warn!(pid = child.pid, "process group ignored SIGTERM; killing");
    child.signal_group(libc::SIGKILL);
}

/// RSS per process group, keyed by group id (our children are group leaders).
pub fn group_rss_kb() -> HashMap<i32, u64> {
    let mut out = HashMap::new();
    let Ok(o) = std::process::Command::new("/bin/ps")
        .args(["-axo", "pgid=,rss="])
        .output()
    else {
        return out;
    };
    for line in String::from_utf8_lossy(&o.stdout).lines() {
        let mut it = line.split_whitespace();
        if let (Some(Ok(g)), Some(Ok(r))) = (
            it.next().map(str::parse::<i32>),
            it.next().map(str::parse::<u64>),
        ) {
            *out.entry(g).or_default() += r;
        }
    }
    out
}

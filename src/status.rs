use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Status {
    pub pid: u32,
    pub uptime_s: u64,
    pub listen: String,
    /// Config file the daemon loaded; the TUI edits this one.
    #[serde(default)]
    pub config: String,
    pub servers: Vec<ServerStatus>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ServerStatus {
    pub name: String,
    pub state: String,
    pub pid: Option<i32>,
    pub rss_kb: Option<u64>,
    pub uptime_s: Option<u64>,
    /// Seconds since the last request finished, while running.
    pub idle_s: Option<u64>,
    pub idle_timeout_s: u64,
    pub inflight: usize,
    pub sessions: usize,
    pub clients: Vec<String>,
    pub calls_1h: usize,
    pub errors_1h: usize,
    pub p50_ms: Option<u64>,
    pub p95_ms: Option<u64>,
    pub last_error: Option<String>,
    pub retry_in_s: Option<u64>,
    pub cached_tools: Option<usize>,
    pub recent: Vec<CallView>,
    pub log: String,
    // Fields below were added after 0.1; `default` keeps a newer TUI working with an older daemon.
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub keep_warm: Option<String>,
    /// Config changed while running; restarts once in-flight calls drain.
    #[serde(default)]
    pub restart_pending: bool,
    #[serde(default)]
    pub tunnel: Option<Tunnel>,
    /// RSS of the most recent run, kept after the server stops (for the savings estimate).
    #[serde(default)]
    pub last_rss_kb: Option<u64>,
    /// Rough token cost of the advertised (non-denied) tool schemas: JSON chars / 4.
    #[serde(default)]
    pub tool_tokens: Option<u64>,
    /// Set when the last spawn advertised a different version or tool set than the cache.
    #[serde(default)]
    pub cache_note: Option<String>,
    /// Last 60 minutes, oldest first.
    #[serde(default)]
    pub history: History,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct History {
    pub calls: Vec<u64>,
    pub errors: Vec<u64>,
    pub p95_ms: Vec<u64>,
    pub rss_mb: Vec<u64>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Tunnel {
    pub addr: String,
    /// None until the first probe.
    pub up: Option<bool>,
    pub hint: Option<String>,
    pub cmd: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolView {
    pub name: String,
    pub description: String,
    pub tokens: u64,
    pub denied: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CallView {
    pub ago_s: u64,
    pub tool: String,
    pub session: String,
    pub ms: u64,
    pub ok: bool,
}

pub fn human_secs(s: u64) -> String {
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

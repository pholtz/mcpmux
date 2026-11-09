use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Status {
    pub pid: u32,
    pub uptime_s: u64,
    pub listen: String,
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

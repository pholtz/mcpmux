use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_idle")]
    pub idle_timeout: String,
    #[serde(default = "default_startup")]
    pub startup_timeout: String,
    /// PATH for children; launchd gives the daemon a minimal one.
    #[serde(default)]
    pub path: Vec<String>,
    /// Working directory for children (asdf reads `.tool-versions` from here). Defaults to $HOME.
    pub cwd: Option<String>,
    /// launchd label for `install`/`uninstall`; also names the plist.
    #[serde(default = "default_label")]
    pub label: String,
    /// macOS notification when a server crashes or fails to start (at most one per server per 10m).
    #[serde(default = "default_true")]
    pub notify: bool,
    /// Size at which a log is cleared, e.g. "10mb" (units b, kb, mb, gb).
    #[serde(default = "default_log_max_size")]
    pub log_max_size: String,
    /// At the limit, keep the old contents as `<name>.log.1` instead of deleting them.
    #[serde(default)]
    pub log_rollover: bool,
    #[serde(default)]
    pub servers: BTreeMap<String, ServerConfig>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, EnvValue>,
    /// `host:port` that must accept TCP before spawning (e.g. an SSH tunnel).
    pub requires_tcp: Option<String>,
    /// Appended to the error when `requires_tcp` isn't listening, e.g. "run `tunnel up`".
    pub requires_tcp_hint: Option<String>,
    /// Run (via `sh -c`) when a call finds `requires_tcp` closed, then wait up to 15s for it.
    pub requires_tcp_cmd: Option<String>,
    /// Local-time window like "09:00-18:00" during which the idle stop is skipped.
    pub keep_warm: Option<String>,
    #[serde(default)]
    pub deny_tools: Vec<String>,
    pub idle_timeout: Option<String>,
    pub max_rss_mb: Option<u64>,
    pub cwd: Option<String>,
    pub protocol_version: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[serde(untagged)]
pub enum EnvValue {
    Plain(String),
    Secret {
        secret_file: String,
        /// `{secret}` is replaced with the file's contents (trailing newline trimmed).
        template: Option<String>,
    },
}

fn default_listen() -> String {
    "127.0.0.1:7800".into()
}
fn default_idle() -> String {
    "10m".into()
}
fn default_log_max_size() -> String {
    "10mb".into()
}
fn default_true() -> bool {
    true
}
fn default_startup() -> String {
    "60s".into()
}
pub fn default_label() -> String {
    "local.mcpmux".into()
}

pub const DEFAULT_PROTOCOL: &str = "2025-06-18";

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        let addr = self.listen_addr()?;
        if !addr.ip().is_loopback() {
            bail!("listen must be a loopback address, got {addr}");
        }
        parse_duration(&self.idle_timeout).context("idle_timeout")?;
        parse_duration(&self.startup_timeout).context("startup_timeout")?;
        parse_size(&self.log_max_size).context("log_max_size")?;
        for (name, s) in &self.servers {
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                bail!("server name {name:?} must be [A-Za-z0-9_-]+");
            }
            if let Some(t) = &s.idle_timeout {
                parse_duration(t).with_context(|| format!("{name}.idle_timeout"))?;
            }
            if let Some(w) = &s.keep_warm {
                parse_window(w).with_context(|| format!("{name}.keep_warm"))?;
            }
            if let Some(t) = &s.requires_tcp {
                t.parse::<SocketAddr>()
                    .with_context(|| format!("{name}.requires_tcp must be ip:port"))?;
            }
        }
        Ok(())
    }

    pub fn listen_addr(&self) -> Result<SocketAddr> {
        self.listen
            .parse()
            .with_context(|| format!("listen {:?} must be ip:port", self.listen))
    }

    pub fn path_env(&self) -> Option<String> {
        (!self.path.is_empty()).then(|| {
            self.path
                .iter()
                .map(|p| expand_tilde(p))
                .collect::<Vec<_>>()
                .join(":")
        })
    }
}

impl ServerConfig {
    /// Resolve env values, reading secret files. Values never leave the child's environment.
    pub fn resolve_env(&self) -> Result<Vec<(String, String)>> {
        self.env
            .iter()
            .map(|(k, v)| {
                let val = match v {
                    EnvValue::Plain(s) => s.clone(),
                    EnvValue::Secret {
                        secret_file,
                        template,
                    } => {
                        let p = expand_tilde(secret_file);
                        let secret = std::fs::read_to_string(&p)
                            .with_context(|| format!("{k}: reading secret file {p}"))?;
                        let secret = secret.trim_end_matches(['\n', '\r']);
                        match template {
                            Some(t) => t.replace("{secret}", secret),
                            None => secret.to_string(),
                        }
                    }
                };
                Ok((k.clone(), val))
            })
            .collect()
    }

    pub fn args_expanded(&self) -> Vec<String> {
        self.args.iter().map(|a| expand_tilde(a)).collect()
    }

    /// Identifies the server build for cache validity. Contains no secret values.
    pub fn fingerprint(&self) -> String {
        let env: BTreeMap<_, _> = self
            .env
            .iter()
            .map(|(k, v)| match v {
                EnvValue::Plain(s) => (k.clone(), s.clone()),
                EnvValue::Secret { secret_file, .. } => (k.clone(), format!("<{secret_file}>")),
            })
            .collect();
        serde_json::json!({
            "command": self.command,
            "args": self.args,
            "env": env,
            "protocol": self.protocol_version,
        })
        .to_string()
    }
}

pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s == "0" {
        return Ok(Duration::ZERO);
    }
    let split = s
        .find(|c: char| !c.is_ascii_digit())
        .with_context(|| format!("duration {s:?} needs a unit (s, m, h)"))?;
    let (n, unit) = s.split_at(split);
    let n: u64 = n.parse().with_context(|| format!("bad duration {s:?}"))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        _ => bail!("duration {s:?}: unit must be s, m or h"),
    };
    Ok(Duration::from_secs(secs))
}

/// "10mb" → bytes. Units b, kb, mb, gb (powers of 1024), case-insensitive.
pub fn parse_size(s: &str) -> Result<u64> {
    let t = s.trim().to_ascii_lowercase();
    let split = t
        .find(|c: char| !c.is_ascii_digit())
        .with_context(|| format!("size {s:?} needs a unit (b, kb, mb, gb)"))?;
    let (n, unit) = t.split_at(split);
    let n: u64 = n.parse().with_context(|| format!("bad size {s:?}"))?;
    let mult: u64 = match unit.trim() {
        "b" => 1,
        "kb" => 1 << 10,
        "mb" => 1 << 20,
        "gb" => 1 << 30,
        _ => bail!("size {s:?}: unit must be b, kb, mb or gb"),
    };
    match n.checked_mul(mult) {
        Some(0) => bail!("size {s:?} must be more than 0"),
        Some(b) => Ok(b),
        None => bail!("size {s:?} is too large"),
    }
}

/// "HH:MM-HH:MM" → (start, end) in minutes after midnight. The window may wrap past midnight.
pub fn parse_window(s: &str) -> Result<(u32, u32)> {
    let hm = |t: &str| -> Result<u32> {
        let (h, m) = t
            .trim()
            .split_once(':')
            .with_context(|| format!("{t:?} must be HH:MM"))?;
        let (h, m): (u32, u32) = (h.parse()?, m.parse()?);
        if h > 23 || m > 59 {
            bail!("{t:?} is not a time of day");
        }
        Ok(h * 60 + m)
    };
    let (a, b) = s
        .split_once('-')
        .with_context(|| format!("{s:?} must look like 09:00-18:00"))?;
    Ok((hm(a)?, hm(b)?))
}

/// Whether local time now falls inside a `keep_warm` window.
pub fn in_window(spec: &str) -> bool {
    let Ok((start, end)) = parse_window(spec) else {
        return false;
    };
    let now = local_minutes();
    if start <= end {
        (start..end).contains(&now)
    } else {
        now >= start || now < end
    }
}

fn local_minutes() -> u32 {
    // SAFETY: localtime_r writes into the tm we own; a null time pointer is never passed.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        (tm.tm_hour * 60 + tm.tm_min) as u32
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .expect("HOME is not set")
}

pub fn expand_tilde(s: &str) -> String {
    if s == "~" {
        home().display().to_string()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home().join(rest).display().to_string()
    } else {
        s.to_string()
    }
}

pub struct Paths {
    pub config: PathBuf,
    pub token: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
    pub logs: PathBuf,
}

impl Paths {
    pub fn new(config_override: Option<PathBuf>) -> Self {
        let h = home();
        let cfg_dir = h.join(".config/mcpmux");
        Paths {
            config: config_override.unwrap_or_else(|| cfg_dir.join("config.toml")),
            token: cfg_dir.join("token"),
            state: h.join(".local/state/mcpmux"),
            cache: h.join(".cache/mcpmux"),
            logs: h.join("Library/Logs/mcpmux"),
        }
    }

    pub fn plist(&self, label: &str) -> PathBuf {
        home().join(format!("Library/LaunchAgents/{label}.plist"))
    }

    pub fn socket(&self) -> PathBuf {
        self.state.join("admin.sock")
    }

    pub fn read_token(&self) -> Result<String> {
        let t = std::fs::read_to_string(&self.token)
            .with_context(|| format!("reading {} (run `mcpmux install`)", self.token.display()))?;
        let t = t.trim().to_string();
        if t.len() < 32 {
            bail!("{} is too short", self.token.display());
        }
        Ok(t)
    }
}

pub fn ensure_private_dir(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(p).with_context(|| format!("creating {}", p.display()))?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

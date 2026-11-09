use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Deserialize, Debug, Clone)]
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
    #[serde(default)]
    pub servers: BTreeMap<String, ServerConfig>,
}

#[derive(Deserialize, Debug, Clone)]
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
    #[serde(default)]
    pub deny_tools: Vec<String>,
    pub idle_timeout: Option<String>,
    pub max_rss_mb: Option<u64>,
    pub cwd: Option<String>,
    pub protocol_version: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
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
fn default_startup() -> String {
    "60s".into()
}
pub fn default_label() -> String {
    "co.pholtz.mcpmux".into()
}

pub const DEFAULT_PROTOCOL: &str = "2025-06-18";

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
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

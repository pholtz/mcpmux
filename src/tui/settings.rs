//! Edit config.toml in place with `toml_edit`, keeping comments and layout, then ask the
//! daemon to reload. Used by the help modal's Settings tab, `:set`, and the tool inspector.

use crate::config::{Config, parse_duration, parse_size, parse_window};
use anyhow::{Context, Result, bail};
use std::path::Path;
use toml_edit::{Array, DocumentMut, Item, Value};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Duration,
    Int,
    Bool,
    Text,
    Window,
    List,
    Size,
}

pub struct Field {
    pub key: &'static str,
    pub kind: Kind,
    pub desc: &'static str,
    /// Default shown when the key is absent.
    pub default: &'static str,
}

pub const GLOBAL: &[Field] = &[
    Field {
        key: "idle_timeout",
        kind: Kind::Duration,
        desc: "stop a server after this long without calls (0 = never)",
        default: "10m",
    },
    Field {
        key: "startup_timeout",
        kind: Kind::Duration,
        desc: "how long a server may take to answer initialize",
        default: "60s",
    },
    Field {
        key: "notify",
        kind: Kind::Bool,
        desc: "macOS notification when a server crashes or fails to start",
        default: "true",
    },
    Field {
        key: "log_max_size",
        kind: Kind::Size,
        desc: "clear a log past this size (b, kb, mb, gb)",
        default: "10mb",
    },
    Field {
        key: "log_rollover",
        kind: Kind::Bool,
        desc: "at the limit keep old lines as <name>.log.1 instead of deleting",
        default: "false",
    },
];

pub const SERVER: &[Field] = &[
    Field {
        key: "idle_timeout",
        kind: Kind::Duration,
        desc: "overrides the global idle_timeout",
        default: "global",
    },
    Field {
        key: "keep_warm",
        kind: Kind::Window,
        desc: "local-time window with no idle stop, e.g. 09:00-18:00",
        default: "-",
    },
    Field {
        key: "max_rss_mb",
        kind: Kind::Int,
        desc: "restart when the process group grows past this",
        default: "-",
    },
    Field {
        key: "deny_tools",
        kind: Kind::List,
        desc: "hidden from tools/list and refused (comma-separated)",
        default: "-",
    },
    Field {
        key: "requires_tcp",
        kind: Kind::Text,
        desc: "ip:port that must accept TCP before spawning",
        default: "-",
    },
    Field {
        key: "requires_tcp_cmd",
        kind: Kind::Text,
        desc: "run when requires_tcp is closed, then wait 15s",
        default: "-",
    },
    Field {
        key: "requires_tcp_hint",
        kind: Kind::Text,
        desc: "appended to the tunnel-down error",
        default: "-",
    },
];

pub fn field(server: bool, key: &str) -> Option<&'static Field> {
    let fields = if server { SERVER } else { GLOBAL };
    fields.iter().find(|f| f.key == key)
}

pub fn load(path: &Path) -> Result<DocumentMut> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    text.parse::<DocumentMut>()
        .with_context(|| format!("parsing {}", path.display()))
}

fn table<'a>(doc: &'a DocumentMut, server: Option<&str>) -> Option<&'a toml_edit::Table> {
    match server {
        None => Some(doc.as_table()),
        Some(s) => doc.get("servers")?.get(s)?.as_table(),
    }
}

/// Current value as the user would type it, or None when unset.
pub fn get(doc: &DocumentMut, server: Option<&str>, key: &str) -> Option<String> {
    let v = table(doc, server)?.get(key)?.as_value()?;
    Some(match v {
        Value::String(s) => s.value().clone(),
        Value::Array(a) => a
            .iter()
            .map(|x| {
                x.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| x.to_string())
            })
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string().trim().to_string(),
    })
}

fn parse(kind: Kind, raw: &str) -> Result<Value> {
    let raw = raw.trim();
    Ok(match kind {
        Kind::Duration => {
            parse_duration(raw)?;
            raw.into()
        }
        Kind::Window => {
            parse_window(raw)?;
            raw.into()
        }
        Kind::Size => {
            parse_size(raw)?;
            raw.into()
        }
        Kind::Int => raw
            .parse::<i64>()
            .with_context(|| format!("{raw:?} is not a number"))?
            .into(),
        Kind::Bool => match raw {
            "true" | "on" | "yes" => true.into(),
            "false" | "off" | "no" => false.into(),
            _ => bail!("{raw:?}: use true or false"),
        },
        Kind::Text => raw.into(),
        Kind::List => {
            let mut a = Array::new();
            for x in raw.split(',').map(str::trim).filter(|x| !x.is_empty()) {
                a.push(x);
            }
            Value::Array(a)
        }
    })
}

/// Set (or with `None`, remove) one key, validate the whole file, and write it atomically.
/// Returns "key: old → new".
pub fn set(path: &Path, server: Option<&str>, key: &str, raw: Option<&str>) -> Result<String> {
    let f = field(server.is_some(), key).with_context(|| {
        let names: Vec<&str> = (if server.is_some() { SERVER } else { GLOBAL })
            .iter()
            .map(|f| f.key)
            .collect();
        format!("unknown setting {key:?}; one of {}", names.join(", "))
    })?;
    let mut doc = load(path)?;
    let old = get(&doc, server, key).unwrap_or_else(|| f.default.to_string());
    let t = match server {
        None => doc.as_table_mut(),
        Some(s) => doc
            .get_mut("servers")
            .and_then(|x| x.get_mut(s))
            .and_then(Item::as_table_mut)
            .with_context(|| format!("no [servers.{s}] table"))?,
    };
    match raw.filter(|r| !r.trim().is_empty()) {
        None => {
            t.remove(key);
        }
        Some(raw) => {
            let mut v = parse(f.kind, raw)?;
            // Replace in place: comments above the key live in the key's decor, and the
            // value's decor holds its spacing and trailing comment.
            match t.get_mut(key) {
                Some(item) => {
                    if let Some(old) = item.as_value() {
                        *v.decor_mut() = old.decor().clone();
                    }
                    *item = Item::Value(v);
                }
                None => {
                    t.insert(key, Item::Value(v));
                }
            }
        }
    }
    let text = doc.to_string();
    Config::parse(&text).context("the change would make the config invalid")?;
    write_atomic(path, &text)?;
    let new = get(&doc, server, key).unwrap_or_else(|| f.default.to_string());
    let scope = server.map(|s| format!("{s}.")).unwrap_or_default();
    Ok(format!("{scope}{key}: {old} → {new}"))
}

/// Add or remove one tool from a server's deny_tools.
pub fn toggle_deny(path: &Path, server: &str, tool: &str) -> Result<String> {
    let doc = load(path)?;
    let mut list: Vec<String> = get(&doc, Some(server), "deny_tools")
        .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
        .unwrap_or_default();
    list.retain(|x| !x.is_empty());
    let denied = if let Some(i) = list.iter().position(|x| x == tool) {
        list.remove(i);
        false
    } else {
        list.push(tool.to_string());
        true
    };
    set(path, Some(server), "deny_tools", Some(&list.join(",")))?;
    Ok(format!(
        "{server}: {tool} {}",
        if denied { "denied" } else { "allowed" }
    ))
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    if let Ok(m) = std::fs::metadata(path) {
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(m.mode() & 0o777))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Editing must not lose the user's comments: the config is hand-maintained.
    #[test]
    fn set_keeps_comments() {
        let dir = std::env::temp_dir().join(format!("mcpmux-set-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(
            &p,
            "# top\nidle_timeout = \"10m\"   # why\n\n[servers.a]\ncommand = \"x\"\n# note\ndeny_tools = [\"b\"]\n",
        )
        .unwrap();
        assert_eq!(
            set(&p, None, "idle_timeout", Some("5m")).unwrap(),
            "idle_timeout: 10m → 5m"
        );
        toggle_deny(&p, "a", "c").unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("idle_timeout = \"5m\"   # why"), "{text}");
        assert!(text.contains("# top") && text.contains("# note"), "{text}");
        assert!(text.contains("deny_tools = [\"b\", \"c\"]"), "{text}");
        assert!(set(&p, None, "idle_timeout", Some("5 parsecs")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

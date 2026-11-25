//! Command registry. Every action has an id (run it with `:id [arg]`), optional keys, and the
//! view it belongs to. The `?` help lists the registry, so keys and docs can't drift apart.

use super::{Action, App, logview::Prompt};
use crate::admin;
use crossterm::event::KeyCode;
use std::{
    fs::File,
    process::{Command as Proc, Stdio},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    /// Any view.
    Global,
    /// Server commands: keys work on the dashboard, `:` works everywhere.
    Dash,
    Log,
    Tools,
}

pub struct Command {
    pub id: &'static str,
    pub scope: Scope,
    pub keys: &'static [KeyCode],
    /// Shown after the id; a leading `<` means the argument is required.
    pub args: &'static str,
    pub desc: &'static str,
    pub run: fn(&mut App, &str) -> Action,
}

use KeyCode::{Char as C, Enter};

pub const COMMANDS: &[Command] = &[
    Command {
        id: "help",
        scope: Scope::Global,
        keys: &[C('?')],
        args: "",
        desc: "keys, settings and guide",
        run: |a, _| {
            a.open_help();
            Action::None
        },
    },
    Command {
        id: "reload",
        scope: Scope::Global,
        keys: &[C('R')],
        args: "",
        desc: "re-read config.toml and apply it",
        run: |a, _| {
            a.reload_job(None);
            Action::None
        },
    },
    Command {
        id: "set",
        scope: Scope::Global,
        keys: &[],
        args: "<[server.]key> [value]",
        desc: "change a setting; no value resets it",
        run: set,
    },
    Command {
        id: "check",
        scope: Scope::Global,
        keys: &[],
        args: "[--live]",
        desc: "validate config, secrets, commands, tunnels",
        run: check,
    },
    Command {
        id: "clients",
        scope: Scope::Global,
        keys: &[],
        args: "<claude|pi>",
        desc: "copy client setup to the clipboard",
        run: clients,
    },
    Command {
        id: "daemon-log",
        scope: Scope::Global,
        keys: &[C('L')],
        args: "",
        desc: "daemon log",
        run: |a, _| {
            a.open_log(None);
            Action::None
        },
    },
    Command {
        id: "quit",
        scope: Scope::Global,
        keys: &[C('q')],
        args: "",
        desc: "quit",
        run: |_, _| Action::Quit,
    },
    Command {
        id: "start",
        scope: Scope::Dash,
        keys: &[C('s')],
        args: "[server|all]",
        desc: "start now instead of on first call",
        run: |a, x| control(a, "start", x),
    },
    Command {
        id: "stop",
        scope: Scope::Dash,
        keys: &[C('x')],
        args: "[server|all]",
        desc: "stop (restarts on next call); clears a failure",
        run: |a, x| control(a, "stop", x),
    },
    Command {
        id: "restart",
        scope: Scope::Dash,
        keys: &[C('r')],
        args: "[server|all]",
        desc: "stop then start",
        run: |a, x| control(a, "restart", x),
    },
    Command {
        id: "log",
        scope: Scope::Dash,
        keys: &[C('l'), Enter],
        args: "[server]",
        desc: "server stderr log",
        run: log,
    },
    Command {
        id: "tools",
        scope: Scope::Dash,
        keys: &[C('t')],
        args: "[server]",
        desc: "tool inspector: schemas, token cost, deny",
        run: tools,
    },
    Command {
        id: "pin",
        scope: Scope::Dash,
        keys: &[C('p')],
        args: "[server|all]",
        desc: "toggle pin: start and skip idle stops",
        run: pin,
    },
    Command {
        id: "unpin",
        scope: Scope::Dash,
        keys: &[],
        args: "[server|all]",
        desc: "undo pin",
        run: |a, x| control(a, "unpin", x),
    },
    Command {
        id: "tunnel",
        scope: Scope::Dash,
        keys: &[C('T')],
        args: "[server]",
        desc: "run requires_tcp_cmd and wait for the port",
        run: |a, x| control(a, "tunnel", x),
    },
    Command {
        id: "search",
        scope: Scope::Log,
        keys: &[C('/')],
        args: "[text]",
        desc: "find (n / N next / previous)",
        run: |a, x| log_prompt(a, Prompt::Search, x),
    },
    Command {
        id: "filter",
        scope: Scope::Log,
        keys: &[C('&')],
        args: "[text]",
        desc: "show only matching lines",
        run: |a, x| log_prompt(a, Prompt::Filter, x),
    },
    Command {
        id: "level",
        scope: Scope::Log,
        keys: &[C('v')],
        args: "",
        desc: "cycle all → warn+ → error",
        run: |a, _| with_log(a, |v| v.cycle_level()),
    },
    Command {
        id: "follow",
        scope: Scope::Log,
        keys: &[C('f')],
        args: "",
        desc: "follow the tail / pause",
        run: |a, _| with_log(a, |v| v.toggle_follow()),
    },
    Command {
        id: "wrap",
        scope: Scope::Log,
        keys: &[C('w')],
        args: "",
        desc: "wrap long lines (←/→ scroll when off)",
        run: |a, _| with_log(a, |v| v.toggle_wrap()),
    },
    Command {
        id: "pager",
        scope: Scope::Log,
        keys: &[C('o')],
        args: "",
        desc: "open in $PAGER (default less)",
        run: |a, _| {
            a.log
                .as_ref()
                .map_or(Action::None, |v| Action::Pager(v.path.clone()))
        },
    },
    Command {
        id: "copy",
        scope: Scope::Log,
        keys: &[C('y')],
        args: "",
        desc: "copy the visible lines",
        run: copy_visible,
    },
    Command {
        id: "deny",
        scope: Scope::Tools,
        keys: &[C(' ')],
        args: "",
        desc: "deny / allow the selected tool",
        run: |a, _| {
            super::tools::toggle(a);
            Action::None
        },
    },
];

/// Commands usable from `view`: global ones, server ones, and the view's own.
pub fn available(view: Scope) -> impl Iterator<Item = &'static Command> {
    COMMANDS
        .iter()
        .filter(move |c| matches!(c.scope, Scope::Global | Scope::Dash) || c.scope == view)
}

/// Key bindings fire only in their own view (or anywhere, for global ones).
pub fn by_key(view: Scope, key: KeyCode) -> Option<&'static Command> {
    COMMANDS
        .iter()
        .filter(|c| c.scope == Scope::Global || c.scope == view)
        .find(|c| c.keys.contains(&key))
}

pub fn key_label(k: &KeyCode) -> String {
    match k {
        C(' ') => "space".into(),
        C(c) => c.to_string(),
        Enter => "enter".into(),
        other => format!("{other:?}").to_lowercase(),
    }
}

pub fn exec(app: &mut App, line: &str) -> Action {
    let line = line.trim().trim_start_matches(':');
    let (id, arg) = line.split_once(' ').unwrap_or((line, ""));
    if id.is_empty() {
        return Action::None;
    }
    match available(app.view()).find(|c| c.id == id) {
        Some(c) => (c.run)(app, arg.trim()),
        None => {
            app.note(format!("unknown command :{id} (? lists them)"));
            Action::None
        }
    }
}

/// Tab completion: the command id, then a server name for server commands.
pub fn complete(app: &App, line: &str) -> (String, Option<String>) {
    let pick = |prefix: &str, options: Vec<String>| -> (Option<String>, Option<String>) {
        let hits: Vec<String> = options
            .into_iter()
            .filter(|o| o.starts_with(prefix))
            .collect();
        match hits.len() {
            0 => (None, None),
            1 => (Some(hits[0].clone()), None),
            _ => {
                let common = hits.iter().skip(1).fold(hits[0].clone(), |acc, h| {
                    acc.chars()
                        .zip(h.chars())
                        .take_while(|(a, b)| a == b)
                        .map(|(a, _)| a)
                        .collect()
                });
                (Some(common), Some(hits.join("  ")))
            }
        }
    };
    match line.split_once(' ') {
        None => {
            let ids = available(app.view()).map(|c| c.id.to_string()).collect();
            match pick(line, ids) {
                (Some(id), None) => (format!("{id} "), None),
                (Some(id), hint) => (id, hint),
                _ => (line.to_string(), None),
            }
        }
        Some((id, arg)) => {
            let Some(c) = COMMANDS.iter().find(|c| c.id == id) else {
                return (line.to_string(), None);
            };
            let mut options: Vec<String> = app.servers().iter().map(|s| s.name.clone()).collect();
            if c.args.contains("all") {
                options.push("all".into());
            }
            if c.id == "clients" {
                options = vec!["claude".into(), "pi".into()];
            }
            if c.id == "set" {
                options = super::settings::GLOBAL
                    .iter()
                    .map(|f| f.key.to_string())
                    .collect();
                for s in app.servers() {
                    options.extend(
                        super::settings::SERVER
                            .iter()
                            .map(|f| format!("{}.{}", s.name, f.key)),
                    );
                }
            }
            if !c.args.starts_with('[') && !c.args.starts_with('<') {
                return (line.to_string(), None);
            }
            match pick(arg.trim_start(), options) {
                (Some(a), hint) => (format!("{id} {a}"), hint),
                _ => (line.to_string(), None),
            }
        }
    }
}

/// `""` → the selected server; `"all"` stays "all".
fn target(app: &mut App, arg: &str) -> Option<String> {
    if !arg.is_empty() {
        return Some(arg.to_string());
    }
    let name = app.selected().map(|s| s.name.clone());
    if name.is_none() {
        app.note("no server selected".into());
    }
    name
}

fn control(app: &mut App, cmd: &'static str, arg: &str) -> Action {
    if let Some(name) = target(app, arg) {
        let socket = app.socket.clone();
        let label = format!("{cmd} {name}");
        app.job(label.clone(), move || {
            admin::call(&socket, cmd, Some(&name))?;
            Ok(format!("{label}: ok"))
        });
    }
    Action::None
}

fn pin(app: &mut App, arg: &str) -> Action {
    let pinned = match arg {
        "all" => false,
        "" => app.selected().is_some_and(|s| s.pinned),
        name => app.servers().iter().any(|s| s.name == name && s.pinned),
    };
    control(app, if pinned { "unpin" } else { "pin" }, arg)
}

fn log(app: &mut App, arg: &str) -> Action {
    if arg.is_empty() && app.selected().is_none() {
        app.open_log(None);
        return Action::None;
    }
    match target(app, arg) {
        Some(name) if app.servers().iter().any(|s| s.name == name) => app.open_log(Some(&name)),
        Some(name) => app.note(format!("unknown server {name:?}")),
        None => app.open_log(None),
    }
    Action::None
}

fn tools(app: &mut App, arg: &str) -> Action {
    if let Some(name) = target(app, arg) {
        app.fetch_tools(name, true);
    }
    Action::None
}

fn set(app: &mut App, arg: &str) -> Action {
    let (key, value) = arg.split_once(' ').unwrap_or((arg, ""));
    if key.is_empty() {
        app.open_settings();
        return Action::None;
    }
    let (server, key) = match key.rsplit_once('.') {
        Some((s, k)) => (Some(s), k),
        None => (None, key),
    };
    let value = (!value.trim().is_empty()).then_some(value.trim());
    match super::settings::set(&app.config, server, key, value) {
        Ok(change) => app.reload_job(Some(change)),
        Err(e) => app.note(format!("{e:#}")),
    }
    Action::None
}

/// Run this binary's `check` into a file under the state dir, then open it as a log.
fn check(app: &mut App, arg: &str) -> Action {
    let live = arg.contains("live");
    let out = app.state_dir.join("check.log");
    let config = app.config.clone();
    let tx = app.tx.clone();
    app.note(format!(
        "running check{}…",
        if live { " --live" } else { "" }
    ));
    std::thread::spawn(move || {
        let run = || -> anyhow::Result<()> {
            let f = File::create(&out)?;
            let mut cmd = Proc::new(std::env::current_exe()?);
            cmd.arg("--config").arg(&config).arg("check");
            if live {
                cmd.arg("--live");
            }
            cmd.stdin(Stdio::null())
                .stdout(f.try_clone()?)
                .stderr(f)
                .status()?;
            Ok(())
        };
        let _ = tx.send(match run() {
            Ok(()) => super::Msg::Open {
                name: "check".into(),
                path: out,
            },
            Err(e) => super::Msg::Note(format!("check: {e:#}")),
        });
    });
    Action::None
}

fn clients(app: &mut App, arg: &str) -> Action {
    if !matches!(arg, "claude" | "pi") {
        app.note(":clients claude or :clients pi".into());
        return Action::None;
    }
    let config = app.config.clone();
    let client = arg.to_string();
    app.job(format!("clients {client}"), move || {
        let out = Proc::new(std::env::current_exe()?)
            .arg("--config")
            .arg(&config)
            .args(["clients", &client])
            .stderr(Stdio::inherit())
            .output()?;
        anyhow::ensure!(out.status.success(), "mcpmux clients failed");
        let text = String::from_utf8_lossy(&out.stdout);
        super::copy(&text)?;
        Ok(format!(
            "copied {} lines of {client} setup",
            text.lines().count()
        ))
    });
    Action::None
}

fn with_log(app: &mut App, f: impl FnOnce(&mut super::logview::LogView)) -> Action {
    if let Some(v) = app.log.as_mut() {
        f(v);
    }
    Action::None
}

fn log_prompt(app: &mut App, kind: Prompt, arg: &str) -> Action {
    with_log(app, |v| {
        v.open_prompt(kind);
        if !arg.is_empty() {
            v.prompt = Some((kind, arg.to_string()));
            v.submit_prompt();
        }
    })
}

fn copy_visible(app: &mut App, _: &str) -> Action {
    with_log(app, |v| {
        let text = v.visible_text();
        v.note = Some(match super::copy(&text) {
            Ok(()) => format!("copied {} lines", text.lines().count()),
            Err(e) => format!("copy failed: {e}"),
        });
    })
}

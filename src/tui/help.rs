//! `?` modal: Keys (the command registry, filterable and runnable), Settings (edit
//! config.toml), and Guide (what the current view and server states mean).

use super::{
    Action, App,
    commands::{self, Command, Scope},
    settings::{self, Field, Kind},
};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::Line,
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Tabs, Wrap},
};
use toml_edit::DocumentMut;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Keys,
    Settings,
    Guide,
}

const TABS: [Tab; 3] = [Tab::Keys, Tab::Settings, Tab::Guide];

pub struct Help {
    pub tab: Tab,
    /// The view help was opened from.
    view: Scope,
    filter: String,
    table: TableState,
    /// Settings target: the dashboard's selected server.
    server: Option<String>,
    doc: Result<DocumentMut, String>,
    /// Value being typed for the selected setting.
    edit: Option<String>,
    scroll: u16,
}

impl Help {
    pub fn new(app: &App) -> Self {
        let mut table = TableState::default();
        table.select(Some(0));
        Help {
            tab: Tab::Keys,
            view: app.view(),
            filter: String::new(),
            table,
            server: app.selected().map(|s| s.name.clone()),
            doc: settings::load(&app.config).map_err(|e| format!("{e:#}")),
            edit: None,
            scroll: 0,
        }
    }

    fn commands(&self) -> Vec<&'static Command> {
        let f = self.filter.to_lowercase();
        commands::available(self.view)
            .filter(|c| fuzzy(c.id, &f) || c.desc.to_lowercase().contains(&f))
            .collect()
    }

    fn fields(&self) -> Vec<(Option<&str>, &'static Field)> {
        let mut v: Vec<(Option<&str>, &'static Field)> =
            settings::GLOBAL.iter().map(|f| (None, f)).collect();
        if let Some(s) = self.server.as_deref() {
            v.extend(settings::SERVER.iter().map(|f| (Some(s), f)));
        }
        v
    }

    fn rows(&self) -> usize {
        match self.tab {
            Tab::Keys => self.commands().len(),
            Tab::Settings => self.fields().len(),
            Tab::Guide => 0,
        }
    }

    fn value(&self, server: Option<&str>, f: &Field) -> Option<String> {
        self.doc
            .as_ref()
            .ok()
            .and_then(|d| settings::get(d, server, f.key))
    }
}

/// Subsequence match, so `rst` finds `restart`.
fn fuzzy(hay: &str, needle: &str) -> bool {
    let hay = hay.to_lowercase();
    let mut it = hay.chars();
    needle.chars().all(|c| it.any(|h| h == c))
}

fn keys(c: &Command) -> String {
    c.keys
        .iter()
        .map(commands::key_label)
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn key(app: &mut App, k: KeyEvent) -> Action {
    let Some(mut h) = app.help.take() else {
        return Action::None;
    };
    let action = handle(app, &mut h, k);
    if let Some(next) = action {
        return next;
    }
    app.help = Some(h);
    Action::None
}

/// `Some` closes the modal.
fn handle(app: &mut App, h: &mut Help, k: KeyEvent) -> Option<Action> {
    if let Some(text) = h.edit.as_mut() {
        match k.code {
            KeyCode::Esc => h.edit = None,
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(c) => text.push(c),
            KeyCode::Enter => {
                let value = h.edit.take().unwrap_or_default();
                save(app, h, Some(&value));
            }
            _ => {}
        }
        return None;
    }
    let n = h.rows();
    let sel = h.table.selected().unwrap_or(0);
    match k.code {
        KeyCode::Esc if h.tab == Tab::Keys && !h.filter.is_empty() => h.filter.clear(),
        KeyCode::Esc | KeyCode::Char('?') => return Some(Action::None),
        KeyCode::Tab | KeyCode::BackTab => {
            let i = TABS.iter().position(|t| *t == h.tab).unwrap_or(0);
            let step = if k.code == KeyCode::Tab {
                1
            } else {
                TABS.len() - 1
            };
            h.tab = TABS[(i + step) % TABS.len()];
            h.table.select(Some(0));
            h.scroll = 0;
        }
        KeyCode::Down if h.tab == Tab::Guide => h.scroll = h.scroll.saturating_add(1),
        KeyCode::Up if h.tab == Tab::Guide => h.scroll = h.scroll.saturating_sub(1),
        KeyCode::Down if n > 0 => h.table.select(Some((sel + 1) % n)),
        KeyCode::Up if n > 0 => h.table.select(Some((sel + n - 1) % n)),
        _ => match h.tab {
            Tab::Keys => return keys_tab(app, h, k),
            Tab::Settings => settings_tab(app, h, k),
            Tab::Guide => {}
        },
    }
    None
}

fn keys_tab(app: &mut App, h: &mut Help, k: KeyEvent) -> Option<Action> {
    match k.code {
        KeyCode::Enter => {
            let c = h.commands().get(h.table.selected().unwrap_or(0)).copied()?;
            if c.args.starts_with('<') {
                app.cmdline = Some(format!("{} ", c.id));
                return Some(Action::None);
            }
            return Some((c.run)(app, ""));
        }
        KeyCode::Backspace => {
            h.filter.pop();
        }
        KeyCode::Char(c) => h.filter.push(c),
        _ => return None,
    }
    h.table.select(Some(0));
    None
}

fn settings_tab(app: &mut App, h: &mut Help, k: KeyEvent) {
    let fields = h.fields();
    let n = fields.len();
    let sel = h.table.selected().unwrap_or(0);
    let Some(&(server, f)) = fields.get(sel) else {
        return;
    };
    let current = h.value(server, f);
    match k.code {
        KeyCode::Char('j') if n > 0 => h.table.select(Some((sel + 1) % n)),
        KeyCode::Char('k') if n > 0 => h.table.select(Some((sel + n - 1) % n)),
        KeyCode::Enter | KeyCode::Char(' ') if f.kind == Kind::Bool => {
            let on = current.as_deref().unwrap_or(f.default) == "true";
            save(app, h, Some(if on { "false" } else { "true" }));
        }
        KeyCode::Enter | KeyCode::Char('e') => h.edit = Some(current.unwrap_or_default()),
        KeyCode::Char('d') => save(app, h, None),
        _ => {}
    }
}

fn save(app: &mut App, h: &mut Help, value: Option<&str>) {
    let fields = h.fields();
    let Some(&(server, f)) = fields.get(h.table.selected().unwrap_or(0)) else {
        return;
    };
    let server = server.map(str::to_string);
    match settings::set(&app.config, server.as_deref(), f.key, value) {
        Ok(change) => app.reload_job(Some(change)),
        Err(e) => app.note(format!("{e:#}")),
    }
    h.doc = settings::load(&app.config).map_err(|e| format!("{e:#}"));
}

pub fn draw(f: &mut Frame, app: &mut App, area: Rect) {
    let msg = super::message(app).map(str::to_string);
    let Some(h) = app.help.as_mut() else {
        return;
    };
    let area = area.inner(Margin {
        horizontal: area.width / 12,
        vertical: area.height / 10,
    });
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Cyan))
        .title(" help ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [tabs, body, foot] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(inner);
    let i = TABS.iter().position(|t| *t == h.tab).unwrap_or(0);
    f.render_widget(
        Tabs::new(["Keys", "Settings", "Guide"])
            .select(i)
            .highlight_style(
                Style::new()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED),
            ),
        tabs,
    );

    let hint = match h.tab {
        Tab::Keys => "type to filter · ↑↓ select · enter runs · tab next tab · esc close",
        Tab::Settings if h.edit.is_some() => "enter saves and reloads · esc cancels",
        Tab::Settings => {
            "↑↓ select · enter edit (toggles booleans) · d reset to default · tab next · esc close"
        }
        Tab::Guide => "↑↓ scroll · tab next tab · esc close",
    };
    let footer = match msg {
        Some(m) => Line::from(m.yellow()),
        None => Line::from(hint.dark_gray()),
    };
    f.render_widget(footer, foot);

    match h.tab {
        Tab::Keys => {
            let [filter, list] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(body);
            f.render_widget(
                Line::from(vec![
                    "filter: ".dark_gray(),
                    h.filter.clone().into(),
                    "█".dark_gray(),
                ]),
                filter,
            );
            let rows: Vec<Row> = h
                .commands()
                .iter()
                .map(|c| {
                    Row::new(vec![
                        Cell::from(keys(c)).style(Style::new().fg(Color::Cyan)),
                        Cell::from(format!(":{} {}", c.id, c.args)),
                        Cell::from(c.desc).style(Style::new().fg(Color::Gray)),
                    ])
                })
                .collect();
            let table = Table::new(
                rows,
                [
                    Constraint::Length(10),
                    Constraint::Length(32),
                    Constraint::Min(10),
                ],
            )
            .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
            f.render_stateful_widget(table, list, &mut h.table);
        }
        Tab::Settings => {
            let [list, edit] = Layout::vertical([
                Constraint::Min(1),
                Constraint::Length(if h.edit.is_some() { 2 } else { 0 }),
            ])
            .areas(body);
            let doc_err = h.doc.as_ref().err().cloned();
            let rows: Vec<Row> = h
                .fields()
                .iter()
                .map(|&(server, fd)| {
                    let v = h.value(server, fd);
                    let set = v.is_some();
                    Row::new(vec![
                        Cell::from(server.unwrap_or("global").to_string())
                            .style(Style::new().fg(Color::DarkGray)),
                        Cell::from(fd.key),
                        Cell::from(v.unwrap_or_else(|| fd.default.to_string())).style(if set {
                            Style::new().fg(Color::Cyan)
                        } else {
                            Style::new().fg(Color::DarkGray)
                        }),
                        Cell::from(fd.desc).style(Style::new().fg(Color::Gray)),
                    ])
                })
                .collect();
            let title = match doc_err {
                Some(e) => format!(" {e} "),
                None => format!(" {} ", app_config_name(&app.config)),
            };
            let table = Table::new(
                rows,
                [
                    Constraint::Length(16),
                    Constraint::Length(18),
                    Constraint::Length(24),
                    Constraint::Min(10),
                ],
            )
            .block(Block::default().title(title.dark_gray()))
            .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
            f.render_stateful_widget(table, list, &mut h.table);
            if let Some(text) = &h.edit {
                f.render_widget(
                    Line::from(vec![
                        "new value: ".yellow(),
                        text.clone().into(),
                        "█".dark_gray(),
                    ]),
                    Rect {
                        y: edit.y + 1,
                        height: 1,
                        ..edit
                    },
                );
            }
        }
        Tab::Guide => {
            f.render_widget(
                Paragraph::new(guide(h.view))
                    .wrap(Wrap { trim: false })
                    .scroll((h.scroll, 0)),
                body,
            );
        }
    }
}

fn app_config_name(p: &std::path::Path) -> String {
    p.display()
        .to_string()
        .replace(&crate::config::home().display().to_string(), "~")
}

fn guide(view: Scope) -> Vec<Line<'static>> {
    let h = |s: &'static str| Line::from(s.bold());
    let p = |s: &'static str| Line::from(s);
    let mut out = match view {
        Scope::Log => vec![
            h("Log viewer"),
            p(
                "Follows the file like tail -f until you scroll; f resumes. Lines are drawn without borders, so mouse selection copies clean text.",
            ),
            p(
                "j/k ↑↓ line · space/b ctrl-d/u page · g/G top/bottom · ←/→ pan when wrap is off · n/N next/previous match",
            ),
            p(
                "& filter keeps only matching lines; v cycles all → warn+ → error. The header shows active filters. Esc clears the search, then the filters, then closes.",
            ),
            p(
                "Tab swaps between the server's stderr and the daemon log. Past log_max_size (default 10mb) a log is cleared, or kept as <name>.log.1 with log_rollover.",
            ),
        ],
        Scope::Tools => vec![
            h("Tool inspector"),
            p(
                "Every tool the server advertises, with a rough token cost of its schema (JSON characters / 4). Agents load these schemas into context at the start of each session, so big, unused tools are a standing cost.",
            ),
            p(
                "Space adds or removes the tool from deny_tools in config.toml and reloads: denied tools vanish from tools/list and calls to them are refused. Connected clients are told the list changed.",
            ),
        ],
        _ => vec![
            h("Dashboard"),
            p(
                "One row per configured server. Servers start on their first call and stop after idle_timeout with no calls; a session never notices.",
            ),
            p(
                "IDLE STOP counts down to the idle stop, or shows pinned / warm when it's skipped. SESS is connected client sessions; LIVE is calls in flight. CALLS, ERR, P50 and P95 cover the last hour.",
            ),
            p(
                "TUNNEL is the requires_tcp port: down means calls fail fast. T runs requires_tcp_cmd, if set, and waits up to 15s for the port. CTX is the token cost of the advertised tool schemas (t opens the inspector).",
            ),
            p(
                "↻ after a name: config changed; the server restarts once in-flight calls finish. ! means the server now advertises a different version or tool set than the cache that clients were served.",
            ),
            p(
                "The header estimates what sharing saves: each session would otherwise run its own copy.",
            ),
            p(
                "p pins a server: it starts now and skips idle stops until unpinned or the daemon restarts. keep_warm in config does the same within a daily time window.",
            ),
        ],
    };
    out.extend([
        p(""),
        h("Server states"),
        p("stopped   not running; the next call starts it"),
        p("starting  spawned, waiting for initialize (startup_timeout)"),
        p("ready     serving calls"),
        p("stopping  draining in-flight calls before exit"),
        p("failed    the last start failed or it crashed; retries back off from 1s to 60s. x (stop) clears the failure; the error and the server's log say why."),
        p(""),
        h("Command line"),
        p(": opens it. :restart postgres, :stop all, :set idle_timeout 5m, :set github.keep_warm 09:00-18:00, :check --live, :clients claude. Tab completes commands, servers and setting names."),
    ]);
    out
}

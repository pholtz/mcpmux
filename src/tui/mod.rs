//! Dashboard. Holds no state of its own: everything comes from the admin socket, polled on a
//! background thread so a slow or wedged daemon never blocks the keyboard.

mod commands;
mod dash;
mod help;
mod logview;
pub mod settings;
mod tools;

use crate::{
    admin,
    config::Paths,
    status::{ServerStatus, Status, ToolView},
};
use anyhow::Result;
use commands::Scope;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use help::Help;
use logview::LogView;
use ratatui::{
    Frame,
    layout::Rect,
    style::Stylize,
    text::Line,
    widgets::{Clear, TableState},
};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};
use tools::ToolsView;

pub enum Msg {
    Status(Result<Status, String>),
    Note(String),
    Tools {
        server: String,
        /// Open the inspector (a user request) rather than only refresh an open one.
        open: bool,
        result: Result<Option<Vec<ToolView>>, String>,
    },
    Open {
        name: String,
        path: PathBuf,
    },
}

pub enum Action {
    None,
    Quit,
    Pager(PathBuf),
}

pub struct App {
    pub socket: PathBuf,
    /// The config the daemon loaded (from status), so edits land in the right file.
    pub config: PathBuf,
    pub state_dir: PathBuf,
    pub status: Result<Status, String>,
    pub table: TableState,
    pub daemon_log: PathBuf,
    pub log: Option<LogView>,
    pub tools: Option<ToolsView>,
    pub help: Option<Help>,
    /// Text typed after `:`.
    pub cmdline: Option<String>,
    pub completions: Option<String>,
    pub message: Option<(String, Instant)>,
    pub tx: mpsc::Sender<Msg>,
    poke: mpsc::Sender<()>,
}

impl App {
    pub fn servers(&self) -> &[ServerStatus] {
        self.status
            .as_ref()
            .map(|s| s.servers.as_slice())
            .unwrap_or(&[])
    }

    pub fn selected(&self) -> Option<&ServerStatus> {
        self.table.selected().and_then(|i| self.servers().get(i))
    }

    pub fn view(&self) -> Scope {
        if self.tools.is_some() {
            Scope::Tools
        } else if self.log.is_some() {
            Scope::Log
        } else {
            Scope::Dash
        }
    }

    pub fn note(&mut self, m: String) {
        if let Some(v) = self.log.as_mut() {
            v.note = Some(m.clone());
        }
        self.message = Some((m, Instant::now()));
    }

    /// Run `f` off the UI thread (admin calls can wait out a cold start), then show its
    /// result and refresh status at once.
    pub fn job(&mut self, label: String, f: impl FnOnce() -> Result<String> + Send + 'static) {
        self.note(format!("{label}…"));
        let (tx, poke) = (self.tx.clone(), self.poke.clone());
        std::thread::spawn(move || {
            let m = f().unwrap_or_else(|e| format!("{label}: {e:#}"));
            let _ = tx.send(Msg::Note(m));
            let _ = poke.send(());
        });
    }

    /// Ask the daemon to reload; `prefix` is the edit that prompted it.
    pub fn reload_job(&mut self, prefix: Option<String>) {
        let socket = self.socket.clone();
        let label = prefix.clone().unwrap_or_else(|| "reload".into());
        self.job(label, move || {
            let v = admin::call(&socket, "reload", None)?;
            let changes: Vec<String> = v["changes"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|c| c.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let applied = if changes.is_empty() {
                "no changes".into()
            } else {
                changes.join("; ")
            };
            Ok(match prefix {
                Some(p) => format!("{p} · reloaded: {applied}"),
                None => format!("reload: {applied}"),
            })
        });
    }

    /// `None` opens the daemon log.
    pub fn open_log(&mut self, server: Option<&str>) {
        let s = server.and_then(|n| self.servers().iter().find(|s| s.name == n));
        self.log = Some(match s {
            Some(s) => LogView::new(s.name.clone(), PathBuf::from(&s.log)),
            None => LogView::new("mcpmux".into(), self.daemon_log.clone()),
        });
        self.tools = None;
    }

    pub fn fetch_tools(&mut self, server: String, open: bool) {
        let socket = self.socket.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = admin::tools(&socket, &server).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Tools {
                server,
                open,
                result,
            });
        });
    }

    pub fn open_help(&mut self) {
        self.help = Some(Help::new(self));
    }

    pub fn open_settings(&mut self) {
        let mut h = Help::new(self);
        h.tab = help::Tab::Settings;
        self.help = Some(h);
    }

    fn handle(&mut self, m: Msg) {
        match m {
            Msg::Status(s) => {
                if let Ok(st) = &s
                    && !st.config.is_empty()
                {
                    self.config = PathBuf::from(&st.config);
                }
                self.status = s;
                let n = self.servers().len();
                if n > 0 && self.table.selected().is_none_or(|i| i >= n) {
                    self.table.select(Some(0));
                }
            }
            Msg::Note(m) => self.note(m),
            Msg::Tools {
                server,
                open,
                result,
            } => match result {
                Ok(Some(list)) => {
                    if let Some(t) = self.tools.as_mut().filter(|t| t.server == server) {
                        t.tools = list;
                    } else if open {
                        self.tools = Some(ToolsView::new(server, list));
                    }
                }
                Ok(None) => self.note(format!(
                    "{server}: no tool list cached yet; start it once (s)"
                )),
                Err(e) => self.note(format!("tools {server}: {e}")),
            },
            Msg::Open { name, path } => {
                self.tools = None;
                self.log = Some(LogView::new(name, path));
            }
        }
    }
}

/// Status every 2s, or at once when poked (after a command finishes).
fn poller(socket: PathBuf, tx: mpsc::Sender<Msg>, poke: mpsc::Receiver<()>) {
    loop {
        let s = admin::status(&socket).map_err(|e| format!("{e:#}"));
        if tx.send(Msg::Status(s)).is_err() {
            return;
        }
        match poke.recv_timeout(Duration::from_secs(2)) {
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            _ => while poke.try_recv().is_ok() {},
        }
    }
}

pub fn run(paths: &Paths) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let (poke, poke_rx) = mpsc::channel();
    let socket = paths.socket();
    {
        let (socket, tx) = (socket.clone(), tx.clone());
        std::thread::spawn(move || poller(socket, tx, poke_rx));
    }
    let mut app = App {
        socket,
        config: paths.config.clone(),
        state_dir: paths.state.clone(),
        status: Err("connecting…".into()),
        table: TableState::default(),
        daemon_log: paths.logs.join("mcpmux.log"),
        log: None,
        tools: None,
        help: None,
        cmdline: None,
        completions: None,
        message: None,
        tx,
        poke,
    };
    // Show the first status without a blank frame when the daemon answers quickly.
    if let Ok(m) = rx.recv_timeout(Duration::from_millis(500)) {
        app.handle(m);
    }
    let mut terminal = ratatui::init();
    alternate_scroll(true);
    let res = (|| -> Result<()> {
        loop {
            if let Some(v) = &mut app.log {
                v.reload();
            }
            terminal.draw(|f| draw(f, &mut app))?;
            if event::poll(Duration::from_millis(200))?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                match key(&mut app, k) {
                    Action::Quit => return Ok(()),
                    Action::Pager(path) => {
                        alternate_scroll(false);
                        ratatui::restore();
                        let msg = pager(&path);
                        terminal = ratatui::init();
                        alternate_scroll(true);
                        terminal.clear()?;
                        if let Some(m) = msg {
                            app.note(m);
                        }
                    }
                    Action::None => {}
                }
            }
            while let Ok(m) = rx.try_recv() {
                app.handle(m);
            }
        }
    })();
    alternate_scroll(false);
    ratatui::restore();
    res
}

fn key(app: &mut App, k: KeyEvent) -> Action {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && k.code == KeyCode::Char('c') {
        return Action::Quit;
    }
    if app.cmdline.is_some() {
        return cmdline_key(app, k);
    }
    if app.help.is_some() {
        return help::key(app, k);
    }
    if let Some(v) = app.log.as_mut()
        && let Some((_, text)) = v.prompt.as_mut()
    {
        match k.code {
            KeyCode::Enter => v.submit_prompt(),
            KeyCode::Esc => v.prompt = None,
            KeyCode::Backspace => {
                text.pop();
            }
            KeyCode::Char(c) => text.push(c),
            _ => {}
        }
        return Action::None;
    }
    if k.code == KeyCode::Char(':') {
        app.cmdline = Some(String::new());
        return Action::None;
    }
    let view = app.view();
    let handled = match view {
        Scope::Log => log_nav(app, k),
        Scope::Tools => tools::nav(app, k),
        _ => dash_nav(app, k),
    };
    if let Some(a) = handled {
        return a;
    }
    match commands::by_key(view, k.code) {
        Some(c) if !ctrl => (c.run)(app, ""),
        _ => Action::None,
    }
}

fn cmdline_key(app: &mut App, k: KeyEvent) -> Action {
    let Some(line) = app.cmdline.as_mut() else {
        return Action::None;
    };
    match k.code {
        KeyCode::Enter => {
            let line = std::mem::take(line);
            app.cmdline = None;
            app.completions = None;
            return commands::exec(app, &line);
        }
        KeyCode::Esc => {
            app.cmdline = None;
            app.completions = None;
        }
        KeyCode::Backspace if line.is_empty() => app.cmdline = None,
        KeyCode::Backspace => {
            line.pop();
        }
        KeyCode::Tab => {
            let current = line.clone();
            let (done, hint) = commands::complete(app, &current);
            app.cmdline = Some(done);
            app.completions = hint;
        }
        KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => line.clear(),
        KeyCode::Char(c)
            if !k
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            line.push(c);
            app.completions = None;
        }
        _ => {}
    }
    Action::None
}

fn dash_nav(app: &mut App, k: KeyEvent) -> Option<Action> {
    let n = app.servers().len();
    match k.code {
        KeyCode::Esc => return Some(Action::Quit),
        KeyCode::Down | KeyCode::Char('j') if n > 0 => {
            app.table
                .select(Some((app.table.selected().unwrap_or(0) + 1) % n));
        }
        KeyCode::Up | KeyCode::Char('k') if n > 0 => {
            app.table
                .select(Some((app.table.selected().unwrap_or(0) + n - 1) % n));
        }
        _ => return None,
    }
    Some(Action::None)
}

fn log_nav(app: &mut App, k: KeyEvent) -> Option<Action> {
    let v = app.log.as_mut()?;
    v.note = None;
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    match k.code {
        KeyCode::Char('d') if ctrl => v.scroll(v.page() / 2),
        KeyCode::Char('u') if ctrl => v.scroll(-v.page() / 2),
        KeyCode::Char('f') if ctrl => v.scroll(v.page()),
        KeyCode::Char('b') if ctrl => v.scroll(-v.page()),
        KeyCode::Esc => {
            if !v.clear_one() {
                app.log = None;
            }
        }
        KeyCode::Char('l') | KeyCode::Char('q') => app.log = None,
        KeyCode::Down | KeyCode::Char('j') => v.scroll(1),
        KeyCode::Up | KeyCode::Char('k') => v.scroll(-1),
        KeyCode::PageDown | KeyCode::Char(' ') => v.scroll(v.page()),
        KeyCode::PageUp | KeyCode::Char('b') => v.scroll(-v.page()),
        KeyCode::Home | KeyCode::Char('g') => v.home(),
        KeyCode::End | KeyCode::Char('G') => v.end(),
        KeyCode::Right => v.hscroll(8),
        KeyCode::Left => v.hscroll(-8),
        KeyCode::Char('n') => v.find(true),
        KeyCode::Char('N') => v.find(false),
        KeyCode::Tab => {
            // Swap between the selected server's stderr and the daemon log.
            let showing_daemon = v.path == app.daemon_log;
            let server = app.selected().map(|s| s.name.clone());
            match server {
                Some(name) if showing_daemon => app.open_log(Some(&name)),
                _ => app.open_log(None),
            }
        }
        _ => return None,
    }
    Some(Action::None)
}

fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    match app.view() {
        Scope::Log => {
            if let Some(v) = app.log.as_mut() {
                v.draw(f, area);
            }
        }
        Scope::Tools => tools::draw(f, app, area),
        _ => dash::draw(f, app, area),
    }
    if let Some(line) = &app.cmdline {
        let bottom = Rect {
            y: area.bottom().saturating_sub(1),
            height: 1,
            ..area
        };
        let hint = app
            .completions
            .as_ref()
            .map(|h| format!("   {h}"))
            .unwrap_or_else(|| "   tab completes · enter runs · esc cancels".into());
        f.render_widget(Clear, bottom);
        f.render_widget(
            Line::from(vec![
                ":".yellow().bold(),
                line.clone().into(),
                "█".dark_gray(),
                hint.dark_gray(),
            ]),
            bottom,
        );
    }
    if app.help.is_some() {
        help::draw(f, app, area);
    }
}

/// The footer message, if it's recent.
pub fn message(app: &App) -> Option<&str> {
    app.message
        .as_ref()
        .filter(|(_, at)| at.elapsed() < Duration::from_secs(10))
        .map(|(m, _)| m.as_str())
}

/// DECSET 1007: in the alternate screen, the mouse wheel sends ↑/↓, so it scrolls the log
/// while leaving mouse selection to the terminal.
fn alternate_scroll(on: bool) {
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b[?1007{}", if on { 'h' } else { 'l' });
    let _ = out.flush();
}

/// Hand the terminal to `$PAGER` (default `less -R +G`, starting at the end).
fn pager(path: &Path) -> Option<String> {
    let status = match std::env::var("PAGER").ok().filter(|p| !p.trim().is_empty()) {
        Some(p) => Command::new("sh")
            .arg("-c")
            .arg(format!("{p} \"$1\""))
            .arg("sh")
            .arg(path)
            .status(),
        None => Command::new("less").args(["-R", "+G"]).arg(path).status(),
    };
    match status {
        Ok(_) => None,
        Err(e) => Some(format!("pager: {e}")),
    }
}

pub fn copy(text: &str) -> std::io::Result<()> {
    let mut child = Command::new("pbcopy").stdin(Stdio::piped()).spawn()?;
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(text.as_bytes())?;
    child.wait()?;
    Ok(())
}

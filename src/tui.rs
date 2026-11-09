//! Dashboard. Holds no state of its own: everything comes from the admin socket.

use crate::{
    admin,
    config::Paths,
    status::{ServerStatus, Status, human_secs},
};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
};
use std::{
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

struct App {
    socket: PathBuf,
    status: Result<Status, String>,
    table: TableState,
    show_log: bool,
    message: Option<(String, Instant)>,
    results: mpsc::Receiver<String>,
    results_tx: mpsc::Sender<String>,
}

impl App {
    fn refresh(&mut self) {
        self.status = admin::status(&self.socket).map_err(|e| format!("{e:#}"));
        let n = self.servers().len();
        if n > 0 && self.table.selected().is_none_or(|i| i >= n) {
            self.table.select(Some(0));
        }
    }

    fn servers(&self) -> &[ServerStatus] {
        self.status.as_ref().map(|s| s.servers.as_slice()).unwrap_or(&[])
    }

    fn selected(&self) -> Option<&ServerStatus> {
        self.table.selected().and_then(|i| self.servers().get(i))
    }

    /// Admin calls can take a while (cold start), so run them off the UI thread.
    fn control(&mut self, cmd: &'static str) {
        let Some(name) = self.selected().map(|s| s.name.clone()) else {
            return;
        };
        let socket = self.socket.clone();
        let tx = self.results_tx.clone();
        self.message = Some((format!("{cmd} {name}…"), Instant::now()));
        std::thread::spawn(move || {
            let msg = match admin::call(&socket, cmd, Some(&name)) {
                Ok(_) => format!("{cmd} {name}: ok"),
                Err(e) => format!("{cmd} {name}: {e:#}"),
            };
            let _ = tx.send(msg);
        });
    }
}

pub fn run(paths: &Paths) -> Result<()> {
    let (results_tx, results) = mpsc::channel();
    let mut app = App {
        socket: paths.socket(),
        status: Err("connecting…".into()),
        table: TableState::default(),
        show_log: false,
        message: None,
        results,
        results_tx,
    };
    app.refresh();
    let mut terminal = ratatui::init();
    let res = (|| -> Result<()> {
        let mut last = Instant::now();
        loop {
            terminal.draw(|f| draw(f, &mut app))?;
            if event::poll(Duration::from_millis(250))?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                let n = app.servers().len();
                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Down | KeyCode::Char('j') if n > 0 => {
                        app.table
                            .select(Some((app.table.selected().unwrap_or(0) + 1) % n));
                    }
                    KeyCode::Up | KeyCode::Char('k') if n > 0 => {
                        app.table
                            .select(Some((app.table.selected().unwrap_or(0) + n - 1) % n));
                    }
                    KeyCode::Char('s') => app.control("start"),
                    KeyCode::Char('x') => app.control("stop"),
                    KeyCode::Char('r') => app.control("restart"),
                    KeyCode::Char('l') => app.show_log = !app.show_log,
                    _ => {}
                }
            }
            while let Ok(m) = app.results.try_recv() {
                app.message = Some((m, Instant::now()));
                app.refresh();
            }
            if last.elapsed() >= Duration::from_secs(2) {
                app.refresh();
                last = Instant::now();
            }
        }
    })();
    ratatui::restore();
    res
}

fn state_style(state: &str) -> Style {
    match state {
        "ready" => Style::new().fg(Color::Green),
        "starting" | "stopping" => Style::new().fg(Color::Yellow),
        "failed" => Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        _ => Style::new().fg(Color::DarkGray),
    }
}

fn dash<T: ToString>(v: Option<T>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "-".into())
}

fn draw(f: &mut Frame, app: &mut App) {
    let [header, table_area, detail, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(app.servers().len() as u16 + 3),
        Constraint::Min(5),
        Constraint::Length(1),
    ])
    .areas(f.area());

    let head = match &app.status {
        Ok(s) => Line::from(vec![
            " mcpmux ".bold().reversed(),
            format!("  pid {} · up {} · {}", s.pid, human_secs(s.uptime_s), s.listen).into(),
        ]),
        Err(e) => Line::from(vec![" mcpmux ".bold().reversed(), format!("  {e}").red()]),
    };
    f.render_widget(head, header);

    let rows = app.servers().iter().map(|s| {
        let idle = match (s.idle_s, s.idle_timeout_s) {
            (Some(i), 0) => human_secs(i),
            (Some(i), t) => human_secs(t.saturating_sub(i)),
            (None, _) => "-".into(),
        };
        Row::new(vec![
            Cell::from(s.name.clone()),
            Cell::from(s.state.clone()).style(state_style(&s.state)),
            Cell::from(dash(s.pid)),
            Cell::from(s.rss_kb.map(|k| format!("{}M", k / 1024)).unwrap_or("-".into())),
            Cell::from(s.uptime_s.map(human_secs).unwrap_or("-".into())),
            Cell::from(idle),
            Cell::from(format!("{}", s.sessions)),
            Cell::from(format!("{}", s.inflight)),
            Cell::from(format!("{}", s.calls_1h)),
            Cell::from(format!("{}", s.errors_1h)).style(if s.errors_1h > 0 {
                Style::new().fg(Color::Red)
            } else {
                Style::new()
            }),
            Cell::from(s.p50_ms.map(|m| format!("{m}ms")).unwrap_or("-".into())),
            Cell::from(s.p95_ms.map(|m| format!("{m}ms")).unwrap_or("-".into())),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Min(16),
            Constraint::Length(9),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(7),
            Constraint::Length(10),
            Constraint::Length(4),
            Constraint::Length(4),
            Constraint::Length(6),
            Constraint::Length(4),
            Constraint::Length(7),
            Constraint::Length(7),
        ],
    )
    .header(
        Row::new([
            "SERVER", "STATE", "PID", "RSS", "UP", "IDLE STOP", "SESS", "LIVE", "CALLS", "ERR", "P50",
            "P95",
        ])
        .style(Style::new().add_modifier(Modifier::BOLD)),
    )
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .block(Block::default().borders(Borders::ALL).title(" servers (1h) "));
    f.render_stateful_widget(table, table_area, &mut app.table);

    if let Some(s) = app.selected().cloned() {
        if app.show_log {
            let text = tail(&s.log, detail.height.saturating_sub(2) as usize);
            f.render_widget(
                Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .block(Block::default().borders(Borders::ALL).title(format!(" {} ", s.log))),
                detail,
            );
        } else {
            let mut lines = vec![];
            if let Some(e) = &s.last_error {
                let retry = s.retry_in_s.map(|r| format!(" (retry in {r}s)")).unwrap_or_default();
                lines.push(Line::from(format!("error: {e}{retry}")).red());
            }
            lines.push(Line::from(format!(
                "cached tools: {} · clients: {}",
                dash(s.cached_tools),
                if s.clients.is_empty() { "-".into() } else { s.clients.join(", ") }
            )).dark_gray());
            for c in &s.recent {
                lines.push(Line::from(vec![
                    Span::raw(format!("{:>6} ago  ", human_secs(c.ago_s))),
                    if c.ok { "ok  ".green() } else { "err ".red() },
                    Span::raw(format!("{:>7}ms  {:<32} {}", c.ms, c.tool, c.session)),
                ]));
            }
            f.render_widget(
                Paragraph::new(lines).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" {} · recent calls ", s.name)),
                ),
                detail,
            );
        }
    }

    let help = " ↑↓ select · s start · x stop · r restart · l log · q quit ";
    let msg = app
        .message
        .as_ref()
        .filter(|(_, at)| at.elapsed() < Duration::from_secs(8))
        .map(|(m, _)| format!("  {m}"))
        .unwrap_or_default();
    f.render_widget(Line::from(vec![help.dark_gray(), msg.yellow()]), footer);
}

fn tail(path: &str, n: usize) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return format!("(no log at {path})");
    };
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

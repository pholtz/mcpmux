//! The server table, savings header, and the selected server's detail pane.

use super::{App, tools::tokens};
use crate::{
    config::in_window,
    status::{ServerStatus, human_secs},
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, Table},
};

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

fn mb(kb: u64) -> String {
    if kb >= 1024 * 1024 {
        format!("{:.1}G", kb as f64 / (1024.0 * 1024.0))
    } else {
        format!("{}M", kb / 1024)
    }
}

/// Without mcpmux every session runs its own copy of each server. Count what sharing saves:
/// sessions × footprint, minus the one copy actually running.
fn savings(servers: &[ServerStatus]) -> Option<String> {
    let (mut kb, mut procs, mut sessions) = (0u64, 0usize, 0usize);
    for s in servers {
        let running = s.rss_kb.is_some() as usize;
        let per = s.rss_kb.or(s.last_rss_kb).unwrap_or(0);
        kb += (per * s.sessions as u64).saturating_sub(s.rss_kb.unwrap_or(0));
        procs += s.sessions.saturating_sub(running);
        sessions += s.sessions;
    }
    (procs > 0).then(|| {
        format!(
            "≈ {} RAM · {procs} processes saved across {sessions} sessions",
            mb(kb)
        )
    })
}

/// What the IDLE STOP column shows.
fn idle(s: &ServerStatus) -> (String, Style) {
    if s.pinned {
        return ("pinned".into(), Style::new().fg(Color::Cyan));
    }
    if s.idle_s.is_some() && s.keep_warm.as_deref().is_some_and(in_window) {
        return ("warm".into(), Style::new().fg(Color::Cyan));
    }
    let t = match (s.idle_s, s.idle_timeout_s) {
        (Some(i), 0) => human_secs(i),
        (Some(i), t) => human_secs(t.saturating_sub(i)),
        (None, _) => "-".into(),
    };
    (t, Style::new())
}

pub fn draw(f: &mut Frame, app: &mut App, area: Rect) {
    let [header, table_area, detail, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(app.servers().len() as u16 + 3),
        Constraint::Min(8),
        Constraint::Length(1),
    ])
    .areas(area);

    let head = match &app.status {
        Ok(s) => {
            let mut spans = vec![
                " mcpmux ".bold().reversed(),
                format!(
                    "  pid {} · up {} · {}",
                    s.pid,
                    human_secs(s.uptime_s),
                    s.listen
                )
                .into(),
            ];
            if let Some(saved) = savings(&s.servers) {
                spans.push("  ".into());
                spans.push(saved.green());
            }
            Line::from(spans)
        }
        Err(e) => Line::from(vec![" mcpmux ".bold().reversed(), format!("  {e}").red()]),
    };
    f.render_widget(head, header);

    let rows = app.servers().iter().map(|s| {
        let (idle, idle_style) = idle(s);
        let mut name = vec![Span::raw(s.name.clone())];
        if s.restart_pending {
            name.push(" ↻".yellow());
        }
        if s.cache_note.is_some() {
            name.push(" !".yellow());
        }
        let tunnel = match &s.tunnel {
            None => Cell::from("-"),
            Some(t) => match t.up {
                Some(true) => Cell::from("up").green(),
                Some(false) => Cell::from("down").red().bold(),
                None => Cell::from("?"),
            },
        };
        Row::new(vec![
            Cell::from(Line::from(name)),
            Cell::from(s.state.clone()).style(state_style(&s.state)),
            Cell::from(dash(s.pid)),
            Cell::from(s.rss_kb.map(mb).unwrap_or("-".into())),
            Cell::from(idle).style(idle_style),
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
            tunnel,
            Cell::from(
                s.tool_tokens
                    .map(|t| format!("~{}", tokens(t)))
                    .unwrap_or("-".into()),
            ),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Min(16),
            Constraint::Length(9),
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(9),
            Constraint::Length(4),
            Constraint::Length(4),
            Constraint::Length(6),
            Constraint::Length(4),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Length(6),
            Constraint::Length(7),
        ],
    )
    .header(
        Row::new([
            "SERVER",
            "STATE",
            "PID",
            "RSS",
            "IDLE STOP",
            "SESS",
            "LIVE",
            "CALLS",
            "ERR",
            "P50",
            "P95",
            "TUNNEL",
            "CTX",
        ])
        .style(Style::new().add_modifier(Modifier::BOLD)),
    )
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" servers (1h) "),
    );
    f.render_stateful_widget(table, table_area, &mut app.table);

    if let Some(s) = app.selected().cloned() {
        detail_pane(f, &s, detail);
    }

    let footer_line = match super::message(app) {
        Some(m) => Line::from(format!(" {m}").yellow()),
        None => {
            " ? help · : command · s start · x stop · r restart · l log · t tools · p pin · q quit"
                .dark_gray()
                .into()
        }
    };
    f.render_widget(footer_line, footer);
}

fn detail_pane(f: &mut Frame, s: &ServerStatus, area: Rect) {
    let mut lines = vec![];
    if let Some(e) = &s.last_error {
        let retry = s
            .retry_in_s
            .map(|r| format!(" (retry in {r}s; x clears)"))
            .unwrap_or_default();
        lines.push(Line::from(format!("error: {e}{retry}")).red());
    }
    if let Some(t) = &s.tunnel
        && t.up == Some(false)
    {
        let mut l = format!("tunnel {} is down", t.addr);
        if let Some(h) = &t.hint {
            l += &format!(": {h}");
        }
        if let Some(c) = &t.cmd {
            l += &format!(" · T runs `{c}`");
        }
        lines.push(Line::from(l).red());
    }
    if let Some(n) = &s.cache_note {
        lines.push(Line::from(format!("! {n}")).yellow());
    }
    if s.restart_pending {
        lines.push(Line::from("↻ config changed; restarts once in-flight calls finish").yellow());
    }
    let mut info = vec![format!(
        "up {}",
        s.uptime_s.map(human_secs).unwrap_or("-".into())
    )];
    info.push(format!(
        "tools {}{}",
        dash(s.cached_tools),
        s.tool_tokens
            .map(|t| format!(" (≈{} tokens)", tokens(t)))
            .unwrap_or_default()
    ));
    if let Some(w) = &s.keep_warm {
        info.push(format!("keep_warm {w}"));
    }
    info.push(format!(
        "clients {}",
        if s.clients.is_empty() {
            "-".into()
        } else {
            s.clients.join(", ")
        }
    ));
    lines.push(Line::from(info.join(" · ")).dark_gray());

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", s.name));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [top, sparks, recent] = Layout::vertical([
        Constraint::Length(lines.len() as u16),
        Constraint::Length(3),
        Constraint::Min(0),
    ])
    .areas(inner);
    f.render_widget(Paragraph::new(lines), top);

    let h = &s.history;
    let cols = Layout::horizontal([Constraint::Ratio(1, 3); 3])
        .spacing(2)
        .split(sparks);
    let series = [
        ("calls/min", &h.calls, Color::Green, ""),
        ("p95", &h.p95_ms, Color::Yellow, "ms"),
        ("rss", &h.rss_mb, Color::Cyan, "M"),
    ];
    for ((label, data, color, unit), area) in series.into_iter().zip(cols.iter()) {
        let w = area.width as usize;
        let tail = &data[data.len().saturating_sub(w)..];
        let max = tail.iter().copied().max().unwrap_or(0);
        let [title, chart] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(2)]).areas(*area);
        f.render_widget(
            Line::from(vec![
                format!("{label} ").bold(),
                format!("60m · max {max}{unit}").dark_gray(),
            ]),
            title,
        );
        f.render_widget(
            Sparkline::default()
                .data(tail)
                .style(Style::new().fg(color)),
            chart,
        );
    }

    let calls: Vec<Line> = s
        .recent
        .iter()
        .map(|c| {
            Line::from(vec![
                Span::raw(format!("{:>6} ago  ", human_secs(c.ago_s))),
                if c.ok { "ok  ".green() } else { "err ".red() },
                Span::raw(format!("{:>7}ms  {:<32} {}", c.ms, c.tool, c.session)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(calls), recent);
}

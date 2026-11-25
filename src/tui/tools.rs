//! Tool inspector: what a server advertises, what each schema costs in context, and deny
//! toggles that write `deny_tools` and reload.

use super::{Action, App, settings};
use crate::status::ToolView;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::Line,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
};

pub struct ToolsView {
    pub server: String,
    pub tools: Vec<ToolView>,
    table: TableState,
}

impl ToolsView {
    pub fn new(server: String, tools: Vec<ToolView>) -> Self {
        let mut table = TableState::default();
        table.select((!tools.is_empty()).then_some(0));
        ToolsView {
            server,
            tools,
            table,
        }
    }

    fn selected(&self) -> Option<&ToolView> {
        self.table.selected().and_then(|i| self.tools.get(i))
    }
}

pub fn nav(app: &mut App, k: KeyEvent) -> Option<Action> {
    let t = app.tools.as_mut()?;
    let n = t.tools.len();
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('t') => app.tools = None,
        KeyCode::Down | KeyCode::Char('j') if n > 0 => t
            .table
            .select(Some((t.table.selected().unwrap_or(0) + 1) % n)),
        KeyCode::Up | KeyCode::Char('k') if n > 0 => t
            .table
            .select(Some((t.table.selected().unwrap_or(0) + n - 1) % n)),
        KeyCode::Home | KeyCode::Char('g') => t.table.select(Some(0)),
        KeyCode::End | KeyCode::Char('G') => t.table.select(n.checked_sub(1)),
        _ => return None,
    }
    Some(Action::None)
}

/// Flip the selected tool in `deny_tools`, reload, and refresh the list.
pub fn toggle(app: &mut App) {
    let Some((server, tool)) = app
        .tools
        .as_ref()
        .and_then(|t| t.selected().map(|x| (t.server.clone(), x.name.clone())))
    else {
        return;
    };
    match settings::toggle_deny(&app.config, &server, &tool) {
        Ok(change) => {
            // Reload first so the refreshed list reflects the edit.
            let socket = app.socket.clone();
            let tx = app.tx.clone();
            app.note(format!("{change}…"));
            std::thread::spawn(move || {
                let note = match crate::admin::call(&socket, "reload", None) {
                    Ok(_) => change,
                    Err(e) => format!("{change}, but reload failed: {e:#}"),
                };
                let result = crate::admin::tools(&socket, &server).map_err(|e| format!("{e:#}"));
                let _ = tx.send(super::Msg::Note(note));
                let _ = tx.send(super::Msg::Tools {
                    server,
                    open: false,
                    result,
                });
            });
        }
        Err(e) => app.note(format!("{e:#}")),
    }
}

pub fn draw(f: &mut Frame, app: &mut App, area: Rect) {
    let msg = super::message(app).map(str::to_string);
    let Some(t) = app.tools.as_mut() else {
        return;
    };
    let [head, list, desc, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(8),
        Constraint::Length(1),
    ])
    .areas(area);

    let live: Vec<&ToolView> = t.tools.iter().filter(|x| !x.denied).collect();
    let total: u64 = live.iter().map(|x| x.tokens).sum();
    let denied = t.tools.len() - live.len();
    f.render_widget(
        Line::from(vec![
            format!(" {} tools ", t.server).bold().reversed(),
            format!(
                "  {} advertised · ≈{} tokens of schema per session{}",
                live.len(),
                tokens(total),
                if denied > 0 {
                    format!(" · {denied} denied")
                } else {
                    String::new()
                }
            )
            .into(),
        ]),
        head,
    );

    let max = t.tools.iter().map(|x| x.tokens).max().unwrap_or(1).max(1);
    let rows = t.tools.iter().map(|x| {
        let bar = "▇".repeat(((x.tokens * 12).div_ceil(max)) as usize);
        let style = if x.denied {
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::CROSSED_OUT)
        } else {
            Style::new()
        };
        Row::new(vec![
            Cell::from(x.name.clone()).style(style),
            Cell::from(format!("{:>6}", tokens(x.tokens))),
            Cell::from(bar).style(Style::new().fg(Color::Cyan)),
            Cell::from(if x.denied { "denied" } else { "" }).style(Style::new().fg(Color::Red)),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Min(24),
            Constraint::Length(7),
            Constraint::Length(13),
            Constraint::Length(7),
        ],
    )
    .header(Row::new(["TOOL", "~TOKENS", "", ""]).style(Style::new().add_modifier(Modifier::BOLD)))
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .block(Block::default().borders(Borders::ALL));
    f.render_stateful_widget(table, list, &mut t.table);

    let text = t
        .selected()
        .map(|x| x.description.clone())
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "(no description)".into());
    f.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: true }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" description "),
        ),
        desc,
    );

    let footer = match msg {
        Some(m) => Line::from(m.yellow()),
        None => " space deny/allow · ↑↓ select · : command · ? help · esc back"
            .dark_gray()
            .into(),
    };
    f.render_widget(footer, foot);
}

pub fn tokens(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

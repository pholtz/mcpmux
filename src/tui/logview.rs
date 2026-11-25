//! Full-screen log pager inside the TUI: follows the file like `tail -f` until you scroll,
//! then holds its place. Drawn without borders so the terminal's own selection copies clean lines.
//! A filter (`&`) and level toggle (`v`) narrow the shown lines; positions index the shown set.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use std::{
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    time::SystemTime,
};

/// Only the tail of a large log is loaded.
const MAX_BYTES: u64 = 8 << 20;

/// Ordered: a line passes when its level is at least the selected one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    All,
    Warn,
    Error,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Prompt {
    Search,
    Filter,
}

pub struct LogView {
    /// Server name, or `mcpmux` for the daemon log.
    pub name: String,
    pub path: PathBuf,
    lines: Vec<String>,
    /// Indices into `lines` that pass the filter and level.
    shown: Vec<usize>,
    /// (len, mtime) of the file when last read.
    stamp: Option<(u64, SystemTime)>,
    truncated: bool,
    /// First visible position in `shown`.
    top: usize,
    pub follow: bool,
    pub wrap: bool,
    hscroll: usize,
    /// Body size from the last frame.
    size: (usize, usize),
    pub query: String,
    pub filter: String,
    pub level: Level,
    /// Position (in `shown`) of the last search hit; `n`/`N` continue from here.
    hit: Option<usize>,
    /// Text being typed into the footer prompt.
    pub prompt: Option<(Prompt, String)>,
    pub note: Option<String>,
}

impl LogView {
    pub fn new(name: String, path: PathBuf) -> Self {
        let mut v = LogView {
            name,
            path,
            lines: vec![],
            shown: vec![],
            stamp: None,
            truncated: false,
            top: 0,
            follow: true,
            wrap: true,
            hscroll: 0,
            size: (80, 20),
            query: String::new(),
            filter: String::new(),
            level: Level::All,
            hit: None,
            prompt: None,
            note: None,
        };
        v.reload();
        v
    }

    /// Re-read the file if it changed since last time.
    pub fn reload(&mut self) {
        let meta = std::fs::metadata(&self.path).ok();
        let stamp = meta
            .as_ref()
            .map(|m| (m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
        if stamp.is_some() && stamp == self.stamp {
            return;
        }
        self.stamp = stamp;
        let Some(meta) = meta else {
            self.lines = vec![format!("(no log at {})", self.path.display())];
            self.refilter();
            return;
        };
        let Ok(mut f) = std::fs::File::open(&self.path) else {
            return;
        };
        self.truncated = meta.len() > MAX_BYTES;
        if self.truncated {
            let _ = f.seek(SeekFrom::Start(meta.len() - MAX_BYTES));
        }
        let mut buf = vec![];
        let _ = f.read_to_end(&mut buf);
        let text = String::from_utf8_lossy(&buf);
        let mut lines: Vec<String> = text.lines().map(clean).collect();
        if self.truncated && !lines.is_empty() {
            lines.remove(0); // partial line at the cut
        }
        self.lines = lines;
        self.refilter();
    }

    /// Recompute the shown lines, keeping the current top line in view where possible.
    fn refilter(&mut self) {
        let anchor = self.shown.get(self.top).copied();
        let f = self.filter.to_lowercase();
        self.shown = (0..self.lines.len())
            .filter(|&i| {
                let l = &self.lines[i];
                level_of(l) >= self.level && (f.is_empty() || l.to_lowercase().contains(&f))
            })
            .collect();
        self.hit = None;
        self.top = anchor
            .map(|a| self.shown.partition_point(|&i| i < a))
            .unwrap_or(0)
            .min(self.max_top());
    }

    fn line(&self, pos: usize) -> &str {
        &self.lines[self.shown[pos]]
    }

    /// Screen rows a line takes at the current width.
    fn rows(&self, line: &str) -> usize {
        if !self.wrap {
            return 1;
        }
        line.chars().count().div_ceil(self.size.0.max(1)).max(1)
    }

    /// The highest `top` that still fills the screen, so the last line sits on the bottom row.
    fn max_top(&self) -> usize {
        let mut rows = 0;
        for pos in (0..self.shown.len()).rev() {
            rows += self.rows(self.line(pos));
            if rows > self.size.1 {
                return pos + 1;
            }
        }
        0
    }

    pub fn scroll(&mut self, by: isize) {
        self.hit = None;
        let max = self.max_top();
        let from = if self.follow { max } else { self.top };
        self.top = from.saturating_add_signed(by).min(max);
        self.follow = self.top == max && by > 0;
    }

    pub fn page(&self) -> isize {
        self.size.1.saturating_sub(2).max(1) as isize
    }

    pub fn home(&mut self) {
        self.top = 0;
        self.follow = false;
    }

    pub fn end(&mut self) {
        self.follow = true;
    }

    pub fn toggle_follow(&mut self) {
        if self.follow {
            self.scroll(-1)
        } else {
            self.end()
        }
    }

    pub fn hscroll(&mut self, by: isize) {
        if !self.wrap {
            self.hscroll = self.hscroll.saturating_add_signed(by);
        }
    }

    pub fn toggle_wrap(&mut self) {
        self.wrap = !self.wrap;
        self.hscroll = 0;
    }

    pub fn cycle_level(&mut self) {
        self.level = match self.level {
            Level::All => Level::Warn,
            Level::Warn => Level::Error,
            Level::Error => Level::All,
        };
        self.refilter();
    }

    /// Jump to the next (or previous) shown line containing the query.
    pub fn find(&mut self, forward: bool) {
        let q = self.query.to_lowercase();
        if q.is_empty() {
            return;
        }
        let n = self.shown.len();
        let start = self
            .hit
            .filter(|&h| h < n)
            .unwrap_or(if self.follow { n } else { self.top });
        let hit = (1..=n)
            .map(|d| {
                if forward {
                    (start + d) % n
                } else {
                    (start + n - d % n) % n
                }
            })
            .find(|&p| self.line(p).to_lowercase().contains(&q));
        match hit {
            Some(p) => {
                self.hit = Some(p);
                // Leave a little context above the match.
                self.top = p.saturating_sub(2).min(self.max_top());
                self.follow = false;
                self.note = None;
            }
            None => self.note = Some(format!("not found: {}", self.query)),
        }
    }

    pub fn open_prompt(&mut self, kind: Prompt) {
        let initial = match kind {
            Prompt::Search => String::new(),
            Prompt::Filter => self.filter.clone(),
        };
        self.prompt = Some((kind, initial));
    }

    /// Apply the typed prompt: search jumps to the newest match when following the tail,
    /// else the next one down; filter narrows the shown lines (empty clears it).
    pub fn submit_prompt(&mut self) {
        let Some((kind, text)) = self.prompt.take() else {
            return;
        };
        match kind {
            Prompt::Search => {
                self.query = text;
                self.hit = None;
                let forward = !self.follow;
                self.find(forward);
            }
            Prompt::Filter => {
                self.filter = text;
                self.refilter();
            }
        }
    }

    /// Esc: clear the search highlight, then the filter, then report nothing to clear.
    pub fn clear_one(&mut self) -> bool {
        if !self.query.is_empty() {
            self.query.clear();
            self.hit = None;
        } else if !self.filter.is_empty() || self.level != Level::All {
            self.filter.clear();
            self.level = Level::All;
            self.refilter();
        } else {
            return false;
        }
        true
    }

    /// Text currently on screen, for copying.
    pub fn visible_text(&self) -> String {
        let top = if self.follow {
            self.max_top()
        } else {
            self.top
        };
        let mut rows = 0;
        let mut out = vec![];
        for pos in top.min(self.shown.len())..self.shown.len() {
            if rows >= self.size.1 {
                break;
            }
            let l = self.line(pos);
            rows += self.rows(l);
            out.push(l);
        }
        out.join("\n")
    }

    pub fn draw(&mut self, f: &mut Frame, area: Rect) {
        let [head, body, foot] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(area);
        self.size = (body.width as usize, body.height as usize);
        let max = self.max_top();
        if self.follow {
            self.top = max;
        }
        self.top = self.top.min(max);

        let q = self.query.to_lowercase();
        let mut rows = 0;
        let mut last = self.top;
        let mut text = vec![];
        for pos in self.top..self.shown.len() {
            if rows >= self.size.1 {
                break;
            }
            let l = self.line(pos);
            rows += self.rows(l);
            last = pos;
            let line = styled(l, &q);
            text.push(if self.hit == Some(pos) {
                line.patch_style(Style::new().bg(Color::DarkGray))
            } else {
                line
            });
        }
        let mut para = Paragraph::new(text);
        para = if self.wrap {
            para.wrap(Wrap { trim: false })
        } else {
            para.scroll((0, self.hscroll.min(u16::MAX as usize) as u16))
        };
        f.render_widget(para, body);

        let n = self.shown.len();
        let pos = if n == 0 {
            "empty".into()
        } else {
            format!(
                "{}–{} of {n}{}",
                self.top + 1,
                last + 1,
                if self.truncated { " (tail)" } else { "" }
            )
        };
        let mode = if self.follow {
            " FOLLOW ".black().on_green()
        } else {
            " PAUSED ".black().on_yellow()
        };
        let mut head_spans = vec![
            format!(" {} ", self.name).bold().reversed(),
            " ".into(),
            mode,
            format!("  {pos}  ").into(),
        ];
        if self.level != Level::All {
            let l = if self.level == Level::Warn {
                " WARN+ "
            } else {
                " ERROR "
            };
            head_spans.push(l.black().on_red());
            head_spans.push(" ".into());
        }
        if !self.filter.is_empty() {
            head_spans.push(format!(" &{} ", self.filter).black().on_cyan());
            head_spans.push(" ".into());
        }
        if !self.wrap {
            head_spans.push("nowrap  ".dark_gray());
        }
        head_spans.push(self.path.display().to_string().dark_gray());
        f.render_widget(Line::from(head_spans), head);

        let footer = if let Some((kind, text)) = &self.prompt {
            Line::from(vec![
                if *kind == Prompt::Search { "/" } else { "&" }.yellow(),
                text.clone().into(),
                "█".dark_gray(),
            ])
        } else if let Some(n) = &self.note {
            Line::from(n.clone().yellow())
        } else {
            " / find · & filter · v level · f follow · w wrap · tab daemon · o pager · y copy · ? help · esc back"
                .dark_gray()
                .into()
        };
        f.render_widget(footer, foot);
    }
}

/// Severity guessed from the text: tracing's level column, or common error words in
/// server stderr.
fn level_of(line: &str) -> Level {
    if line.contains("ERROR") || line.contains("panicked") || line.contains("Traceback") {
        Level::Error
    } else if line.contains(" WARN ") || line.contains("WARNING") {
        Level::Warn
    } else {
        Level::All
    }
}

/// Color by level and highlight search hits.
fn styled<'a>(line: &'a str, q: &str) -> Line<'a> {
    let base = match level_of(line) {
        Level::Error => Style::new().fg(Color::Red),
        Level::Warn => Style::new().fg(Color::Yellow),
        Level::All => Style::new(),
    };
    if q.is_empty() {
        return Line::styled(line, base);
    }
    // Lowercasing ASCII keeps byte offsets; skip highlighting otherwise.
    let lower = line.to_ascii_lowercase();
    if !line.is_ascii() || !lower.contains(q) {
        return Line::styled(line, base);
    }
    let hit = Style::new()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let mut spans = vec![];
    let mut at = 0;
    for (i, m) in lower.match_indices(q) {
        spans.push(Span::styled(&line[at..i], base));
        spans.push(Span::styled(&line[i..i + m.len()], hit));
        at = i + m.len();
    }
    spans.push(Span::styled(&line[at..], base));
    Line::from(spans)
}

/// Drop ANSI escapes and control characters that would corrupt the screen; expand tabs.
fn clean(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    // CSI: parameters, then one final byte in @..~
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                } else {
                    chars.next();
                }
            }
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::clean;

    #[test]
    fn strips_ansi_and_controls() {
        assert_eq!(clean("\x1b[31mred\x1b[0m\tx\r"), "red    x");
    }
}

//! The overview: every agent as a live card in a grid.
//!
//! Each card shows the bottom of the agent's terminal (colours included),
//! its state and how long it has been in it, and its worktree's diff
//! stats. The border colour is the state, so a blocked agent stands out
//! from across the room.

use std::collections::HashMap;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget};
use tui_term::widget::{Cursor, PseudoTerminal};
use unicode_width::UnicodeWidthStr;

use crate::protocol::{AgentState, PaneId, PaneInfo};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub const MIN_CARD_W: u16 = 44;
pub const MIN_CARD_H: u16 = 7;
const MAX_COLS: u16 = 4;

/// Panes in display order: grouped by workspace, workspaces in order of
/// their first pane. Number keys follow this order.
pub fn display_order(panes: &[PaneInfo]) -> Vec<&PaneInfo> {
    let mut workspaces: Vec<&str> = Vec::new();
    for p in panes {
        if !workspaces.contains(&p.workspace.as_str()) {
            workspaces.push(&p.workspace);
        }
    }
    workspaces
        .iter()
        .flat_map(|w| panes.iter().filter(move |p| p.workspace == *w))
        .collect()
}

pub fn state_color(s: AgentState) -> Color {
    match s {
        AgentState::Working => Color::Yellow,
        AgentState::Blocked => Color::Red,
        AgentState::Done => Color::Green,
        AgentState::Idle => Color::Blue,
        AgentState::Exited => Color::DarkGray,
    }
}

pub fn state_icon(s: AgentState, frame: usize) -> &'static str {
    match s {
        AgentState::Working => SPINNER[frame % SPINNER.len()],
        AgentState::Blocked => "●",
        AgentState::Done => "✓",
        AgentState::Idle => "○",
        AgentState::Exited => "✗",
    }
}

/// `blocked 4m`, `exit 1`, `idle`.
pub fn state_label(p: &PaneInfo, age: Duration) -> String {
    match p.state {
        AgentState::Exited => match p.exit_code {
            Some(c) => format!("exit {c}"),
            None => "exited".into(),
        },
        AgentState::Idle => "idle".into(),
        s => format!("{s} {}", short_duration(age)),
    }
}

/// `12s`, `4m`, `1h5m`, `2d`.
pub fn short_duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => match (s % 3600) / 60 {
            0 => format!("{}h", s / 3600),
            m => format!("{}h{m}m", s / 3600),
        },
        _ => format!("{}d", s / 86_400),
    }
}

/// `⎇ corral/fix-auth · 3 files +42 −7`, or `None` without a worktree.
pub fn worktree_label(p: &PaneInfo) -> Option<String> {
    let w = p.worktree.as_ref()?;
    Some(match w.diff_files {
        0 => format!("⎇ {} · no changes", w.branch),
        n => format!(
            "⎇ {} · {n} {} +{} −{}",
            w.branch,
            if n == 1 { "file" } else { "files" },
            w.added,
            w.removed
        ),
    })
}

/// Cut `s` to at most `width` columns, ending with `…` if shortened.
pub fn truncate(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// How cards are arranged in an area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridLayout {
    pub cols: usize,
    /// Rows of cards that fit on screen at once.
    pub rows_visible: usize,
    pub card_h: u16,
}

pub fn grid_layout(n: usize, area: Rect) -> GridLayout {
    let n = n.max(1);
    let cols = ((area.width / MIN_CARD_W).clamp(1, MAX_COLS) as usize).min(n);
    let rows_needed = n.div_ceil(cols);
    let fit = ((area.height / MIN_CARD_H) as usize).max(1);
    let rows_visible = rows_needed.min(fit);
    let card_h = area.height / rows_visible as u16;
    GridLayout {
        cols,
        rows_visible,
        card_h,
    }
}

/// A pane's latest preview, and an emulator built from it at the height it
/// was last drawn (rebuilt when either changes).
pub struct Preview {
    pub cols: u16,
    pub bytes: Vec<u8>,
    cache: Option<(u16, vt100::Parser)>,
}

impl Preview {
    pub fn new(cols: u16, bytes: Vec<u8>) -> Self {
        Preview {
            cols,
            bytes,
            cache: None,
        }
    }

    /// An emulator `rows` high: feeding it the preview rows scrolls the
    /// bottom ones into view, which is exactly what a card shows.
    pub fn screen(&mut self, rows: u16) -> &vt100::Screen {
        let rows = rows.max(1);
        if self.cache.as_ref().is_none_or(|(h, _)| *h != rows) {
            let mut p = vt100::Parser::new(rows, self.cols.max(1), 0);
            p.process(&self.bytes);
            self.cache = Some((rows, p));
        }
        self.cache
            .as_ref()
            .map(|(_, p)| p.screen())
            .expect("just built")
    }
}

pub struct Grid<'a> {
    pub panes: &'a [PaneInfo],
    pub selected: Option<&'a str>,
    /// Seconds in state when the pane list arrived, plus time since.
    pub age_offset: Duration,
    pub frame: usize,
    /// First row of cards shown.
    pub scroll: usize,
}

impl Grid<'_> {
    /// Draw the cards; returns each visible card's area for mouse hits.
    pub fn render(
        &self,
        area: Rect,
        buf: &mut Buffer,
        previews: &mut HashMap<PaneId, Preview>,
    ) -> Vec<(Rect, PaneId)> {
        let order = display_order(self.panes);
        let layout = grid_layout(order.len(), area);
        let mut hits = Vec::new();
        let card_w = area.width / layout.cols as u16;
        for (i, p) in order.iter().enumerate() {
            let row = i / layout.cols;
            if row < self.scroll || row >= self.scroll + layout.rows_visible {
                continue;
            }
            let col = i % layout.cols;
            let x = area.x + col as u16 * card_w;
            let w = if col + 1 == layout.cols {
                area.width - col as u16 * card_w
            } else {
                card_w
            };
            let y = area.y + (row - self.scroll) as u16 * layout.card_h;
            let rect = Rect::new(x, y, w, layout.card_h);
            let age = Duration::from_secs(p.state_secs) + self.age_offset;
            self.card(p, i, age, rect, buf, previews.get_mut(&p.id));
            hits.push((rect, p.id.clone()));
        }
        hits
    }

    fn card(
        &self,
        p: &PaneInfo,
        index: usize,
        age: Duration,
        area: Rect,
        buf: &mut Buffer,
        preview: Option<&mut Preview>,
    ) {
        let selected = self.selected == Some(p.id.as_str());
        let color = state_color(p.state);
        let border = Style::new().fg(color);
        let border = if selected {
            border.add_modifier(Modifier::BOLD)
        } else if matches!(p.state, AgentState::Idle | AgentState::Exited) {
            border.add_modifier(Modifier::DIM)
        } else {
            border
        };
        let num = if index < 9 {
            format!("{} ", index + 1)
        } else {
            String::new()
        };
        let title = Line::from(vec![
            Span::raw(" "),
            Span::styled(num, Style::new().fg(Color::DarkGray)),
            Span::styled(state_icon(p.state, self.frame), Style::new().fg(color)),
            Span::raw(" "),
            Span::styled(
                p.name.clone(),
                Style::new().add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ),
            Span::styled(
                format!(" · {} ", p.workspace),
                Style::new().fg(Color::DarkGray),
            ),
        ]);
        let label = Line::from(Span::styled(
            format!(" {} ", state_label(p, age)),
            if p.state == AgentState::Blocked {
                Style::new()
                    .fg(Color::Black)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(color)
            },
        ))
        .right_aligned();
        let mut block = Block::new()
            .borders(Borders::ALL)
            .border_type(if selected {
                BorderType::Thick
            } else {
                BorderType::Rounded
            })
            .border_style(border)
            .title(title)
            .title(label);
        let room = area.width.saturating_sub(4) as usize;
        if let Some(wt) = worktree_label(p) {
            block = block.title_bottom(Line::styled(
                format!(" {} ", truncate(&wt, room)),
                Style::new().fg(Color::Magenta),
            ));
        } else if let Some(t) = &p.title {
            block = block.title_bottom(Line::styled(
                format!(" {} ", truncate(t, room)),
                Style::new().fg(Color::DarkGray),
            ));
        }
        let inner = block.inner(area);
        block.render(area, buf);

        match preview {
            Some(pv) if inner.height > 0 => {
                let screen = pv.screen(inner.height);
                PseudoTerminal::new(screen)
                    .cursor(Cursor::default().visibility(false))
                    .render(inner, buf);
            }
            _ => {
                Paragraph::new(Line::styled("…", Style::new().fg(Color::DarkGray)))
                    .render(inner, buf);
            }
        }
    }
}

/// Counts for the top bar: `(blocked, done, working, idle)`.
pub fn counts<'a>(panes: impl IntoIterator<Item = &'a PaneInfo>) -> (usize, usize, usize, usize) {
    let mut c = (0, 0, 0, 0);
    for p in panes {
        match p.state {
            AgentState::Blocked => c.0 += 1,
            AgentState::Done => c.1 += 1,
            AgentState::Working => c.2 += 1,
            AgentState::Idle | AgentState::Exited => c.3 += 1,
        }
    }
    c
}

/// `● 2 need you  ✓ 1 done  ⠋ 3 working  ○ 1 idle`, skipping zeros.
pub fn summary_spans(c: (usize, usize, usize, usize), frame: usize) -> Vec<Span<'static>> {
    let (blocked, done, working, idle) = c;
    let mut spans = Vec::new();
    let mut push = |n: usize, icon: &str, what: &str, color: Color, strong: bool| {
        if n == 0 {
            return;
        }
        let style = Style::new().fg(color);
        let style = if strong {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        };
        spans.push(Span::styled(format!("  {icon} {n} {what}"), style));
    };
    push(blocked, "●", "need you", Color::Red, true);
    push(done, "✓", "done", Color::Green, false);
    push(
        working,
        SPINNER[frame % SPINNER.len()],
        "working",
        Color::Yellow,
        false,
    );
    push(idle, "○", "idle", Color::Blue, false);
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::WorktreeInfo;
    use ratatui::{Terminal, backend::TestBackend};

    fn pane(id: &str, name: &str, ws: &str, state: AgentState, secs: u64) -> PaneInfo {
        PaneInfo {
            id: id.into(),
            name: name.into(),
            workspace: ws.into(),
            command: vec![name.into()],
            cwd: "/tmp".into(),
            agent: Some(name.into()),
            state,
            pid: Some(1),
            exit_code: None,
            title: None,
            worktree: None,
            cols: 80,
            rows: 24,
            state_secs: secs,
        }
    }

    fn sample() -> Vec<PaneInfo> {
        let mut a = pane("p1", "claude", "api", AgentState::Blocked, 250);
        a.worktree = Some(WorktreeInfo {
            branch: "corral/fix-auth".into(),
            diff_files: 3,
            added: 42,
            removed: 7,
            ..Default::default()
        });
        let b = pane("p2", "codex", "web", AgentState::Working, 30);
        let c = pane("p3", "opencode", "api", AgentState::Done, 125);
        let mut d = pane("p4", "zsh", "web", AgentState::Exited, 0);
        d.exit_code = Some(1);
        vec![a, b, c, d]
    }

    fn previews() -> HashMap<PaneId, Preview> {
        let mut m = HashMap::new();
        m.insert(
            "p1".to_string(),
            Preview::new(
                60,
                b"Edit src/auth.rs\r\nDo you want to proceed?\r\n\x1b[36m> 1. Yes\x1b[m\r\n  2. No"
                    .to_vec(),
            ),
        );
        m.insert(
            "p2".to_string(),
            Preview::new(
                60,
                b"test parser::ok ... ok\r\ntest lexer::ws ... ok".to_vec(),
            ),
        );
        m
    }

    fn render(
        panes: &[PaneInfo],
        selected: Option<&str>,
        w: u16,
        h: u16,
    ) -> (String, Vec<(Rect, PaneId)>) {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut hits = Vec::new();
        let mut pv = previews();
        term.draw(|f| {
            hits = Grid {
                panes,
                selected,
                age_offset: Duration::ZERO,
                frame: 0,
                scroll: 0,
            }
            .render(f.area(), f.buffer_mut(), &mut pv);
        })
        .unwrap();
        let buf = term.backend().buffer();
        let mut s = String::new();
        for y in 0..buf.area.height {
            let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            s.push_str(line.trim_end());
            s.push('\n');
        }
        (s, hits)
    }

    #[test]
    fn grid_snapshot() {
        let (text, hits) = render(&sample(), Some("p1"), 100, 18);
        insta::assert_snapshot!(text);
        let ids: Vec<&str> = hits.iter().map(|(_, id)| id.as_str()).collect();
        assert_eq!(ids, vec!["p1", "p3", "p2", "p4"]);
    }

    #[test]
    fn narrow_grid_is_one_column_and_scrolls() {
        let l = grid_layout(4, Rect::new(0, 0, 60, 20));
        assert_eq!(
            l,
            GridLayout {
                cols: 1,
                rows_visible: 2,
                card_h: 10
            }
        );
        let (_, hits) = render(&sample(), Some("p1"), 60, 20);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn layouts() {
        assert_eq!(grid_layout(1, Rect::new(0, 0, 200, 50)).cols, 1);
        assert_eq!(grid_layout(6, Rect::new(0, 0, 200, 50)).cols, 4);
        assert_eq!(
            grid_layout(3, Rect::new(0, 0, 100, 40)),
            GridLayout {
                cols: 2,
                rows_visible: 2,
                card_h: 20
            }
        );
    }

    #[test]
    fn durations_and_labels() {
        assert_eq!(short_duration(Duration::from_secs(12)), "12s");
        assert_eq!(short_duration(Duration::from_secs(250)), "4m");
        assert_eq!(short_duration(Duration::from_secs(3900)), "1h5m");
        assert_eq!(short_duration(Duration::from_secs(7200)), "2h");
        let p = &sample()[0];
        assert_eq!(state_label(p, Duration::from_secs(250)), "blocked 4m");
        assert_eq!(
            worktree_label(p).unwrap(),
            "⎇ corral/fix-auth · 3 files +42 −7"
        );
    }

    #[test]
    fn preview_shows_the_bottom_rows() {
        let mut pv = Preview::new(20, b"one\r\ntwo\r\nthree".to_vec());
        assert_eq!(pv.screen(2).contents(), "two\nthree");
        assert_eq!(pv.screen(5).contents(), "one\ntwo\nthree");
    }

    #[test]
    fn summary() {
        let spans = summary_spans(counts(&sample()), 0);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "  ● 1 need you  ✓ 1 done  ⠋ 1 working  ○ 1 idle");
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("日本語", 5), "日本…");
    }
}

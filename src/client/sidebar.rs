//! The sidebar: panes grouped by workspace, with state badges and
//! worktree status. Rendering returns which row belongs to which pane so
//! mouse clicks can be mapped back.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Widget};
use std::time::Duration;

use unicode_width::UnicodeWidthStr;

use crate::protocol::{AgentState, PaneId, PaneInfo};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Panes in sidebar order: grouped by workspace, workspaces in order of
/// their first pane. Number keys and j/k follow this order.
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

/// State badge; blocked and done show how long they've been waiting.
pub fn badge(p: &PaneInfo, age: Duration, frame: usize) -> (String, Style) {
    let age = short_duration(age);
    match p.state {
        AgentState::Working => (
            format!("{} working", SPINNER[frame % SPINNER.len()]),
            Style::new().fg(Color::Yellow),
        ),
        AgentState::Blocked => (
            format!("● blocked {age}"),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        AgentState::Done => (format!("✓ done {age}"), Style::new().fg(Color::Green)),
        AgentState::Idle => ("○ idle".into(), Style::new().fg(Color::DarkGray)),
        AgentState::Exited => (
            match p.exit_code {
                Some(c) => format!("✗ exit {c}"),
                None => "✗ exited".into(),
            },
            Style::new().fg(Color::DarkGray),
        ),
    }
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

/// What clicking somewhere in the sidebar does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Click {
    Focus(PaneId),
    /// The "+ new agent" / "⎇ worktree" buttons.
    New {
        worktree: bool,
    },
    /// The "✕ exit" button.
    Exit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub area: Rect,
    pub click: Click,
}

impl Hit {
    pub fn contains(&self, x: u16, y: u16) -> bool {
        self.area.contains((x, y).into())
    }
}

pub struct Sidebar<'a> {
    pub panes: &'a [PaneInfo],
    pub focus: Option<&'a str>,
    pub frame: usize,
    /// Time since `panes` arrived, added to each pane's `state_secs`.
    pub age_offset: Duration,
}

impl Sidebar<'_> {
    /// Render into `area`; returns `(row, pane id)` for clickable rows.
    pub fn render(&self, area: Rect, buf: &mut Buffer) -> Vec<Hit> {
        let block = Block::new()
            .borders(Borders::RIGHT)
            .border_style(Style::new().fg(Color::DarkGray));
        let inner = block.inner(area);
        block.render(area, buf);
        let width = inner.width as usize;
        if width < 8 || inner.height == 0 {
            return Vec::new();
        }

        let mut lines: Vec<(Line, Option<String>)> = Vec::new();
        let blocked = self
            .panes
            .iter()
            .filter(|p| p.state == AgentState::Blocked)
            .count();
        let done = self
            .panes
            .iter()
            .filter(|p| p.state == AgentState::Done)
            .count();
        let mut title = vec![Span::styled(
            " corral",
            Style::new().add_modifier(Modifier::BOLD),
        )];
        if blocked > 0 {
            title.push(Span::styled(
                format!("  ●{blocked}"),
                Style::new().fg(Color::Red),
            ));
        }
        if done > 0 {
            title.push(Span::styled(
                format!("  ✓{done}"),
                Style::new().fg(Color::Green),
            ));
        }
        lines.push((Line::from(title), None));

        if self.panes.is_empty() {
            lines.push((Line::default(), None));
            lines.push((
                Line::styled(" no agents yet", Style::new().fg(Color::DarkGray)),
                None,
            ));
        }

        let mut last_ws: Option<&str> = None;
        for (n, p) in display_order(self.panes).into_iter().enumerate() {
            if last_ws != Some(p.workspace.as_str()) {
                lines.push((Line::default(), None));
                lines.push((
                    Line::styled(
                        format!(" {}", truncate(&p.workspace, width.saturating_sub(1))),
                        Style::new()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::BOLD),
                    ),
                    None,
                ));
                last_ws = Some(&p.workspace);
            }
            let focused = self.focus == Some(p.id.as_str());
            let age = Duration::from_secs(p.state_secs) + self.age_offset;
            let (badge, badge_style) = badge(p, age, self.frame);
            let marker = if focused { "▌" } else { " " };
            let num = if n < 9 {
                format!("{}", n + 1)
            } else {
                " ".into()
            };
            let left_fixed = format!("{marker}{num} ");
            let name_room = width
                .saturating_sub(left_fixed.width() + badge.width() + 1)
                .max(1);
            let name = truncate(&p.name, name_room);
            let pad = width.saturating_sub(left_fixed.width() + name.width() + badge.width());
            let row_style = if focused {
                Style::new().bg(Color::Indexed(236))
            } else {
                Style::new()
            };
            let name_style = if focused {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            let line = Line::from(vec![
                Span::styled(marker.to_string(), Style::new().fg(Color::Cyan)),
                Span::styled(format!("{num} "), Style::new().fg(Color::DarkGray)),
                Span::styled(name, name_style),
                Span::raw(" ".repeat(pad)),
                Span::styled(badge, badge_style),
            ])
            .style(row_style);
            lines.push((line, Some(p.id.clone())));

            if let Some(w) = &p.worktree {
                // Lines added/removed since the branch started.
                let text = match w.diff_files {
                    0 => format!("   ⎇ {}", w.branch),
                    _ => format!("   ⎇ {} +{} −{}", w.branch, w.added, w.removed),
                };
                lines.push((
                    Line::styled(truncate(&text, width), Style::new().fg(Color::Magenta))
                        .style(row_style),
                    Some(p.id.clone()),
                ));
            }
        }

        // The buttons sit on the last row, with a blank row above them.
        let show_buttons = inner.height >= 4;
        let list_rows = if show_buttons {
            inner.height - 2
        } else {
            inner.height
        };
        let mut hits = Vec::new();
        for (i, (line, id)) in lines.into_iter().take(list_rows as usize).enumerate() {
            let y = inner.y + i as u16;
            let row = Rect::new(inner.x, y, inner.width, 1);
            if line.style.bg.is_some() {
                buf.set_style(row, line.style);
            }
            line.render(row, buf);
            if let Some(id) = id {
                hits.push(Hit {
                    area: row,
                    click: Click::Focus(id),
                });
            }
        }
        hits.push(render_exit_button(
            Rect::new(inner.x, inner.y, inner.width, 1),
            buf,
        ));
        if show_buttons {
            hits.extend(render_buttons(
                Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
                buf,
            ));
        }
        hits
    }
}

/// `✕ exit`, right-aligned on the title row (`✕` alone when narrow).
fn render_exit_button(row: Rect, buf: &mut Buffer) -> Hit {
    let label = if row.width >= 24 {
        " ✕ exit "
    } else {
        " ✕ "
    };
    let w = label.width() as u16;
    let area = Rect::new(row.x + row.width.saturating_sub(w + 1), row.y, w, 1);
    Line::styled(label, Style::new().fg(Color::Black).bg(Color::Gray)).render(area, buf);
    Hit {
        area,
        click: Click::Exit,
    }
}

/// `[ + new agent ] [ ⎇ worktree ]`, shortened when the sidebar is narrow.
fn render_buttons(row: Rect, buf: &mut Buffer) -> Vec<Hit> {
    let (new, wt) = if row.width >= 27 {
        (" + new agent ", " ⎇ worktree ")
    } else {
        (" + new ", " ⎇ wt ")
    };
    let buttons = [
        (
            new,
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            false,
        ),
        (wt, Style::new().fg(Color::Black).bg(Color::Magenta), true),
    ];
    let mut hits = Vec::new();
    let mut x = row.x + 1;
    for (label, style, worktree) in buttons {
        let w = label.width() as u16;
        if x + w > row.x + row.width {
            break;
        }
        let area = Rect::new(x, row.y, w, 1);
        Line::styled(label, style).render(area, buf);
        hits.push(Hit {
            area,
            click: Click::New { worktree },
        });
        x += w + 1;
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::WorktreeInfo;
    use ratatui::{Terminal, backend::TestBackend};

    fn pane(id: &str, name: &str, ws: &str, state: AgentState) -> PaneInfo {
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
            state_secs: 250,
        }
    }

    fn render(panes: &[PaneInfo], focus: Option<&str>, width: u16) -> (String, Vec<Hit>) {
        let mut term = Terminal::new(TestBackend::new(width, 12)).unwrap();
        let mut hits = Vec::new();
        term.draw(|f| {
            hits = Sidebar {
                panes,
                focus,
                frame: 0,
                age_offset: Duration::ZERO,
            }
            .render(f.area(), f.buffer_mut());
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

    fn sample() -> Vec<PaneInfo> {
        let mut a = pane("p1", "claude", "corral", AgentState::Blocked);
        a.worktree = Some(WorktreeInfo {
            path: "/w/corral-1/claude-3f9a".into(),
            branch: "corral/claude-3f9a".into(),
            repo: "/r".into(),
            changed_files: 3,
            diff_files: 3,
            added: 42,
            removed: 7,
            pane: Some("p1".into()),
            ..Default::default()
        });
        let b = pane("p2", "codex", "website", AgentState::Working);
        let c = pane("p3", "a-very-long-agent-name", "corral", AgentState::Done);
        let mut d = pane("p4", "zsh", "website", AgentState::Exited);
        d.exit_code = Some(1);
        vec![a, b, c, d]
    }

    #[test]
    fn groups_by_workspace() {
        let panes = sample();
        let order: Vec<&str> = display_order(&panes)
            .iter()
            .map(|p| p.id.as_str())
            .collect();
        assert_eq!(order, vec!["p1", "p3", "p2", "p4"]);
    }

    #[test]
    fn sidebar_snapshot() {
        let (text, hits) = render(&sample(), Some("p1"), 32);
        insta::assert_snapshot!(text);
        let clicks: Vec<Click> = hits.iter().map(|h| h.click.clone()).collect();
        let f = |id: &str| Click::Focus(id.into());
        assert_eq!(
            clicks,
            vec![
                f("p1"),
                f("p1"),
                f("p3"),
                f("p2"),
                f("p4"),
                Click::Exit,
                Click::New { worktree: false },
                Click::New { worktree: true },
            ]
        );
        // Exit is on the title row; the new buttons on the last row.
        assert_eq!(hits[5].area.y, 0);
        let (a, b) = (hits[6].area, hits[7].area);
        assert_eq!((a.y, b.y), (11, 11));
        assert!(a.x + a.width < b.x);
    }

    #[test]
    fn sidebar_empty_snapshot() {
        let (text, hits) = render(&[], None, 32);
        insta::assert_snapshot!(text);
        // Only the buttons are clickable.
        assert!(hits.iter().all(|h| !matches!(h.click, Click::Focus(_))));
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn sidebar_narrow_snapshot() {
        let (text, _) = render(&sample(), Some("p2"), 20);
        insta::assert_snapshot!(text);
    }

    #[test]
    fn durations() {
        assert_eq!(short_duration(Duration::from_secs(12)), "12s");
        assert_eq!(short_duration(Duration::from_secs(250)), "4m");
        assert_eq!(short_duration(Duration::from_secs(3900)), "1h5m");
        assert_eq!(short_duration(Duration::from_secs(7200)), "2h");
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("日本語", 5), "日本…");
    }
}

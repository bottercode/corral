//! The sidebar: panes grouped by workspace, with state badges and
//! worktree status. Rendering returns which row belongs to which pane so
//! mouse clicks can be mapped back.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Widget};
use unicode_width::UnicodeWidthStr;

use crate::protocol::{AgentState, PaneInfo};

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

pub fn badge(p: &PaneInfo, frame: usize) -> (String, Style) {
    match p.state {
        AgentState::Working => (
            format!("{} working", SPINNER[frame % SPINNER.len()]),
            Style::new().fg(Color::Yellow),
        ),
        AgentState::Blocked => (
            "● blocked".into(),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        AgentState::Done => ("✓ done".into(), Style::new().fg(Color::Green)),
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

pub struct Sidebar<'a> {
    pub panes: &'a [PaneInfo],
    pub focus: Option<&'a str>,
    pub frame: usize,
}

impl Sidebar<'_> {
    /// Render into `area`; returns `(row, pane id)` for clickable rows.
    pub fn render(&self, area: Rect, buf: &mut Buffer) -> Vec<(u16, String)> {
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
            let (badge, badge_style) = badge(p, self.frame);
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
                // `+N`: files changed in the worktree (as in `corral ls`).
                let text = match w.changed_files {
                    0 => format!("   ⎇ {}", w.branch),
                    n => format!("   ⎇ {} +{n}", w.branch),
                };
                lines.push((
                    Line::styled(truncate(&text, width), Style::new().fg(Color::Magenta))
                        .style(row_style),
                    Some(p.id.clone()),
                ));
            }
        }

        let mut hits = Vec::new();
        for (i, (line, id)) in lines.into_iter().take(inner.height as usize).enumerate() {
            let y = inner.y + i as u16;
            let row = Rect::new(inner.x, y, inner.width, 1);
            if line.style.bg.is_some() {
                buf.set_style(row, line.style);
            }
            line.render(row, buf);
            if let Some(id) = id {
                hits.push((y, id));
            }
        }
        hits
    }
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
        }
    }

    fn render(panes: &[PaneInfo], focus: Option<&str>, width: u16) -> (String, Vec<(u16, String)>) {
        let mut term = Terminal::new(TestBackend::new(width, 12)).unwrap();
        let mut hits = Vec::new();
        term.draw(|f| {
            hits = Sidebar {
                panes,
                focus,
                frame: 0,
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
            pane: Some("p1".into()),
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
        let ids: Vec<&str> = hits.iter().map(|(_, id)| id.as_str()).collect();
        assert_eq!(ids, vec!["p1", "p1", "p3", "p2", "p4"]);
    }

    #[test]
    fn sidebar_empty_snapshot() {
        let (text, hits) = render(&[], None, 32);
        insta::assert_snapshot!(text);
        assert!(hits.is_empty());
    }

    #[test]
    fn sidebar_narrow_snapshot() {
        let (text, _) = render(&sample(), Some("p2"), 20);
        insta::assert_snapshot!(text);
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("日本語", 5), "日本…");
    }
}

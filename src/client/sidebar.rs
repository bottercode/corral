//! The sidebar. By default it's organised for triage: agents that need you
//! (blocked, or finished and unseen) on top, then the ones working, then the
//! quiet ones. It can list them by project instead. The focused agent opens
//! up to show its title, project and worktree. Rendering returns what each
//! row does when clicked.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use std::time::Duration;

use unicode_width::UnicodeWidthStr;

use super::theme;
use crate::protocol::{AgentState, PaneId, PaneInfo};

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How the sidebar lists agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Order {
    /// NEEDS YOU, WORKING, QUIET.
    #[default]
    Need,
    /// One section per project (workspace), in the order they started.
    Project,
}

impl Order {
    /// From the config's `sidebar_order`.
    pub fn from_config(s: &str) -> Order {
        match s {
            "project" => Order::Project,
            _ => Order::Need,
        }
    }

    pub fn config_value(self) -> &'static str {
        match self {
            Order::Need => "need",
            Order::Project => "project",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Order::Need => "by need",
            Order::Project => "by project",
        }
    }

    pub fn toggled(self) -> Order {
        match self {
            Order::Need => Order::Project,
            Order::Project => Order::Need,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Need {
    NeedsYou,
    Working,
    Quiet,
}

impl Need {
    fn of(s: AgentState) -> Need {
        match s {
            AgentState::Blocked | AgentState::Done => Need::NeedsYou,
            AgentState::Working => Need::Working,
            AgentState::Idle | AgentState::Exited => Need::Quiet,
        }
    }
}

/// A titled run of agents in the sidebar.
pub struct Section<'a> {
    pub title: String,
    /// Colour of the count next to the title.
    pub color: Color,
    pub members: Vec<&'a PaneInfo>,
}

/// Non-empty sections in sidebar order.
///
/// By need, blocked comes before done and exited after idle within a
/// section; otherwise agents keep their creation order. By project, each
/// project lists its agents in creation order.
pub fn sections(panes: &[PaneInfo], order: Order) -> Vec<Section<'_>> {
    let t = theme::get();
    match order {
        Order::Need => {
            let rank = |s: AgentState| match s {
                AgentState::Blocked | AgentState::Working | AgentState::Idle => 0,
                AgentState::Done | AgentState::Exited => 1,
            };
            [
                (Need::NeedsYou, "NEEDS YOU"),
                (Need::Working, "WORKING"),
                (Need::Quiet, "QUIET"),
            ]
            .into_iter()
            .filter_map(|(need, title)| {
                let mut members: Vec<&PaneInfo> =
                    panes.iter().filter(|p| Need::of(p.state) == need).collect();
                members.sort_by_key(|p| rank(p.state));
                let color = match need {
                    Need::NeedsYou if members.iter().any(|p| p.state == AgentState::Blocked) => {
                        t.blocked
                    }
                    Need::NeedsYou => t.done,
                    Need::Working => t.working,
                    Need::Quiet => t.muted,
                };
                (!members.is_empty()).then(|| Section {
                    title: title.into(),
                    color,
                    members,
                })
            })
            .collect()
        }
        Order::Project => {
            let mut projects: Vec<&str> = Vec::new();
            for p in panes {
                if !projects.contains(&p.workspace.as_str()) {
                    projects.push(&p.workspace);
                }
            }
            projects
                .into_iter()
                .map(|w| {
                    let members: Vec<&PaneInfo> =
                        panes.iter().filter(|p| p.workspace == w).collect();
                    // The count takes the colour of the most pressing agent.
                    let color = [AgentState::Blocked, AgentState::Done, AgentState::Working]
                        .into_iter()
                        .find(|s| members.iter().any(|p| p.state == *s))
                        .map_or(t.muted, |s| t.state(s));
                    Section {
                        title: w.to_string(),
                        color,
                        members,
                    }
                })
                .collect()
        }
    }
}

/// Panes in the order the sidebar lists them; j/k follow this order.
pub fn display_order(panes: &[PaneInfo], order: Order) -> Vec<&PaneInfo> {
    sections(panes, order)
        .into_iter()
        .flat_map(|s| s.members)
        .collect()
}

/// A pane's number for `^b 1`…`^b 9`: its position by creation, so it stays
/// put while the pane moves between groups.
pub fn number(panes: &[PaneInfo], id: &str) -> Option<usize> {
    panes.iter().position(|p| p.id == id).map(|i| i + 1)
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

/// State badge without the word, for tight spaces: `◆ 4m`, `✓ 1m`.
pub fn compact_badge(p: &PaneInfo, age: Duration, frame: usize) -> (String, Style) {
    let (text, style) = badge(p, age, frame);
    let text = match p.state {
        AgentState::Blocked | AgentState::Done => {
            text.replacen(" blocked", "", 1).replacen(" done", "", 1)
        }
        _ => text,
    };
    (text, style)
}

/// State badge: what the agent is doing and for how long.
pub fn badge(p: &PaneInfo, age: Duration, frame: usize) -> (String, Style) {
    let t = theme::get();
    let age = short_duration(age);
    let style = t.fg(t.state(p.state));
    match p.state {
        AgentState::Working => (format!("{} {age}", SPINNER[frame % SPINNER.len()]), style),
        AgentState::Blocked => (
            format!("◆ blocked {age}"),
            style.add_modifier(Modifier::BOLD),
        ),
        AgentState::Done => (format!("✓ done {age}"), style),
        AgentState::Idle => ("idle".into(), style),
        AgentState::Exited => (
            match p.exit_code {
                Some(c) => format!("exited {c}"),
                None => "exited".into(),
            },
            t.faint(),
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

/// A terminal title without the status glyph agents put in front of it
/// (`✳ Fix the login bug` → `Fix the login bug`).
pub fn clean_title(t: &str) -> &str {
    t.trim_start_matches(|c: char| !c.is_alphanumeric())
        .trim_end()
}

/// The task an agent is on, from the title it gives its terminal. Shells'
/// own titles (`user@host: ~/dir`) aren't tasks, so plain shells have none.
pub fn task_title(p: &PaneInfo) -> Option<&str> {
    p.agent.as_ref()?;
    let t = clean_title(p.title.as_deref()?);
    let looks_like_prompt = t.split_whitespace().next().is_some_and(|w| w.contains('@'));
    (!t.is_empty() && !looks_like_prompt).then_some(t)
}

/// What clicking somewhere in the sidebar does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Click {
    Focus(PaneId),
    /// The "+ new" / "⎇ worktree" buttons.
    New {
        worktree: bool,
    },
    /// The "≡ menu" button.
    Menu,
    /// The "✕ exit" button.
    Exit,
    /// The "⇅ by need" / "⇅ by project" switch.
    Order,
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
    /// The pane with the keyboard.
    pub focus: Option<&'a str>,
    /// Panes on screen next to the focused one.
    pub shown: &'a [PaneId],
    pub order: Order,
    pub frame: usize,
    /// Time since `panes` arrived, added to each pane's `state_secs`.
    pub age_offset: Duration,
}

struct Row<'a> {
    line: Line<'a>,
    pane: Option<PaneId>,
    selected: bool,
}

impl Row<'_> {
    fn plain(line: Line<'static>) -> Row<'static> {
        Row {
            line,
            pane: None,
            selected: false,
        }
    }
}

impl Sidebar<'_> {
    /// Render into `area`; returns the clickable regions.
    pub fn render(&self, area: Rect, buf: &mut Buffer) -> Vec<Hit> {
        let t = theme::get();
        // A faint rule separates the sidebar from the panes.
        let rule_x = area.x + area.width.saturating_sub(1);
        for y in area.y..area.y + area.height {
            buf[(rule_x, y)].set_symbol("│").set_style(t.faint());
        }
        let inner = Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height);
        let width = inner.width as usize;
        if width < 10 || inner.height == 0 {
            return Vec::new();
        }

        let mut rows: Vec<Row> = vec![
            Row::plain(Line::from(Span::styled(
                " corral",
                t.accent().add_modifier(Modifier::BOLD),
            ))),
            // The rule; the order switch is drawn over its right end.
            Row::plain(Line::styled(
                format!(" {}", "─".repeat(width.saturating_sub(2))),
                t.faint(),
            )),
        ];

        if self.panes.is_empty() {
            rows.push(Row::plain(Line::styled(" no agents yet", t.muted())));
            rows.push(Row::plain(Line::styled(" start one below", t.faint())));
        }

        for (i, section) in sections(self.panes, self.order).into_iter().enumerate() {
            if i > 0 {
                rows.push(Row::plain(Line::default()));
            }
            let count = format!("  {}", section.members.len());
            let title = truncate(&section.title, width.saturating_sub(count.width() + 1));
            rows.push(Row::plain(Line::from(vec![
                Span::styled(format!(" {title}"), t.muted().add_modifier(Modifier::BOLD)),
                Span::styled(count, t.fg(section.color)),
            ])));
            for p in section.members {
                self.agent_rows(p, width, &mut rows);
            }
        }

        // Buttons on the last row, under a rule.
        let show_buttons = inner.height >= 6;
        let list_rows = if show_buttons {
            inner.height - 2
        } else {
            inner.height
        } as usize;
        let mut hits = Vec::new();
        for (i, row) in rows.into_iter().take(list_rows).enumerate() {
            let rect = Rect::new(inner.x, inner.y + i as u16, inner.width, 1);
            if row.selected {
                buf.set_style(rect, t.selected());
            }
            row.line.render(rect, buf);
            if let Some(id) = row.pane {
                hits.push(Hit {
                    area: rect,
                    click: Click::Focus(id),
                });
            }
        }
        hits.extend(render_title_buttons(
            Rect::new(inner.x, inner.y, inner.width, 1),
            buf,
        ));
        if !self.panes.is_empty() {
            hits.push(render_order_switch(
                Rect::new(inner.x, inner.y + 1, inner.width, 1),
                self.order,
                buf,
            ));
        }
        if show_buttons {
            let y = inner.y + inner.height - 2;
            Line::styled(
                format!(" {}", "─".repeat(width.saturating_sub(2))),
                t.faint(),
            )
            .render(Rect::new(inner.x, y, inner.width, 1), buf);
            hits.extend(render_buttons(
                Rect::new(inner.x, y + 1, inner.width, 1),
                buf,
            ));
        }
        hits
    }

    fn agent_rows<'a>(&self, p: &'a PaneInfo, width: usize, rows: &mut Vec<Row<'a>>) {
        let t = theme::get();
        let focused = self.focus == Some(p.id.as_str());
        let shown = !focused && self.shown.contains(&p.id);
        let age = Duration::from_secs(p.state_secs) + self.age_offset;
        let (mut badge, mut badge_style) = badge(p, age, self.frame);
        if width < 8 + 3 + badge.width() + 2 {
            (badge, badge_style) = compact_badge(p, age, self.frame);
        }
        let (marker, marker_style) = if focused {
            ("▍", t.accent())
        } else if shown {
            ("▏", t.muted())
        } else {
            (" ", Style::new())
        };
        let num = match number(self.panes, &p.id) {
            Some(n @ 1..=9) => n.to_string(),
            _ => " ".into(),
        };
        let fixed = 1 + num.width() + 1;
        let name_room = width.saturating_sub(fixed + badge.width() + 2).max(1);
        let name = truncate(&p.name, name_room);
        let pad = width.saturating_sub(fixed + name.width() + badge.width() + 1);
        let name_style = if focused {
            t.bold()
        } else if p.state == AgentState::Exited {
            t.muted()
        } else {
            Style::new().fg(t.text)
        };
        rows.push(Row {
            line: Line::from(vec![
                Span::styled(marker, marker_style),
                Span::styled(num, t.muted()),
                Span::raw(" "),
                Span::styled(name, name_style),
                Span::raw(" ".repeat(pad)),
                Span::styled(badge, badge_style),
            ]),
            pane: Some(p.id.clone()),
            selected: focused,
        });
        if !focused {
            return;
        }

        // The focused agent opens up: its title, where it runs, its branch.
        let indent = "   ";
        let room = width.saturating_sub(indent.len() + 1);
        let mut detail = |line: Line<'a>| {
            rows.push(Row {
                line,
                pane: Some(p.id.clone()),
                selected: true,
            })
        };
        let marker = || Span::styled("▍", t.accent());
        if let Some(title) = task_title(p) {
            detail(Line::from(vec![
                marker(),
                Span::raw("  "),
                Span::styled(truncate(title, room), Style::new().fg(t.text)),
            ]));
        }
        let what = p
            .agent
            .clone()
            .or_else(|| p.command.first().cloned())
            .unwrap_or_else(|| "shell".into());
        // By project, the section title already names the project.
        let place = match self.order {
            Order::Need => format!("{} · {what}", p.workspace),
            Order::Project => what,
        };
        detail(Line::from(vec![
            marker(),
            Span::raw("  "),
            Span::styled(truncate(&place, room), t.muted()),
        ]));
        if let Some(w) = &p.worktree {
            let stats = if w.diff_files > 0 {
                format!(" +{} −{}", w.added, w.removed)
            } else {
                String::new()
            };
            let branch = truncate(
                &format!("⎇ {}", w.branch),
                room.saturating_sub(stats.width()),
            );
            let mut spans = vec![marker(), Span::raw("  "), Span::styled(branch, t.fg(t.git))];
            if w.diff_files > 0 {
                spans.push(Span::styled(format!(" +{}", w.added), t.fg(t.added)));
                spans.push(Span::styled(format!(" −{}", w.removed), t.fg(t.removed)));
            }
            detail(Line::from(spans));
        }
    }
}

/// `≡ menu   ✕ exit`, right-aligned on the title row (`≡  ✕` when narrow).
fn render_title_buttons(row: Rect, buf: &mut Buffer) -> Vec<Hit> {
    let t = theme::get();
    let (menu, exit) = if row.width >= 26 {
        ("≡ menu", "✕ exit")
    } else {
        ("≡", "✕")
    };
    let exit_w = exit.width() as u16;
    let exit_area = Rect::new(
        row.x + row.width.saturating_sub(exit_w + 1),
        row.y,
        exit_w,
        1,
    );
    let menu_w = menu.width() as u16;
    let menu_area = Rect::new(exit_area.x.saturating_sub(menu_w + 3), row.y, menu_w, 1);
    Line::styled(menu, t.accent()).render(menu_area, buf);
    Line::styled(exit, t.muted()).render(exit_area, buf);
    vec![
        Hit {
            area: menu_area,
            click: Click::Menu,
        },
        Hit {
            area: exit_area,
            click: Click::Exit,
        },
    ]
}

/// ` ⇅ by need ` over the right end of the rule under the title.
fn render_order_switch(row: Rect, order: Order, buf: &mut Buffer) -> Hit {
    let t = theme::get();
    let label = format!(" ⇅ {} ", order.label());
    let w = (label.width() as u16).min(row.width.saturating_sub(2));
    let area = Rect::new(row.x + row.width.saturating_sub(w + 1), row.y, w, 1);
    Line::styled(label, t.muted()).render(area, buf);
    Hit {
        area,
        click: Click::Order,
    }
}

/// `+ new shell   ⎇ worktree`, shortened when the sidebar is narrow.
fn render_buttons(row: Rect, buf: &mut Buffer) -> Vec<Hit> {
    let t = theme::get();
    let (new, wt) = if row.width >= 24 {
        ("+ new shell", "⎇ worktree")
    } else {
        ("+ new", "⎇ wt")
    };
    let buttons = [
        (new, t.accent().add_modifier(Modifier::BOLD), false),
        (wt, t.fg(t.git), true),
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
        x += w + 3;
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

    fn render(
        panes: &[PaneInfo],
        focus: Option<&str>,
        shown: &[PaneId],
        width: u16,
    ) -> (String, Vec<Hit>) {
        render_in(panes, focus, shown, width, Order::Need)
    }

    fn render_in(
        panes: &[PaneInfo],
        focus: Option<&str>,
        shown: &[PaneId],
        width: u16,
        order: Order,
    ) -> (String, Vec<Hit>) {
        let mut term = Terminal::new(TestBackend::new(width, 22)).unwrap();
        let mut hits = Vec::new();
        term.draw(|f| {
            hits = Sidebar {
                panes,
                focus,
                shown,
                order,
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
        a.title = Some("✳ Fix token refresh".into());
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
        let e = pane("p5", "docs", "website", AgentState::Idle);
        vec![a, b, c, d, e]
    }

    #[test]
    fn orders() {
        let panes = sample();
        let ids = |order| -> Vec<&str> {
            display_order(&panes, order)
                .iter()
                .map(|p| p.id.as_str())
                .collect()
        };
        // Needs you (blocked, done), working, quiet (idle, exited).
        assert_eq!(ids(Order::Need), vec!["p1", "p3", "p2", "p5", "p4"]);
        // By project, in creation order.
        assert_eq!(ids(Order::Project), vec!["p1", "p3", "p2", "p4", "p5"]);
        // Numbers follow creation, not position.
        assert_eq!(number(&panes, "p4"), Some(4));
    }

    #[test]
    fn sidebar_snapshot() {
        let (text, hits) = render(&sample(), Some("p1"), &["p1".into(), "p2".into()], 32);
        insta::assert_snapshot!(text);
        let clicks: Vec<Click> = hits.iter().map(|h| h.click.clone()).collect();
        let f = |id: &str| Click::Focus(id.into());
        assert_eq!(
            clicks,
            vec![
                // The focused agent's row plus its three detail rows.
                f("p1"),
                f("p1"),
                f("p1"),
                f("p1"),
                f("p3"),
                f("p2"),
                f("p5"),
                f("p4"),
                Click::Menu,
                Click::Exit,
                Click::Order,
                Click::New { worktree: false },
                Click::New { worktree: true },
            ]
        );
        // Menu and exit on the title row, the order switch on the rule
        // under it, the new buttons on the last row.
        assert_eq!((hits[8].area.y, hits[9].area.y, hits[10].area.y), (0, 0, 1));
        assert!(hits[8].area.x + hits[8].area.width < hits[9].area.x);
        let (a, b) = (hits[11].area, hits[12].area);
        assert_eq!((a.y, b.y), (21, 21));
        assert!(a.x + a.width < b.x);
    }

    #[test]
    fn sidebar_by_project_snapshot() {
        let (text, _) = render_in(&sample(), Some("p1"), &[], 32, Order::Project);
        insta::assert_snapshot!(text);
    }

    #[test]
    fn sidebar_empty_snapshot() {
        let (text, hits) = render(&[], None, &[], 32);
        insta::assert_snapshot!(text);
        // Only the buttons are clickable (no order switch with no agents).
        assert!(hits.iter().all(|h| !matches!(h.click, Click::Focus(_))));
        assert_eq!(hits.len(), 4);
    }

    #[test]
    fn sidebar_narrow_snapshot() {
        let (text, _) = render(&sample(), Some("p2"), &[], 20);
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

    #[test]
    fn only_agents_have_task_titles() {
        let mut p = pane("p1", "claude", "w", AgentState::Working);
        p.title = Some("✳ Fix token refresh".into());
        assert_eq!(task_title(&p), Some("Fix token refresh"));
        p.title = Some("me@laptop: ~/src".into());
        assert_eq!(task_title(&p), None);
        p.title = Some("✳ Fix token refresh".into());
        p.agent = None;
        assert_eq!(task_title(&p), None);
    }

    #[test]
    fn titles_lose_their_status_glyph() {
        assert_eq!(clean_title("✳ Fix token refresh"), "Fix token refresh");
        assert_eq!(clean_title("⠂ Working "), "Working");
        assert_eq!(clean_title("plain"), "plain");
    }
}

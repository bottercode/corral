//! The sidebar. By default it's organised for triage: agents that need you
//! (blocked, or finished and unseen) on top, then the ones working, then the
//! quiet ones. It can list them by project instead. The focused agent opens
//! up to show its title and branch. Collapsed, it shrinks to a
//! rail of agent numbers and state marks. Rendering returns what each row
//! does when clicked.

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

/// Width of the collapsed sidebar, its rule included.
pub const RAIL_WIDTH: u16 = 7;

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

/// One mark for an agent's state, for the collapsed rail.
pub fn state_mark(p: &PaneInfo, frame: usize) -> (&'static str, Style) {
    let t = theme::get();
    let style = t.fg(t.state(p.state));
    match p.state {
        AgentState::Working => (SPINNER[frame % SPINNER.len()], style),
        AgentState::Blocked => ("◆", style.add_modifier(Modifier::BOLD)),
        AgentState::Done => ("✓", style),
        AgentState::Idle => ("·", style),
        AgentState::Exited => ("×", t.faint()),
    }
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
    /// The "+ new" button.
    New,
    /// The "≡ menu" button.
    Menu,
    /// The "✕ exit" button.
    Exit,
    /// The "⇅ by need" / "⇅ by project" switch.
    Order,
    /// The "✦ stories" button.
    Stories,
    /// The "✎ notes" button.
    Notes,
    /// The "«" / "»" button that collapses or expands the sidebar.
    Collapse,
    /// The "↓ archive" button on the focused agent.
    Archive(PaneId),
    /// The "↺ archive" button: the archived panes.
    Archived,
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
    /// Draw the narrow rail instead of the full list.
    pub collapsed: bool,
    pub frame: usize,
    /// Time since `panes` arrived, added to each pane's `state_secs`.
    pub age_offset: Duration,
    /// How many panes are archived.
    pub archived: usize,
}

struct Row<'a> {
    line: Line<'a>,
    pane: Option<PaneId>,
    selected: bool,
    /// A button at the row's right end.
    button: Option<(&'static str, Click)>,
}

impl Row<'_> {
    fn plain(line: Line<'static>) -> Row<'static> {
        Row {
            line,
            pane: None,
            selected: false,
            button: None,
        }
    }
}

/// A section's header: its title, a rule, and its count at the right end
/// (` WORKING ──────── 2`).
fn section_header(section: &Section, width: usize) -> Line<'static> {
    let t = theme::get();
    let count = section.members.len().to_string();
    // A space before the title, around the rule, and after the count.
    let title = truncate(&section.title, width.saturating_sub(count.width() + 6));
    let rule = width.saturating_sub(title.width() + count.width() + 4);
    Line::from(vec![
        Span::styled(format!(" {title} "), t.muted().add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat(rule), t.faint()),
        Span::raw(" "),
        Span::styled(count, t.fg(section.color).add_modifier(Modifier::BOLD)),
    ])
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
        if self.collapsed {
            return self.render_rail(inner, buf);
        }
        let width = inner.width as usize;
        if width < 10 || inner.height == 0 {
            return Vec::new();
        }

        let mut rows: Vec<Row> = vec![
            Row::plain(Line::from(Span::styled(
                " corral",
                t.accent().add_modifier(Modifier::BOLD),
            ))),
            Row::plain(Line::default()),
        ];

        if self.panes.is_empty() {
            rows.push(Row::plain(Line::styled(" no agents yet", t.muted())));
            rows.push(Row::plain(Line::styled(" start one below", t.faint())));
        }

        for (i, section) in sections(self.panes, self.order).into_iter().enumerate() {
            if i > 0 {
                rows.push(Row::plain(Line::default()));
            }
            rows.push(Row::plain(section_header(&section, width)));
            for p in section.members {
                self.agent_rows(p, width, &mut rows);
            }
        }

        // Buttons on the last four rows: new and menu, a rule, then two rows
        // of the rest.
        let show_buttons = inner.height >= 8;
        let list_rows = if show_buttons {
            inner.height - 4
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
            // Before the row's own hit, so it wins the click.
            if let Some((label, click)) = row.button {
                let w = label.width() as u16;
                let area = Rect::new(
                    rect.x + rect.width.saturating_sub(w + 1),
                    rect.y,
                    w.min(rect.width),
                    1,
                );
                Line::styled(label, t.muted()).render(area, buf);
                hits.push(Hit { area, click });
            }
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
        if show_buttons {
            let y = inner.y + inner.height - 4;
            hits.extend(render_new_and_menu(
                Rect::new(inner.x, y, inner.width, 1),
                buf,
            ));
            Line::styled(
                format!(" {}", "─".repeat(width.saturating_sub(2))),
                t.faint(),
            )
            .render(Rect::new(inner.x, y + 1, inner.width, 1), buf);
            // No agents, nothing to order.
            let order = (!self.panes.is_empty()).then_some(self.order);
            hits.extend(render_buttons(
                Rect::new(inner.x, y + 2, inner.width, 2),
                order,
                self.archived,
                buf,
            ));
        }
        hits
    }

    /// The collapsed sidebar: menu and expand buttons, then each agent's
    /// number and state mark, grouped like the full list, then the new
    /// buttons.
    fn render_rail(&self, inner: Rect, buf: &mut Buffer) -> Vec<Hit> {
        let t = theme::get();
        if inner.width < 5 || inner.height == 0 {
            return Vec::new();
        }
        let mut hits = Vec::new();
        let mut button = |x: u16, y: u16, label: &str, style: Style, click: Click| {
            let area = Rect::new(inner.x + x, y, 1, 1);
            Line::styled(label.to_string(), style).render(area, buf);
            hits.push(Hit { area, click });
        };
        button(2, inner.y, "≡", t.accent(), Click::Menu);
        button(4, inner.y, "»", t.muted(), Click::Collapse);
        let rule = || {
            Line::styled(
                format!(" {}", "─".repeat(inner.width as usize - 2)),
                t.faint(),
            )
        };
        if inner.height > 1 {
            rule().render(Rect::new(inner.x, inner.y + 1, inner.width, 1), buf);
        }

        let mut rows: Vec<Option<&PaneInfo>> = Vec::new();
        for (i, section) in sections(self.panes, self.order).into_iter().enumerate() {
            if i > 0 {
                rows.push(None);
            }
            rows.extend(section.members.into_iter().map(Some));
        }
        let show_buttons = inner.height >= 7;
        let bottom = inner.y + inner.height - if show_buttons { 3 } else { 0 };
        for (i, p) in rows.into_iter().enumerate() {
            let y = inner.y + 2 + i as u16;
            if y >= bottom {
                break;
            }
            let Some(p) = p else { continue };
            let rect = Rect::new(inner.x, y, inner.width, 1);
            let focused = self.focus == Some(p.id.as_str());
            let (marker, marker_style) = if focused {
                ("▍", t.accent())
            } else if self.shown.contains(&p.id) {
                ("▏", t.muted())
            } else {
                (" ", Style::new())
            };
            let num = match number(self.panes, &p.id) {
                Some(n @ 1..=99) => format!("{n:>2}"),
                _ => " …".into(),
            };
            let num_style = if focused {
                t.bold()
            } else if p.state == AgentState::Exited {
                t.muted()
            } else {
                Style::new().fg(t.text)
            };
            let (mark, mark_style) = state_mark(p, self.frame);
            if focused {
                buf.set_style(rect, t.selected());
            }
            Line::from(vec![
                Span::styled(marker, marker_style),
                Span::styled(num, num_style),
                Span::raw(" "),
                Span::styled(mark, mark_style),
            ])
            .render(rect, buf);
            hits.push(Hit {
                area: rect,
                click: Click::Focus(p.id.clone()),
            });
        }

        if show_buttons {
            let y = inner.y + inner.height - 3;
            rule().render(Rect::new(inner.x, y, inner.width, 1), buf);
            let mut button = |x: u16, dy: u16, label: &str, style: Style, click: Click| {
                let area = Rect::new(inner.x + x, y + dy, 1, 1);
                Line::styled(label.to_string(), style).render(area, buf);
                hits.push(Hit { area, click });
            };
            let new = t.accent().add_modifier(Modifier::BOLD);
            button(2, 1, "+", new, Click::New);
            button(4, 1, "✎", t.muted(), Click::Notes);
            button(2, 2, "✦", t.fg(t.done), Click::Stories);
            if self.archived > 0 {
                button(4, 2, "↺", t.muted(), Click::Archived);
            }
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
            button: None,
        });
        if !focused {
            return;
        }

        // The focused agent opens up: its title and branch.
        let indent = "   ";
        let room = width.saturating_sub(indent.len() + 1);
        let mut detail = |line: Line<'a>, button: Option<(&'static str, Click)>| {
            rows.push(Row {
                line,
                pane: Some(p.id.clone()),
                selected: true,
                button,
            })
        };
        let marker = || Span::styled("▍", t.accent());
        // The archive button sits at the right end of the last detail row,
        // two columns clear of its text; it drops its word when tight.
        let archive = |used: usize| {
            let label = if used + "↓ archive".width() + 2 <= room {
                "↓ archive"
            } else {
                "↓"
            };
            (label, room.saturating_sub(label.width() + 2))
        };
        let title = task_title(p);
        if let Some(title) = title {
            let (button, room) = match p.git {
                Some(_) => (None, room),
                None => {
                    let (label, room) = archive(title.width());
                    (Some((label, Click::Archive(p.id.clone()))), room)
                }
            };
            detail(
                Line::from(vec![
                    marker(),
                    Span::raw("  "),
                    Span::styled(truncate(title, room), Style::new().fg(t.text)),
                ]),
                button,
            );
        }
        if let Some(git) = &p.git {
            let mut tail = Vec::new();
            if let Some(pr) = &git.pr {
                let (label, style) = match pr.state.as_str() {
                    "MERGED" => (format!(" #{} merged", pr.number), t.fg(t.done)),
                    "CLOSED" => (format!(" #{} closed", pr.number), t.muted()),
                    _ => (format!(" #{}", pr.number), t.accent()),
                };
                tail.push(Span::styled(label, style));
            }
            let tail_width: usize = tail.iter().map(|s| s.content.width()).sum();
            let branch = format!("⎇ {}", git.branch);
            let (label, room) = archive(branch.width() + tail_width);
            let branch = truncate(&branch, room.saturating_sub(tail_width));
            let mut spans = vec![marker(), Span::raw("  "), Span::styled(branch, t.fg(t.git))];
            spans.extend(tail);
            detail(
                Line::from(spans),
                Some((label, Click::Archive(p.id.clone()))),
            );
        } else if title.is_none() {
            // Nothing else to show: the button gets a row of its own.
            detail(
                Line::from(marker()),
                Some((archive(0).0, Click::Archive(p.id.clone()))),
            );
        }
    }
}

/// `✕ exit  «`, right-aligned on the title row (`✕  «` when that would
/// crowd the wordmark).
fn render_title_buttons(row: Rect, buf: &mut Buffer) -> Vec<Hit> {
    let t = theme::get();
    let exit = if row.width >= 20 { "✕ exit" } else { "✕" };
    let buttons = [
        ("«", t.muted(), Click::Collapse),
        (exit, t.muted(), Click::Exit),
    ];
    // Right to left, one column in from the edge, two apart.
    let mut right = row.x + row.width.saturating_sub(1);
    let mut hits = Vec::new();
    for (label, style, click) in buttons {
        let w = label.width() as u16;
        let area = Rect::new(right.saturating_sub(w), row.y, w, 1);
        Line::styled(label, style).render(area, buf);
        hits.push(Hit { area, click });
        right = area.x.saturating_sub(2);
    }
    // Left to right, like the rest of the sidebar.
    hits.reverse();
    hits
}

/// `+ new` with `≡ menu` at the other end, the row above the rule.
fn render_new_and_menu(row: Rect, buf: &mut Buffer) -> Vec<Hit> {
    let t = theme::get();
    // One column in from each side.
    let room = row.width.saturating_sub(2) as usize;
    let new = if room >= 5 { "+ new" } else { "+" };
    let menu = if new.width() + 2 + "≡ menu".width() <= room {
        "≡ menu"
    } else {
        "≡"
    };
    let new_area = Rect::new(row.x + 1, row.y, new.width() as u16, 1);
    Line::styled(new, t.accent().add_modifier(Modifier::BOLD)).render(new_area, buf);
    let w = menu.width() as u16;
    let menu_area = Rect::new(row.x + 1 + (room as u16).saturating_sub(w), row.y, w, 1);
    Line::styled(menu, t.accent()).render(menu_area, buf);
    vec![
        Hit {
            area: new_area,
            click: Click::New,
        },
        Hit {
            area: menu_area,
            click: Click::Menu,
        },
    ]
}

/// Two rows of buttons in two columns, under the rule: `✎ notes` and the
/// `⇅ by need` switch (when there are agents to order), then `✦ stories`
/// and `↺ archive` (with how many, when any are). A column's labels shorten
/// to their icons when they don't fit.
fn render_buttons(area: Rect, order: Option<Order>, archived: usize, buf: &mut Buffer) -> Vec<Hit> {
    let t = theme::get();
    // One column in from each side, two between the columns.
    let room = area.width.saturating_sub(2) as usize;
    let col = room.saturating_sub(2) / 2;
    let order_label = order.map(|o| format!("⇅ {}", o.label()));
    let archive_label = if archived > 0 {
        format!("↺ archive {archived}")
    } else {
        "↺ archive".to_string()
    };
    let left = [
        Some(("✎ notes".to_string(), "✎", t.muted(), Click::Notes)),
        Some(("✦ stories".to_string(), "✦", t.fg(t.done), Click::Stories)),
    ];
    let right = [
        order_label.map(|l| (l, "⇅", t.muted(), Click::Order)),
        Some((archive_label, "↺", t.muted(), Click::Archived)),
    ];
    let fits = |c: &[Option<(String, &str, Style, Click)>]| {
        c.iter().flatten().all(|(l, ..)| l.width() <= col)
    };
    let (long_left, long_right) = (fits(&left), fits(&right));
    let mut hits = Vec::new();
    for (dy, (l, r)) in left.into_iter().zip(right).enumerate() {
        let y = area.y + dy as u16;
        if dy as u16 >= area.height {
            break;
        }
        for (button, long, x) in [
            (l, long_left, area.x + 1),
            (r, long_right, area.x + 1 + (col + 2) as u16),
        ] {
            let Some((label, short, style, click)) = button else {
                continue;
            };
            let label = if long { label.as_str() } else { short };
            let w = label.width() as u16;
            if x + w > area.x + area.width {
                continue;
            }
            let rect = Rect::new(x, y, w, 1);
            Line::styled(label.to_string(), style).render(rect, buf);
            hits.push(Hit { area: rect, click });
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::GitInfo;
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
            git: None,
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
        render_with(panes, focus, shown, width, order, false)
    }

    fn render_with(
        panes: &[PaneInfo],
        focus: Option<&str>,
        shown: &[PaneId],
        width: u16,
        order: Order,
        collapsed: bool,
    ) -> (String, Vec<Hit>) {
        let mut term = Terminal::new(TestBackend::new(width, 22)).unwrap();
        let mut hits = Vec::new();
        term.draw(|f| {
            hits = Sidebar {
                panes,
                focus,
                shown,
                order,
                collapsed,
                frame: 0,
                age_offset: Duration::ZERO,
                archived: 0,
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
        a.git = Some(GitInfo {
            branch: "fix/token-refresh".into(),
            pr: None,
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
    fn branch_and_pull_request() {
        use crate::protocol::{GitInfo, PullRequest};
        let mut a = pane("p1", "claude", "corral", AgentState::Working);
        a.git = Some(GitInfo {
            branch: "fix/login".into(),
            pr: Some(PullRequest {
                number: 42,
                state: "OPEN".into(),
            }),
        });
        let (text, _) = render(&[a.clone()], Some("p1"), &["p1".into()], 32);
        assert!(text.contains("⎇ fix/login #42"), "{text}");
        a.git.as_mut().unwrap().pr.as_mut().unwrap().state = "MERGED".into();
        let (text, _) = render(&[a.clone()], Some("p1"), &["p1".into()], 32);
        assert!(text.contains("⎇ fix/login #42 merged"), "{text}");
        // No repository, no branch line.
        a.git = None;
        let (text, _) = render(&[a], Some("p1"), &["p1".into()], 32);
        assert!(!text.contains("⎇ fix"), "{text}");
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
                // The focused agent's row plus its two detail rows, the
                // branch row with the archive button.
                f("p1"),
                f("p1"),
                Click::Archive("p1".into()),
                f("p1"),
                f("p3"),
                f("p2"),
                f("p5"),
                f("p4"),
                Click::Exit,
                Click::Collapse,
                Click::New,
                Click::Menu,
                Click::Notes,
                Click::Order,
                Click::Stories,
                Click::Archived,
            ]
        );
        // Exit and collapse on the title row.
        let ys: Vec<u16> = hits[8..10].iter().map(|h| h.area.y).collect();
        assert_eq!(ys, vec![0, 0]);
        assert!(hits[8].area.x + hits[8].area.width < hits[9].area.x);
        // New, and menu at its right end, above the rule.
        let (new, menu) = (hits[10].area, hits[11].area);
        assert_eq!((new.y, menu.y), (18, 18));
        assert_eq!(menu.x + menu.width, 30);
        // Notes and the order switch under the rule, stories and archive
        // under them, in two columns.
        let [notes, order, stories, archive] = [12, 13, 14, 15].map(|i| hits[i].area);
        assert_eq!((notes.y, order.y), (20, 20));
        assert_eq!((stories.y, archive.y), (21, 21));
        assert_eq!((notes.x, stories.x), (new.x, new.x));
        assert_eq!(order.x, archive.x);
        assert!(notes.x + notes.width < order.x);
        // The archive button sits at the right end of the branch row.
        let archive = hits[2].area;
        assert_eq!((archive.y, archive.x + archive.width), (5, 30));
    }

    #[test]
    fn archived_button_shows_with_archived_panes() {
        for (width, collapsed, label) in [(32, false, "↺ archive 2"), (RAIL_WIDTH, true, "↺")] {
            let mut term = Terminal::new(TestBackend::new(width, 22)).unwrap();
            let mut hits = Vec::new();
            term.draw(|f| {
                hits = Sidebar {
                    panes: &sample(),
                    focus: None,
                    shown: &[],
                    order: Order::Need,
                    collapsed,
                    frame: 0,
                    age_offset: Duration::ZERO,
                    archived: 2,
                }
                .render(f.area(), f.buffer_mut());
            })
            .unwrap();
            let hit = hits.iter().find(|h| h.click == Click::Archived).unwrap();
            let buf = term.backend().buffer();
            let text: String = (hit.area.x..hit.area.x + hit.area.width)
                .map(|x| buf[(x, hit.area.y)].symbol())
                .collect();
            assert_eq!(text, label);
        }
        // None archived: the button still shows, without a count.
        let (text, hits) = render(&sample(), None, &[], 32);
        assert!(hits.iter().any(|h| h.click == Click::Archived));
        assert!(text.contains("↺ archive "), "{text}");
        assert!(!text.contains("↺ archive 0"), "{text}");
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
        assert_eq!(hits.len(), 7);
    }

    #[test]
    fn collapsed_sidebar_snapshot() {
        let (text, hits) = render_with(
            &sample(),
            Some("p1"),
            &["p1".into(), "p2".into()],
            RAIL_WIDTH,
            Order::Need,
            true,
        );
        insta::assert_snapshot!(text);
        let clicks: Vec<Click> = hits.iter().map(|h| h.click.clone()).collect();
        let f = |id: &str| Click::Focus(id.into());
        assert_eq!(
            clicks,
            vec![
                Click::Menu,
                Click::Collapse,
                // One row per agent, grouped like the full list.
                f("p1"),
                f("p3"),
                f("p2"),
                f("p5"),
                f("p4"),
                Click::New,
                Click::Notes,
                Click::Stories,
            ]
        );
        // A blank row between groups.
        assert_eq!(hits[3].area.y, 3);
        assert_eq!(hits[4].area.y, 5);
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

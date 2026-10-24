//! The sidebar. On top, the spaces: one per project (workspace), with its
//! branch and a mark when an agent in it needs you. Under them, the agents
//! of the space you're in, grouped for triage by default (those that need
//! you, then the ones working, then the quiet ones) or in the order they
//! started. The focused agent opens up to show its title and branch.
//! Collapsed, it shrinks to a rail of agent numbers and state marks.
//! Rendering returns what each row does when clicked.

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
    /// In the order they started.
    Flat,
}

impl Order {
    /// From the config's `sidebar_order`.
    pub fn from_config(s: &str) -> Order {
        match s {
            // `project` is what this was called before spaces.
            "flat" | "project" => Order::Flat,
            _ => Order::Need,
        }
    }

    pub fn config_value(self) -> &'static str {
        match self {
            Order::Need => "need",
            Order::Flat => "flat",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Order::Need => "grouped",
            Order::Flat => "in order",
        }
    }

    pub fn toggled(self) -> Order {
        match self {
            Order::Need => Order::Flat,
            Order::Flat => Order::Need,
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

/// A project (workspace) and its panes, in the order they started.
pub struct Space<'a> {
    pub name: &'a str,
    pub panes: Vec<&'a PaneInfo>,
}

/// The spaces, in the order their first pane started.
pub fn spaces(panes: &[PaneInfo]) -> Vec<Space<'_>> {
    let mut out: Vec<Space> = Vec::new();
    for p in panes {
        match out.iter_mut().find(|s| s.name == p.workspace) {
            Some(s) => s.panes.push(p),
            None => out.push(Space {
                name: &p.workspace,
                panes: vec![p],
            }),
        }
    }
    out
}

/// Non-empty sections of `panes` in sidebar order.
///
/// Grouped, blocked comes before done and exited after idle within a
/// section; otherwise agents keep their creation order. In order, there's
/// one untitled section.
pub fn sections<'a>(panes: &[&'a PaneInfo], order: Order) -> Vec<Section<'a>> {
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
                let mut members: Vec<&PaneInfo> = panes
                    .iter()
                    .copied()
                    .filter(|p| Need::of(p.state) == need)
                    .collect();
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
        Order::Flat if panes.is_empty() => Vec::new(),
        Order::Flat => vec![Section {
            title: String::new(),
            color: t.muted,
            members: panes.to_vec(),
        }],
    }
}

/// Panes in the order the sidebar lists them, space by space; j/k follow
/// this order.
pub fn display_order(panes: &[PaneInfo], order: Order) -> Vec<&PaneInfo> {
    spaces(panes)
        .into_iter()
        .flat_map(|s| sections(&s.panes, order))
        .flat_map(|s| s.members)
        .collect()
}

/// The pane to bring up for a space: the one that most needs you, else its
/// first.
pub fn space_pick<'a>(panes: &'a [PaneInfo], name: &str) -> Option<&'a PaneInfo> {
    let space = spaces(panes).into_iter().find(|s| s.name == name)?;
    sections(&space.panes, Order::Need)
        .into_iter()
        .flat_map(|s| s.members)
        .next()
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
/// Like [`truncate`], but keeps the end, where a path's folder is.
fn truncate_start(s: &str, width: usize) -> String {
    if s.width() <= width || width == 0 {
        return truncate(s, width);
    }
    let mut tail = Vec::new();
    let mut w = 1;
    for c in s.chars().rev() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > width {
            break;
        }
        tail.push(c);
        w += cw;
    }
    std::iter::once('…').chain(tail.into_iter().rev()).collect()
}

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
    /// A space's row: bring up its agent that most needs you.
    Space(String),
    /// The "✕ exit" button.
    Exit,
    /// The "⇅ grouped" / "⇅ in order" switch on the agents header.
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
    /// Anywhere in a half that scrolls, for the wheel: how far it's
    /// scrolled, and how far it can go.
    Scroll {
        half: Half,
        offset: usize,
        max: usize,
    },
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
    /// How far the spaces and the agents are scrolled; `None` follows the
    /// space you're in and the focused agent.
    pub scroll: [Option<usize>; 2],
}

/// The two halves of the sidebar that scroll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Half {
    Spaces = 0,
    Agents = 1,
}

struct Row<'a> {
    line: Line<'a>,
    /// What clicking anywhere on the row does.
    click: Option<Click>,
    selected: bool,
    /// A button at the row's right end.
    button: Option<(&'static str, Click)>,
}

impl Row<'_> {
    fn plain(line: Line<'static>) -> Row<'static> {
        Row {
            line,
            click: None,
            selected: false,
            button: None,
        }
    }
}

/// A header: its title, a rule, and something at the right end, a count or
/// a switch (` AGENTS ──────── ⇅ grouped`).
fn header(title: &str, right: &str, right_style: Style, width: usize) -> Line<'static> {
    let t = theme::get();
    // A space before the title, around the rule, and after the right end.
    let title = truncate(title, width.saturating_sub(right.width() + 6));
    let rule = width.saturating_sub(title.width() + right.width() + 4);
    Line::from(vec![
        Span::styled(format!(" {title} "), t.muted().add_modifier(Modifier::BOLD)),
        Span::styled("─".repeat(rule), t.faint()),
        Span::raw(" "),
        Span::styled(right.to_string(), right_style),
    ])
}

/// A group's title inside the agents, quieter than a header: ` WORKING 1`.
fn group_title(section: &Section) -> Line<'static> {
    let t = theme::get();
    Line::from(vec![
        Span::styled(format!(" {}  ", section.title), t.muted()),
        Span::styled(
            section.members.len().to_string(),
            t.fg(section.color).add_modifier(Modifier::BOLD),
        ),
    ])
}

/// Draw `rows` from `y`, stopping before `end`; returns the clicks and the
/// row after the last one drawn.
fn draw_rows(
    rows: Vec<Row>,
    inner: Rect,
    mut y: u16,
    end: u16,
    buf: &mut Buffer,
) -> (Vec<Hit>, u16) {
    let t = theme::get();
    let mut hits = Vec::new();
    for row in rows {
        if y >= end {
            break;
        }
        let rect = Rect::new(inner.x, y, inner.width, 1);
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
        if let Some(click) = row.click {
            hits.push(Hit { area: rect, click });
        }
        y += 1;
    }
    (hits, y)
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

        let mut hits = render_title_buttons(Rect::new(inner.x, inner.y, inner.width, 1), buf);
        let rule = |y: u16, buf: &mut Buffer| {
            Line::styled(
                format!(" {}", "─".repeat(width.saturating_sub(2))),
                t.faint(),
            )
            .render(Rect::new(inner.x, y, inner.width, 1), buf);
        };
        // The foot, when there's room: a rule, then notes, stories, archive.
        let bottom = inner.y + inner.height;
        let end = if inner.height >= 10 {
            rule(bottom - 2, buf);
            hits.extend(render_footer(
                Rect::new(inner.x, bottom - 1, inner.width, 1),
                self.archived,
                buf,
            ));
            bottom - 2
        } else {
            bottom
        };
        let top = inner.y + 2;
        let new_and_menu = |y: u16, buf: &mut Buffer| {
            if y < end {
                render_new_and_menu(Rect::new(inner.x, y, inner.width, 1), buf)
            } else {
                Vec::new()
            }
        };

        if self.panes.is_empty() {
            let rows = vec![
                Row::plain(Line::styled(" no agents yet", t.muted())),
                Row::plain(Line::styled(" start one below", t.faint())),
                Row::plain(Line::default()),
            ];
            let (h, y) = draw_rows(rows, inner, top, end, buf);
            hits.extend(h);
            hits.extend(new_and_menu(y, buf));
            return hits;
        }

        let spaces = spaces(self.panes);
        let here = self
            .focus
            .and_then(|f| self.panes.iter().find(|p| p.id == f))
            .map_or(spaces[0].name, |p| p.workspace.as_str());

        // Spaces in the top half, agents in the bottom one; new and menu, and
        // a rule, at the midway. Too short for both, just the agents.
        let mid = if end.saturating_sub(top) >= 10 {
            top + (end - top) / 2
        } else {
            top
        };
        let mut scrolls = Vec::new();
        if mid > top {
            let count = spaces.len().to_string();
            header("SPACES", &count, t.muted(), width)
                .render(Rect::new(inner.x, top, inner.width, 1), buf);
            let mut rows = Vec::new();
            for space in &spaces {
                self.space_rows(space, space.name == here, width, &mut rows);
            }
            let region = Rect::new(inner.x, top + 1, inner.width, mid - top - 1);
            let (h, scroll) = self.draw_half(Half::Spaces, rows, region, area, buf);
            hits.extend(h);
            scrolls.push(scroll);
        }
        hits.extend(new_and_menu(mid, buf));
        if mid + 1 < end {
            rule(mid + 1, buf);
        }

        // The agents of the space you're in, with the switch between grouped
        // and in order at the right end of their header.
        let y = mid + 2;
        let members: Vec<&PaneInfo> = self.panes.iter().filter(|p| p.workspace == here).collect();
        if y < end {
            let label = format!("⇅ {}", self.order.label());
            // `AGENTS`, a short rule and the switch, or just its mark.
            let label = if label.width() + 14 <= width {
                label
            } else {
                "⇅".into()
            };
            header("AGENTS", &label, t.muted(), width)
                .render(Rect::new(inner.x, y, inner.width, 1), buf);
            let w = label.width() as u16;
            hits.push(Hit {
                area: Rect::new(inner.x + inner.width.saturating_sub(w + 1), y, w, 1),
                click: Click::Order,
            });
        }
        let mut rows = Vec::new();
        for (i, section) in sections(&members, self.order).into_iter().enumerate() {
            if !section.title.is_empty() {
                if i > 0 {
                    rows.push(Row::plain(Line::default()));
                }
                rows.push(Row::plain(group_title(&section)));
            }
            for p in section.members {
                self.agent_rows(p, width, &mut rows);
            }
        }
        if y + 1 < end {
            let region = Rect::new(inner.x, y + 1, inner.width, end - y - 1);
            let (h, scroll) = self.draw_half(Half::Agents, rows, region, area, buf);
            hits.extend(h);
            scrolls.push(scroll);
        }
        // Last, so the rows' own clicks win.
        hits.extend(scrolls);
        hits
    }

    /// Draw a half's rows into `region`, scrolled: as far as the wheel took
    /// it, or else so the lit rows (the space you're in, the focused agent)
    /// show. A thumb on the sidebar's edge shows where you are when they
    /// don't all fit. Returns the rows' clicks and the half's wheel area.
    fn draw_half(
        &self,
        half: Half,
        rows: Vec<Row>,
        region: Rect,
        area: Rect,
        buf: &mut Buffer,
    ) -> (Vec<Hit>, Hit) {
        let t = theme::get();
        let height = region.height as usize;
        let max = rows.len().saturating_sub(height);
        let offset = match self.scroll[half as usize] {
            Some(n) => n.min(max),
            None => {
                let lit: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].selected).collect();
                match (lit.first(), lit.last()) {
                    // The lit rows' end in view, and their start if they fit.
                    (Some(&first), Some(&last)) if last >= height => {
                        (last + 1 - height).min(first).min(max)
                    }
                    _ => 0,
                }
            }
        };
        if max > 0 && height > 0 {
            let x = area.x + area.width.saturating_sub(1);
            let len = (height * height / rows.len()).max(1);
            let start = (offset * (height - len)).checked_div(max).unwrap_or(0);
            for i in start..start + len {
                buf[(x, region.y + i as u16)]
                    .set_symbol("┃")
                    .set_style(t.muted());
            }
        }
        let rows: Vec<Row> = rows.into_iter().skip(offset).collect();
        let (hits, _) = draw_rows(rows, region, region.y, region.y + region.height, buf);
        let wheel = Hit {
            area: region,
            click: Click::Scroll { half, offset, max },
        };
        (hits, wheel)
    }

    /// A space's two rows: its name, with a mark when an agent in it needs
    /// you, then its branch (or folder). The space you're in is lit.
    fn space_rows(&self, space: &Space, here: bool, width: usize, rows: &mut Vec<Row<'_>>) {
        let t = theme::get();
        let count = |s: AgentState| space.panes.iter().filter(|p| p.state == s).count();
        let (mark, mark_style) = match (count(AgentState::Blocked), count(AgentState::Done)) {
            (n @ 1.., _) => (
                format!("◆ {n}"),
                t.fg(t.blocked).add_modifier(Modifier::BOLD),
            ),
            (_, n @ 1..) => (format!("✓ {n}"), t.fg(t.done)),
            _ if count(AgentState::Working) > 0 => (
                SPINNER[self.frame % SPINNER.len()].to_string(),
                t.fg(t.working),
            ),
            _ => (String::new(), Style::new()),
        };
        let (marker, marker_style) = if here {
            ("▍", t.accent())
        } else {
            (" ", Style::new())
        };
        let gone = space.panes.iter().all(|p| p.state == AgentState::Exited);
        let name_style = if here {
            t.bold()
        } else if gone {
            t.muted()
        } else {
            Style::new().fg(t.text)
        };
        let name = truncate(space.name, width.saturating_sub(mark.width() + 4).max(1));
        let pad = width.saturating_sub(2 + name.width() + mark.width() + 1);
        let click = Some(Click::Space(space.name.to_string()));
        rows.push(Row {
            line: Line::from(vec![
                Span::styled(marker, marker_style),
                Span::raw(" "),
                Span::styled(name, name_style),
                Span::raw(" ".repeat(pad)),
                Span::styled(mark, mark_style),
            ]),
            click: click.clone(),
            selected: here,
            button: None,
        });
        // The focused agent's branch when it's here, else the first one in
        // a repository, else the first one's folder.
        let focused = space
            .panes
            .iter()
            .copied()
            .find(|p| self.focus == Some(p.id.as_str()));
        let place = focused
            .and_then(|p| p.git.as_ref())
            .or_else(|| space.panes.iter().find_map(|p| p.git.as_ref()))
            .map(|g| truncate(&format!("⎇ {}", g.branch), width.saturating_sub(3)))
            .unwrap_or_else(|| {
                let p = focused.unwrap_or(space.panes[0]);
                truncate_start(&super::ui::home_relative(&p.cwd), width.saturating_sub(3))
            });
        let place_style = if here {
            t.fg(t.git)
        } else if gone {
            t.faint()
        } else {
            t.muted()
        };
        rows.push(Row {
            line: Line::from(vec![
                Span::styled(marker, marker_style),
                Span::raw(" "),
                Span::styled(place, place_style),
            ]),
            click,
            selected: here,
            button: None,
        });
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
        let all: Vec<&PaneInfo> = self.panes.iter().collect();
        for (i, section) in sections(&all, self.order).into_iter().enumerate() {
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
            click: Some(Click::Focus(p.id.clone())),
            selected: focused,
            button: None,
        });
        // Under the name, where it's working: its branch, or its folder
        // when it isn't in a repository.
        let indent = "   ";
        let room = width.saturating_sub(indent.len() + 1);
        let (place, fit): (String, fn(&str, usize) -> String) = match &p.git {
            Some(git) => (format!("⎇ {}", git.branch), truncate),
            None => (super::ui::home_relative(&p.cwd), truncate_start),
        };
        if !focused {
            let style = if p.state == AgentState::Exited {
                t.faint()
            } else {
                t.muted()
            };
            rows.push(Row {
                line: Line::from(vec![
                    Span::styled(marker, marker_style),
                    Span::raw("  "),
                    Span::styled(fit(&place, room), style),
                ]),
                click: Some(Click::Focus(p.id.clone())),
                selected: false,
                button: None,
            });
            return;
        }

        // The focused agent opens up: its task, then where it's working in
        // colour, with the archive button at that row's right end.
        let mut detail = |line: Line<'a>, button: Option<(&'static str, Click)>| {
            rows.push(Row {
                line,
                click: Some(Click::Focus(p.id.clone())),
                selected: true,
                button,
            })
        };
        let marker = || Span::styled("▍", t.accent());
        if let Some(title) = task_title(p) {
            detail(
                Line::from(vec![
                    marker(),
                    Span::raw("  "),
                    Span::styled(truncate(title, room), Style::new().fg(t.text)),
                ]),
                None,
            );
        }
        let mut tail = Vec::new();
        if let Some(pr) = p.git.as_ref().and_then(|g| g.pr.as_ref()) {
            let (label, style) = match pr.state.as_str() {
                "MERGED" => (format!(" #{} merged", pr.number), t.fg(t.done)),
                "CLOSED" => (format!(" #{} closed", pr.number), t.muted()),
                _ => (format!(" #{}", pr.number), t.accent()),
            };
            tail.push(Span::styled(label, style));
        }
        let tail_width: usize = tail.iter().map(|s| s.content.width()).sum();
        // Two columns clear of the text; the button drops its word when tight.
        let used = place.width() + tail_width;
        let label = if used + "↓ archive".width() + 2 <= room {
            "↓ archive"
        } else {
            "↓"
        };
        let room = room.saturating_sub(label.width() + 2);
        let place = fit(&place, room.saturating_sub(tail_width));
        let mut spans = vec![marker(), Span::raw("  "), Span::styled(place, t.fg(t.git))];
        spans.extend(tail);
        detail(
            Line::from(spans),
            Some((label, Click::Archive(p.id.clone()))),
        );
    }
}

/// The title row: `✕ exit` at the left (`✕` when narrow), `«` at the right.
fn render_title_buttons(row: Rect, buf: &mut Buffer) -> Vec<Hit> {
    let t = theme::get();
    let exit = if row.width >= 20 { "✕ exit" } else { "✕" };
    // One column in from each edge.
    let exit_area = Rect::new(row.x + 1, row.y, exit.width() as u16, 1);
    let collapse_area = Rect::new(row.x + row.width.saturating_sub(2), row.y, 1, 1);
    Line::styled(exit, t.muted()).render(exit_area, buf);
    Line::styled("«", t.muted()).render(collapse_area, buf);
    vec![
        Hit {
            area: exit_area,
            click: Click::Exit,
        },
        Hit {
            area: collapse_area,
            click: Click::Collapse,
        },
    ]
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

/// The foot's row: `✎ notes`, `✦ stories` and `↺ archive` (with how many,
/// when any are). Labels shorten until the row fits.
fn render_footer(row: Rect, archived: usize, buf: &mut Buffer) -> Vec<Hit> {
    let t = theme::get();
    // One column in from each side.
    let room = row.width.saturating_sub(2) as usize;
    let count = |long: bool| match (archived, long) {
        (0, true) => "↺ archive".to_string(),
        (0, false) => "↺".to_string(),
        (n, true) => format!("↺ archive {n}"),
        (n, false) => format!("↺ {n}"),
    };
    // Notes and stories give up their words before archive does.
    let levels = [
        ["✎ notes".to_string(), "✦ stories".into(), count(true)],
        ["✎ notes".to_string(), "✦ news".into(), count(true)],
        ["✎".to_string(), "✦ news".into(), count(true)],
        ["✎".to_string(), "✦".into(), count(true)],
        ["✎".to_string(), "✦".into(), count(false)],
    ];
    let fits = |l: &[String; 3]| l.iter().map(|s| s.width()).sum::<usize>() + 4 <= room;
    let labels = levels.iter().find(|l| fits(l)).unwrap_or(&levels[4]);
    let styles = [
        (t.fg(t.working), Click::Notes),
        (t.fg(t.done), Click::Stories),
        (t.muted(), Click::Archived),
    ];
    let mut hits = Vec::new();
    let mut x = row.x + 1;
    for (label, (style, click)) in labels.iter().zip(styles) {
        let w = label.width() as u16;
        if x + w > row.x + row.width {
            break;
        }
        let r = Rect::new(x, row.y, w, 1);
        Line::styled(label.clone(), style).render(r, buf);
        hits.push(Hit { area: r, click });
        x += w + 2;
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
                scroll: [None, None],
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
        // Space by space (corral, then website), in creation order.
        assert_eq!(ids(Order::Flat), vec!["p1", "p3", "p2", "p4", "p5"]);
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
    fn folders_keep_their_end() {
        assert_eq!(truncate_start("~/src/corral", 20), "~/src/corral");
        assert_eq!(truncate_start("~/Developer/corral", 10), "…er/corral");
        let mut a = pane("p1", "claude", "corral", AgentState::Idle);
        a.cwd = "/srv/some/deep/project/folder".into();
        let (text, _) = render(&[a], None, &[], 24);
        assert!(
            text.contains("…") && text.contains("project/folder"),
            "{text}"
        );
        assert!(!text.contains("/srv"), "{text}");
    }

    #[test]
    fn sidebar_snapshot() {
        let (text, hits) = render(&sample(), Some("p1"), &["p1".into(), "p2".into()], 32);
        insta::assert_snapshot!(text);
        let clicks: Vec<Click> = hits.iter().map(|h| h.click.clone()).collect();
        let f = |id: &str| Click::Focus(id.into());
        let space = |name: &str| Click::Space(name.into());
        assert_eq!(
            clicks,
            vec![
                // Exit and collapse on the title row, then the foot.
                Click::Exit,
                Click::Collapse,
                Click::Notes,
                Click::Stories,
                Click::Archived,
                // Each space's two rows.
                space("corral"),
                space("corral"),
                space("website"),
                space("website"),
                Click::New,
                Click::Menu,
                Click::Order,
                // Only the agents of the space you're in: the focused one
                // opens up, with the archive button on its branch row.
                f("p1"),
                f("p1"),
                Click::Archive("p1".into()),
                f("p1"),
                f("p3"),
                f("p3"),
                // Each half, for the wheel; everything fits.
                Click::Scroll {
                    half: Half::Spaces,
                    offset: 0,
                    max: 0,
                },
                Click::Scroll {
                    half: Half::Agents,
                    offset: 0,
                    max: 0,
                },
            ]
        );
        let at = |c: Click| hits.iter().find(|h| h.click == c).unwrap().area;
        // Exit and collapse on the title row.
        assert_eq!((at(Click::Exit).y, at(Click::Collapse).y), (0, 0));
        // The spaces from row 3; corral, where the focus is, first.
        assert_eq!(hits[5].area.y, 3);
        // New with menu at its right end at the midway, between the title
        // row and the foot's rule (rows 2 to 20), then the agents' header
        // with its switch at the right end.
        let (new, menu, order) = (at(Click::New), at(Click::Menu), at(Click::Order));
        assert_eq!((new.y, menu.y, order.y), (11, 11, 13));
        assert_eq!(menu.x + menu.width, 30);
        assert_eq!(order.x + order.width, 30);
        // Notes, stories and archive on the last row.
        let ys: Vec<u16> = [Click::Notes, Click::Stories, Click::Archived]
            .map(|c| at(c).y)
            .to_vec();
        assert_eq!(ys, vec![21, 21, 21]);
        // The archive button sits at the right end of the branch row.
        let archive = at(Click::Archive("p1".into()));
        assert_eq!((archive.y, archive.x + archive.width), (17, 30));
    }

    #[test]
    fn a_space_brings_up_its_most_pressing_agent() {
        let panes = sample();
        // website: codex working, zsh exited, docs idle.
        assert_eq!(space_pick(&panes, "website").unwrap().id, "p2");
        assert_eq!(space_pick(&panes, "corral").unwrap().id, "p1");
        assert!(space_pick(&panes, "nowhere").is_none());
        // Focused in website, its agents list and corral's don't.
        let (text, _) = render(&panes, Some("p5"), &[], 32);
        assert!(text.contains("docs") && text.contains("codex"), "{text}");
        assert!(!text.contains("claude"), "{text}");
        // corral's blocked agent still shows on its space.
        assert!(text.contains("corral                   ◆ 1"), "{text}");
    }

    fn render_scrolled(
        panes: &[PaneInfo],
        focus: &str,
        scroll: [Option<usize>; 2],
    ) -> (String, Vec<Hit>) {
        let mut term = Terminal::new(TestBackend::new(32, 22)).unwrap();
        let mut hits = Vec::new();
        term.draw(|f| {
            hits = Sidebar {
                panes,
                focus: Some(focus),
                shown: &[],
                order: Order::Need,
                collapsed: false,
                frame: 0,
                age_offset: Duration::ZERO,
                archived: 0,
                scroll,
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

    fn wheel(hits: &[Hit], half: Half) -> (usize, usize) {
        hits.iter()
            .find_map(|h| match h.click {
                Click::Scroll {
                    half: h2,
                    offset,
                    max,
                } if h2 == half => Some((offset, max)),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn the_spaces_scroll_and_follow_yours() {
        let panes: Vec<PaneInfo> = (1..=12)
            .map(|i| pane(&format!("p{i}"), "zsh", &format!("ws{i}"), AgentState::Idle))
            .collect();
        // Following the focus: the last space shows, the first doesn't, and
        // a thumb marks the spot.
        let (text, hits) = render_scrolled(&panes, "p12", [None, None]);
        assert!(text.contains("ws12"), "{text}");
        assert!(!text.contains("ws1 "), "{text}");
        assert!(text.contains('┃'), "{text}");
        let (offset, max) = wheel(&hits, Half::Spaces);
        assert!(max > 0 && offset == max, "{offset} {max}");
        // Scrolled to the top by the wheel, the first space shows.
        let (text, hits) = render_scrolled(&panes, "p12", [Some(0), None]);
        assert!(text.contains("ws1 "), "{text}");
        assert!(!text.contains("ws12"), "{text}");
        assert_eq!(wheel(&hits, Half::Spaces).0, 0);
        // The midway stays put either way.
        let new_y = |hits: &[Hit]| hits.iter().find(|h| h.click == Click::New).unwrap().area.y;
        assert_eq!(new_y(&hits), 11);
        // Everything fits in a half: nothing to scroll.
        let (_, hits) = render_scrolled(&sample(), "p1", [None, None]);
        assert_eq!(wheel(&hits, Half::Spaces).1, 0);
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
                    scroll: [None, None],
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
        let (text, _) = render_in(&sample(), Some("p1"), &[], 32, Order::Flat);
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

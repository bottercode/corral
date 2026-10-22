//! Layout, dialogs and drawing for the TUI.

use std::path::Path;

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use tui_term::widget::{Cursor, PseudoTerminal};
use unicode_width::UnicodeWidthStr;

use super::input::Key;
use super::sidebar::{RAIL_WIDTH, Sidebar, badge, task_title, truncate};
use super::{App, MENU, MenuItem, Mode, diff, theme};
use crate::protocol::{AgentState, PaneInfo, WorktreeAction};

/// Below this width the sidebar is hidden…
const MIN_WIDTH_FOR_SIDEBAR: u16 = 60;
/// …and below this one the collapsed rail too.
const MIN_WIDTH_FOR_RAIL: u16 = 40;
/// Narrowest a tile gets before tiles stack instead of sitting side by side.
const MIN_TILE_WIDTH: u16 = 40;

pub struct Areas {
    pub sidebar: Rect,
    pub pane: Rect,
    pub hint: Rect,
}

/// Split the screen; a collapsed sidebar is `RAIL_WIDTH` wide.
pub fn layout(area: Rect, sidebar_width: u16, collapsed: bool) -> Areas {
    let hint_h = u16::from(area.height > 2);
    let body_h = area.height - hint_h;
    let sw = if collapsed {
        if area.width >= MIN_WIDTH_FOR_RAIL {
            RAIL_WIDTH
        } else {
            0
        }
    } else if area.width >= MIN_WIDTH_FOR_SIDEBAR {
        sidebar_width.min(area.width / 2)
    } else {
        0
    };
    Areas {
        sidebar: Rect::new(area.x, area.y, sw, body_h),
        pane: Rect::new(area.x + sw, area.y, area.width - sw, body_h),
        hint: Rect::new(area.x, area.y + body_h, area.width, hint_h),
    }
}

/// Areas for `n` tiles in `area`. One fills it; two sit side by side; three
/// are one tall tile and two stacked; four make a grid. When side by side
/// would be too narrow, tiles stack. Side-by-side tiles leave a one-column
/// gap for a divider.
pub fn tiles(area: Rect, n: usize) -> Vec<Rect> {
    let stack = |area: Rect, n: usize| -> Vec<Rect> {
        let n = n as u16;
        (0..n)
            .map(|i| {
                let y0 = area.y + area.height * i / n;
                let y1 = area.y + area.height * (i + 1) / n;
                Rect::new(area.x, y0, area.width, y1 - y0)
            })
            .collect()
    };
    if n <= 1 {
        return if n == 1 { vec![area] } else { Vec::new() };
    }
    if area.width < MIN_TILE_WIDTH * 2 + 1 {
        return stack(area, n);
    }
    let left_w = (area.width - 1) / 2;
    let left = Rect::new(area.x, area.y, left_w, area.height);
    let right = Rect::new(
        area.x + left_w + 1,
        area.y,
        area.width - left_w - 1,
        area.height,
    );
    match n {
        2 => vec![left, right],
        3 => [vec![left], stack(right, 2)].concat(),
        _ => {
            let (l, r) = (stack(left, 2), stack(right, 2));
            vec![l[0], r[0], l[1], r[1]]
        }
    }
}

/// The terminal part of a tile: all of it when alone, else below a header.
pub fn tile_body(tile: Rect, n: usize) -> Rect {
    if n > 1 && tile.height > 1 {
        Rect::new(tile.x, tile.y + 1, tile.width, tile.height - 1)
    } else {
        tile
    }
}

/// A single-line text field.
#[derive(Debug, Clone, Default)]
pub struct TextInput {
    pub text: String,
    /// Cursor position in chars.
    cursor: usize,
}

impl TextInput {
    pub fn new(s: &str) -> Self {
        TextInput {
            text: s.to_string(),
            cursor: s.chars().count(),
        }
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map_or(self.text.len(), |(b, _)| b)
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars().filter(|c| !c.is_control()) {
            let at = self.byte_at(self.cursor);
            self.text.insert(at, c);
            self.cursor += 1;
        }
    }

    /// Returns true if the key was an editing key.
    pub fn handle(&mut self, key: Key) -> bool {
        let len = self.text.chars().count();
        match key {
            Key::Char(c) => self.insert_str(&c.to_string()),
            Key::Backspace if self.cursor > 0 => {
                let at = self.byte_at(self.cursor - 1);
                self.text.remove(at);
                self.cursor -= 1;
            }
            Key::Delete if self.cursor < len => {
                let at = self.byte_at(self.cursor);
                self.text.remove(at);
            }
            Key::Left => self.cursor = self.cursor.saturating_sub(1),
            Key::Right => self.cursor = (self.cursor + 1).min(len),
            Key::Home | Key::Ctrl('a') => self.cursor = 0,
            Key::End | Key::Ctrl('e') => self.cursor = len,
            Key::Ctrl('u') => {
                let at = self.byte_at(self.cursor);
                self.text.replace_range(..at, "");
                self.cursor = 0;
            }
            Key::Ctrl('k') => {
                let at = self.byte_at(self.cursor);
                self.text.truncate(at);
            }
            Key::Ctrl('w') => {
                let chars: Vec<char> = self.text.chars().collect();
                let mut i = self.cursor;
                while i > 0 && chars[i - 1] == ' ' {
                    i -= 1;
                }
                while i > 0 && chars[i - 1] != ' ' {
                    i -= 1;
                }
                let (from, to) = (self.byte_at(i), self.byte_at(self.cursor));
                self.text.replace_range(from..to, "");
                self.cursor = i;
            }
            Key::Backspace | Key::Delete => {}
            _ => return false,
        }
        true
    }

    /// Spans for the field, with the cursor drawn when `active`, plus the
    /// cursor's column offset.
    fn spans(&self, active: bool, placeholder: &str, width: usize) -> (Vec<Span<'static>>, u16) {
        if self.text.is_empty() && !active {
            return (
                vec![Span::styled(
                    truncate(placeholder, width),
                    theme::get().muted(),
                )],
                0,
            );
        }
        let chars: Vec<char> = self.text.chars().collect();
        if !active {
            // Show the end of long values (paths) behind an ellipsis.
            let mut start = 0;
            while start < chars.len() && chars[start..].iter().collect::<String>().width() > width {
                start += 1;
            }
            let tail: String = chars[start..].iter().collect();
            let text = if start > 0 {
                let t: String = tail.chars().skip(1).collect();
                format!("…{t}")
            } else {
                tail
            };
            return (vec![Span::raw(text)], 0);
        }
        // Scroll horizontally so the cursor stays visible.
        let mut start = 0;
        while start < self.cursor
            && chars[start..self.cursor].iter().collect::<String>().width() + 1 > width
        {
            start += 1;
        }
        let visible: String = chars[start..].iter().collect();
        let visible = truncate(&visible, width);
        let cur = chars[start..self.cursor].iter().collect::<String>().width() as u16;
        let style = if active {
            Style::new().add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        let mut spans = vec![Span::styled(visible, style)];
        if self.text.is_empty() {
            spans = vec![Span::styled(
                truncate(placeholder, width),
                theme::get().muted(),
            )];
        }
        (spans, cur)
    }
}

/// New worktree: the repo directory and an optional branch name.
#[derive(Debug, Clone)]
pub struct WorktreeDialog {
    pub cwd: TextInput,
    pub branch: TextInput,
    pub field: usize,
}

impl WorktreeDialog {
    pub fn new(cwd: &Path) -> Self {
        WorktreeDialog {
            cwd: TextInput::new(&home_relative(cwd)),
            branch: TextInput::default(),
            // Usually the directory is right; start on the branch.
            field: 1,
        }
    }

    /// Returns Some(true) to submit, Some(false) to cancel.
    pub fn handle(&mut self, key: Key) -> Option<bool> {
        match key {
            Key::Esc | Key::Ctrl('c') => return Some(false),
            Key::Enter => return Some(true),
            Key::Tab | Key::BackTab | Key::Down | Key::Up => self.field = 1 - self.field,
            k => {
                match self.field {
                    0 => self.cwd.handle(k),
                    _ => self.branch.handle(k),
                };
            }
        }
        None
    }

    pub fn paste(&mut self, text: &str) {
        match self.field {
            0 => self.cwd.insert_str(text),
            _ => self.branch.insert_str(text),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseStep {
    /// Plain pane: confirm ending the process.
    Confirm,
    /// Pane with a worktree: keep / remove / remove + delete branch.
    Worktree,
    /// Removal was refused; offer to force it.
    Force {
        action: WorktreeAction,
        message: String,
    },
}

pub fn home_relative(p: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = p.strip_prefix(&home)
    {
        if rest.as_os_str().is_empty() {
            return "~".into();
        }
        return format!("~/{}", rest.display());
    }
    p.display().to_string()
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2)).max(1);
    let h = height.min(area.height.saturating_sub(2)).max(1);
    Rect::new(
        area.x + (area.width.saturating_sub(w)) / 2,
        area.y + (area.height.saturating_sub(h)) / 3,
        w,
        h,
    )
}

fn dialog_block(title: &str) -> Block<'static> {
    let t = theme::get();
    Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(t.accent())
        .title(Span::styled(format!(" {title} "), t.bold()))
}

fn key_hint(keys: &[(&str, &str)]) -> Line<'static> {
    let t = theme::get();
    let mut spans = Vec::new();
    for (i, (k, what)) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", t.muted()));
        }
        spans.push(Span::styled(k.to_string(), t.key()));
        spans.push(Span::styled(format!(" {what}"), t.muted()));
    }
    Line::from(spans)
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let areas = layout(f.area(), app.sidebar_width, app.sidebar_collapsed);
    app.sidebar_area = areas.sidebar;
    app.pane_area = areas.pane;

    if areas.sidebar.width > 0 {
        let shown = app.tiles.clone();
        app.hits = Sidebar {
            panes: &app.panes,
            focus: app.focus.as_deref(),
            shown: &shown,
            order: app.order,
            collapsed: app.sidebar_collapsed,
            frame: app.frame,
            age_offset: app.state_at.elapsed(),
        }
        .render(areas.sidebar, f.buffer_mut());
    } else {
        app.hits.clear();
    }

    draw_hint(f, app, areas.hint);
    if let Mode::Diff(v) = &mut app.mode {
        // The diff takes the whole screen above the hint line.
        let body = Rect::new(
            areas.sidebar.x,
            areas.sidebar.y,
            areas.sidebar.width + areas.pane.width,
            areas.pane.height,
        );
        diff::draw(f, v, body);
        return;
    }
    if matches!(app.mode, Mode::Stories) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        f.render_widget(ratatui::widgets::Clear, areas.pane);
        app.stories.render(areas.pane, f.buffer_mut(), now);
    } else {
        draw_tiles(f, app, areas.pane);
    }

    match &app.mode {
        Mode::Normal | Mode::Scroll { .. } | Mode::Diff(_) | Mode::Stories => {}
        Mode::Quit => draw_quit(f, app.running_agents(), areas.pane),
        Mode::Restore => draw_restore(f, app, areas.pane),
        Mode::Help => draw_help(f, app, areas.pane),
        Mode::Worktree(d) => {
            let d = d.clone();
            draw_worktree(f, &d, areas.pane);
        }
        Mode::Menu { selected } => {
            let selected = *selected;
            draw_menu(f, app, selected);
        }
        Mode::Hooks(agent) => draw_hooks(f, *agent, areas.pane),
        Mode::Rename { pane, input } => {
            let (pane, input) = (pane.clone(), input.clone());
            draw_rename(f, &pane, &input, areas.pane);
        }
        Mode::Close { pane, step } => {
            let info = app.panes.iter().find(|p| &p.id == pane).cloned();
            if let Some(info) = info {
                let step = step.clone();
                draw_close(f, &info, &step, areas.pane);
            }
        }
    }
}

fn draw_empty(f: &mut Frame, app: &App, area: Rect) {
    let t = theme::get();
    let p = &app.prefix_label;
    let text = if app.panes.is_empty() {
        vec![
            Line::styled("No agents yet.", t.bold()),
            Line::default(),
            Line::from(vec![
                Span::raw("Press "),
                Span::styled(format!("{p} n"), t.key()),
                Span::raw(" for a shell to start one in, "),
                Span::styled(format!("{p} w"), t.key()),
                Span::raw(" for one in its own git worktree (or click the buttons),"),
            ]),
            Line::from(vec![
                Span::raw("or run "),
                Span::styled("corral new -- claude", t.accent()),
                Span::raw(" from any shell."),
            ]),
        ]
    } else {
        vec![Line::styled("…", t.muted())]
    };
    let h = text.len() as u16;
    let r = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(h) / 2,
        area.width,
        h.min(area.height),
    );
    f.render_widget(Paragraph::new(text).alignment(Alignment::Center), r);
}

fn draw_tiles(f: &mut Frame, app: &mut App, area: Rect) {
    let t = theme::get();
    let rects = app.tile_rects();
    if rects.is_empty() {
        if matches!(app.mode, Mode::Normal) {
            draw_empty(f, app, area);
        }
        return;
    }
    // Dividers in the gaps between side-by-side tiles.
    let buf = f.buffer_mut();
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if !rects.iter().any(|(_, r, _)| r.contains((x, y).into())) {
                buf[(x, y)].set_symbol("│").set_style(t.faint());
            }
        }
    }
    let n = rects.len();
    let scroll = match app.mode {
        Mode::Scroll { offset } => offset,
        _ => 0,
    };
    for (id, tile, body) in rects {
        let focused = app.focus.as_ref() == Some(&id);
        let info = app.panes.iter().find(|p| p.id == id).cloned();
        if n > 1
            && let Some(p) = &info
        {
            draw_tile_header(f, app, p, Rect::new(tile.x, tile.y, tile.width, 1), focused);
        }
        let Some(parser) = app.screens.get_mut(&id) else {
            let r = Rect::new(body.x, body.y + body.height / 2, body.width, 1);
            f.render_widget(
                Paragraph::new(Line::styled("…", t.muted())).alignment(Alignment::Center),
                r,
            );
            continue;
        };
        let scroll = if focused { scroll } else { 0 };
        parser.screen_mut().set_scrollback(scroll);
        let screen = parser.screen();
        f.render_widget(
            PseudoTerminal::new(screen).cursor(Cursor::default().visibility(false)),
            body,
        );
        let (row, col) = screen.cursor_position();
        if focused
            && scroll == 0
            && matches!(app.mode, Mode::Normal)
            && !screen.hide_cursor()
            && row < body.height
            && col < body.width
        {
            f.set_cursor_position((body.x + col, body.y + row));
        }

        if let Some(p) = info
            && p.state == AgentState::Exited
        {
            let msg = match p.exit_code {
                Some(c) => format!(" process exited ({c}) · {} x to close ", app.prefix_label),
                None => format!(" process exited · {} x to close ", app.prefix_label),
            };
            let w = (msg.width() as u16).min(body.width);
            let r = Rect::new(
                body.x + body.width.saturating_sub(w),
                body.y + body.height.saturating_sub(1),
                w,
                1,
            );
            f.render_widget(Clear, r);
            f.render_widget(
                Paragraph::new(msg).style(Style::new().fg(t.on_accent).bg(t.muted)),
                r,
            );
        }
    }
}

/// One row above each tile: number, name and title on the left, state on
/// the right. The focused tile's header carries the accent.
fn draw_tile_header(f: &mut Frame, app: &App, p: &PaneInfo, row: Rect, focused: bool) {
    let t = theme::get();
    let (badge, badge_style) = badge(p, app.age(p), app.frame);
    let num = super::sidebar::number(&app.panes, &p.id)
        .filter(|n| *n <= 9)
        .map_or(String::new(), |n| format!("{n} "));
    let (mark, name_style, base) = if focused {
        ("▍", t.bold(), t.selected())
    } else {
        (" ", t.muted(), Style::new())
    };
    let mut left = vec![
        Span::styled(mark, t.accent()),
        Span::styled(num, t.muted()),
        Span::styled(p.name.clone(), name_style),
    ];
    let used: usize = left.iter().map(|s| s.content.width()).sum();
    let room = (row.width as usize).saturating_sub(used + badge.width() + 4);
    if let Some(title) = task_title(p)
        && room > 6
    {
        left.push(Span::styled(
            format!(" · {}", truncate(title, room)),
            t.muted(),
        ));
    }
    let used: usize = left.iter().map(|s| s.content.width()).sum();
    let pad = (row.width as usize).saturating_sub(used + badge.width() + 1);
    left.push(Span::raw(" ".repeat(pad)));
    left.push(Span::styled(badge, badge_style));
    f.buffer_mut().set_style(row, base);
    f.render_widget(Line::from(left), row);
}

fn draw_hint(f: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let t = theme::get();
    let p = &app.prefix_label;
    let line = if let Some((msg, error)) = app.status() {
        let style = if error { t.fg(t.error) } else { t.accent() };
        Line::styled(format!(" {msg}"), style)
    } else if matches!(app.mode, Mode::Diff(_)) {
        // The diff view lists its own keys.
        Line::default()
    } else if matches!(app.mode, Mode::Stories) {
        let mut l = key_hint(&[
            ("↑↓", "pick"),
            ("Enter", "open"),
            ("c", "comments"),
            ("Tab", "source"),
            ("r", "refresh"),
            ("Esc", "back to agents"),
        ]);
        l.spans
            .insert(0, Span::styled(" top stories ", t.on_accent_style()));
        l.spans.insert(1, Span::raw(" "));
        l
    } else if app.awaiting_command {
        let mut l = key_hint(&[
            ("n", "new"),
            ("w", "worktree"),
            ("m", "menu"),
            ("t", "stories"),
            ("j/k", "switch"),
            ("b", "blocked"),
            ("s", "split"),
            ("o", "only"),
            ("D", "diff"),
            ("x", "close"),
            ("r", "rename"),
            ("q", "quit"),
            ("?", "help"),
        ]);
        l.spans
            .insert(0, Span::styled(format!(" {p} "), t.on_accent_style()));
        l.spans.insert(1, Span::raw(" "));
        l
    } else if let Mode::Scroll { offset } = &app.mode {
        let mut spans = vec![
            Span::styled(
                format!(" scrollback {offset}/{} ", app.scroll_max),
                t.on_accent_style(),
            ),
            Span::raw(" "),
        ];
        spans.extend(
            key_hint(&[
                ("k/j", "line"),
                ("PgUp/PgDn", "page"),
                ("g/G", "top/bottom"),
                ("q", "exit"),
            ])
            .spans,
        );
        Line::from(spans)
    } else {
        // Quiet by default: one pointer to the help, on the right.
        let hint = Line::from(vec![
            Span::styled(p.clone(), t.key()),
            Span::styled(" ? keys ", t.muted()),
        ]);
        f.render_widget(Paragraph::new(hint).alignment(Alignment::Right), area);
        return;
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_help(f: &mut Frame, app: &App, area: Rect) {
    let t = theme::get();
    let p = &app.prefix_label;
    let sections: Vec<(&str, Vec<(&str, &str)>)> = vec![
        (
            "agents",
            vec![
                ("n", "new shell (then run claude, codex, …)"),
                ("w", "new shell in its own git worktree"),
                ("j / k, ↑ / ↓", "next / previous agent"),
                ("1 … 9", "jump to agent by number"),
                ("b", "jump to the next blocked agent"),
                (
                    "D",
                    "review the agent's worktree diff (commit, merge, discard)",
                ),
                ("x", "close the agent"),
                ("r", "rename the agent"),
            ],
        ),
        (
            "screen",
            vec![
                ("s", "show another agent alongside (up to 4)"),
                ("S", "take this one off screen (it keeps running)"),
                ("o", "only this one; again to bring the others back"),
                ("c", "collapse the sidebar to agent numbers, or expand it"),
                ("h / l, Tab", "move between agents on screen"),
                ("[", "scrollback (mouse wheel works too)"),
            ],
        ),
        (
            "corral",
            vec![
                (
                    "m",
                    "menu: theme, sidebar order, notifications, agent hooks",
                ),
                ("t", "top stories from Hacker News and lobste.rs"),
                ("d", "detach (agents keep running)"),
                ("q", "exit: detach, or stop corral and every agent"),
                ("?", "this help"),
            ],
        ),
    ];
    let mut text = vec![Line::from(vec![
        Span::raw("Press "),
        Span::styled(p.clone(), t.key()),
        Span::raw(", then:"),
    ])];
    for (title, rows) in sections {
        text.push(Line::default());
        text.push(Line::styled(
            title.to_uppercase(),
            t.muted().add_modifier(Modifier::BOLD),
        ));
        for (k, what) in rows {
            text.push(Line::from(vec![
                Span::styled(format!("  {k:<14}"), t.key()),
                Span::raw(what),
            ]));
        }
    }
    text.push(Line::default());
    text.push(Line::styled(
        format!("{p} {p} sends {p} to the agent. Click an agent in the sidebar or on screen to focus it."),
        t.muted(),
    ));
    text.push(Line::styled(
        "Hold Shift (Option in some terminals) to select text with the mouse.",
        t.muted(),
    ));
    text.push(Line::default());
    text.push(key_hint(&[("any key", "close")]));
    let width = text.iter().map(Line::width).max().unwrap_or(0) as u16 + 2;
    let r = centered(area, width, text.len() as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(text).block(dialog_block("corral keys")), r);
}

fn field_line(
    label: &str,
    input: &TextInput,
    active: bool,
    placeholder: &str,
    width: u16,
) -> (Line<'static>, Option<u16>) {
    let label_w = 10;
    let room = (width as usize).saturating_sub(label_w + 1);
    let (spans, cur) = input.spans(active, placeholder, room);
    let marker = if active { "›" } else { " " };
    let mut all = vec![
        Span::styled(marker.to_string(), theme::get().accent()),
        Span::styled(
            format!("{label:<w$}", w = label_w - 1),
            theme::get().muted(),
        ),
    ];
    all.extend(spans);
    (Line::from(all), active.then_some(label_w as u16 + cur))
}

fn draw_worktree(f: &mut Frame, d: &WorktreeDialog, area: Rect) {
    let r = centered(area, 64, 7);
    let inner_w = r.width.saturating_sub(2);
    let (cwd, c0) = field_line("Repo", &d.cwd, d.field == 0, "~", inner_w);
    let (branch, c1) = field_line(
        "Branch",
        &d.branch,
        d.field == 1,
        "new corral/agent-xxxx, or an existing branch",
        inner_w,
    );
    let text = vec![
        cwd,
        branch,
        Line::default(),
        Line::styled(
            "Opens a shell in a fresh checkout on that branch.",
            theme::get().muted(),
        ),
        key_hint(&[("Tab", "next"), ("Enter", "create"), ("Esc", "cancel")]),
    ];
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(text).block(dialog_block("New worktree")), r);
    let cursor = [(0u16, c0), (1, c1)]
        .into_iter()
        .find_map(|(row, c)| c.map(|c| (row, c)));
    if let Some((row, col)) = cursor {
        f.set_cursor_position((r.x + 1 + col, r.y + 1 + row));
    }
}

/// The menu drops down from its button at the top of the sidebar.
fn draw_menu(f: &mut Frame, app: &mut App, selected: usize) {
    let t = theme::get();
    let label = |item: MenuItem| match item {
        MenuItem::Keys => "keys".to_string(),
        MenuItem::Stories => "top stories".into(),
        MenuItem::Theme => "theme".into(),
        MenuItem::Order => "sidebar".into(),
        MenuItem::Notifications => "notifications".into(),
        MenuItem::Hooks(a) => format!("{} hooks", a.label()),
        MenuItem::Detach => "detach".into(),
        MenuItem::Exit => "exit corral…".into(),
    };
    let rows: Vec<(String, String)> = MENU
        .iter()
        .map(|&item| (label(item), app.menu_value(item)))
        .collect();
    let hint = key_hint(&[("↑↓", "pick"), ("←→", "change"), ("Esc", "close")]);
    let left_w = rows.iter().map(|(l, _)| l.width()).max().unwrap_or(0);
    let right_w = rows.iter().map(|(_, v)| v.width()).max().unwrap_or(0);
    let width = (left_w + right_w + 6).max(hint.width() + 3) as u16;
    let area = f.area();
    // Under the sidebar's title row, right-aligned with the menu button.
    let x = (app.sidebar_area.x + app.sidebar_area.width)
        .saturating_sub(width + 1)
        .max(area.x);
    let height = rows.len() as u16 + 4;
    let r = Rect::new(
        x,
        area.y + 1,
        width.min(area.width),
        height.min(area.height.saturating_sub(1)),
    );
    f.render_widget(Clear, r);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(t.accent());
    let inner = block.inner(r);
    f.render_widget(block, r);
    app.menu_hits.clear();
    for (i, (l, v)) in rows.iter().enumerate() {
        let y = inner.y + i as u16;
        if y >= inner.y + inner.height {
            break;
        }
        let row = Rect::new(inner.x, y, inner.width, 1);
        let sel = i == selected;
        let pad = (inner.width as usize).saturating_sub(l.width() + v.width() + 2);
        let value_style = match MENU[i] {
            MenuItem::Hooks(_) if v == "installed" => t.fg(t.done),
            MenuItem::Hooks(_) if v == "install…" => t.accent(),
            MenuItem::Keys | MenuItem::Stories | MenuItem::Detach | MenuItem::Exit => t.muted(),
            _ => Style::new().fg(t.text),
        };
        let line = Line::from(vec![
            Span::styled(if sel { "▍" } else { " " }, t.accent()),
            Span::styled(
                l.clone(),
                if sel {
                    t.bold()
                } else {
                    Style::new().fg(t.text)
                },
            ),
            Span::raw(" ".repeat(pad)),
            Span::styled(v.clone(), value_style),
        ]);
        if sel {
            f.buffer_mut().set_style(row, t.selected());
        }
        f.render_widget(line, row);
        app.menu_hits.push((row, i));
    }
    let hint_y = inner.y + inner.height.saturating_sub(1);
    f.render_widget(
        hint,
        Rect::new(inner.x + 1, hint_y, inner.width.saturating_sub(1), 1),
    );
}

fn draw_hooks(f: &mut Frame, agent: crate::integrate::Agent, area: Rect) {
    let t = theme::get();
    let path = home_relative(&agent.config_path());
    let text = vec![
        Line::from(vec![
            Span::raw("Add corral's hooks to "),
            Span::styled(path, t.accent()),
            Span::raw("?"),
        ]),
        Line::styled(
            format!(
                "{} then tells corral exactly when it's working, blocked or done.",
                agent.label()
            ),
            t.muted(),
        ),
        Line::styled(
            "Your other settings are kept, and the original file is backed up.",
            t.muted(),
        ),
        Line::default(),
        key_hint(&[("y", "install"), ("Esc", "cancel")]),
    ];
    let width = text.iter().map(Line::width).max().unwrap_or(0) as u16 + 2;
    let r = centered(area, width, text.len() as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(text).block(dialog_block(&format!("{} hooks", agent.label()))),
        r,
    );
}

/// After corral stopped (a reboot, an update): bring the panes back?
fn draw_restore(f: &mut Frame, app: &App, area: Rect) {
    const SHOWN: usize = 10;
    let t = theme::get();
    let n = app.restorable.len();
    let mut text = vec![
        Line::styled(
            format!(
                " corral stopped with {} open. Bring {} back?",
                match n {
                    1 => "1 pane".to_string(),
                    n => format!("{n} panes"),
                },
                if n == 1 { "it" } else { "them" }
            ),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        Line::default(),
    ];
    let name_w = app
        .restorable
        .iter()
        .map(|r| r.name.width())
        .max()
        .unwrap_or(0)
        .min(16);
    let place_w = app
        .restorable
        .iter()
        .map(|r| r.workspace.width() + if r.worktree { 2 } else { 0 })
        .max()
        .unwrap_or(0)
        .min(20);
    for r in app.restorable.iter().take(SHOWN) {
        let name = truncate(&r.name, name_w);
        let place = truncate(
            &r.workspace,
            place_w.saturating_sub(if r.worktree { 2 } else { 0 }),
        );
        let place_pad = place_w.saturating_sub(place.width() + if r.worktree { 2 } else { 0 });
        let mut spans = vec![
            Span::styled(
                format!(
                    "  {name}{}  ",
                    " ".repeat(name_w.saturating_sub(name.width()))
                ),
                if r.agent.is_some() {
                    t.bold()
                } else {
                    Style::new().fg(t.text)
                },
            ),
            Span::styled(place, t.muted()),
        ];
        if r.worktree {
            spans.push(Span::styled(" ⎇", t.fg(t.git)));
        }
        spans.push(Span::raw(" ".repeat(place_pad + 2)));
        spans.push(Span::styled(r.how.clone(), t.muted()));
        text.push(Line::from(spans));
    }
    if n > SHOWN {
        text.push(Line::styled(format!("  and {} more", n - SHOWN), t.muted()));
    }
    text.push(Line::default());
    text.extend(options(&[
        ("Enter", "bring them back"),
        ("n", "start fresh (worktrees stay on disk)"),
        ("Esc", "decide later (`corral restore`)"),
    ]));
    let width = (text.iter().map(Line::width).max().unwrap_or(0) as u16 + 4)
        .max(44)
        .min(area.width.saturating_sub(2));
    let r = centered(area, width, text.len() as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(text).block(dialog_block("welcome back")), r);
}

fn draw_quit(f: &mut Frame, running: usize, area: Rect) {
    let agents = match running {
        0 => "No agents are running.".to_string(),
        1 => "1 agent is running.".to_string(),
        n => format!("{n} agents are running."),
    };
    let mut text = vec![Line::styled(
        agents,
        Style::new().add_modifier(Modifier::BOLD),
    )];
    text.push(Line::default());
    text.extend(options(&[
        (
            "d",
            "detach: close this window, agents keep running (corral to return)",
        ),
        (
            "s",
            "stop corral: end every agent and the background server",
        ),
        ("Esc", "cancel"),
    ]));
    if running > 0 {
        text.push(Line::default());
        text.push(Line::styled(
            "Stopping ends the agents' processes; worktrees and their changes stay on disk, \
             and next time corral offers to bring the agents back.",
            theme::get().muted(),
        ));
    }
    let width = 78u16.min(area.width.saturating_sub(2));
    let inner = width.saturating_sub(2).max(1) as usize;
    let rows: usize = text.iter().map(|l| l.width().div_ceil(inner).max(1)).sum();
    let r = centered(area, width, rows as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(dialog_block("Exit corral")),
        r,
    );
}

fn draw_rename(f: &mut Frame, pane: &str, input: &TextInput, area: Rect) {
    let r = centered(area, 50, 5);
    let (line, c) = field_line("Name", input, true, "", r.width.saturating_sub(2));
    let text = vec![
        line,
        Line::default(),
        key_hint(&[("Enter", "rename"), ("Esc", "cancel")]),
    ];
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(text).block(dialog_block(&format!("Rename {pane}"))),
        r,
    );
    if let Some(c) = c {
        f.set_cursor_position((r.x + 1 + c, r.y + 1));
    }
}

fn draw_close(f: &mut Frame, p: &PaneInfo, step: &CloseStep, area: Rect) {
    let title = format!("Close {} · {}", p.id, p.name);
    let running = p.state != AgentState::Exited;
    let mut text: Vec<Line> = Vec::new();
    match (step, &p.worktree) {
        (CloseStep::Confirm, _) | (CloseStep::Worktree, None) => {
            text.push(Line::raw(if running {
                "End the process and close this pane?"
            } else {
                "Close this pane?"
            }));
            text.push(Line::default());
            text.extend(options(&[("y", "close it"), ("Esc", "cancel")]));
        }
        (CloseStep::Worktree, Some(w)) => {
            if running {
                text.push(Line::raw("End the process and close this pane."));
            }
            text.push(Line::from(vec![
                Span::raw("Worktree "),
                Span::styled(w.branch.clone(), theme::get().fg(theme::get().git)),
                Span::raw(match w.changed_files {
                    0 => " · clean".to_string(),
                    n => format!(" · {n} changed {}", crate::worktree::plural(n, "file")),
                }),
            ]));
            text.push(Line::styled(home_relative(&w.path), theme::get().muted()));
            text.push(Line::default());
            text.extend(options(&[
                ("k", "close, keep the worktree"),
                ("r", "close and remove the worktree (keeps the branch)"),
                ("d", "close, remove the worktree and delete the branch"),
                ("Esc", "cancel"),
            ]));
        }
        (CloseStep::Force { message, .. }, _) => {
            text.push(Line::styled(
                message.clone(),
                theme::get().fg(theme::get().error),
            ));
            text.push(Line::default());
            text.extend(options(&[
                ("f", "force it (that work is lost)"),
                ("k", "close, keep the worktree"),
                ("Esc", "cancel"),
            ]));
        }
    }
    let width = 72u16.min(area.width.saturating_sub(2));
    let inner = width.saturating_sub(2).max(1) as usize;
    let rows: usize = text.iter().map(|l| l.width().div_ceil(inner).max(1)).sum();
    let r = centered(area, width, rows as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(dialog_block(&title)),
        r,
    );
}

/// One `key  description` line per choice.
fn options(items: &[(&str, &str)]) -> Vec<Line<'static>> {
    items
        .iter()
        .map(|(k, what)| {
            Line::from(vec![
                Span::styled(format!(" {k:<5} "), theme::get().accent()),
                Span::raw(what.to_string()),
            ])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_hides_sidebar_when_narrow() {
        let a = layout(Rect::new(0, 0, 120, 40), 30, false);
        assert_eq!(a.sidebar.width, 30);
        assert_eq!(a.pane, Rect::new(30, 0, 90, 39));
        assert_eq!(a.hint, Rect::new(0, 39, 120, 1));
        let a = layout(Rect::new(0, 0, 50, 20), 30, false);
        assert_eq!(a.sidebar.width, 0);
        assert_eq!(a.pane.width, 50);
    }

    #[test]
    fn collapsed_sidebar_is_a_rail() {
        let a = layout(Rect::new(0, 0, 120, 40), 30, true);
        assert_eq!(a.sidebar.width, RAIL_WIDTH);
        assert_eq!(a.pane, Rect::new(RAIL_WIDTH, 0, 120 - RAIL_WIDTH, 39));
        // The rail fits where the full sidebar wouldn't.
        assert_eq!(
            layout(Rect::new(0, 0, 50, 20), 30, true).sidebar.width,
            RAIL_WIDTH
        );
        assert_eq!(layout(Rect::new(0, 0, 30, 20), 30, true).sidebar.width, 0);
    }

    #[test]
    fn text_input_editing() {
        let mut t = TextInput::new("claude");
        t.handle(Key::Char(' '));
        t.insert_str("--x");
        assert_eq!(t.text, "claude --x");
        t.handle(Key::Ctrl('w'));
        assert_eq!(t.text, "claude ");
        t.handle(Key::Home);
        t.handle(Key::Delete);
        assert_eq!(t.text, "laude ");
        t.handle(Key::End);
        t.handle(Key::Backspace);
        t.handle(Key::Char('é'));
        assert_eq!(t.text, "laudeé");
        t.handle(Key::Left);
        t.handle(Key::Ctrl('k'));
        assert_eq!(t.text, "laude");
        t.handle(Key::Ctrl('u'));
        assert_eq!(t.text, "");
    }

    #[test]
    fn inactive_fields_show_the_end_of_long_values() {
        let t = TextInput::new("/very/long/path/to/project");
        let (spans, _) = t.spans(false, "", 12);
        assert_eq!(spans[0].content, "…/to/project");
        let (spans, _) = t.spans(false, "", 40);
        assert_eq!(spans[0].content, "/very/long/path/to/project");
    }

    #[test]
    fn worktree_dialog_navigation() {
        let mut d = WorktreeDialog::new(Path::new("/tmp"));
        assert_eq!(d.field, 1);
        d.handle(Key::Char('b'));
        assert_eq!(d.branch.text, "b");
        d.handle(Key::Tab);
        assert_eq!(d.field, 0);
        d.handle(Key::Char('/'));
        assert_eq!(d.cwd.text, "/tmp/");
        d.handle(Key::BackTab);
        assert_eq!(d.field, 1);
        assert_eq!(d.handle(Key::Enter), Some(true));
        assert_eq!(d.handle(Key::Esc), Some(false));
    }
}

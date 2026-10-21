//! Layout, dialogs and drawing for the TUI.

use std::path::Path;

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use tui_term::widget::{Cursor, PseudoTerminal};
use unicode_width::UnicodeWidthStr;

use super::input::Key;
use super::sidebar::{Sidebar, truncate};
use super::{App, Mode};
use crate::protocol::{AgentState, PaneInfo, WorktreeAction};

/// Below this width the sidebar is hidden.
const MIN_WIDTH_FOR_SIDEBAR: u16 = 60;

pub struct Areas {
    pub sidebar: Rect,
    pub pane: Rect,
    pub hint: Rect,
}

pub fn layout(area: Rect, sidebar_width: u16) -> Areas {
    let hint_h = u16::from(area.height > 2);
    let body_h = area.height - hint_h;
    let sw = if area.width >= MIN_WIDTH_FOR_SIDEBAR {
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
                    Style::new().fg(Color::DarkGray),
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
                Style::new().fg(Color::DarkGray),
            )];
        }
        (spans, cur)
    }
}

#[derive(Debug, Clone)]
pub struct NewDialog {
    pub command: TextInput,
    pub cwd: TextInput,
    pub worktree: bool,
    pub branch: TextInput,
    pub field: usize,
}

impl NewDialog {
    pub const FIELDS: usize = 4;

    pub fn new(command: &str, cwd: &Path, worktree: bool) -> Self {
        NewDialog {
            command: TextInput::new(command),
            cwd: TextInput::new(&home_relative(cwd)),
            worktree,
            branch: TextInput::default(),
            field: 0,
        }
    }

    fn next_field(&mut self, delta: isize) {
        let mut f = self.field as isize;
        loop {
            f = (f + delta).rem_euclid(Self::FIELDS as isize);
            // The branch field only matters with a worktree.
            if f != 3 || self.worktree {
                break;
            }
        }
        self.field = f as usize;
    }

    /// Returns Some(true) to submit, Some(false) to cancel.
    pub fn handle(&mut self, key: Key) -> Option<bool> {
        match key {
            Key::Esc | Key::Ctrl('c') => return Some(false),
            Key::Enter => return Some(true),
            Key::Tab | Key::Down => self.next_field(1),
            Key::BackTab | Key::Up => self.next_field(-1),
            Key::Char(' ') if self.field == 2 => self.worktree = !self.worktree,
            Key::Char('x' | 'y') if self.field == 2 => self.worktree = true,
            Key::Char('n') if self.field == 2 => self.worktree = false,
            k => {
                match self.field {
                    0 => self.command.handle(k),
                    1 => self.cwd.handle(k),
                    3 => self.branch.handle(k),
                    _ => false,
                };
            }
        }
        None
    }

    pub fn paste(&mut self, text: &str) {
        match self.field {
            0 => self.command.insert_str(text),
            1 => self.cwd.insert_str(text),
            3 => self.branch.insert_str(text),
            _ => {}
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
    Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Cyan))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().add_modifier(Modifier::BOLD),
        ))
}

fn key_hint(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, (k, what)) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", Style::new().fg(Color::DarkGray)));
        }
        spans.push(Span::styled(k.to_string(), Style::new().fg(Color::Cyan)));
        spans.push(Span::styled(
            format!(" {what}"),
            Style::new().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let areas = layout(f.area(), app.sidebar_width);
    app.sidebar_area = areas.sidebar;
    app.pane_area = areas.pane;

    if areas.sidebar.width > 0 {
        app.hits = Sidebar {
            panes: &app.panes,
            focus: app.focus.as_deref(),
            frame: app.frame,
        }
        .render(areas.sidebar, f.buffer_mut());
    } else {
        app.hits.clear();
    }

    draw_pane(f, app, areas.pane);
    draw_hint(f, app, areas.hint);

    match &app.mode {
        Mode::Normal | Mode::Scroll { .. } => {}
        Mode::Help => draw_help(f, app, areas.pane),
        Mode::New(d) => {
            let d = d.clone();
            draw_new(f, &d, areas.pane);
        }
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

fn draw_pane(f: &mut Frame, app: &mut App, area: Rect) {
    let focused = app
        .focus
        .as_ref()
        .and_then(|id| app.panes.iter().find(|p| &p.id == id))
        .cloned();
    let scroll = match app.mode {
        Mode::Scroll { offset } => offset,
        _ => 0,
    };
    let Some(parser) = app.parser.as_mut() else {
        if !matches!(app.mode, Mode::Normal) {
            return;
        }
        let text = if app.panes.is_empty() {
            vec![
                Line::styled("No agents yet.", Style::new().add_modifier(Modifier::BOLD)),
                Line::default(),
                Line::from(vec![
                    Span::raw("Press "),
                    Span::styled(
                        format!("{} n", app.prefix_label),
                        Style::new().fg(Color::Cyan),
                    ),
                    Span::raw(" to start one, "),
                    Span::styled(
                        format!("{} w", app.prefix_label),
                        Style::new().fg(Color::Cyan),
                    ),
                    Span::raw(" for one in its own git worktree,"),
                ]),
                Line::from(vec![
                    Span::raw("or run "),
                    Span::styled("corral new -- claude", Style::new().fg(Color::Cyan)),
                    Span::raw(" from any shell."),
                ]),
            ]
        } else {
            vec![Line::styled("…", Style::new().fg(Color::DarkGray))]
        };
        let h = text.len() as u16;
        let r = Rect::new(
            area.x,
            area.y + area.height.saturating_sub(h) / 2,
            area.width,
            h.min(area.height),
        );
        f.render_widget(Paragraph::new(text).alignment(Alignment::Center), r);
        return;
    };

    parser.screen_mut().set_scrollback(scroll);
    let screen = parser.screen();
    f.render_widget(
        PseudoTerminal::new(screen).cursor(Cursor::default().visibility(false)),
        area,
    );
    let (row, col) = screen.cursor_position();
    if scroll == 0
        && matches!(app.mode, Mode::Normal)
        && !screen.hide_cursor()
        && row < area.height
        && col < area.width
    {
        f.set_cursor_position((area.x + col, area.y + row));
    }

    if let Some(p) = focused
        && p.state == AgentState::Exited
    {
        let msg = match p.exit_code {
            Some(c) => format!(" process exited ({c}) · {} x to close ", app.prefix_label),
            None => format!(" process exited · {} x to close ", app.prefix_label),
        };
        let w = (msg.width() as u16).min(area.width);
        let r = Rect::new(
            area.x + area.width.saturating_sub(w),
            area.y + area.height.saturating_sub(1),
            w,
            1,
        );
        f.render_widget(Clear, r);
        f.render_widget(
            Paragraph::new(msg).style(Style::new().fg(Color::Black).bg(Color::DarkGray)),
            r,
        );
    }
}

fn draw_hint(f: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let p = &app.prefix_label;
    let line = if let Some((msg, error)) = app.status() {
        let style = if error {
            Style::new().fg(Color::Red)
        } else {
            Style::new().fg(Color::Yellow)
        };
        Line::styled(format!(" {msg}"), style)
    } else if app.awaiting_command {
        key_hint(&[
            ("n", "new"),
            ("w", "worktree"),
            ("j/k", "switch"),
            ("b", "blocked"),
            ("x", "close"),
            ("r", "rename"),
            ("d", "detach"),
            ("[", "scroll"),
            ("?", "help"),
        ])
    } else {
        match &app.mode {
            Mode::Scroll { offset } => {
                let mut spans = vec![
                    Span::styled(
                        format!(" scrollback {offset}/{} ", app.scroll_max),
                        Style::new().fg(Color::Black).bg(Color::Yellow),
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
            }
            _ => {
                let mut l = key_hint(&[
                    ("n", "new"),
                    ("w", "worktree"),
                    ("j/k", "switch"),
                    ("b", "next blocked"),
                    ("x", "close"),
                    ("d", "detach"),
                    ("?", "help"),
                ]);
                let lead = vec![
                    Span::raw(" "),
                    Span::styled(
                        p.clone(),
                        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" then  ", Style::new().fg(Color::DarkGray)),
                ];
                l.spans.splice(0..0, lead);
                l
            }
        }
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_help(f: &mut Frame, app: &App, area: Rect) {
    let p = &app.prefix_label;
    let rows: Vec<(&str, &str)> = vec![
        ("n", "start a new agent"),
        ("w", "start a new agent in its own git worktree"),
        ("j / k, ↑ / ↓", "next / previous pane"),
        ("1 … 9", "jump to pane"),
        ("b", "jump to the next blocked agent"),
        ("x", "close the pane"),
        ("r", "rename the pane"),
        ("d", "detach (agents keep running)"),
        ("[", "scrollback (mouse wheel works too)"),
        ("?", "this help"),
    ];
    let mut text = vec![
        Line::from(vec![
            Span::raw("Press "),
            Span::styled(p.clone(), Style::new().fg(Color::Cyan)),
            Span::raw(", then:"),
        ]),
        Line::default(),
    ];
    for (k, what) in rows {
        text.push(Line::from(vec![
            Span::styled(format!("  {k:<14}"), Style::new().fg(Color::Cyan)),
            Span::raw(what),
        ]));
    }
    text.push(Line::default());
    text.push(Line::styled(
        format!("{p} {p} sends {p} to the pane. Click a sidebar row to focus it."),
        Style::new().fg(Color::DarkGray),
    ));
    text.push(Line::styled(
        "Hold Shift (Option in some terminals) to select text with the mouse.",
        Style::new().fg(Color::DarkGray),
    ));
    text.push(Line::default());
    text.push(key_hint(&[("any key", "close")]));
    let r = centered(area, 66, text.len() as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(text).block(dialog_block("corral help")), r);
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
        Span::styled(marker.to_string(), Style::new().fg(Color::Cyan)),
        Span::styled(
            format!("{label:<w$}", w = label_w - 1),
            Style::new().fg(Color::DarkGray),
        ),
    ];
    all.extend(spans);
    (Line::from(all), active.then_some(label_w as u16 + cur))
}

fn draw_new(f: &mut Frame, d: &NewDialog, area: Rect) {
    let r = centered(area, 64, 8);
    let inner_w = r.width.saturating_sub(2);
    let (cmd, c0) = field_line("Command", &d.command, d.field == 0, "your shell", inner_w);
    let (cwd, c1) = field_line("Dir", &d.cwd, d.field == 1, "~", inner_w);
    let check = if d.worktree { "[x]" } else { "[ ]" };
    let wt = Line::from(vec![
        Span::styled(
            if d.field == 2 { "›" } else { " " }.to_string(),
            Style::new().fg(Color::Cyan),
        ),
        Span::styled(
            format!("{:<9}", "Worktree"),
            Style::new().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("{check} own git worktree and branch"),
            if d.field == 2 {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            },
        ),
    ]);
    let (branch, c3) = if d.worktree {
        field_line(
            "Branch",
            &d.branch,
            d.field == 3,
            "corral/<agent>-xxxx",
            inner_w,
        )
    } else {
        (Line::default(), None)
    };
    let text = vec![
        cmd,
        cwd,
        wt,
        branch,
        Line::default(),
        key_hint(&[
            ("Tab", "next"),
            ("Space", "toggle"),
            ("Enter", "start"),
            ("Esc", "cancel"),
        ]),
    ];
    let title = if d.worktree {
        "New agent · worktree"
    } else {
        "New agent"
    };
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(text).block(dialog_block(title)), r);
    let cursor = [(0u16, c0), (1, c1), (3, c3)]
        .into_iter()
        .find_map(|(row, c)| c.map(|c| (row, c)));
    if let Some((row, col)) = cursor {
        f.set_cursor_position((r.x + 1 + col, r.y + 1 + row));
    }
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
                Span::styled(w.branch.clone(), Style::new().fg(Color::Magenta)),
                Span::raw(match w.changed_files {
                    0 => " · clean".to_string(),
                    n => format!(" · {n} changed {}", crate::worktree::plural(n, "file")),
                }),
            ]));
            text.push(Line::styled(
                home_relative(&w.path),
                Style::new().fg(Color::DarkGray),
            ));
            text.push(Line::default());
            text.extend(options(&[
                ("k", "close, keep the worktree"),
                ("r", "close and remove the worktree (keeps the branch)"),
                ("d", "close, remove the worktree and delete the branch"),
                ("Esc", "cancel"),
            ]));
        }
        (CloseStep::Force { message, .. }, _) => {
            text.push(Line::styled(message.clone(), Style::new().fg(Color::Red)));
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
                Span::styled(format!(" {k:<4}"), Style::new().fg(Color::Cyan)),
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
        let a = layout(Rect::new(0, 0, 120, 40), 30);
        assert_eq!(a.sidebar.width, 30);
        assert_eq!(a.pane, Rect::new(30, 0, 90, 39));
        assert_eq!(a.hint, Rect::new(0, 39, 120, 1));
        let a = layout(Rect::new(0, 0, 50, 20), 30);
        assert_eq!(a.sidebar.width, 0);
        assert_eq!(a.pane.width, 50);
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
    fn new_dialog_navigation() {
        let mut d = NewDialog::new("claude", Path::new("/tmp"), false);
        d.handle(Key::Tab);
        assert_eq!(d.field, 1);
        d.handle(Key::Tab);
        assert_eq!(d.field, 2);
        // Branch is skipped without a worktree.
        d.handle(Key::Tab);
        assert_eq!(d.field, 0);
        d.handle(Key::BackTab);
        d.handle(Key::Char(' '));
        assert!(d.worktree);
        d.handle(Key::Tab);
        assert_eq!(d.field, 3);
        d.handle(Key::Char('b'));
        assert_eq!(d.branch.text, "b");
        assert_eq!(d.handle(Key::Enter), Some(true));
        assert_eq!(d.handle(Key::Esc), Some(false));
    }
}

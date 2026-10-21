//! Diff review: what an agent changed in its worktree, with commit, merge
//! and discard, without leaving corral.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use super::grid::truncate;
use super::input::Key;
use super::ui::TextInput;
use crate::protocol::{DiffReport, FileDiff, PaneId};

#[derive(Debug, Clone)]
pub enum Prompt {
    Commit(TextInput),
    Merge,
    Discard,
}

#[derive(Debug, Clone)]
pub struct DiffView {
    pub pane: PaneId,
    pub name: String,
    pub report: Option<DiffReport>,
    pub file: usize,
    pub scroll: usize,
    pub prompt: Option<Prompt>,
    /// A request is in flight.
    pub busy: bool,
    /// Lines in the visible patch area at the last draw (for paging).
    pub page: usize,
}

/// What the user asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Close,
    Refresh,
    Commit(String),
    Merge,
    Discard,
}

impl DiffView {
    pub fn new(pane: PaneId, name: String) -> Self {
        DiffView {
            pane,
            name,
            report: None,
            file: 0,
            scroll: 0,
            prompt: None,
            busy: true,
            page: 20,
        }
    }

    pub fn set_report(&mut self, report: DiffReport) {
        // Keep the same file selected across refreshes when it's still there.
        let current = self.current().map(|f| f.path.clone());
        self.file = current
            .and_then(|p| report.files.iter().position(|f| f.path == p))
            .unwrap_or(0);
        if self.file == 0 {
            self.scroll = 0;
        }
        self.report = Some(report);
        self.busy = false;
    }

    pub fn current(&self) -> Option<&FileDiff> {
        self.report.as_ref()?.files.get(self.file)
    }

    fn patch_len(&self) -> usize {
        self.current().map_or(0, |f| f.patch.lines().count())
    }

    fn scroll_by(&mut self, delta: isize) {
        let max = self.patch_len().saturating_sub(1) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max.max(0)) as usize;
    }

    fn select(&mut self, delta: isize) {
        let n = self.report.as_ref().map_or(0, |r| r.files.len());
        if n == 0 {
            return;
        }
        self.file = (self.file as isize + delta).clamp(0, n as isize - 1) as usize;
        self.scroll = 0;
    }

    pub fn handle(&mut self, key: Key) -> Action {
        if let Some(prompt) = self.prompt.as_mut() {
            match (prompt, key) {
                (_, Key::Esc | Key::Ctrl('c')) => self.prompt = None,
                (Prompt::Commit(input), Key::Enter) => {
                    let msg = input.text.trim().to_string();
                    if !msg.is_empty() {
                        self.prompt = None;
                        self.busy = true;
                        return Action::Commit(msg);
                    }
                }
                (Prompt::Commit(input), k) => {
                    input.handle(k);
                }
                (Prompt::Merge, Key::Char('y' | 'Y') | Key::Enter) => {
                    self.prompt = None;
                    self.busy = true;
                    return Action::Merge;
                }
                (Prompt::Discard, Key::Char('y' | 'Y')) => {
                    self.prompt = None;
                    self.busy = true;
                    return Action::Discard;
                }
                (_, Key::Char('n' | 'N')) => self.prompt = None,
                _ => {}
            }
            return Action::None;
        }
        let page = self.page.max(2) as isize - 1;
        match key {
            Key::Char('q') | Key::Esc | Key::Ctrl('c') => return Action::Close,
            Key::Char('j') | Key::Down | Key::Tab => self.select(1),
            Key::Char('k') | Key::Up | Key::BackTab => self.select(-1),
            Key::Char('J') | Key::Char(' ') | Key::PageDown | Key::Ctrl('d') => {
                self.scroll_by(page)
            }
            Key::Char('K') | Key::PageUp | Key::Ctrl('u') => self.scroll_by(-page),
            Key::Char('g') | Key::Home => self.scroll = 0,
            Key::Char('G') | Key::End => self.scroll_by(isize::MAX / 2),
            Key::Char('r') => {
                self.busy = true;
                return Action::Refresh;
            }
            Key::Char('c') => {
                if self.report.as_ref().is_some_and(|r| r.uncommitted > 0) {
                    let msg = format!("Work from {}", self.name);
                    self.prompt = Some(Prompt::Commit(TextInput::new(&msg)));
                }
            }
            Key::Char('m') => self.prompt = Some(Prompt::Merge),
            Key::Char('x') => {
                if self.report.as_ref().is_some_and(|r| r.uncommitted > 0) {
                    self.prompt = Some(Prompt::Discard);
                }
            }
            _ => {}
        }
        Action::None
    }

    pub fn paste(&mut self, text: &str) {
        if let Some(Prompt::Commit(input)) = self.prompt.as_mut() {
            input.insert_str(text);
        }
    }
}

fn status_style(status: &str) -> (Color, &'static str) {
    match status {
        "A" => (Color::Green, "added"),
        "D" => (Color::Red, "deleted"),
        "?" => (Color::Green, "new, untracked"),
        _ => (Color::Yellow, "modified"),
    }
}

fn patch_line(line: &str) -> Line<'static> {
    let style = if line.starts_with("@@") {
        Style::new().fg(Color::Cyan)
    } else if line.starts_with('+') {
        Style::new().fg(Color::Green)
    } else if line.starts_with('-') {
        Style::new().fg(Color::Red)
    } else if line.starts_with('\\') || line.starts_with('…') {
        Style::new().fg(Color::DarkGray)
    } else {
        Style::new()
    };
    Line::styled(line.replace('\t', "    "), style)
}

fn hint(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
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

pub fn draw(f: &mut Frame, v: &mut DiffView, area: Rect) {
    f.render_widget(Clear, area);
    let Some(report) = v.report.clone() else {
        let msg = if v.busy { "loading diff…" } else { "no diff" };
        f.render_widget(
            Paragraph::new(Line::styled(msg, Style::new().fg(Color::DarkGray)))
                .block(frame_block(&v.name, None)),
            area,
        );
        return;
    };
    let total_add: u32 = report.files.iter().map(|f| f.added).sum();
    let total_rm: u32 = report.files.iter().map(|f| f.removed).sum();
    let block = frame_block(&v.name, Some(&report)).title_bottom(
        Line::from(vec![Span::styled(
            format!(
                " {} {} +{total_add} −{total_rm} · {} {} · {} uncommitted ",
                report.files.len(),
                if report.files.len() == 1 {
                    "file"
                } else {
                    "files"
                },
                report.commits,
                if report.commits == 1 {
                    "commit"
                } else {
                    "commits"
                },
                report.uncommitted
            ),
            Style::new().fg(Color::DarkGray),
        )])
        .right_aligned(),
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 3 || inner.width < 20 {
        return;
    }
    let body = Rect::new(inner.x, inner.y, inner.width, inner.height - 1);
    let keys = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);

    if report.files.is_empty() {
        f.render_widget(
            Paragraph::new(vec![
                Line::default(),
                Line::styled(
                    format!(
                        "  No changes on {} compared with {}.",
                        report.branch, report.base
                    ),
                    Style::new().fg(Color::DarkGray),
                ),
            ]),
            body,
        );
    } else {
        let list_w = (body.width / 3).clamp(20, 44);
        let list = Rect::new(body.x, body.y, list_w, body.height);
        let patch = Rect::new(
            body.x + list_w + 1,
            body.y,
            body.width - list_w - 1,
            body.height,
        );
        let sep = Rect::new(body.x + list_w, body.y, 1, body.height);
        f.render_widget(
            Block::new()
                .borders(Borders::LEFT)
                .border_style(Style::new().fg(Color::DarkGray)),
            sep,
        );

        // File list, scrolled to keep the selection visible.
        let skip = v.file.saturating_sub(list.height as usize - 1);
        let mut lines = Vec::new();
        for (i, file) in report
            .files
            .iter()
            .enumerate()
            .skip(skip)
            .take(list.height as usize)
        {
            let (color, _) = status_style(&file.status);
            let counts = if file.binary {
                "bin".to_string()
            } else {
                format!("+{} −{}", file.added, file.removed)
            };
            let name_w = (list_w as usize).saturating_sub(counts.len() + 4);
            let name = truncate_left(&file.path, name_w);
            let pad = (list_w as usize).saturating_sub(2 + name.chars().count() + 1 + counts.len());
            let sel = i == v.file;
            let row_style = if sel {
                Style::new()
                    .bg(Color::Indexed(237))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            lines.push(
                Line::from(vec![
                    Span::styled(format!("{} ", file.status), Style::new().fg(color)),
                    Span::raw(name),
                    Span::raw(" ".repeat(pad + 1)),
                    Span::styled(counts, Style::new().fg(Color::DarkGray)),
                ])
                .style(row_style),
            );
        }
        f.render_widget(Paragraph::new(lines), list);

        // Selected file's patch.
        if let Some(file) = report.files.get(v.file) {
            let (color, what) = status_style(&file.status);
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(
                        truncate(&file.path, patch.width as usize / 2),
                        Style::new().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  {what}"), Style::new().fg(color)),
                ]),
                Line::default(),
            ];
            if file.binary {
                lines.push(Line::styled(
                    "binary file",
                    Style::new().fg(Color::DarkGray),
                ));
            } else {
                lines.extend(file.patch.lines().skip(v.scroll).map(patch_line));
            }
            v.page = patch.height.saturating_sub(2) as usize;
            f.render_widget(Paragraph::new(lines), patch);
        }
    }

    let k = match &v.prompt {
        None => {
            let mut k = vec![
                ("j/k", "file"),
                ("space/K", "scroll"),
                ("c", "commit"),
                ("m", "merge"),
                ("x", "discard"),
                ("r", "refresh"),
                ("q", "back"),
            ];
            if report.uncommitted == 0 {
                k.retain(|(key, _)| *key != "c" && *key != "x");
            }
            hint(&k)
        }
        Some(_) => hint(&[("Enter/y", "confirm"), ("Esc", "cancel")]),
    };
    f.render_widget(
        Paragraph::new(if v.busy {
            hint(&[("…", "working")])
        } else {
            k
        }),
        keys,
    );

    if let Some(p) = v.prompt.clone() {
        draw_prompt(f, &p, &report, area);
    }
}

/// Keep the end of long paths (the file name) visible.
fn truncate_left(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n <= width || width < 2 {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n - (width - 1)).collect();
    format!("…{tail}")
}

fn frame_block(name: &str, report: Option<&DiffReport>) -> Block<'static> {
    let mut title = vec![
        Span::styled(
            " diff ",
            Style::new()
                .fg(Color::Black)
                .bg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(name.to_string(), Style::new().add_modifier(Modifier::BOLD)),
    ];
    if let Some(r) = report {
        title.push(Span::styled(
            format!("  ⎇ {} → {} ", r.branch, r.base),
            Style::new().fg(Color::Magenta),
        ));
    }
    Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::Magenta))
        .title(Line::from(title))
}

fn draw_prompt(f: &mut Frame, p: &Prompt, report: &DiffReport, area: Rect) {
    let w = 64.min(area.width.saturating_sub(4));
    let (title, mut lines, cursor) = match p {
        Prompt::Commit(input) => {
            let room = w.saturating_sub(4) as usize;
            let text = truncate(&input.text, room);
            (
                "Commit",
                vec![
                    Line::raw(format!(
                        "Commit {} uncommitted {} on {}:",
                        report.uncommitted,
                        if report.uncommitted == 1 {
                            "file"
                        } else {
                            "files"
                        },
                        report.branch
                    )),
                    Line::styled(text.clone(), Style::new().add_modifier(Modifier::BOLD)),
                ],
                Some(unicode_width::UnicodeWidthStr::width(text.as_str()) as u16),
            )
        }
        Prompt::Merge => (
            "Merge",
            vec![
                Line::raw(format!(
                    "Merge {} into {} in your checkout?",
                    report.branch, report.base
                )),
                Line::styled(
                    "Uses a merge commit; refuses (and changes nothing) on conflicts.",
                    Style::new().fg(Color::DarkGray),
                ),
            ],
            None,
        ),
        Prompt::Discard => (
            "Discard",
            vec![
                Line::styled(
                    format!(
                        "Throw away {} uncommitted {} in this worktree?",
                        report.uncommitted,
                        if report.uncommitted == 1 {
                            "change"
                        } else {
                            "changes"
                        }
                    ),
                    Style::new().fg(Color::Red),
                ),
                Line::styled(
                    "This can't be undone. Committed work is kept.",
                    Style::new().fg(Color::DarkGray),
                ),
            ],
            None,
        ),
    };
    lines.push(Line::default());
    lines.push(hint(&[("Enter/y", "confirm"), ("Esc", "cancel")]));
    let h = lines.len() as u16 + 2;
    let r = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + area.height / 3,
        w,
        h,
    );
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(Color::Magenta))
                .title(format!(" {title} ")),
        ),
        r,
    );
    if let Some(c) = cursor {
        f.set_cursor_position((r.x + 1 + c, r.y + 2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> DiffReport {
        DiffReport {
            pane: Some("p1".into()),
            branch: "corral/x".into(),
            base: "main".into(),
            commits: 0,
            uncommitted: 2,
            files: vec![
                FileDiff {
                    path: "a.rs".into(),
                    status: "M".into(),
                    added: 1,
                    removed: 1,
                    binary: false,
                    patch: "@@ -1 +1 @@\n-a\n+b\n".into(),
                },
                FileDiff {
                    path: "b.rs".into(),
                    status: "?".into(),
                    added: 30,
                    removed: 0,
                    binary: false,
                    patch: (0..30).map(|i| format!("+{i}\n")).collect(),
                },
            ],
        }
    }

    #[test]
    fn navigation_and_actions() {
        let mut v = DiffView::new("p1".into(), "claude".into());
        v.set_report(report());
        assert_eq!(v.handle(Key::Char('j')), Action::None);
        assert_eq!(v.current().unwrap().path, "b.rs");
        v.handle(Key::Char(' '));
        assert!(v.scroll > 0);
        v.handle(Key::Char('k'));
        assert_eq!((v.file, v.scroll), (0, 0));

        v.handle(Key::Char('c'));
        assert!(matches!(v.prompt, Some(Prompt::Commit(_))));
        assert_eq!(
            v.handle(Key::Enter),
            Action::Commit("Work from claude".into())
        );

        v.busy = false;
        v.handle(Key::Char('x'));
        assert_eq!(v.handle(Key::Char('n')), Action::None);
        assert!(v.prompt.is_none());
        v.handle(Key::Char('x'));
        assert_eq!(v.handle(Key::Char('y')), Action::Discard);
        v.handle(Key::Char('m'));
        assert_eq!(v.handle(Key::Enter), Action::Merge);
        assert_eq!(v.handle(Key::Char('q')), Action::Close);
    }

    #[test]
    fn refresh_keeps_selected_file() {
        let mut v = DiffView::new("p1".into(), "claude".into());
        v.set_report(report());
        v.handle(Key::Char('j'));
        let mut r = report();
        r.files.insert(
            0,
            FileDiff {
                path: "0.rs".into(),
                status: "A".into(),
                ..Default::default()
            },
        );
        v.set_report(r);
        assert_eq!(v.current().unwrap().path, "b.rs");
    }

    #[test]
    fn nothing_to_commit_disables_commit_and_discard() {
        let mut v = DiffView::new("p1".into(), "c".into());
        let mut r = report();
        r.uncommitted = 0;
        v.set_report(r);
        v.handle(Key::Char('c'));
        v.handle(Key::Char('x'));
        assert!(v.prompt.is_none());
    }

    #[test]
    fn left_truncation() {
        assert_eq!(truncate_left("src/client/diff.rs", 10), "…t/diff.rs");
        assert_eq!(truncate_left("a.rs", 10), "a.rs");
    }
}

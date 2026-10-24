//! Notes, on a page drawn over the panes (`^b w`): the notes down the left,
//! the selected one on the right. Each note is a Markdown file in
//! `~/.corral/notes`, so they can be grepped or opened in an editor too.
//!
//! The editor works on the Markdown source and draws its formatting in
//! place: `#` headings, **bold**, *italic*, `code`, ~~strikethrough~~,
//! lists, `- [ ]` tasks, `>` quotes and `---` rules. Markers stay visible
//! but faint, so what's on screen is exactly what's in the file. Edits are
//! saved a moment after typing stops.
//!
//! `s` (or `^s` while editing, or the page's send button) pastes the note
//! into the agent you were on, so it can act on it.

use std::io::ErrorKind;
use std::ops::Range;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::input::Key;
use super::sidebar::truncate;
use super::stories::rough_age;
use super::theme;

/// Edits are written once typing has paused this long.
const SAVE_AFTER: Duration = Duration::from_secs(1);
/// Widest the page gets; wider panes center it.
const MAX_WIDTH: u16 = 120;
/// Rows per note in the list: the title, then its age and a preview.
const ENTRY_ROWS: u16 = 2;
/// Narrower than this, the list and the note take turns.
const SIDE_BY_SIDE: u16 = 60;
/// Tab indents a list item by this much.
const INDENT: &str = "  ";

#[derive(Debug, Clone)]
pub struct Note {
    /// The file's name without `.md`.
    pub id: String,
    pub lines: Vec<String>,
    pub modified: SystemTime,
    /// Edited since it was last written.
    unsaved: bool,
}

impl Note {
    fn new(id: String, text: &str, modified: SystemTime) -> Self {
        let text = clean(text);
        let text = text.strip_suffix('\n').unwrap_or(&text);
        Note {
            id,
            lines: text.split('\n').map(str::to_string).collect(),
            modified,
            unsaved: false,
        }
    }

    #[cfg(test)]
    pub fn new_for_test(text: &str) -> Self {
        Note::new("test".into(), text, SystemTime::now())
    }

    pub fn text(&self) -> String {
        let mut s = self.lines.join("\n");
        s.push('\n');
        s
    }

    /// Nothing but whitespace and heading marks: not worth keeping.
    pub fn is_blank(&self) -> bool {
        self.lines.iter().all(|l| {
            l.trim_matches(|c: char| c == '#' || c.is_whitespace())
                .is_empty()
        })
    }

    /// The first line with anything on it, without its Markdown.
    pub fn title(&self) -> String {
        self.plain_lines()
            .next()
            .unwrap_or_else(|| "untitled".into())
    }

    /// The line after the title, for the list.
    fn preview(&self) -> String {
        self.plain_lines().nth(1).unwrap_or_default()
    }

    fn plain_lines(&self) -> impl Iterator<Item = String> {
        self.lines
            .iter()
            .map(|l| plain(l))
            .filter(|l| !l.is_empty())
    }

    fn end(&self) -> (usize, usize) {
        let last = self.lines.len().saturating_sub(1);
        (last, self.lines.get(last).map_or(0, |l| l.chars().count()))
    }
}

pub enum Outcome {
    Stay,
    Close,
    /// Paste this note into the agent you were on.
    Send,
}

/// The notes, and where the reader is.
#[derive(Debug)]
pub struct Notes {
    /// Where notes are kept; `None` keeps them in memory (tests).
    dir: Option<PathBuf>,
    /// Most recently edited first.
    pub notes: Vec<Note>,
    pub selected: usize,
    /// The keyboard is in the note rather than the list.
    pub editing: bool,
    /// Line, and column in chars.
    pub cursor: (usize, usize),
    /// The column (in cells) ↑ and ↓ aim for.
    want_x: Option<usize>,
    /// The first row of the note shown, and the first note in the list.
    top: usize,
    list_top: usize,
    /// `x` was pressed: the next key confirms deleting the selected note.
    pub confirm_delete: bool,
    /// The agent a note would be sent to, for the send button; set before
    /// each draw.
    pub send_to: Option<String>,
    last_edit: Option<Instant>,
    pub error: Option<String>,
    // As of the last draw, for moving by rows and for the mouse.
    wrap: usize,
    page_rows: usize,
    new_button: Rect,
    send_button: Rect,
    list_area: Rect,
    text_area: Rect,
    /// The note's rows on screen: line, and its chars on that row.
    drawn: Vec<(usize, Range<usize>)>,
}

impl Notes {
    pub fn new(dir: Option<PathBuf>) -> Self {
        Notes {
            dir,
            notes: Vec::new(),
            selected: 0,
            editing: false,
            cursor: (0, 0),
            want_x: None,
            top: 0,
            list_top: 0,
            confirm_delete: false,
            send_to: None,
            last_edit: None,
            error: None,
            wrap: 80,
            page_rows: 10,
            new_button: Rect::default(),
            send_button: Rect::default(),
            list_area: Rect::default(),
            text_area: Rect::default(),
            drawn: Vec::new(),
        }
    }

    /// The selected note's text to send to an agent: its Markdown, without
    /// the blank lines around it. `None` when there's nothing to send.
    pub fn to_send(&self) -> Option<String> {
        let note = self.current().filter(|n| !n.is_blank())?;
        Some(note.text().trim_matches('\n').to_string())
    }

    pub fn current(&self) -> Option<&Note> {
        self.notes.get(self.selected)
    }

    /// The page opens: write what's pending, then read the folder again
    /// (notes may have changed outside corral). With no notes yet it opens
    /// straight into a new one.
    pub fn open(&mut self) {
        self.save_all();
        self.editing = false;
        self.confirm_delete = false;
        if let Some(dir) = self.dir.clone() {
            let keep = self.current().map(|n| n.id.clone());
            match load(&dir) {
                Ok(notes) => {
                    self.notes = notes;
                    self.selected = keep
                        .and_then(|id| self.notes.iter().position(|n| n.id == id))
                        .unwrap_or(0);
                }
                Err(e) => self.error = Some(format!("{e:#}")),
            }
        } else {
            self.notes.retain(|n| !n.is_blank());
        }
        if self.notes.is_empty() {
            self.new_note();
        } else {
            self.select(self.selected.min(self.notes.len() - 1));
        }
    }

    fn select(&mut self, i: usize) {
        self.selected = i;
        self.cursor = self.current().map_or((0, 0), Note::end);
        self.top = 0;
        self.want_x = None;
        self.confirm_delete = false;
        // Rows drawn for another note mean nothing for this one.
        self.drawn.clear();
    }

    fn select_by(&mut self, step: isize) {
        let last = self.notes.len().saturating_sub(1);
        self.select(self.selected.saturating_add_signed(step).min(last));
    }

    pub fn new_note(&mut self) {
        if self.editing {
            self.leave_editor();
        }
        let id = self.new_id();
        // Starts as a heading: the first line is the title.
        self.notes.insert(0, Note::new(id, "# ", SystemTime::now()));
        self.select(0);
        self.list_top = 0;
        self.editing = true;
    }

    /// Milliseconds since 1970, nudged past any note or file using it.
    fn new_id(&self) -> String {
        let mut ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        loop {
            let id = ms.to_string();
            let taken = self.notes.iter().any(|n| n.id == id)
                || self
                    .dir
                    .as_ref()
                    .is_some_and(|d| d.join(format!("{id}.md")).exists());
            if !taken {
                return id;
            }
            ms += 1;
        }
    }

    /// Back to the list; a note left blank is dropped.
    fn leave_editor(&mut self) {
        self.editing = false;
        if self.current().is_some_and(Note::is_blank) {
            self.delete_selected();
        }
    }

    fn delete_selected(&mut self) {
        if self.selected >= self.notes.len() {
            return;
        }
        let note = self.notes.remove(self.selected);
        if let Some(dir) = &self.dir
            && let Err(e) = remove(&dir.join(format!("{}.md", note.id)))
        {
            self.error = Some(format!("{e:#}"));
        }
        self.select(self.selected.min(self.notes.len().saturating_sub(1)));
    }

    /// Write every note with unsaved edits.
    pub fn save_all(&mut self) {
        let Some(dir) = self.dir.clone() else {
            self.notes.iter_mut().for_each(|n| n.unsaved = false);
            return;
        };
        for n in self.notes.iter_mut().filter(|n| n.unsaved) {
            match write(&dir, n) {
                Ok(()) => {
                    n.unsaved = false;
                    self.error = None;
                }
                Err(e) => {
                    self.error = Some(format!("{e:#}"));
                    // Try again after the next pause rather than every tick.
                    self.last_edit = Some(Instant::now());
                }
            }
        }
    }

    /// Called every tick: save once typing pauses, or right away when the
    /// page isn't open.
    pub fn autosave(&mut self, on_page: bool) {
        let paused = self.last_edit.is_none_or(|t| t.elapsed() >= SAVE_AFTER);
        if (paused || !on_page) && self.notes.iter().any(|n| n.unsaved) {
            self.save_all();
        }
    }

    pub fn key(&mut self, key: Key) -> Outcome {
        if self.confirm_delete {
            self.confirm_delete = false;
            if matches!(key, Key::Char('y' | 'Y') | Key::Enter) {
                self.delete_selected();
            }
            return Outcome::Stay;
        }
        if self.editing {
            if key == Key::Ctrl('s') {
                return Outcome::Send;
            }
            self.edit_key(key);
            return Outcome::Stay;
        }
        let some = !self.notes.is_empty();
        match key {
            Key::Esc | Key::Ctrl('c') | Key::Char('q') => return Outcome::Close,
            Key::Char('s') | Key::Ctrl('s') if some => return Outcome::Send,
            Key::Down | Key::Char('j') => self.select_by(1),
            Key::Up | Key::Char('k') => self.select_by(-1),
            Key::Home | Key::Char('g') => self.select(0),
            Key::End | Key::Char('G') => self.select(self.notes.len().saturating_sub(1)),
            Key::Enter | Key::Right | Key::Tab | Key::Char('e' | 'l' | 'i') if some => {
                self.editing = true
            }
            Key::Char('n') => self.new_note(),
            Key::Char('x' | 'd') | Key::Delete | Key::Backspace if some => {
                self.confirm_delete = true
            }
            _ => {}
        }
        Outcome::Stay
    }

    fn edit_key(&mut self, key: Key) {
        if !matches!(key, Key::Up | Key::Down | Key::PageUp | Key::PageDown) {
            self.want_x = None;
        }
        let page = self.page_rows as isize;
        match key {
            Key::Esc | Key::Ctrl('c') => self.leave_editor(),
            Key::Char(c) => self.insert_char(c),
            Key::Enter => self.newline(true),
            Key::Tab => self.indent(),
            Key::BackTab => self.outdent(),
            Key::Backspace => self.backspace(),
            Key::Delete => self.delete(),
            Key::Left => self.left(),
            Key::Right => self.right(),
            Key::Up => self.vertical(-1),
            Key::Down => self.vertical(1),
            Key::PageUp => self.vertical(-page),
            Key::PageDown => self.vertical(page),
            Key::Home | Key::Ctrl('a') => self.cursor.1 = 0,
            Key::End | Key::Ctrl('e') => self.cursor.1 = self.line_len(),
            Key::Ctrl('t') => self.toggle_task(),
            Key::Ctrl('k') => self.kill_to_end(),
            Key::Ctrl('u') => {
                let at = self.byte_at(self.cursor.1);
                self.line_mut().replace_range(..at, "");
                self.cursor.1 = 0;
            }
            Key::Ctrl('w') => self.delete_word(),
            _ => {}
        }
    }

    /// Pasted text goes in as typed, newlines and all.
    pub fn paste(&mut self, text: &str) {
        if !self.editing || self.notes.is_empty() {
            return;
        }
        for c in clean(text).chars() {
            if c == '\n' {
                self.newline(false);
            } else {
                self.insert_char(c);
            }
        }
    }

    // Editing.

    fn line(&self) -> &str {
        &self.notes[self.selected].lines[self.cursor.0]
    }

    fn line_len(&self) -> usize {
        self.line().chars().count()
    }

    fn byte_at(&self, col: usize) -> usize {
        byte_at(self.line(), col)
    }

    /// The cursor's line, for changing: the note is marked edited and moves
    /// to the top of the list.
    fn line_mut(&mut self) -> &mut String {
        self.lines_mut();
        &mut self.notes[0].lines[self.cursor.0]
    }

    fn lines_mut(&mut self) -> &mut Vec<String> {
        let mut note = self.notes.remove(self.selected);
        note.unsaved = true;
        note.modified = SystemTime::now();
        self.notes.insert(0, note);
        self.selected = 0;
        self.list_top = 0;
        self.last_edit = Some(Instant::now());
        &mut self.notes[0].lines
    }

    fn insert_char(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        let at = self.byte_at(self.cursor.1);
        self.line_mut().insert(at, c);
        self.cursor.1 += 1;
    }

    fn insert_at(&mut self, col: usize, s: &str) {
        let at = self.byte_at(col);
        self.line_mut().insert_str(at, s);
        if self.cursor.1 >= col {
            self.cursor.1 += s.chars().count();
        }
    }

    /// Split the line. With `continue_list`, a list item starts the next
    /// item, and Enter on an empty item ends the list instead.
    fn newline(&mut self, continue_list: bool) {
        let (l, c) = self.cursor;
        let mut prefix = String::new();
        if continue_list && let Some(m) = marker(self.line()) {
            if self.line_len() == m.len {
                self.line_mut().clear();
                self.cursor.1 = 0;
                return;
            }
            if c >= m.len {
                prefix = m.next(self.line());
            }
        }
        let at = self.byte_at(c);
        let tail = self.line_mut().split_off(at);
        let n = prefix.chars().count();
        self.lines_mut().insert(l + 1, prefix + &tail);
        self.cursor = (l + 1, n);
    }

    fn backspace(&mut self) {
        let (l, c) = self.cursor;
        if c == 0 {
            if l > 0 {
                let line = self.lines_mut().remove(l);
                let prev = &mut self.notes[0].lines[l - 1];
                let end = prev.chars().count();
                prev.push_str(&line);
                self.cursor = (l - 1, end);
            }
            return;
        }
        // Right after a list marker: step the item out, or drop the marker.
        if let Some(m) = marker(self.line())
            && c == m.len
        {
            if m.indent >= INDENT.len() {
                self.outdent();
            } else {
                let at = self.byte_at(m.len);
                self.line_mut().replace_range(..at, "");
                self.cursor.1 = 0;
            }
            return;
        }
        let at = self.byte_at(c - 1);
        self.line_mut().remove(at);
        self.cursor.1 -= 1;
    }

    fn delete(&mut self) {
        let (l, c) = self.cursor;
        if c < self.line_len() {
            let at = self.byte_at(c);
            self.line_mut().remove(at);
        } else if l + 1 < self.notes[self.selected].lines.len() {
            let next = self.lines_mut().remove(l + 1);
            self.notes[0].lines[l].push_str(&next);
        }
    }

    fn kill_to_end(&mut self) {
        if self.cursor.1 < self.line_len() {
            let at = self.byte_at(self.cursor.1);
            self.line_mut().truncate(at);
        } else {
            self.delete();
        }
    }

    fn delete_word(&mut self) {
        let chars: Vec<char> = self.line().chars().collect();
        let mut i = self.cursor.1;
        while i > 0 && chars[i - 1] == ' ' {
            i -= 1;
        }
        while i > 0 && chars[i - 1] != ' ' {
            i -= 1;
        }
        if i == self.cursor.1 {
            self.backspace();
            return;
        }
        let (from, to) = (self.byte_at(i), self.byte_at(self.cursor.1));
        self.line_mut().replace_range(from..to, "");
        self.cursor.1 = i;
    }

    /// Tab nests a list item; elsewhere it's spaces.
    fn indent(&mut self) {
        let col = if marker(self.line()).is_some() {
            0
        } else {
            self.cursor.1
        };
        self.insert_at(col, INDENT);
    }

    fn outdent(&mut self) {
        let n = self
            .line()
            .chars()
            .take(INDENT.len())
            .take_while(|&c| c == ' ')
            .count();
        if n > 0 {
            self.line_mut().replace_range(..n, "");
            self.cursor.1 = self.cursor.1.saturating_sub(n);
        }
    }

    /// `^t`: tick or untick a task; a list item or a plain line becomes one.
    fn toggle_task(&mut self) {
        match marker(self.line()) {
            Some(Marker {
                indent,
                bullet,
                task: Some(done),
                ..
            }) => {
                let at = indent + bullet + 1;
                self.line_mut()
                    .replace_range(at..at + 1, if done { " " } else { "x" });
            }
            Some(m) => self.insert_at(m.indent + m.bullet, "[ ] "),
            None => {
                let indent = self.line().chars().take_while(|&c| c == ' ').count();
                self.insert_at(indent, "- [ ] ");
            }
        }
    }

    fn left(&mut self) {
        let (l, c) = self.cursor;
        if c > 0 {
            self.cursor.1 -= 1;
        } else if l > 0 {
            self.cursor = (
                l - 1,
                self.notes[self.selected].lines[l - 1].chars().count(),
            );
        }
    }

    fn right(&mut self) {
        let (l, c) = self.cursor;
        if c < self.line_len() {
            self.cursor.1 += 1;
        } else if l + 1 < self.notes[self.selected].lines.len() {
            self.cursor = (l + 1, 0);
        }
    }

    /// Move by rows as wrapped on screen, keeping to a column.
    fn vertical(&mut self, step: isize) {
        let Some(note) = self.notes.get(self.selected) else {
            return;
        };
        let rows = visual_rows(&note.lines, self.wrap);
        let cur = cursor_row(&rows, self.cursor);
        let (l, r) = &rows[cur];
        let chars: Vec<char> = note.lines[*l].chars().collect();
        let want = *self
            .want_x
            .get_or_insert_with(|| width_of(&chars[r.start..self.cursor.1]));
        let target = cur.saturating_add_signed(step).min(rows.len() - 1);
        if target == cur {
            // Past the first or last row: to the start or end of the line.
            self.cursor.1 = if step < 0 { 0 } else { chars.len() };
            return;
        }
        let (tl, tr) = &rows[target];
        let last = rows.get(target + 1).is_none_or(|(n, _)| n != tl);
        let chars: Vec<char> = note.lines[*tl].chars().collect();
        self.cursor = (*tl, col_at_x(&chars, tr.clone(), want, last));
    }

    // The mouse.

    pub fn click(&mut self, x: u16, y: u16) -> Outcome {
        let p = (x, y).into();
        self.confirm_delete = false;
        if self.send_button.contains(p) {
            return Outcome::Send;
        }
        if self.new_button.contains(p) {
            self.new_note();
        } else if self.list_area.contains(p) {
            let i = self.list_top + usize::from((y - self.list_area.y) / ENTRY_ROWS);
            let Some(id) = self.notes.get(i).map(|n| n.id.clone()) else {
                return Outcome::Stay;
            };
            if self.editing {
                self.leave_editor();
            }
            // Leaving may have dropped a blank note above it.
            if let Some(i) = self.notes.iter().position(|n| n.id == id) {
                self.select(i);
                self.editing = true;
            }
        } else if self.text_area.contains(p) && !self.notes.is_empty() {
            let slot = usize::from(y - self.text_area.y);
            self.cursor = match self.drawn.get(slot) {
                Some((l, r)) => {
                    let lines = &self.notes[self.selected].lines;
                    let last = lines[*l].chars().count() == r.end;
                    let chars: Vec<char> = lines[*l].chars().collect();
                    let x = usize::from(x - self.text_area.x);
                    (*l, col_at_x(&chars, r.clone(), x, last))
                }
                None => self.notes[self.selected].end(),
            };
            self.want_x = None;
            self.editing = true;
        }
        Outcome::Stay
    }

    pub fn wheel(&mut self, x: u16, y: u16, step: isize) {
        let p = (x, y).into();
        if self.list_area.contains(p) && !self.editing {
            self.select_by(step);
        } else if self.text_area.contains(p) {
            if self.editing {
                self.vertical(step * 3);
            } else {
                self.top = self.top.saturating_add_signed(step * 3);
            }
        }
    }

    // Drawing.

    /// Draw the page into `area`; returns where the cursor goes when a note
    /// is being edited.
    pub fn render(&mut self, area: Rect, buf: &mut Buffer) -> Option<(u16, u16)> {
        let t = theme::get();
        self.new_button = Rect::default();
        self.send_button = Rect::default();
        self.list_area = Rect::default();
        self.text_area = Rect::default();
        self.drawn.clear();
        if area.width < 20 || area.height < 5 {
            return None;
        }
        let width = area.width.saturating_sub(4).min(MAX_WIDTH);
        let x = area.x + (area.width - width) / 2;
        let row = |y: u16| Rect::new(x, y, width, 1);

        // Title, the new button, and what's going on.
        Line::styled("NOTES", t.muted().add_modifier(Modifier::BOLD)).render(row(area.y), buf);
        let label = "+ new note";
        let mut w = label.width() as u16;
        if 8 + w < width {
            self.new_button = Rect::new(x + 8, area.y, w, 1);
            Line::styled(label, t.accent().add_modifier(Modifier::BOLD))
                .render(self.new_button, buf);
        }
        // Beside it, sending the selected note to the agent you were on.
        if let Some(to) = self.send_to.as_deref().filter(|_| self.to_send().is_some()) {
            let send = format!("↳ send to {}", truncate(to, 16));
            let sw = send.width() as u16;
            if 8 + w + 3 + sw + 12 < width {
                self.send_button = Rect::new(x + 8 + w + 3, area.y, sw, 1);
                Line::styled(send, t.fg(t.working).add_modifier(Modifier::BOLD))
                    .render(self.send_button, buf);
                w += 3 + sw;
            }
        }
        let (status, style) = if self.confirm_delete {
            let title = self.current().map(Note::title).unwrap_or_default();
            (format!("delete “{title}”?  y to delete"), t.fg(t.blocked))
        } else if let Some(e) = &self.error {
            (e.lines().next().unwrap_or("").to_string(), t.fg(t.error))
        } else {
            let n = self.notes.iter().filter(|n| !n.is_blank()).count();
            let s = if n == 1 { "" } else { "s" };
            (format!("{n} note{s}"), t.faint())
        };
        let room = (width as usize).saturating_sub(8 + w as usize + 4);
        if room > 8 {
            let status = truncate(&status, room);
            let sw = status.width() as u16;
            Line::styled(status, style).render(Rect::new(x + width - sw, area.y, sw, 1), buf);
        }
        Line::styled("─".repeat(width as usize), t.faint()).render(row(area.y + 1), buf);

        let body = Rect::new(x, area.y + 3, width, area.height - 3);
        if width >= SIDE_BY_SIDE {
            let list_w = (width / 3).clamp(20, 34);
            self.render_list(Rect::new(x, body.y, list_w, body.height), buf);
            for y in body.y..body.bottom() {
                buf[(x + list_w + 1, y)]
                    .set_symbol("│")
                    .set_style(t.faint());
            }
            let text = Rect::new(x + list_w + 3, body.y, width - list_w - 3, body.height);
            self.render_note(text, buf)
        } else if self.editing {
            self.render_note(body, buf)
        } else {
            self.render_list(body, buf);
            None
        }
    }

    fn render_list(&mut self, area: Rect, buf: &mut Buffer) {
        let t = theme::get();
        self.list_area = area;
        if self.notes.is_empty() {
            Line::styled("no notes yet", t.muted()).render(Rect { height: 1, ..area }, buf);
            if area.height > 1 {
                Line::styled("n starts one", t.faint())
                    .render(Rect::new(area.x, area.y + 1, area.width, 1), buf);
            }
            return;
        }
        let visible = usize::from(area.height / ENTRY_ROWS).max(1);
        self.list_top = self.list_top.min(self.selected);
        if self.selected >= self.list_top + visible {
            self.list_top = self.selected + 1 - visible;
        }
        let now = SystemTime::now();
        for (slot, i) in (self.list_top..self.notes.len()).take(visible).enumerate() {
            let note = &self.notes[i];
            let y = area.y + slot as u16 * ENTRY_ROWS;
            let rows = [y, y + 1].map(|y| Rect::new(area.x, y, area.width, 1));
            let selected = i == self.selected;
            if selected {
                let mark = if self.editing { t.faint() } else { t.accent() };
                for r in rows {
                    buf.set_style(r, t.selected());
                    buf[(r.x, r.y)].set_symbol("▍").set_style(mark);
                }
            }
            let room = usize::from(area.width.saturating_sub(2));
            let title_style = if selected {
                t.bold()
            } else {
                Style::new().fg(t.text)
            };
            Line::styled(truncate(&note.title(), room), title_style)
                .render(Rect::new(area.x + 2, y, area.width - 2, 1), buf);
            let age = now.duration_since(note.modified).map_or(0, |d| d.as_secs());
            let age = if age < 60 {
                "now".to_string()
            } else {
                rough_age(age)
            };
            let preview = note.preview();
            let mut meta = vec![Span::styled(age.clone(), t.muted())];
            if !preview.is_empty() {
                meta.push(Span::styled(" · ", t.faint()));
                let room = room.saturating_sub(age.width() + 3);
                meta.push(Span::styled(truncate(&preview, room), t.faint()));
            }
            Line::from(meta).render(Rect::new(area.x + 2, y + 1, area.width - 2, 1), buf);
        }
    }

    fn render_note(&mut self, area: Rect, buf: &mut Buffer) -> Option<(u16, u16)> {
        let t = theme::get();
        self.text_area = area;
        let note = self.notes.get(self.selected)?;
        // A column spare for the cursor at the end of a full row.
        self.wrap = usize::from(area.width.saturating_sub(1)).max(1);
        self.page_rows = usize::from(area.height.saturating_sub(1)).max(1);
        let rows = visual_rows(&note.lines, self.wrap);
        let formatted = format(&note.lines);
        let h = usize::from(area.height);
        let cur = cursor_row(&rows, self.cursor);
        self.top = self.top.min(rows.len().saturating_sub(h));
        if self.editing {
            self.top = self.top.min(cur);
            if cur >= self.top + h {
                self.top = cur + 1 - h;
            }
        }

        let mut cursor = None;
        for (slot, (l, range)) in rows.iter().enumerate().skip(self.top).take(h) {
            let y = area.y + (slot - self.top) as u16;
            let chars: Vec<char> = note.lines[*l].chars().collect();
            let (cells, rule) = &formatted[*l];
            let mut cx = area.x;
            for i in range.clone() {
                let w = chars[i].width().unwrap_or(0) as u16;
                if w == 0 || cx + w > area.right() {
                    continue;
                }
                let glyph = cells[i].glyph.unwrap_or(chars[i]);
                buf.set_stringn(cx, y, glyph.to_string(), w.into(), cells[i].style);
                cx += w;
            }
            if *rule {
                let rest = area.right().saturating_sub(cx).saturating_sub(1);
                buf.set_string(cx, y, "─".repeat(rest.into()), t.faint());
            }
            if self.editing && slot == cur {
                let x = width_of(&chars[range.start..self.cursor.1.min(chars.len())]);
                cursor = Some((area.x + x as u16, y));
            }
            self.drawn.push((*l, range.clone()));
        }
        cursor
    }
}

// Files.

fn load(dir: &Path) -> Result<Vec<Note>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut notes: Vec<Note> = entries
        .filter_map(|e| {
            let path = e.ok()?.path();
            let id = path.file_stem()?.to_str()?;
            if path.extension()? != "md" || id.starts_with('.') {
                return None;
            }
            let text = std::fs::read_to_string(&path).ok()?;
            let modified = path.metadata().and_then(|m| m.modified()).ok()?;
            Some(Note::new(id.to_string(), &text, modified)).filter(|n| !n.is_blank())
        })
        .collect();
    notes.sort_by_key(|n| std::cmp::Reverse(n.modified));
    Ok(notes)
}

/// Write a note (through a temporary file, so a crash never leaves half of
/// one); a blank note's file is removed instead.
fn write(dir: &Path, note: &Note) -> Result<()> {
    let path = dir.join(format!("{}.md", note.id));
    if note.is_blank() {
        return remove(&path);
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".{}.md.tmp", note.id));
    std::fs::write(&tmp, note.text()).with_context(|| format!("saving {}", path.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("saving {}", path.display()))
}

fn remove(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != ErrorKind::NotFound => {
            Err(e).with_context(|| format!("deleting {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Text goes on the terminal as is, so no control characters but newlines;
/// tabs become spaces.
fn clean(s: &str) -> String {
    s.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ")
        .chars()
        .filter(|&c| c == '\n' || !c.is_control())
        .collect()
}

/// A line without its Markdown, for titles and previews.
fn plain(line: &str) -> String {
    let mut s = line.trim().trim_start_matches(['#', '>']).trim_start();
    if let Some(m) = marker(s) {
        s = &s[m.len..];
    }
    s.replace("**", "")
        .replace("~~", "")
        .replace('`', "")
        .trim()
        .to_string()
}

// Wrapping. A line is cut into rows at spaces where it can be, otherwise
// mid-word; rows are char ranges of the line.

fn width_of(chars: &[char]) -> usize {
    chars.iter().map(|c| c.width().unwrap_or(0)).sum()
}

fn byte_at(s: &str, col: usize) -> usize {
    s.char_indices().nth(col).map_or(s.len(), |(b, _)| b)
}

fn wrap(chars: &[char], width: usize) -> Vec<Range<usize>> {
    let mut rows = Vec::new();
    let (mut start, mut w, mut space) = (0, 0, None);
    let mut i = 0;
    while i < chars.len() {
        let cw = chars[i].width().unwrap_or(0);
        if w + cw > width && i > start {
            let at = match space {
                Some(s) if s >= start => s + 1,
                _ => i,
            };
            rows.push(start..at);
            start = at;
            w = width_of(&chars[start..i]);
            space = None;
            continue;
        }
        if chars[i] == ' ' {
            space = Some(i);
        }
        w += cw;
        i += 1;
    }
    rows.push(start..chars.len());
    rows
}

fn visual_rows(lines: &[String], width: usize) -> Vec<(usize, Range<usize>)> {
    lines
        .iter()
        .enumerate()
        .flat_map(|(l, line)| {
            let chars: Vec<char> = line.chars().collect();
            wrap(&chars, width).into_iter().map(move |r| (l, r))
        })
        .collect()
}

/// The row the cursor is on: the one its column falls in, or the line's
/// last row when it's at the end.
fn cursor_row(rows: &[(usize, Range<usize>)], (l, c): (usize, usize)) -> usize {
    rows.iter()
        .enumerate()
        .position(|(i, (line, r))| {
            *line == l && (c < r.end || rows.get(i + 1).is_none_or(|(n, _)| *n != l))
        })
        .unwrap_or(0)
}

/// The column in `range` drawn at `x` cells in. A row that wraps ends a
/// char early, or the cursor would show on the next row.
fn col_at_x(chars: &[char], range: Range<usize>, x: usize, last: bool) -> usize {
    let mut acc = 0;
    for i in range.clone() {
        let w = chars[i].width().unwrap_or(0);
        if acc + w > x {
            return i;
        }
        acc += w;
    }
    if last {
        range.end
    } else {
        range.end.saturating_sub(1).max(range.start)
    }
}

// Markdown.

/// A list item's marker: `- `, `* `, `+ ` or `12. `, maybe then `[ ] `.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Marker {
    /// Spaces before it.
    indent: usize,
    /// The bullet or number with its space.
    bullet: usize,
    /// A task, and whether it's done.
    task: Option<bool>,
    /// All of it, indent included, in chars.
    len: usize,
}

impl Marker {
    /// What Enter puts at the start of the next item.
    fn next(&self, line: &str) -> String {
        let bullet = &line[self.indent..self.indent + self.bullet];
        let bullet = match bullet.find(['.', ')']) {
            Some(d) => {
                let n: u64 = bullet[..d].parse().unwrap_or(0);
                format!("{}{}", n + 1, &bullet[d..])
            }
            None => bullet.to_string(),
        };
        let task = if self.task.is_some() { "[ ] " } else { "" };
        format!("{}{bullet}{task}", " ".repeat(self.indent))
    }
}

fn marker(line: &str) -> Option<Marker> {
    let rest = line.trim_start_matches(' ');
    let indent = line.len() - rest.len();
    let b = rest.as_bytes();
    let bullet = if matches!(b.first(), Some(b'-' | b'*' | b'+')) && b.get(1) == Some(&b' ') {
        2
    } else {
        let d = b.iter().take_while(|c| c.is_ascii_digit()).count();
        if !(1..=3).contains(&d)
            || !matches!(b.get(d), Some(b'.' | b')'))
            || b.get(d + 1) != Some(&b' ')
        {
            return None;
        }
        d + 2
    };
    let after = &rest[bullet..];
    let (task, task_len) = match after.get(..3) {
        Some("[ ]") => (Some(false), 3),
        Some("[x]" | "[X]") => (Some(true), 3),
        _ => (None, 0),
    };
    let task_len = match (task, after.as_bytes().get(3)) {
        (None, _) => 0,
        (Some(_), Some(b' ')) => task_len + 1,
        (Some(_), None) => task_len,
        // `[ ]x` isn't a task.
        (Some(_), Some(_)) => return finish(indent, bullet, None, 0),
    };
    finish(indent, bullet, task, task_len)
}

fn finish(indent: usize, bullet: usize, task: Option<bool>, task_len: usize) -> Option<Marker> {
    Some(Marker {
        indent,
        bullet,
        task,
        len: indent + bullet + task_len,
    })
}

fn heading_level(s: &str) -> Option<usize> {
    let n = s.bytes().take_while(|&c| c == b'#').count();
    ((1..=6).contains(&n) && matches!(s.as_bytes().get(n), None | Some(b' '))).then_some(n)
}

/// `---`, `***` or `___`, spaces allowed between.
fn is_rule(s: &str) -> bool {
    let marks: Vec<char> = s.chars().filter(|c| *c != ' ').collect();
    marks.len() >= 3 && matches!(marks[0], '-' | '*' | '_') && marks.iter().all(|&c| c == marks[0])
}

/// How one char of the source is drawn.
#[derive(Debug, Clone, Copy)]
struct Cell {
    style: Style,
    /// Drawn instead of the char (`•` for a `-` bullet); always as wide.
    glyph: Option<char>,
}

/// Each line's cells, and whether it's a rule (drawn across the row).
fn format(lines: &[String]) -> Vec<(Vec<Cell>, bool)> {
    let t = theme::get();
    let text = Style::new().fg(t.text);
    let code = t.fg(t.working);
    let mut fence = false;
    lines
        .iter()
        .map(|line| {
            let chars: Vec<char> = line.chars().collect();
            let plain = Cell {
                style: text,
                glyph: None,
            };
            let mut cells = vec![plain; chars.len()];
            let paint = |cells: &mut [Cell], r: Range<usize>, style: Style| {
                for c in &mut cells[r] {
                    c.style = style;
                }
            };
            let trimmed = line.trim_start_matches(' ');
            let indent = chars.len() - trimmed.chars().count();
            if trimmed.starts_with("```") {
                fence = !fence;
                paint(&mut cells, 0..chars.len(), t.faint());
                return (cells, false);
            }
            if fence {
                paint(&mut cells, 0..chars.len(), code);
                return (cells, false);
            }
            if is_rule(trimmed) {
                cells.fill(Cell {
                    style: t.faint(),
                    glyph: Some('─'),
                });
                return (cells, true);
            }
            let mut base = text;
            let mut start = indent;
            if let Some(level) = heading_level(trimmed) {
                start = (indent + level + 1).min(chars.len());
                paint(&mut cells, indent..start, t.faint());
                base = match level {
                    1 => t.accent().add_modifier(Modifier::BOLD),
                    2 => t.bold(),
                    _ => t.muted().add_modifier(Modifier::BOLD),
                };
            } else if trimmed.starts_with('>') {
                cells[indent] = Cell {
                    style: t.accent(),
                    glyph: Some('▎'),
                };
                start = indent + 1;
                base = t.muted().add_modifier(Modifier::ITALIC);
            } else if let Some(m) = marker(line) {
                let numbered = chars[indent].is_ascii_digit();
                paint(&mut cells, indent..indent + m.bullet, t.accent());
                if !numbered {
                    cells[indent].glyph = Some('•');
                }
                let b = indent + m.bullet;
                match m.task {
                    Some(true) => {
                        paint(&mut cells, b..b + 3, t.fg(t.done));
                        cells[b + 1].glyph = Some('✓');
                        base = t.muted().add_modifier(Modifier::CROSSED_OUT);
                    }
                    Some(false) => paint(&mut cells, b..b + 3, t.accent()),
                    None => {}
                }
                start = m.len;
            }
            paint(&mut cells, start..chars.len(), base);
            inline(&chars, start..chars.len(), base, &mut cells);
            (cells, false)
        })
        .collect()
}

/// `**bold**`, `*italic*` / `_italic_`, `~~struck~~` and `` `code` ``
/// within `range`, over `base`. The marks are drawn faint.
fn inline(chars: &[char], range: Range<usize>, base: Style, cells: &mut [Cell]) {
    let t = theme::get();
    let end = range.end;
    let find = |from: usize, pat: &[char]| {
        (from..end.saturating_sub(pat.len() - 1)).find(|&j| chars[j..j + pat.len()] == *pat)
    };
    let mut i = range.start;
    while i < end {
        let c = chars[i];
        let next = (i + 1 < end).then(|| chars[i + 1]);
        let span = if c == '`' {
            find(i + 1, &['`'])
                .filter(|&j| j > i + 1)
                .map(|j| (1, j, t.fg(t.working), false))
        } else if (c == '*' || c == '~') && next == Some(c) {
            let modifier = if c == '*' {
                Modifier::BOLD
            } else {
                Modifier::CROSSED_OUT
            };
            find(i + 2, &[c, c])
                .filter(|&j| j > i + 2)
                .map(|j| (2, j, base.add_modifier(modifier), true))
        } else if c == '*' || c == '_' {
            // `snake_case` isn't italic: `_` only counts between words.
            let opens = next.is_some_and(|n| !n.is_whitespace())
                && (c == '*' || i == 0 || !chars[i - 1].is_alphanumeric());
            let closes = |j: usize| {
                chars[j] == c
                    && !chars[j - 1].is_whitespace()
                    && (c == '*' || chars.get(j + 1).is_none_or(|n| !n.is_alphanumeric()))
            };
            (i + 2..end)
                .filter(|_| opens)
                .find(|&j| closes(j))
                .map(|j| (1, j, base.add_modifier(Modifier::ITALIC), true))
        } else {
            None
        };
        match span {
            Some((n, j, style, nested)) => {
                for k in (i..i + n).chain(j..j + n) {
                    cells[k].style = t.faint();
                }
                for cell in &mut cells[i + n..j] {
                    cell.style = style;
                }
                if nested {
                    inline(chars, i + n..j, style, cells);
                }
                i = j + n;
            }
            None => i += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn notes_with(texts: &[&str]) -> Notes {
        let mut n = Notes::new(None);
        for (i, text) in texts.iter().enumerate() {
            n.notes
                .push(Note::new(i.to_string(), text, SystemTime::now()));
        }
        n.open();
        n
    }

    fn editing(texts: &[&str]) -> Notes {
        let mut n = notes_with(texts);
        n.editing = true;
        n
    }

    fn typed(n: &mut Notes, s: &str) {
        for k in crate::client::input::decode_keys(s.as_bytes()) {
            n.key(k);
        }
    }

    fn render(n: &mut Notes, w: u16, h: u16) -> (String, Option<(u16, u16)>) {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut cursor = None;
        term.draw(|f| cursor = n.render(f.area(), f.buffer_mut()))
            .unwrap();
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            out.push_str(line.trim_end());
            out.push('\n');
        }
        (out, cursor)
    }

    #[test]
    fn opening_with_no_notes_starts_one() {
        let mut n = notes_with(&[]);
        assert!(n.editing);
        assert_eq!((n.notes.len(), n.cursor), (1, (0, 2)));
        typed(&mut n, "Groceries\rmilk");
        assert_eq!(n.notes[0].lines, vec!["# Groceries", "milk"]);
        assert_eq!(n.notes[0].title(), "Groceries");
        assert_eq!(n.notes[0].preview(), "milk");
        // Esc goes back to the list; q closes the page.
        typed(&mut n, "\x1b");
        assert!(!n.editing);
        assert!(matches!(n.key(Key::Char('q')), Outcome::Close));
    }

    #[test]
    fn a_note_left_blank_is_dropped() {
        let mut n = notes_with(&["kept"]);
        typed(&mut n, "n");
        assert_eq!(n.notes.len(), 2);
        typed(&mut n, "\x1b");
        assert_eq!(n.notes.len(), 1);
        assert_eq!(n.notes[0].title(), "kept");
    }

    #[test]
    fn lists_continue_on_enter() {
        let mut n = editing(&["- one"]);
        typed(&mut n, "\r");
        typed(&mut n, "two\r\r");
        // Enter on an empty item ends the list.
        assert_eq!(n.notes[0].lines, vec!["- one", "- two", ""]);

        let mut n = editing(&["9. nine"]);
        typed(&mut n, "\r\x14ten");
        assert_eq!(n.notes[0].lines, vec!["9. nine", "10. [ ] ten"]);
        // Tasks continue as unticked tasks.
        let mut n = editing(&["- [x] done"]);
        typed(&mut n, "\rnext");
        assert_eq!(n.notes[0].lines, vec!["- [x] done", "- [ ] next"]);
        // Tab nests the item, Backspace right after the marker un-nests it.
        typed(&mut n, "\x01\t");
        assert_eq!(n.notes[0].lines[1], "  - [ ] next");
        n.cursor.1 = 8;
        typed(&mut n, "\x7f");
        assert_eq!(n.notes[0].lines[1], "- [ ] next");
        assert_eq!(n.cursor.1, 6);
        typed(&mut n, "\x7f");
        assert_eq!((n.notes[0].lines[1].as_str(), n.cursor.1), ("next", 0));
    }

    #[test]
    fn ctrl_t_ticks_tasks() {
        let mut n = editing(&["buy milk"]);
        typed(&mut n, "\x14");
        assert_eq!(n.notes[0].lines[0], "- [ ] buy milk");
        assert_eq!(n.cursor.1, 14);
        typed(&mut n, "\x14");
        assert_eq!(n.notes[0].lines[0], "- [x] buy milk");
        typed(&mut n, "\x14");
        assert_eq!(n.notes[0].lines[0], "- [ ] buy milk");
        let mut n = editing(&["  * item"]);
        typed(&mut n, "\x14");
        assert_eq!(n.notes[0].lines[0], "  * [ ] item");
    }

    #[test]
    fn editing_across_lines() {
        let mut n = editing(&["ab\ncd"]);
        assert_eq!(n.cursor, (1, 2));
        typed(&mut n, "\x01\x7f");
        assert_eq!(
            (n.notes[0].lines.clone(), n.cursor),
            (vec!["abcd".into()], (0, 2))
        );
        typed(&mut n, "\r");
        assert_eq!(n.notes[0].lines, vec!["ab", "cd"]);
        typed(&mut n, "\x1b[A\x1b[F\x1b[3~");
        assert_eq!(n.notes[0].lines, vec!["abcd"]);
        // Pastes keep their newlines and lose control characters.
        n.paste("x\ny\x1b[31m\tz");
        assert_eq!(n.notes[0].lines, vec!["abx", "y[31m    zcd"]);
        typed(&mut n, "\x17");
        assert_eq!(n.notes[0].lines[1], "y[31m    cd");
    }

    #[test]
    fn edited_notes_move_to_the_top() {
        let mut n = notes_with(&["first", "second"]);
        typed(&mut n, "j\r!");
        assert_eq!(n.selected, 0);
        assert_eq!(n.notes[0].lines, vec!["second!"]);
        assert!(n.notes[0].unsaved);
    }

    #[test]
    fn deleting_asks_first() {
        let mut n = notes_with(&["a", "b"]);
        typed(&mut n, "x");
        assert!(n.confirm_delete);
        typed(&mut n, "n");
        assert_eq!(n.notes.len(), 2);
        typed(&mut n, "xy");
        assert_eq!(n.notes.len(), 1);
        assert_eq!(n.notes[0].title(), "b");
    }

    #[test]
    fn wrapping_and_moving_by_rows() {
        let chars: Vec<char> = "hello big world".chars().collect();
        assert_eq!(wrap(&chars, 10), vec![0..10, 10..15]);
        let chars: Vec<char> = "abcdefghij".chars().collect();
        assert_eq!(wrap(&chars, 4), vec![0..4, 4..8, 8..10]);
        let chars: Vec<char> = "日本語です".chars().collect();
        assert_eq!(wrap(&chars, 5), vec![0..2, 2..4, 4..5]);
        assert_eq!(wrap(&[], 5), vec![0..0]);

        let mut n = editing(&["hello big world\nx"]);
        n.wrap = 10;
        n.cursor = (0, 2);
        typed(&mut n, "\x1b[B");
        assert_eq!(n.cursor, (0, 12));
        typed(&mut n, "\x1b[B");
        assert_eq!(n.cursor, (1, 1));
        // The column it came from is kept when a row is shorter.
        typed(&mut n, "\x1b[A\x1b[A");
        assert_eq!(n.cursor, (0, 2));
    }

    #[test]
    fn markdown_formatting() {
        let lines: Vec<String> = [
            "# Title",
            "a **b** *c* `d` ~~e~~ snake_case",
            "- [x] done",
            "---",
        ]
        .map(String::from)
        .to_vec();
        let f = format(&lines);
        let has = |l: usize, i: usize, m: Modifier| f[l].0[i].style.add_modifier.contains(m);
        assert!(has(0, 3, Modifier::BOLD));
        assert!(has(1, 4, Modifier::BOLD));
        assert!(!has(1, 2, Modifier::BOLD));
        assert!(has(1, 9, Modifier::ITALIC));
        assert!(has(1, 18, Modifier::CROSSED_OUT));
        assert!(!(22..32).any(|i| has(1, i, Modifier::ITALIC)));
        assert_eq!(f[2].0[0].glyph, Some('•'));
        assert_eq!(f[2].0[3].glyph, Some('✓'));
        assert!(has(2, 7, Modifier::CROSSED_OUT));
        assert!(f[3].1);
        assert_eq!(plain("- [ ] **buy** milk"), "buy milk");
        assert_eq!(plain("## Plans"), "Plans");
    }

    #[test]
    fn page_snapshot() {
        let mut n = notes_with(&[
            "# Release checklist\n\nShip **notes** in the next *minor*:\n\n- [x] editor\n- [ ] save to `~/.corral/notes`\n> quotes too\n---\nend",
            "# Groceries\nmilk",
        ]);
        n.editing = true;
        let (text, cursor) = render(&mut n, 80, 14);
        insta::assert_snapshot!(text);
        // The cursor sits at the end of the note.
        assert_eq!(cursor, Some((33, 11)));
        // Clicking the second note opens it; clicking text moves the cursor.
        n.click(5, 5);
        assert_eq!((n.selected, n.editing), (1, true));
        render(&mut n, 80, 14);
        n.click(32, 3);
        assert_eq!(n.cursor, (0, 2));
        n.click(10, 0);
        assert_eq!(n.notes.len(), 3);
        // Narrow: only the list, until a note is opened.
        let mut n = notes_with(&["a", "b"]);
        let (text, cursor) = render(&mut n, 40, 8);
        assert!(text.contains("a\n"), "{text}");
        assert_eq!(cursor, None);
    }

    #[test]
    fn notes_are_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("notes");
        let mut n = Notes::new(Some(dir.clone()));
        n.open();
        typed(&mut n, "Ideas\r- one");
        n.autosave(false);
        let id = n.notes[0].id.clone();
        let path = dir.join(format!("{id}.md"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# Ideas\n- one\n");
        // Changes made elsewhere show up when the page opens again.
        std::fs::write(&path, "# Ideas\n- two\n").unwrap();
        std::fs::write(dir.join("other.md"), "hi\r\n").unwrap();
        std::fs::write(dir.join("empty.md"), "  \n").unwrap();
        std::fs::write(dir.join("readme.txt"), "not a note").unwrap();
        n.open();
        assert_eq!(n.notes.len(), 2);
        let ideas = n.notes.iter().find(|x| x.id == id).unwrap();
        assert_eq!(ideas.lines, vec!["# Ideas", "- two"]);
        // Deleting removes the file.
        let i = n.notes.iter().position(|x| x.id == "other").unwrap();
        n.select(i);
        typed(&mut n, "xy");
        assert!(!dir.join("other.md").exists());
        // Emptying a note and leaving it removes it too.
        n.editing = true;
        n.cursor = (0, 0);
        typed(&mut n, "\x0b\x0b\x0b\x1b");
        n.autosave(false);
        assert!(n.notes.is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn a_note_can_be_sent_to_an_agent() {
        let mut n = notes_with(&["# Fix login\n\n- [ ] check the token refresh\n"]);
        // From the list: `s`.
        assert!(matches!(n.key(Key::Char('s')), Outcome::Send));
        assert_eq!(
            n.to_send().as_deref(),
            Some("# Fix login\n\n- [ ] check the token refresh")
        );
        // While editing, `s` is typing and `^s` sends.
        n.editing = true;
        assert!(matches!(n.key(Key::Char('s')), Outcome::Stay));
        assert!(matches!(n.key(Key::Ctrl('s')), Outcome::Send));
        // The page's button names the agent, and a click on it sends.
        n.editing = false;
        n.send_to = Some("api".into());
        let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
        term.draw(|f| {
            n.render(f.area(), f.buffer_mut());
        })
        .unwrap();
        let top: String = (0..100)
            .map(|x| term.backend().buffer()[(x, 0)].symbol().to_string())
            .collect();
        assert!(top.contains("↳ send to api"), "{top}");
        let b = n.send_button;
        assert!(matches!(n.click(b.x, b.y), Outcome::Send));
        // A blank note has nothing to send, and no button.
        let mut n = Notes::new(None);
        n.open();
        n.send_to = Some("api".into());
        assert_eq!(n.to_send(), None);
        term.draw(|f| {
            n.render(f.area(), f.buffer_mut());
        })
        .unwrap();
        assert_eq!(n.send_button, Rect::default());
    }
}

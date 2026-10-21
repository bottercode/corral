//! The terminal UI client.
//!
//! Two views: the overview, a grid of live cards for every agent where
//! keys drive corral itself; and a zoomed agent, where stdin is forwarded
//! byte-for-byte to the agent except for the prefix key, mouse events and
//! bracketed pastes (which pass through whole). The zoomed agent is mirrored
//! locally in a vt100 parser (snapshot plus live output).

pub mod conn;
pub mod diff;
pub mod grid;
pub mod input;
pub mod ui;

use std::collections::HashMap;
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures::{SinkExt, StreamExt};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use crate::config::{Config, prefix_label};
use crate::paths;
use crate::protocol::{
    self, AgentState, ClientMsg, Command, NewPane, PaneId, PaneInfo, Reply, ServerMsg,
    WorktreeAction,
};
use conn::Conn;
use diff::DiffView;
use input::{Chunk, Key, Mouse, MouseKind, Splitter, decode_keys};
use ui::{CloseStep, NewDialog, TextInput};

const STATUS_FOR: Duration = Duration::from_secs(6);
const WHEEL_LINES: usize = 3;

#[derive(Debug, Clone)]
pub enum Mode {
    Normal,
    Help,
    Scroll { offset: usize },
    New(NewDialog),
    Rename { pane: PaneId, input: TextInput },
    Close { pane: PaneId, step: CloseStep },
    Diff(DiffView),
}

#[derive(Debug)]
enum Pending {
    New,
    Kill { pane: PaneId },
    Diff { pane: PaneId },
    DiffAction { pane: PaneId },
    Other,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Exit {
    Detached,
    ServerGone(String),
}

pub struct App {
    pub panes: Vec<PaneInfo>,
    /// When `panes` arrived (their `state_secs` count from then).
    pub state_at: Instant,
    /// The zoomed agent; `None` is the overview.
    pub focus: Option<PaneId>,
    /// The highlighted card on the overview.
    pub selected: Option<PaneId>,
    pub parser: Option<vt100::Parser>,
    parser_pane: Option<PaneId>,
    pub previews: HashMap<PaneId, grid::Preview>,
    pub mode: Mode,
    status: Option<(String, Instant, bool)>,
    pub prefix_label: String,
    prefix: u8,
    pub awaiting_command: bool,
    pub body_area: Rect,
    pub card_hits: Vec<(Rect, PaneId)>,
    pub grid_scroll: usize,
    pub frame: usize,
    pub scroll_max: usize,
    splitter: Splitter,
    default_command: String,
    cwd: PathBuf,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    seen_state: bool,
    pub outbox: Vec<ClientMsg>,
    /// Raw bytes for the outer terminal (bell, OSC 9).
    pub term_writes: Vec<u8>,
    pub exit: Option<Exit>,
    pub dirty: bool,
}

impl App {
    pub fn new(config: &Config, cwd: PathBuf) -> Result<Self> {
        let prefix = config.prefix_byte()?;
        Ok(App {
            panes: Vec::new(),
            state_at: Instant::now(),
            focus: None,
            selected: None,
            parser: None,
            parser_pane: None,
            previews: HashMap::new(),
            mode: Mode::Normal,
            status: None,
            prefix_label: prefix_label(prefix),
            prefix,
            awaiting_command: false,
            body_area: Rect::default(),
            card_hits: Vec::new(),
            grid_scroll: 0,
            frame: 0,
            scroll_max: 0,
            splitter: Splitter::new(prefix),
            default_command: config.default_command.clone(),
            cwd,
            next_id: 1,
            pending: HashMap::new(),
            seen_state: false,
            outbox: Vec::new(),
            term_writes: Vec::new(),
            exit: None,
            dirty: true,
        })
    }

    pub fn status(&self) -> Option<(&str, bool)> {
        self.status
            .as_ref()
            .filter(|(_, at, _)| at.elapsed() < STATUS_FOR)
            .map(|(m, _, e)| (m.as_str(), *e))
    }

    fn set_status(&mut self, msg: impl Into<String>, error: bool) {
        self.status = Some((msg.into(), Instant::now(), error));
    }

    /// Time `p` has spent in its current state.
    pub fn age(&self, p: &PaneInfo) -> Duration {
        Duration::from_secs(p.state_secs) + self.state_at.elapsed()
    }

    fn request(&mut self, cmd: Command, kind: Pending) {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, kind);
        self.outbox.push(ClientMsg::Command { id, cmd });
    }

    pub fn focused(&self) -> Option<&PaneInfo> {
        let id = self.focus.as_ref()?;
        self.panes.iter().find(|p| &p.id == id)
    }

    pub fn selected_pane(&self) -> Option<&PaneInfo> {
        let id = self.selected.as_ref()?;
        self.panes.iter().find(|p| &p.id == id)
    }

    /// The pane commands like close or rename apply to.
    fn target(&self) -> Option<&PaneInfo> {
        if self.focus.is_some() {
            self.focused()
        } else {
            self.selected_pane()
        }
    }

    fn order(&self) -> Vec<PaneId> {
        grid::display_order(&self.panes)
            .into_iter()
            .map(|p| p.id.clone())
            .collect()
    }

    fn selected_index(&self) -> Option<usize> {
        let sel = self.selected.as_ref()?;
        self.order().iter().position(|p| p == sel)
    }

    /// Scroll the grid so the selected card is on screen.
    pub fn keep_selection_visible(&mut self) {
        let n = self.panes.len();
        let layout = grid::grid_layout(n, self.body_area);
        let row = self.selected_index().unwrap_or(0) / layout.cols;
        if row < self.grid_scroll {
            self.grid_scroll = row;
        } else if row >= self.grid_scroll + layout.rows_visible {
            self.grid_scroll = row + 1 - layout.rows_visible;
        }
        let rows = n.div_ceil(layout.cols);
        self.grid_scroll = self
            .grid_scroll
            .min(rows.saturating_sub(layout.rows_visible));
    }

    /// Open an agent full screen.
    fn zoom(&mut self, pane: PaneId) {
        self.selected = Some(pane.clone());
        if self.focus.as_ref() != Some(&pane) {
            self.outbox.push(ClientMsg::Focus {
                pane: Some(pane.clone()),
            });
            self.focus = Some(pane);
            self.parser = None;
            self.parser_pane = None;
        }
        if !matches!(self.mode, Mode::Normal) {
            self.mode = Mode::Normal;
        }
    }

    /// Back to the overview.
    fn unzoom(&mut self) {
        if let Some(p) = self.focus.take() {
            self.selected = Some(p);
            self.outbox.push(ClientMsg::Focus { pane: None });
        }
        self.parser = None;
        self.parser_pane = None;
        if matches!(self.mode, Mode::Scroll { .. }) {
            self.mode = Mode::Normal;
        }
    }

    /// Move to another pane: zoomed stays zoomed, the overview moves the
    /// selection.
    fn go_to(&mut self, pane: PaneId) {
        if self.focus.is_some() {
            self.zoom(pane);
        } else {
            self.selected = Some(pane);
        }
    }

    fn cycle(&mut self, delta: isize) {
        let order = self.order();
        if order.is_empty() {
            return;
        }
        let cur = self
            .focus
            .as_ref()
            .or(self.selected.as_ref())
            .and_then(|f| order.iter().position(|p| p == f))
            .map_or(if delta > 0 { -1 } else { 0 }, |i| i as isize);
        let next = (cur + delta).rem_euclid(order.len() as isize) as usize;
        self.go_to(order[next].clone());
    }

    /// Arrow keys on the grid move in two dimensions.
    fn move_selection(&mut self, dx: isize, dy: isize) {
        let order = self.order();
        if order.is_empty() {
            return;
        }
        let cols = grid::grid_layout(order.len(), self.body_area).cols as isize;
        let cur = self.selected_index().map_or(0, |i| i as isize);
        let mut next = cur + dx + dy * cols;
        if dx != 0 {
            next = next.rem_euclid(order.len() as isize);
        } else if !(0..order.len() as isize).contains(&next) {
            return;
        }
        self.selected = Some(order[next as usize].clone());
    }

    fn next_blocked(&mut self) {
        let order = self.order();
        let cur = self
            .focus
            .as_ref()
            .or(self.selected.as_ref())
            .and_then(|f| order.iter().position(|p| p == f))
            .unwrap_or(0);
        let n = order.len();
        let blocked = |id: &PaneId| {
            self.panes
                .iter()
                .any(|p| &p.id == id && p.state == AgentState::Blocked)
        };
        match (1..=n)
            .map(|i| &order[(cur + i) % n])
            .find(|id| blocked(id))
        {
            Some(id) => {
                let id = id.clone();
                self.go_to(id);
            }
            None => self.set_status("no agent needs you", false),
        }
    }

    /// Directory a new agent should start in by default.
    fn default_dir(&self) -> PathBuf {
        match self.target() {
            Some(p) => p
                .worktree
                .as_ref()
                .map_or_else(|| p.cwd.clone(), |w| w.repo.clone()),
            None => self.cwd.clone(),
        }
    }

    fn open_new(&mut self, worktree: bool) {
        let dir = self.default_dir();
        self.mode = Mode::New(NewDialog::new(&self.default_command, &dir, worktree));
    }

    fn submit_new(&mut self, d: NewDialog) {
        let cwd = paths::expand_tilde(d.cwd.text.trim());
        let cwd = if cwd.is_relative() {
            self.cwd.join(cwd)
        } else {
            cwd
        };
        let branch = d.branch.text.trim().to_string();
        let line = d.command.text.trim().to_string();
        let req = NewPane {
            name: None,
            cwd: Some(cwd),
            workspace: None,
            command: if line.is_empty() {
                Vec::new()
            } else {
                vec![line]
            },
            shell: true,
            worktree: d.worktree,
            branch: (d.worktree && !branch.is_empty()).then_some(branch),
            focus: true,
        };
        self.set_status(
            if d.worktree {
                "creating worktree…"
            } else {
                "starting…"
            },
            false,
        );
        self.request(Command::New(req), Pending::New);
    }

    fn open_close(&mut self) {
        let Some(p) = self.target() else { return };
        let step = if p.worktree.is_some() {
            CloseStep::Worktree
        } else {
            CloseStep::Confirm
        };
        self.mode = Mode::Close {
            pane: p.id.clone(),
            step,
        };
    }

    fn open_rename(&mut self) {
        if let Some(p) = self.target() {
            self.mode = Mode::Rename {
                pane: p.id.clone(),
                input: TextInput::new(&p.name),
            };
        }
    }

    fn open_diff(&mut self) {
        let Some(p) = self.target() else { return };
        if p.worktree.is_none() {
            self.set_status(
                format!(
                    "{} has no worktree; start agents with w to review their diffs",
                    p.name
                ),
                false,
            );
            return;
        }
        let (id, name) = (p.id.clone(), p.name.clone());
        self.mode = Mode::Diff(DiffView::new(id.clone(), name));
        self.request(
            Command::Diff { pane: id.clone() },
            Pending::Diff { pane: id },
        );
    }

    fn kill(&mut self, pane: PaneId, worktree: WorktreeAction, force: bool) {
        self.request(
            Command::Kill {
                pane: pane.clone(),
                worktree,
                force,
            },
            Pending::Kill { pane },
        );
    }

    fn forward(&mut self, bytes: &[u8], is_paste: bool) {
        let Some(pane) = self.focus.clone() else {
            return;
        };
        let (bp, app_cursor) = self.parser.as_ref().map_or((false, false), |p| {
            let s = p.screen();
            (s.bracketed_paste(), s.application_cursor())
        });
        let bytes = input::adapt_for_pane(bytes, bp, app_cursor, is_paste);
        if !bytes.is_empty() {
            self.outbox.push(ClientMsg::Input { pane, bytes });
        }
    }

    fn detach(&mut self) {
        self.outbox.push(ClientMsg::Detach);
        self.exit = Some(Exit::Detached);
    }

    /// Keys after the prefix (work in both views).
    fn prefix_command(&mut self, key: Key) {
        match key {
            Key::Char('g') | Key::Esc => self.unzoom(),
            Key::Char('n') => self.open_new(false),
            Key::Char('w') => self.open_new(true),
            Key::Char('j') | Key::Down => self.cycle(1),
            Key::Char('k') | Key::Up => self.cycle(-1),
            Key::Char(c @ '1'..='9') => {
                if let Some(id) = self.order().get(c as usize - '1' as usize).cloned() {
                    self.zoom(id);
                }
            }
            Key::Char('b') => self.next_blocked(),
            Key::Char('x') => self.open_close(),
            Key::Char('r') => self.open_rename(),
            Key::Char('D') => self.open_diff(),
            Key::Char('d') => self.detach(),
            Key::Char('[') if self.focus.is_some() => self.enter_scroll(0),
            Key::Char('?') => self.mode = Mode::Help,
            _ => {}
        }
    }

    /// Keys on the overview, where nothing is forwarded to agents.
    fn grid_key(&mut self, key: Key) {
        match key {
            Key::Left | Key::Char('h') => self.move_selection(-1, 0),
            Key::Right | Key::Char('l') => self.move_selection(1, 0),
            Key::Up | Key::Char('k') => self.move_selection(0, -1),
            Key::Down | Key::Char('j') => self.move_selection(0, 1),
            Key::Tab => self.cycle(1),
            Key::BackTab => self.cycle(-1),
            Key::Enter => {
                if let Some(id) = self.selected_pane().map(|p| p.id.clone()) {
                    self.zoom(id);
                }
            }
            Key::Char(c @ '1'..='9') => {
                if let Some(id) = self.order().get(c as usize - '1' as usize).cloned() {
                    self.zoom(id);
                }
            }
            Key::Char('n') => self.open_new(false),
            Key::Char('w') => self.open_new(true),
            Key::Char('d') => self.open_diff(),
            Key::Char('b') => self.next_blocked(),
            Key::Char('x') => self.open_close(),
            Key::Char('r') => self.open_rename(),
            Key::Char('q') => self.detach(),
            Key::Char('?') => self.mode = Mode::Help,
            Key::Ctrl('c') => self.set_status("press q to detach (agents keep running)", false),
            _ => {}
        }
    }

    fn scroll_limit(&mut self) -> usize {
        let Some(p) = self.parser.as_mut() else {
            return 0;
        };
        let s = p.screen_mut();
        let cur = s.scrollback();
        s.set_scrollback(usize::MAX);
        let max = s.scrollback();
        s.set_scrollback(cur);
        self.scroll_max = max;
        max
    }

    fn enter_scroll(&mut self, offset: usize) {
        let max = self.scroll_limit();
        if max == 0 && offset == 0 && self.parser.is_some() {
            self.set_status("no scrollback in this pane", false);
            return;
        }
        self.mode = Mode::Scroll {
            offset: offset.min(max),
        };
    }

    fn scroll_by(&mut self, delta: isize) {
        let max = self.scroll_limit();
        let cur = match self.mode {
            Mode::Scroll { offset } => offset as isize,
            _ => 0,
        };
        let next = (cur + delta).clamp(0, max as isize) as usize;
        self.mode = if next == 0 && delta < 0 {
            Mode::Normal
        } else {
            Mode::Scroll { offset: next }
        };
    }

    fn scroll_key(&mut self, key: Key) {
        let page = self.body_area.height.max(2) as isize - 1;
        match key {
            Key::Char('q') | Key::Esc | Key::Ctrl('c') | Key::Enter => self.mode = Mode::Normal,
            Key::Char('k') | Key::Up => self.scroll_by(1),
            Key::Char('j') | Key::Down => self.scroll_by(-1),
            Key::PageUp | Key::Ctrl('u') => self.scroll_by(page),
            Key::PageDown | Key::Ctrl('d') => self.scroll_by(-page),
            Key::Char('g') | Key::Home => self.scroll_by(isize::MAX / 2),
            Key::Char('G') | Key::End => self.mode = Mode::Normal,
            _ => {}
        }
    }

    fn on_mouse(&mut self, m: Mouse) {
        if self.focus.is_none() {
            match m.kind {
                MouseKind::LeftDown => {
                    let hit = self
                        .card_hits
                        .iter()
                        .find(|(r, _)| r.contains((m.x, m.y).into()))
                        .map(|(_, id)| id.clone());
                    if let Some(id) = hit {
                        // Click selects; clicking the selected card opens it.
                        if self.selected.as_ref() == Some(&id) {
                            self.zoom(id);
                        } else {
                            self.selected = Some(id);
                        }
                    }
                }
                MouseKind::WheelDown => self.move_selection(0, 1),
                MouseKind::WheelUp => self.move_selection(0, -1),
                _ => {}
            }
            return;
        }
        match m.kind {
            // The top bar's "‹" goes back to the overview.
            MouseKind::LeftDown if m.y < self.body_area.y && m.x < 12 => self.unzoom(),
            MouseKind::WheelUp if matches!(self.mode, Mode::Normal | Mode::Scroll { .. }) => {
                if matches!(self.mode, Mode::Normal) {
                    self.enter_scroll(WHEEL_LINES);
                    if !matches!(self.mode, Mode::Scroll { .. }) {
                        self.status = None;
                    }
                } else {
                    self.scroll_by(WHEEL_LINES as isize);
                }
            }
            MouseKind::WheelDown if matches!(self.mode, Mode::Scroll { .. }) => {
                self.scroll_by(-(WHEEL_LINES as isize));
            }
            _ => {}
        }
    }

    fn dialog_key(&mut self, key: Key) {
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Help => {}
            Mode::New(mut d) => match d.handle(key) {
                Some(true) => self.submit_new(d),
                Some(false) => {}
                None => self.mode = Mode::New(d),
            },
            Mode::Rename { pane, mut input } => match key {
                Key::Esc | Key::Ctrl('c') => {}
                Key::Enter => {
                    let name = input.text.trim().to_string();
                    if !name.is_empty() {
                        self.request(Command::Rename { pane, name }, Pending::Other);
                    }
                }
                k => {
                    input.handle(k);
                    self.mode = Mode::Rename { pane, input };
                }
            },
            Mode::Close { pane, step } => {
                let keep = |s: &mut Self, pane: PaneId| s.kill(pane, WorktreeAction::Keep, false);
                match (&step, key) {
                    (_, Key::Esc | Key::Ctrl('c')) => {}
                    (CloseStep::Confirm, Key::Char('y' | 'Y' | 'x') | Key::Enter) => {
                        keep(self, pane)
                    }
                    (CloseStep::Confirm, Key::Char('n' | 'N')) => {}
                    (CloseStep::Worktree, Key::Char('k')) => keep(self, pane),
                    (CloseStep::Worktree, Key::Char('r')) => {
                        self.kill(pane, WorktreeAction::Remove, false)
                    }
                    (CloseStep::Worktree, Key::Char('d')) => {
                        self.kill(pane, WorktreeAction::RemoveAndDeleteBranch, false)
                    }
                    (CloseStep::Force { action, .. }, Key::Char('f')) => {
                        let action = *action;
                        self.kill(pane, action, true)
                    }
                    (CloseStep::Force { .. }, Key::Char('k')) => keep(self, pane),
                    _ => self.mode = Mode::Close { pane, step },
                }
            }
            Mode::Diff(mut v) => {
                let pane = v.pane.clone();
                match v.handle(key) {
                    diff::Action::Close => return,
                    diff::Action::None => {}
                    diff::Action::Refresh => {
                        self.request(Command::Diff { pane: pane.clone() }, Pending::Diff { pane })
                    }
                    diff::Action::Commit(message) => self.request(
                        Command::Commit {
                            pane: pane.clone(),
                            message,
                        },
                        Pending::DiffAction { pane },
                    ),
                    diff::Action::Merge => self.request(
                        Command::Merge { pane: pane.clone() },
                        Pending::DiffAction { pane },
                    ),
                    diff::Action::Discard => self.request(
                        Command::Discard { pane: pane.clone() },
                        Pending::DiffAction { pane },
                    ),
                }
                self.mode = Mode::Diff(v);
            }
            m @ (Mode::Normal | Mode::Scroll { .. }) => self.mode = m,
        }
    }

    fn paste_into_dialog(&mut self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
        let text = text
            .trim_start_matches("\x1b[200~")
            .trim_end_matches("\x1b[201~")
            .replace(['\r', '\n'], " ");
        match &mut self.mode {
            Mode::New(d) => d.paste(&text),
            Mode::Rename { input, .. } => input.insert_str(&text),
            Mode::Diff(v) => v.paste(&text),
            _ => {}
        }
    }

    pub fn on_stdin(&mut self, bytes: &[u8]) {
        self.dirty = true;
        for chunk in self.splitter.feed(bytes) {
            let in_dialog = !matches!(self.mode, Mode::Normal | Mode::Scroll { .. });
            let zoomed = self.focus.is_some();
            match chunk {
                Chunk::Mouse(m) => {
                    if !in_dialog {
                        self.on_mouse(m);
                    }
                }
                Chunk::Paste(p) => {
                    if in_dialog {
                        self.paste_into_dialog(&p);
                    } else if zoomed {
                        self.awaiting_command = false;
                        self.mode = Mode::Normal;
                        self.forward(&p, true);
                    }
                }
                Chunk::Prefix if in_dialog => {
                    if matches!(self.mode, Mode::Help) {
                        self.mode = Mode::Normal;
                    }
                }
                Chunk::Prefix => {
                    if self.awaiting_command {
                        // Prefix twice sends it to the agent.
                        self.awaiting_command = false;
                        let p = [self.prefix];
                        self.forward(&p, false);
                    } else {
                        self.awaiting_command = true;
                    }
                }
                Chunk::Bytes(b) => {
                    if in_dialog {
                        for k in decode_keys(&b) {
                            if matches!(self.mode, Mode::Normal | Mode::Scroll { .. }) {
                                break;
                            }
                            self.dialog_key(k);
                        }
                    } else if self.awaiting_command {
                        self.awaiting_command = false;
                        if let Some(k) = decode_keys(&b).into_iter().next() {
                            self.prefix_command(k);
                        }
                    } else if matches!(self.mode, Mode::Scroll { .. }) {
                        for k in decode_keys(&b) {
                            self.scroll_key(k);
                        }
                    } else if zoomed {
                        self.forward(&b, false);
                    } else {
                        for k in decode_keys(&b) {
                            if !matches!(self.mode, Mode::Normal)
                                || self.focus.is_some()
                                || self.exit.is_some()
                            {
                                break;
                            }
                            self.grid_key(k);
                        }
                    }
                }
            }
        }
    }

    pub fn on_server(&mut self, msg: ServerMsg) {
        self.dirty = true;
        match msg {
            ServerMsg::State { panes, focus } => {
                self.panes = panes;
                self.state_at = Instant::now();
                self.previews
                    .retain(|id, _| self.panes.iter().any(|p| &p.id == id));
                // The server decides what this client is looking at: a new
                // agent opens zoomed; a closed one falls back to the grid.
                if focus != self.focus {
                    if focus.is_none() {
                        self.parser = None;
                        self.parser_pane = None;
                        if matches!(self.mode, Mode::Scroll { .. }) {
                            self.mode = Mode::Normal;
                        }
                    }
                    if let Some(f) = &focus {
                        self.selected = Some(f.clone());
                    }
                    self.focus = focus;
                }
                let valid = self
                    .selected
                    .as_ref()
                    .is_some_and(|s| self.panes.iter().any(|p| &p.id == s));
                if !valid {
                    self.selected = self.order().first().cloned();
                }
                if !self.seen_state {
                    self.seen_state = true;
                    if self.panes.is_empty() {
                        self.open_new(false);
                    }
                }
            }
            ServerMsg::Screen {
                pane,
                cols,
                rows,
                bytes,
                ..
            } => {
                let mut p = vt100::Parser::new(rows, cols, crate::server::pane::SCROLLBACK);
                p.process(&bytes);
                self.parser = Some(p);
                self.parser_pane = Some(pane);
                if matches!(self.mode, Mode::Scroll { .. }) {
                    self.mode = Mode::Normal;
                }
            }
            ServerMsg::Output { pane, bytes, .. } => {
                if self.parser_pane.as_ref() == Some(&pane)
                    && let Some(p) = self.parser.as_mut()
                {
                    p.process(&bytes);
                }
            }
            ServerMsg::Preview {
                pane, cols, bytes, ..
            } => {
                self.previews.insert(pane, grid::Preview::new(cols, bytes));
            }
            ServerMsg::Notify { message, .. } => {
                self.term_writes.push(0x07);
                let clean: String = message.chars().filter(|c| !c.is_control()).collect();
                self.term_writes
                    .extend_from_slice(format!("\x1b]9;corral: {clean}\x07").as_bytes());
                self.set_status(message, false);
            }
            ServerMsg::Reply { id, result } => {
                let kind = self.pending.remove(&id).unwrap_or(Pending::Other);
                self.on_reply(kind, result);
            }
            ServerMsg::Shutdown { reason } => self.exit = Some(Exit::ServerGone(reason)),
            ServerMsg::Hello { .. } => {}
        }
    }

    fn on_reply(&mut self, kind: Pending, result: Result<Reply, String>) {
        match (kind, result) {
            (Pending::New, Ok(Reply::Created { pane, warning })) => match warning {
                Some(w) => self.set_status(w, true),
                None => match &pane.worktree {
                    Some(w) => self.set_status(
                        format!("{} started in worktree {}", pane.id, w.branch),
                        false,
                    ),
                    None => self.status = None,
                },
            },
            (Pending::Kill { .. }, Ok(Reply::Text(t))) => self.set_status(t, false),
            (Pending::Kill { pane }, Err(e))
                if e.contains("uncommitted changes") || e.contains("on any other branch") =>
            {
                let action = if e.contains("on any other branch") {
                    WorktreeAction::RemoveAndDeleteBranch
                } else {
                    WorktreeAction::Remove
                };
                self.mode = Mode::Close {
                    pane,
                    step: CloseStep::Force { action, message: e },
                };
            }
            (Pending::Diff { pane }, result) => {
                if let Mode::Diff(v) = &mut self.mode
                    && v.pane == pane
                {
                    match result {
                        Ok(Reply::Diff(report)) => v.set_report(report),
                        Ok(_) => v.busy = false,
                        Err(e) => {
                            v.busy = false;
                            self.set_status(e, true);
                        }
                    }
                }
            }
            (Pending::DiffAction { pane }, result) => {
                match result {
                    Ok(Reply::Text(t)) => self.set_status(t, false),
                    Ok(_) => {}
                    Err(e) => self.set_status(e, true),
                }
                if let Mode::Diff(v) = &mut self.mode
                    && v.pane == pane
                {
                    v.busy = true;
                    self.request(Command::Diff { pane: pane.clone() }, Pending::Diff { pane });
                }
            }
            (_, Err(e)) => self.set_status(e, true),
            (_, Ok(_)) => {}
        }
    }

    pub fn tick(&mut self) {
        let animated = self.panes.iter().any(|p| p.state == AgentState::Working);
        self.frame = self.frame.wrapping_add(1);
        // Spinners animate; waiting timers tick once a second.
        if animated || self.frame.is_multiple_of(10) {
            self.dirty = true;
        }
        if let Some((_, at, _)) = &self.status
            && at.elapsed() >= STATUS_FOR
        {
            self.status = None;
            self.dirty = true;
        }
    }
}

/// Puts the terminal in raw mode on the alternate screen with mouse and
/// bracketed paste reporting, and restores it on drop (and on panic).
struct TerminalGuard;

const ENTER: &[u8] = b"\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?2004h";
const LEAVE: &[u8] = b"\x1b[?2004l\x1b[?1006l\x1b[?1000l\x1b[?25h\x1b[?1049l";

fn restore_terminal() {
    let mut out = std::io::stdout();
    let _ = out.write_all(LEAVE);
    let _ = out.flush();
    let _ = crossterm::terminal::disable_raw_mode();
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        crossterm::terminal::enable_raw_mode().context("switching the terminal to raw mode")?;
        let mut out = std::io::stdout();
        out.write_all(ENTER)?;
        out.flush()?;
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            default(info);
        }));
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// The size a zoomed agent gets: the whole terminal under the top bar.
fn pane_size() -> Result<(u16, u16)> {
    let (w, h) = crossterm::terminal::size()?;
    let a = ui::layout(Rect::new(0, 0, w, h), true).body;
    Ok((a.width.max(2), a.height.max(1)))
}

pub async fn run(workspace: Option<String>) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!(
            "the corral UI needs a terminal; use `corral ls`, `corral read` and friends from scripts"
        );
    }
    if std::env::var_os(paths::PANE_ENV).is_some() {
        bail!(
            "already inside a corral pane; detach first (or unset {})",
            paths::PANE_ENV
        );
    }
    let config = Config::load()?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let mut app = App::new(&config, cwd)?;
    let conn = Conn::connect_or_start(&paths::socket_path()).await?;

    let exit = {
        let _guard = TerminalGuard::enter()?;
        event_loop(&mut app, conn, workspace).await
    };
    match exit? {
        Exit::Detached => {
            println!("[detached from corral; agents keep running — `corral` to reattach]")
        }
        Exit::ServerGone(reason) => println!("[corral: {reason}]"),
    }
    Ok(())
}

async fn event_loop(app: &mut App, conn: Conn, workspace: Option<String>) -> Result<Exit> {
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    term.clear()?;

    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::Builder::new()
        .name("stdin".into())
        .spawn(move || {
            let mut buf = vec![0u8; 16 * 1024];
            let stdin = std::io::stdin();
            loop {
                match stdin.lock().read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stdin_tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        })?;

    let (mut sink, mut stream) = conn.transport.split();
    let mut size = pane_size()?;
    let attach = ClientMsg::Attach {
        cols: size.0,
        rows: size.1,
        workspace: workspace.clone(),
    };
    sink.send(protocol::encode(&attach)?).await?;

    use tokio::signal::unix::{SignalKind, signal};
    let mut winch = signal(SignalKind::window_change())?;
    let mut redraw = tokio::time::interval(Duration::from_millis(16));
    redraw.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut anim = tokio::time::interval(Duration::from_millis(100));
    let mut checked_workspace = false;

    loop {
        tokio::select! {
            bytes = stdin_rx.recv() => match bytes {
                Some(b) => app.on_stdin(&b),
                None => return Ok(Exit::Detached),
            },
            frame = stream.next() => match frame {
                Some(Ok(f)) => {
                    let msg: ServerMsg = protocol::decode(&f)?;
                    app.on_server(msg);
                    if !checked_workspace && app.seen_state {
                        checked_workspace = true;
                        if let Some(ws) = &workspace {
                            match app.panes.iter().find(|p| &p.workspace == ws) {
                                Some(p) => app.selected = Some(p.id.clone()),
                                None => app.set_status(format!("no workspace named {ws}"), true),
                            }
                        }
                    }
                }
                Some(Err(e)) => return Err(e).context("reading from the server"),
                None => return Ok(Exit::ServerGone("server closed the connection".into())),
            },
            _ = winch.recv() => {
                app.dirty = true;
                let new = pane_size()?;
                if new != size {
                    size = new;
                    app.outbox.push(ClientMsg::Resize { cols: size.0, rows: size.1 });
                }
            }
            _ = redraw.tick() => {
                if app.dirty {
                    app.dirty = false;
                    term.draw(|f| ui::draw(f, app))?;
                }
            }
            _ = anim.tick() => app.tick(),
        }

        for msg in app.outbox.drain(..) {
            sink.send(protocol::encode(&msg)?).await?;
        }
        if !app.term_writes.is_empty() {
            let mut out = std::io::stdout();
            out.write_all(&app.term_writes)?;
            out.flush()?;
            app.term_writes.clear();
        }
        if let Some(exit) = app.exit.take() {
            let _ = sink.flush().await;
            return Ok(exit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: &str, state: AgentState) -> PaneInfo {
        PaneInfo {
            id: id.into(),
            name: id.into(),
            workspace: "w".into(),
            command: vec![],
            cwd: "/tmp".into(),
            agent: None,
            state,
            pid: None,
            exit_code: None,
            title: None,
            worktree: None,
            cols: 80,
            rows: 24,
            state_secs: 0,
        }
    }

    /// An app on the overview with three panes, p1 selected.
    fn app() -> App {
        let mut a = App::new(&Config::default(), "/tmp".into()).unwrap();
        a.body_area = Rect::new(0, 1, 140, 40);
        a.on_server(ServerMsg::State {
            panes: vec![
                pane("p1", AgentState::Idle),
                pane("p2", AgentState::Blocked),
                pane("p3", AgentState::Working),
            ],
            focus: None,
        });
        a
    }

    fn zoomed() -> App {
        let mut a = app();
        a.on_stdin(b"\r");
        a.on_server(ServerMsg::Screen {
            pane: "p1".into(),
            cols: 80,
            rows: 24,
            seq: 0,
            bytes: vec![],
        });
        a.outbox.clear();
        a
    }

    #[test]
    fn overview_keys_drive_corral_not_agents() {
        let mut a = app();
        assert_eq!(a.selected.as_deref(), Some("p1"));
        a.on_stdin(b"l");
        assert_eq!(a.selected.as_deref(), Some("p2"));
        a.on_stdin(b"\x1b[C");
        assert_eq!(a.selected.as_deref(), Some("p3"));
        // Nothing was typed into an agent.
        assert!(a.outbox.is_empty());
        a.on_stdin(b"\r");
        assert_eq!(
            a.outbox,
            vec![ClientMsg::Focus {
                pane: Some("p3".into())
            }]
        );
        assert_eq!(a.focus.as_deref(), Some("p3"));
    }

    #[test]
    fn zoomed_typing_is_forwarded_and_prefix_g_returns() {
        let mut a = zoomed();
        a.on_stdin(b"hi\r");
        assert_eq!(
            a.outbox,
            vec![ClientMsg::Input {
                pane: "p1".into(),
                bytes: b"hi\r".to_vec()
            }]
        );
        a.outbox.clear();
        a.on_stdin(b"\x02g");
        assert_eq!(a.outbox, vec![ClientMsg::Focus { pane: None }]);
        assert!(a.focus.is_none());
        assert_eq!(a.selected.as_deref(), Some("p1"));
    }

    #[test]
    fn prefix_commands_when_zoomed() {
        let mut a = zoomed();
        a.on_stdin(b"\x02j");
        assert_eq!(
            a.outbox,
            vec![ClientMsg::Focus {
                pane: Some("p2".into())
            }]
        );
        a.outbox.clear();
        a.on_stdin(b"\x023");
        assert_eq!(
            a.outbox,
            vec![ClientMsg::Focus {
                pane: Some("p3".into())
            }]
        );
        a.outbox.clear();
        // Prefix split from its key across reads.
        a.on_stdin(b"\x02");
        a.on_stdin(b"b");
        assert_eq!(
            a.outbox,
            vec![ClientMsg::Focus {
                pane: Some("p2".into())
            }]
        );
        a.outbox.clear();
        a.on_stdin(b"\x02\x02");
        assert_eq!(
            a.outbox,
            vec![ClientMsg::Input {
                pane: "p2".into(),
                bytes: vec![2]
            }]
        );
        a.outbox.clear();
        a.on_stdin(b"\x02d");
        assert_eq!(a.outbox, vec![ClientMsg::Detach]);
        assert_eq!(a.exit, Some(Exit::Detached));
    }

    #[test]
    fn q_detaches_from_the_overview() {
        let mut a = app();
        a.on_stdin(b"q");
        assert_eq!(a.exit, Some(Exit::Detached));
    }

    #[test]
    fn b_selects_the_next_blocked_agent() {
        let mut a = app();
        a.on_stdin(b"b");
        assert_eq!(a.selected.as_deref(), Some("p2"));
        assert!(a.outbox.is_empty());
    }

    #[test]
    fn new_agent_opens_zoomed() {
        let mut a = app();
        a.on_stdin(b"w");
        assert!(matches!(&a.mode, Mode::New(d) if d.worktree));
        a.on_stdin(b" --resume\r");
        let Some(ClientMsg::Command {
            cmd: Command::New(req),
            ..
        }) = a.outbox.first()
        else {
            panic!("{:?}", a.outbox);
        };
        assert_eq!(req.command, vec!["claude --resume"]);
        assert!(req.shell && req.worktree && req.focus);
        // The server focuses the new pane: the client follows.
        a.on_server(ServerMsg::State {
            panes: a.panes.clone(),
            focus: Some("p3".into()),
        });
        assert_eq!(a.focus.as_deref(), Some("p3"));
    }

    #[test]
    fn closed_zoomed_pane_returns_to_grid() {
        let mut a = zoomed();
        a.on_server(ServerMsg::State {
            panes: vec![pane("p2", AgentState::Idle)],
            focus: None,
        });
        assert!(a.focus.is_none() && a.parser.is_none());
        assert_eq!(a.selected.as_deref(), Some("p2"));
    }

    #[test]
    fn diff_needs_a_worktree_and_requests_one() {
        let mut a = app();
        a.on_stdin(b"d");
        assert!(matches!(a.mode, Mode::Normal));
        assert!(a.status().unwrap().0.contains("no worktree"));
        let mut panes = a.panes.clone();
        panes[0].worktree = Some(Default::default());
        a.on_server(ServerMsg::State { panes, focus: None });
        a.on_stdin(b"d");
        assert!(matches!(a.mode, Mode::Diff(_)));
        let ClientMsg::Command {
            id,
            cmd: Command::Diff { pane },
        } = &a.outbox[0]
        else {
            panic!()
        };
        assert_eq!(pane, "p1");
        let id = *id;
        a.outbox.clear();
        a.on_server(ServerMsg::Reply {
            id,
            result: Ok(Reply::Diff(protocol::DiffReport {
                uncommitted: 1,
                ..Default::default()
            })),
        });
        let Mode::Diff(v) = &a.mode else { panic!() };
        assert!(v.report.is_some() && !v.busy);
        // Commit from the diff view, then it refreshes.
        a.on_stdin(b"c\r");
        assert!(matches!(
            &a.outbox[0],
            ClientMsg::Command {
                cmd: Command::Commit { .. },
                ..
            }
        ));
        a.on_stdin(b"q");
        assert!(matches!(a.mode, Mode::Normal));
    }

    #[test]
    fn dirty_worktree_offers_force() {
        let mut a = app();
        a.kill("p1".into(), WorktreeAction::Remove, false);
        let ClientMsg::Command { id, .. } = a.outbox[0] else {
            panic!()
        };
        a.on_server(ServerMsg::Reply {
            id,
            result: Err(
                "…/x has uncommitted changes (2 files); commit them or force removal".into(),
            ),
        });
        assert!(matches!(
            &a.mode,
            Mode::Close {
                step: CloseStep::Force {
                    action: WorktreeAction::Remove,
                    ..
                },
                ..
            }
        ));
        a.outbox.clear();
        a.on_stdin(b"f");
        assert!(matches!(
            &a.outbox[0],
            ClientMsg::Command {
                cmd: Command::Kill {
                    force: true,
                    worktree: WorktreeAction::Remove,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn notifications_ring_the_bell() {
        let mut a = app();
        a.on_server(ServerMsg::Notify {
            pane: "p2".into(),
            state: AgentState::Blocked,
            message: "codex (p2) needs your input".into(),
        });
        let w = String::from_utf8(a.term_writes.clone()).unwrap();
        assert!(w.starts_with('\x07'));
        assert!(w.contains("\x1b]9;corral: codex (p2) needs your input\x07"));
    }

    #[test]
    fn opens_new_dialog_when_empty() {
        let mut a = App::new(&Config::default(), "/tmp".into()).unwrap();
        a.on_server(ServerMsg::State {
            panes: vec![],
            focus: None,
        });
        assert!(matches!(a.mode, Mode::New(_)));
    }

    #[test]
    fn clicking_cards() {
        let mut a = app();
        a.card_hits = vec![
            (Rect::new(0, 1, 40, 10), "p1".into()),
            (Rect::new(40, 1, 40, 10), "p2".into()),
        ];
        a.on_stdin(b"\x1b[<0;45;5M");
        assert_eq!(a.selected.as_deref(), Some("p2"));
        assert!(a.outbox.is_empty());
        a.on_stdin(b"\x1b[<0;45;5M");
        assert_eq!(
            a.outbox,
            vec![ClientMsg::Focus {
                pane: Some("p2".into())
            }]
        );
    }
}

//! The terminal UI client.
//!
//! Stdin goes to the terminal in raw mode and is forwarded byte-for-byte to
//! the focused pane, except for the prefix key, mouse events and corral's
//! dialogs. Up to four panes are on screen at once, tiled next to the
//! sidebar; each is mirrored locally in a vt100 parser (snapshot plus live
//! output) and drawn with tui-term. The layout is the client's: it tells the
//! server which panes it shows, at what size, with `View`.

pub mod conn;
pub mod images;
pub mod input;
pub mod notes;
pub mod select;
pub mod sidebar;
pub mod stories;
pub mod theme;
pub mod ui;
pub mod update;

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
use crate::integrate::{self, Agent, HookStatus};
use crate::paths;
use crate::protocol::{
    self, AgentState, ArchivedPane, ClientMsg, Command, NewPane, PaneId, PaneInfo, Reply,
    Restorable, ServerMsg, Viewport,
};
use conn::Conn;
use input::{Chunk, Key, Mouse, MouseKind, Splitter, decode_keys};
use notes::Notes;
use sidebar::Order;
use stories::Stories;
use ui::TextInput;

const STATUS_FOR: Duration = Duration::from_secs(6);
/// How long the new-release toast stays up.
const TOAST_FOR: Duration = Duration::from_secs(15);

/// The menu's rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
    Keys,
    Stories,
    Notes,
    Theme,
    Order,
    Notifications,
    Hooks(Agent),
    Detach,
    Exit,
}

pub const MENU: [MenuItem; 10] = [
    MenuItem::Keys,
    MenuItem::Stories,
    MenuItem::Notes,
    MenuItem::Theme,
    MenuItem::Order,
    MenuItem::Notifications,
    MenuItem::Hooks(Agent::Claude),
    MenuItem::Hooks(Agent::Codex),
    MenuItem::Detach,
    MenuItem::Exit,
];
const WHEEL_LINES: usize = 3;
/// Most panes on screen at once.
pub const MAX_TILES: usize = 4;

#[derive(Debug, Clone)]
pub enum Mode {
    Normal,
    Help,
    Scroll {
        offset: usize,
    },
    /// The menu, with the selected row.
    Menu {
        selected: usize,
    },
    /// Confirm installing an agent's hooks.
    Hooks(Agent),
    Rename {
        pane: PaneId,
        input: TextInput,
    },
    Close {
        pane: PaneId,
    },
    /// Top stories, over the panes (`App::stories` has the page).
    Stories,
    /// Notes, over the panes (`App::notes` has the page).
    Notes,
    /// Detach, or stop corral and every agent.
    Quit,
    /// Bring back the panes from before corral stopped?
    Restore,
    /// Archived panes, with the selected row.
    Archive {
        selected: usize,
    },
}

#[derive(Debug)]
enum Pending {
    New,
    Kill,
    Restore { discard: bool },
    Archive,
    Unarchive,
    Other,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Exit {
    Detached,
    /// The user stopped corral; this many agents were still running.
    Stopped(usize),
    ServerGone(String),
}

pub struct App {
    pub panes: Vec<PaneInfo>,
    /// When `panes` arrived (their `state_secs` count from then).
    pub state_at: Instant,
    /// The pane with the keyboard; always one of `tiles`.
    pub focus: Option<PaneId>,
    /// Panes on screen, in layout order.
    pub tiles: Vec<PaneId>,
    /// The layout before `^b o` kept only the focused pane.
    others: Vec<PaneId>,
    /// Local copies of the panes on screen.
    pub screens: HashMap<PaneId, vt100::Parser>,
    /// Generation of the last `View` sent, and what it said.
    view_generation: u64,
    sent_view: Option<(Option<PaneId>, Vec<Viewport>, bool)>,
    /// Panes from before the server stopped, waiting for a decision.
    pub restorable: Vec<Restorable>,
    /// "Later" was picked: don't ask again this time.
    restore_later: bool,
    /// Panes put away with `^b a`, newest first.
    pub archived: Vec<ArchivedPane>,
    /// Where the archive list's rows were drawn, for clicks.
    pub archive_hits: Vec<(Rect, usize)>,
    pub mode: Mode,
    status: Option<(String, Instant, bool)>,
    pub prefix_label: String,
    prefix: u8,
    pub awaiting_command: bool,
    pub sidebar_width: u16,
    /// The sidebar is down to its rail of agent numbers.
    pub sidebar_collapsed: bool,
    /// The whole terminal, as last measured.
    term_area: Rect,
    pub sidebar_area: Rect,
    pub pane_area: Rect,
    pub hits: Vec<sidebar::Hit>,
    /// How far the sidebar's spaces and agents are scrolled with the wheel;
    /// `None` follows the focus.
    pub sidebar_scroll: [Option<usize>; 2],
    pub frame: usize,
    pub scroll_max: usize,
    splitter: Splitter,
    /// Run in each new shell; empty for just the shell.
    default_command: String,
    pub order: Order,
    /// Ring the bell and send desktop notifications.
    pub notifications: bool,
    /// The top stories page's feeds and position.
    pub stories: Stories,
    /// Feeds to fetch; the run loop fetches them off the UI thread.
    pub fetches: Vec<stories::Source>,
    /// Look for a newer release once a day.
    pub update_check: bool,
    /// A newer release, and when its toast went up.
    update: Option<(String, Instant)>,
    /// The notes page's notes and position.
    pub notes: Notes,
    /// Show images pasted into agents.
    pub image_previews: bool,
    /// How images are drawn in this terminal, once asked (none in tests).
    pub picker: Option<ratatui_image::picker::Picker>,
    /// Images to read; the run loop reads them off the UI thread.
    pub image_loads: Vec<(PaneId, images::Source)>,
    /// Images pasted into each pane's prompt and not sent yet.
    pub previews: HashMap<PaneId, Vec<images::Preview>>,
    /// Where each preview's close button was drawn, for clicks.
    pub preview_hits: Vec<(Rect, PaneId, usize)>,
    /// The prefix was pressed on a page, which it closed; the key that
    /// opens that page.
    closed_page: Option<char>,
    /// Hook status per agent, read when the menu opens.
    pub hooks: Vec<(Agent, HookStatus)>,
    /// Where the menu's rows were drawn, for clicks.
    pub menu_hits: Vec<(Rect, usize)>,
    /// The config file settings changed in the menu are saved to (none in
    /// tests).
    save_to: Option<PathBuf>,
    cwd: PathBuf,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    seen_state: bool,
    pub outbox: Vec<ClientMsg>,
    /// Raw bytes for the outer terminal (bell, OSC 9).
    pub term_writes: Vec<u8>,
    /// Text selected with the mouse, highlighted until the next click or key.
    pub selection: Option<select::Selection>,
    pub exit: Option<Exit>,
    /// Asked the server to stop; its shutdown is expected.
    stopping: Option<usize>,
    pub dirty: bool,
}

impl App {
    pub fn new(config: &Config, cwd: PathBuf) -> Result<Self> {
        let prefix = config.prefix_byte()?;
        Ok(App {
            panes: Vec::new(),
            state_at: Instant::now(),
            focus: None,
            tiles: Vec::new(),
            others: Vec::new(),
            screens: HashMap::new(),
            view_generation: 0,
            sent_view: None,
            restorable: Vec::new(),
            restore_later: false,
            archived: Vec::new(),
            archive_hits: Vec::new(),
            mode: Mode::Normal,
            status: None,
            prefix_label: prefix_label(prefix),
            prefix,
            awaiting_command: false,
            sidebar_width: config.sidebar_width,
            sidebar_collapsed: config.sidebar_collapsed,
            term_area: Rect::default(),
            sidebar_area: Rect::default(),
            pane_area: Rect::default(),
            hits: Vec::new(),
            sidebar_scroll: [None, None],
            frame: 0,
            scroll_max: 0,
            splitter: Splitter::new(prefix),
            default_command: config.default_command.trim().to_string(),
            order: Order::from_config(&config.sidebar_order),
            notifications: config.notifications,
            stories: Stories::default(),
            fetches: Vec::new(),
            update_check: config.update_check,
            update: None,
            notes: Notes::new(None),
            image_previews: config.image_previews,
            picker: None,
            image_loads: Vec::new(),
            previews: HashMap::new(),
            preview_hits: Vec::new(),
            closed_page: None,
            hooks: Vec::new(),
            menu_hits: Vec::new(),
            save_to: None,
            cwd,
            next_id: 1,
            pending: HashMap::new(),
            seen_state: false,
            outbox: Vec::new(),
            term_writes: Vec::new(),
            selection: None,
            exit: None,
            stopping: None,
            dirty: true,
        })
    }

    pub fn status(&self) -> Option<(&str, bool)> {
        self.status
            .as_ref()
            .filter(|(_, at, _)| at.elapsed() < STATUS_FOR)
            .map(|(m, _, e)| (m.as_str(), *e))
    }

    /// The newer release to show a toast for, while it's up.
    pub fn update(&self) -> Option<&str> {
        self.update
            .as_ref()
            .filter(|(_, at)| at.elapsed() < TOAST_FOR)
            .map(|(v, _)| v.as_str())
    }

    /// The run loop's update check found a newer release.
    /// An image pasted into `pane` was read.
    pub fn on_image(&mut self, pane: PaneId, preview: images::Preview) {
        if self.panes.iter().any(|p| p.id == pane) {
            self.previews.entry(pane).or_default().push(preview);
            self.dirty = true;
        }
    }

    /// Watch what's typed into the focused pane: Ctrl-V in an agent may
    /// paste an image from the clipboard, and Enter sends the prompt along
    /// with the images in it.
    fn watch_typing(&mut self, bytes: &[u8]) {
        let Some(pane) = self.focus.clone() else {
            return;
        };
        if bytes.contains(&b'\r') {
            self.previews.remove(&pane);
        }
        if self.image_previews && images::is_paste_key(bytes) && self.focus_is_agent() {
            self.image_loads.push((pane, images::Source::Clipboard));
        }
    }

    /// A paste that's only image files (a drop on the terminal) previews them.
    fn watch_paste(&mut self, paste: &[u8]) {
        let Some(pane) = self.focus.clone() else {
            return;
        };
        if !self.image_previews || !self.focus_is_agent() {
            return;
        }
        let text = String::from_utf8_lossy(paste);
        let text = text
            .trim_start_matches("\x1b[200~")
            .trim_end_matches("\x1b[201~");
        for path in images::image_paths(text) {
            self.image_loads
                .push((pane.clone(), images::Source::File(path)));
        }
    }

    fn focus_is_agent(&self) -> bool {
        self.focused().is_some_and(|p| p.agent.is_some())
    }

    pub fn on_update(&mut self, version: String) {
        self.update = Some((version, Instant::now()));
        self.dirty = true;
    }

    fn set_status(&mut self, msg: impl Into<String>, error: bool) {
        self.status = Some((msg.into(), Instant::now(), error));
    }

    fn request(&mut self, cmd: Command, kind: Pending) {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, kind);
        self.outbox.push(ClientMsg::Command { id, cmd });
    }

    fn focused(&self) -> Option<&PaneInfo> {
        let id = self.focus.as_ref()?;
        self.panes.iter().find(|p| &p.id == id)
    }

    fn order(&self) -> Vec<PaneId> {
        sidebar::display_order(&self.panes, self.order)
            .into_iter()
            .map(|p| p.id.clone())
            .collect()
    }

    /// The focused pane's local screen.
    pub fn screen(&self) -> Option<&vt100::Parser> {
        self.screens.get(self.focus.as_ref()?)
    }

    fn screen_mut(&mut self) -> Option<&mut vt100::Parser> {
        self.screens.get_mut(self.focus.as_ref()?)
    }

    /// Show `pane` with the keyboard. A pane already on screen just gets
    /// the focus; otherwise it takes the focused tile's place. The server
    /// makes the same substitution when it moves the focus itself.
    fn show(&mut self, pane: Option<PaneId>) {
        match pane {
            None => self.tiles.clear(),
            Some(ref p) if self.tiles.contains(p) => {}
            Some(ref p) => match self
                .focus
                .as_ref()
                .and_then(|f| self.tiles.iter().position(|t| t == f))
            {
                Some(i) => self.tiles[i] = p.clone(),
                None => self.tiles = vec![p.clone()],
            },
        }
        self.focus = pane;
    }

    fn set_focus(&mut self, pane: PaneId) {
        // The sidebar follows the focus again.
        self.sidebar_scroll = [None, None];
        self.show(Some(pane));
        if matches!(self.mode, Mode::Scroll { .. } | Mode::Stories | Mode::Notes) {
            self.mode = Mode::Normal;
        }
    }

    /// The tiles' areas: `(pane, whole tile, terminal part)`.
    pub fn tile_rects(&self) -> Vec<(PaneId, Rect, Rect)> {
        let n = self.tiles.len();
        ui::tiles(self.pane_area, n)
            .into_iter()
            .zip(&self.tiles)
            .map(|(r, id)| (id.clone(), r, ui::tile_body(r, n)))
            .collect()
    }

    /// Tell the server about layout changes (new panes on screen, sizes,
    /// focus). Called after every input and server message.
    pub fn sync_view(&mut self) {
        if !self.seen_state || self.pane_area.is_empty() {
            return;
        }
        let view: Vec<Viewport> = self
            .tile_rects()
            .into_iter()
            .map(|(pane, _, body)| Viewport {
                pane,
                cols: body.width.max(2),
                rows: body.height.max(1),
            })
            .collect();
        // Pages drawn over the panes leave them unseen.
        let covered = matches!(self.mode, Mode::Stories | Mode::Notes);
        let now = (self.focus.clone(), view, covered);
        if self.sent_view.as_ref() == Some(&now) {
            return;
        }
        self.view_generation += 1;
        self.outbox.push(ClientMsg::View {
            generation: self.view_generation,
            focus: now.0.clone(),
            panes: now.1.clone(),
            covered,
        });
        self.sent_view = Some(now);
    }

    /// `^b s`: show another agent next to the ones on screen, preferring
    /// the ones that need you.
    fn split(&mut self) {
        if self.tiles.len() >= MAX_TILES {
            self.set_status(format!("up to {MAX_TILES} agents fit side by side"), false);
            return;
        }
        let next = self.order().into_iter().find(|id| !self.tiles.contains(id));
        let Some(next) = next else {
            self.set_status(
                format!(
                    "every agent is on screen; start another with {} n",
                    self.prefix_label
                ),
                false,
            );
            return;
        };
        let at = self
            .focus
            .as_ref()
            .and_then(|f| self.tiles.iter().position(|t| t == f))
            .map_or(self.tiles.len(), |i| i + 1);
        self.tiles.insert(at, next.clone());
        self.others.clear();
        self.set_focus(next);
    }

    /// `^b S`: take the focused pane off screen (the agent keeps running).
    fn hide(&mut self) {
        if self.tiles.len() < 2 {
            self.set_status("this is the only pane on screen", false);
            return;
        }
        let Some(i) = self
            .focus
            .as_ref()
            .and_then(|f| self.tiles.iter().position(|t| t == f))
        else {
            return;
        };
        self.tiles.remove(i);
        self.screens.retain(|id, _| self.tiles.contains(id));
        let next = self.tiles[i.min(self.tiles.len() - 1)].clone();
        self.set_focus(next);
    }

    /// `^b o`: only the focused pane on screen; again to bring the others
    /// back.
    fn only(&mut self) {
        if self.tiles.len() > 1 {
            self.others = std::mem::take(&mut self.tiles);
            self.tiles = self.focus.clone().into_iter().collect();
            self.screens.retain(|id, _| self.tiles.contains(id));
        } else if !self.others.is_empty() {
            let mut back = std::mem::take(&mut self.others);
            back.retain(|id| self.panes.iter().any(|p| &p.id == id));
            if let Some(f) = &self.focus
                && !back.contains(f)
            {
                back.insert(0, f.clone());
            }
            back.truncate(MAX_TILES);
            self.tiles = back;
        } else {
            self.set_status(
                format!("one pane on screen; {} s shows another", self.prefix_label),
                false,
            );
        }
    }

    /// `^b h` / `^b l` / `^b Tab`: move the keyboard between tiles.
    fn cycle_tile(&mut self, delta: isize) {
        let n = self.tiles.len();
        if n < 2 {
            return;
        }
        let cur = self
            .focus
            .as_ref()
            .and_then(|f| self.tiles.iter().position(|t| t == f))
            .unwrap_or(0) as isize;
        let next = self.tiles[(cur + delta).rem_euclid(n as isize) as usize].clone();
        self.set_focus(next);
    }

    fn cycle_focus(&mut self, delta: isize) {
        let order = self.order();
        if order.is_empty() {
            return;
        }
        let cur = self
            .focus
            .as_ref()
            .and_then(|f| order.iter().position(|p| p == f))
            .unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(order.len() as isize) as usize;
        self.set_focus(order[next].clone());
    }

    fn focus_next_blocked(&mut self) {
        let order = self.order();
        let cur = self
            .focus
            .as_ref()
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
                self.set_focus(id);
            }
            None => self.set_status("no agent is blocked", false),
        }
    }

    /// Directory a new agent should start in by default.
    fn default_dir(&self) -> PathBuf {
        match self.focused() {
            Some(p) => p.cwd.clone(),
            None => self.cwd.clone(),
        }
    }

    /// A request for a new pane in `cwd`: your shell, running the
    /// configured default command first if there is one.
    fn new_pane_request(&self, cwd: PathBuf) -> NewPane {
        let cmd = &self.default_command;
        NewPane {
            name: None,
            cwd: Some(cwd),
            workspace: None,
            command: if cmd.is_empty() {
                Vec::new()
            } else {
                vec![cmd.clone()]
            },
            shell: true,
            then_shell: true,
            focus: true,
        }
    }

    /// `^b n` / "+ new": a shell next to the focused agent's work.
    fn new_shell(&mut self) {
        let req = self.new_pane_request(self.default_dir());
        self.request(Command::New(req), Pending::New);
    }

    fn save_setting(&mut self, key: &str, value: impl Into<toml_edit::Value>) {
        if let Some(path) = &self.save_to
            && let Err(e) = crate::config::save_to(path, key, value)
        {
            self.set_status(format!("couldn't save {key}: {e:#}"), true);
        }
    }

    fn open_menu(&mut self) {
        self.hooks = [Agent::Claude, Agent::Codex]
            .map(|a| (a, integrate::status(a)))
            .to_vec();
        self.mode = Mode::Menu { selected: 0 };
    }

    /// The menu row's current value, as shown on the right.
    pub fn menu_value(&self, item: MenuItem) -> String {
        let p = &self.prefix_label;
        match item {
            MenuItem::Keys => format!("{p} ?"),
            MenuItem::Stories => format!("{p} t"),
            MenuItem::Notes => format!("{p} w"),
            MenuItem::Theme => format!("‹ {} ›", theme::get().name),
            MenuItem::Order => format!("‹ {} ›", self.order.label()),
            MenuItem::Notifications => {
                format!("‹ {} ›", if self.notifications { "on" } else { "off" })
            }
            MenuItem::Hooks(a) => match self.hooks.iter().find(|(x, _)| *x == a) {
                Some((_, HookStatus::Installed)) => "installed".into(),
                Some((_, HookStatus::Missing)) => "install…".into(),
                Some((_, HookStatus::NotFound)) | None => "not found".into(),
            },
            MenuItem::Detach => format!("{p} d"),
            MenuItem::Exit => format!("{p} q"),
        }
    }

    /// Recompute the sidebar and pane areas for `term_area`.
    fn relayout(&mut self) {
        let areas = ui::layout(self.term_area, self.sidebar_width, self.sidebar_collapsed);
        self.sidebar_area = areas.sidebar;
        self.pane_area = areas.pane;
    }

    /// Shrink the sidebar to its rail, or bring it back. The panes get the
    /// room (`sync_view` resizes them).
    fn toggle_sidebar(&mut self) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        self.relayout();
        self.save_setting("sidebar_collapsed", self.sidebar_collapsed);
    }

    fn toggle_order(&mut self) {
        self.order = self.order.toggled();
        self.save_setting("sidebar_order", self.order.config_value());
    }

    /// Activate a menu row; `step` is -1/+1 for ←/→ on a setting.
    fn menu_activate(&mut self, selected: usize, step: isize) {
        let stay = Mode::Menu { selected };
        match MENU[selected] {
            MenuItem::Keys => self.mode = Mode::Help,
            MenuItem::Stories => self.open_stories(),
            MenuItem::Notes => self.open_notes(),
            MenuItem::Theme => {
                let next = theme::cycle(step);
                theme::set(next);
                self.save_setting("theme", next.name);
                self.mode = stay;
            }
            MenuItem::Order => {
                self.toggle_order();
                self.mode = stay;
            }
            MenuItem::Notifications => {
                self.notifications = !self.notifications;
                self.save_setting("notifications", self.notifications);
                self.mode = stay;
            }
            MenuItem::Hooks(a) => match self.hooks.iter().find(|(x, _)| *x == a) {
                Some((_, HookStatus::Missing)) => self.mode = Mode::Hooks(a),
                Some((_, HookStatus::Installed)) => {
                    self.set_status(
                        format!(
                            "{} hooks are installed; `corral integrate {} --uninstall` removes them",
                            a.label(),
                            a.label()
                        ),
                        false,
                    );
                    self.mode = stay;
                }
                _ => {
                    self.set_status(
                        format!("{} isn't installed on this machine", a.label()),
                        false,
                    );
                    self.mode = stay;
                }
            },
            MenuItem::Detach => self.detach(),
            MenuItem::Exit => self.mode = Mode::Quit,
        }
    }

    fn menu_key(&mut self, selected: usize, key: Key) {
        let n = MENU.len();
        let is_setting = matches!(
            MENU[selected],
            MenuItem::Theme | MenuItem::Order | MenuItem::Notifications
        );
        match key {
            Key::Esc | Key::Ctrl('c') | Key::Char('q' | 'm') => {}
            Key::Up | Key::Char('k') | Key::BackTab => {
                self.mode = Mode::Menu {
                    selected: (selected + n - 1) % n,
                }
            }
            Key::Down | Key::Char('j') | Key::Tab => {
                self.mode = Mode::Menu {
                    selected: (selected + 1) % n,
                }
            }
            Key::Left | Key::Char('h') if is_setting => self.menu_activate(selected, -1),
            Key::Right | Key::Char('l') if is_setting => self.menu_activate(selected, 1),
            Key::Enter | Key::Char(' ') => self.menu_activate(selected, 1),
            _ => self.mode = Mode::Menu { selected },
        }
    }

    /// A click while a dialog is open: the menu's rows work, and clicking
    /// elsewhere closes the menu.
    fn dialog_mouse(&mut self, m: Mouse) {
        if matches!(self.mode, Mode::Stories) {
            self.stories_mouse(m);
            return;
        }
        if matches!(self.mode, Mode::Notes) {
            self.notes_mouse(m);
            return;
        }
        if !matches!(m.kind, MouseKind::LeftDown) {
            return;
        }
        if let Mode::Menu { .. } = self.mode {
            match self
                .menu_hits
                .iter()
                .find(|(r, _)| r.contains((m.x, m.y).into()))
            {
                Some(&(_, i)) => self.menu_activate(i, 1),
                None => self.mode = Mode::Normal,
            }
        }
        // A row of the archive brings that pane back.
        if let Mode::Archive { .. } = self.mode {
            match self
                .archive_hits
                .iter()
                .find(|(r, _)| r.contains((m.x, m.y).into()))
            {
                Some(&(_, i)) => {
                    self.mode = Mode::Normal;
                    self.unarchive(i);
                }
                None => self.mode = Mode::Normal,
            }
        }
    }

    fn decide_restore(&mut self, discard: bool) {
        self.request(Command::Restore { discard }, Pending::Restore { discard });
    }

    /// `^b t`: the top stories page, fetching its feed if it's stale.
    fn open_stories(&mut self) {
        self.mode = Mode::Stories;
        self.fetch_stories(false);
    }

    fn fetch_stories(&mut self, force: bool) {
        if let Some(source) = self.stories.want(force) {
            self.fetches.push(source);
            self.dirty = true;
        }
    }

    /// A feed arrived (or failed) from the run loop's worker.
    pub fn on_stories(
        &mut self,
        source: stories::Source,
        result: Result<Vec<stories::Story>, String>,
    ) {
        self.stories.loaded(source, result);
        self.dirty = true;
    }

    fn open_link(&mut self, url: String) {
        match stories::open_in_browser(&url) {
            Ok(()) => self.set_status(format!("opened {url}"), false),
            Err(e) => self.set_status(format!("{e:#}"), true),
        }
    }

    fn stories_key(&mut self, key: Key) {
        let page = self.stories.visible.max(1) as isize;
        match key {
            Key::Esc | Key::Ctrl('c') | Key::Char('q') => return,
            Key::Down | Key::Char('j') => self.stories.move_by(1),
            Key::Up | Key::Char('k') => self.stories.move_by(-1),
            Key::PageDown | Key::Ctrl('d') | Key::Char(' ') => self.stories.move_by(page),
            Key::PageUp | Key::Ctrl('u') => self.stories.move_by(-page),
            Key::Home | Key::Char('g') => self.stories.move_by(isize::MIN / 2),
            Key::End | Key::Char('G') => self.stories.move_by(isize::MAX / 2),
            Key::Enter | Key::Char('o') => {
                if let Some(s) = self.stories.current() {
                    self.open_link(s.link().to_string());
                }
            }
            Key::Char('c') => {
                if let Some(s) = self.stories.current() {
                    self.open_link(s.discussion.clone());
                }
            }
            Key::Tab | Key::BackTab | Key::Left | Key::Right | Key::Char('h' | 'l') => {
                self.stories.switch(self.stories.source.other());
                self.fetch_stories(false);
            }
            Key::Char('r') => self.fetch_stories(true),
            _ => {}
        }
        self.mode = Mode::Stories;
    }

    fn stories_mouse(&mut self, m: Mouse) {
        let in_sidebar = m.x < self.sidebar_area.x + self.sidebar_area.width;
        match m.kind {
            MouseKind::LeftDown if in_sidebar => {
                // Picking an agent or starting one leaves the page.
                let leaves = self.hits.iter().any(|h| {
                    h.contains(m.x, m.y)
                        && matches!(h.click, sidebar::Click::Focus(_) | sidebar::Click::New)
                });
                if leaves {
                    self.mode = Mode::Normal;
                }
                self.on_mouse(m);
            }
            MouseKind::LeftDown => match self.stories.click_at(m.x, m.y) {
                Some(stories::Click::Source(s)) => {
                    self.stories.switch(s);
                    self.fetch_stories(false);
                }
                Some(stories::Click::Open(i)) => {
                    self.stories.selected = i;
                    if let Some(url) = self.stories.story(i).map(|s| s.link().to_string()) {
                        self.open_link(url);
                    }
                }
                Some(stories::Click::Comments(i)) => {
                    self.stories.selected = i;
                    if let Some(url) = self.stories.story(i).map(|s| s.discussion.clone()) {
                        self.open_link(url);
                    }
                }
                None => {}
            },
            MouseKind::WheelDown if !in_sidebar => self.stories.scroll_by(1),
            MouseKind::WheelUp if !in_sidebar => self.stories.scroll_by(-1),
            _ => {}
        }
    }

    /// `^b w`: the notes page.
    fn open_notes(&mut self) {
        self.mode = Mode::Notes;
        self.notes.open();
    }

    fn notes_key(&mut self, key: Key) {
        if let notes::Outcome::Stay = self.notes.key(key) {
            self.mode = Mode::Notes;
        }
    }

    fn notes_mouse(&mut self, m: Mouse) {
        let in_sidebar = m.x < self.sidebar_area.x + self.sidebar_area.width;
        match m.kind {
            MouseKind::LeftDown if in_sidebar => {
                // Picking an agent or starting one leaves the page.
                let leaves = self.hits.iter().any(|h| {
                    h.contains(m.x, m.y)
                        && matches!(h.click, sidebar::Click::Focus(_) | sidebar::Click::New)
                });
                if leaves {
                    self.mode = Mode::Normal;
                }
                self.on_mouse(m);
            }
            MouseKind::LeftDown => self.notes.click(m.x, m.y),
            MouseKind::WheelDown if !in_sidebar => self.notes.wheel(m.x, m.y, 1),
            MouseKind::WheelUp if !in_sidebar => self.notes.wheel(m.x, m.y, -1),
            _ => {}
        }
    }

    fn open_close(&mut self) {
        let Some(p) = self.focused() else { return };
        self.mode = Mode::Close { pane: p.id.clone() };
    }

    /// `^b a`: put the focused agent away, to bring back later.
    fn archive(&mut self, pane: PaneId) {
        self.request(Command::Archive { pane }, Pending::Archive);
    }

    /// `^b A`: the archived panes.
    fn open_archive(&mut self) {
        if self.archived.is_empty() {
            self.set_status(
                format!(
                    "nothing archived; {} a archives the focused agent",
                    self.prefix_label
                ),
                false,
            );
            return;
        }
        self.mode = Mode::Archive { selected: 0 };
    }

    fn unarchive(&mut self, selected: usize) {
        if let Some(a) = self.archived.get(selected) {
            self.request(Command::Unarchive { id: a.id }, Pending::Unarchive);
        }
    }

    fn archive_key(&mut self, selected: usize, key: Key) {
        let n = self.archived.len();
        if n == 0 {
            return;
        }
        let selected = selected.min(n - 1);
        match key {
            Key::Esc | Key::Ctrl('c') | Key::Char('q') => {}
            Key::Up | Key::Char('k') | Key::BackTab => {
                self.mode = Mode::Archive {
                    selected: (selected + n - 1) % n,
                }
            }
            Key::Down | Key::Char('j') | Key::Tab => {
                self.mode = Mode::Archive {
                    selected: (selected + 1) % n,
                }
            }
            Key::Enter | Key::Char(' ') => self.unarchive(selected),
            Key::Char('x') | Key::Delete | Key::Backspace => {
                let id = self.archived[selected].id;
                self.request(Command::ForgetArchived { id }, Pending::Other);
                // Stays open on the next one; the state update closes it
                // when that was the last.
                self.mode = Mode::Archive {
                    selected: selected.min(n.saturating_sub(2)),
                };
            }
            _ => self.mode = Mode::Archive { selected },
        }
    }

    fn kill(&mut self, pane: PaneId) {
        self.request(Command::Kill { pane }, Pending::Kill);
    }

    fn detach(&mut self) {
        self.outbox.push(ClientMsg::Detach);
        self.exit = Some(Exit::Detached);
    }

    /// Agents whose processes are still running.
    pub fn running_agents(&self) -> usize {
        self.panes
            .iter()
            .filter(|p| p.state != AgentState::Exited)
            .count()
    }

    /// Stop the server and every agent it runs.
    fn stop_corral(&mut self) {
        self.stopping = Some(self.running_agents());
        self.set_status("stopping corral…", false);
        self.request(Command::KillServer, Pending::Other);
    }

    fn forward(&mut self, bytes: &[u8], is_paste: bool) {
        self.selection = None;
        let Some(pane) = self.focus.clone() else {
            return;
        };
        let (bp, app_cursor) = self.screen().map_or((false, false), |p| {
            let s = p.screen();
            (s.bracketed_paste(), s.application_cursor())
        });
        let bytes = input::adapt_for_pane(bytes, bp, app_cursor, is_paste);
        if !bytes.is_empty() {
            self.outbox.push(ClientMsg::Input { pane, bytes });
        }
    }

    fn prefix_command(&mut self, key: Key) {
        let closed_page = self.closed_page.take();
        match key {
            // `^b t` on the page closes it (the prefix already did).
            Key::Char(c) if closed_page == Some(c) => {}
            Key::Char('t') => self.open_stories(),
            Key::Char('w') => self.open_notes(),
            Key::Char('n') => self.new_shell(),
            Key::Char('m') => self.open_menu(),
            Key::Char('j') | Key::Down => self.cycle_focus(1),
            Key::Char('k') | Key::Up => self.cycle_focus(-1),
            Key::Char(c @ '1'..='9') => {
                let i = c as usize - '1' as usize;
                if let Some(p) = self.panes.get(i) {
                    let id = p.id.clone();
                    self.set_focus(id);
                }
            }
            Key::Char('b') => self.focus_next_blocked(),
            Key::Char('s') => self.split(),
            Key::Char('S') => self.hide(),
            Key::Char('o') => self.only(),
            Key::Char('c') => self.toggle_sidebar(),
            Key::Char('h') | Key::Left | Key::BackTab => self.cycle_tile(-1),
            Key::Char('l') | Key::Right | Key::Tab => self.cycle_tile(1),
            Key::Char('x') => self.open_close(),
            Key::Char('a') => {
                if let Some(p) = self.focused() {
                    let id = p.id.clone();
                    self.archive(id);
                }
            }
            Key::Char('A') => self.open_archive(),
            Key::Char('r') => {
                if let Some(p) = self.focused() {
                    self.mode = Mode::Rename {
                        pane: p.id.clone(),
                        input: TextInput::new(&p.name),
                    };
                }
            }
            Key::Char('d') => self.detach(),
            Key::Char('q') => self.mode = Mode::Quit,
            Key::Char('[') => self.enter_scroll(0),
            Key::Char('?') => self.mode = Mode::Help,
            _ => {}
        }
    }

    fn scroll_limit(&mut self) -> usize {
        let Some(p) = self.screen_mut() else {
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
        if max == 0 && offset == 0 && self.screen().is_some() {
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
        let page = self.pane_area.height.max(2) as isize - 1;
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

    /// The pane drawn at a screen position.
    fn tile_at(&self, x: u16, y: u16) -> Option<PaneId> {
        self.tile_rects()
            .into_iter()
            .find(|(_, r, _)| r.contains((x, y).into()))
            .map(|(id, ..)| id)
    }

    /// The bytes a wheel notch sends to the focused pane's program, when
    /// it takes the wheel itself.
    fn wheel_bytes(&self, m: Mouse) -> Option<Vec<u8>> {
        let focus = self.focus.as_ref()?;
        let (_, _, body) = self.tile_rects().into_iter().find(|(id, ..)| id == focus)?;
        if !body.contains((m.x, m.y).into()) {
            return None;
        }
        let up = m.kind == MouseKind::WheelUp;
        let screen = self.screen()?.screen();
        input::wheel_for_pane(screen, up, m.x - body.x, m.y - body.y, WHEEL_LINES)
    }

    /// The cell under the mouse in a pane's terminal, if it's over one.
    fn cell_at(&self, x: u16, y: u16) -> Option<(PaneId, (u16, u16))> {
        self.tile_rects()
            .into_iter()
            .find(|(_, _, body)| body.contains((x, y).into()))
            .map(|(id, _, body)| (id, (y - body.y, x - body.x)))
    }

    /// Drag to select inside a pane; letting go copies. Returns true when
    /// the event was part of a selection.
    fn select_mouse(&mut self, m: Mouse) -> bool {
        match m.kind {
            MouseKind::LeftDown => {
                self.selection = None;
                if matches!(self.mode, Mode::Normal | Mode::Scroll { .. })
                    && let Some((pane, at)) = self.cell_at(m.x, m.y)
                {
                    self.selection = Some(select::Selection::new(pane, at));
                }
                false
            }
            MouseKind::Drag => {
                let Some(s) = self.selection.as_ref().filter(|s| s.dragging) else {
                    return false;
                };
                // Stay inside the pane the drag started in.
                let Some((_, _, body)) =
                    self.tile_rects().into_iter().find(|(id, ..)| *id == s.pane)
                else {
                    return true;
                };
                let col = m.x.clamp(body.x, body.x + body.width.saturating_sub(1)) - body.x;
                let row = m.y.clamp(body.y, body.y + body.height.saturating_sub(1)) - body.y;
                if let Some(s) = self.selection.as_mut() {
                    s.head = (row, col);
                }
                true
            }
            MouseKind::LeftUp => {
                let Some(s) = self.selection.as_mut().filter(|s| s.dragging) else {
                    return false;
                };
                s.dragging = false;
                if s.is_empty() {
                    self.selection = None;
                } else {
                    self.copy_selection();
                }
                true
            }
            _ => false,
        }
    }

    fn copy_selection(&mut self) {
        let Some(s) = &self.selection else { return };
        let Some(parser) = self.screens.get(&s.pane) else {
            return;
        };
        let text = s.text(parser.screen());
        if text.trim().is_empty() {
            return;
        }
        self.term_writes.extend(select::osc52(&text));
        select::copy_locally(&text);
        let n = text.chars().count();
        self.set_status(
            format!("copied {n} character{}", if n == 1 { "" } else { "s" }),
            false,
        );
    }

    fn on_mouse(&mut self, m: Mouse) {
        let in_sidebar = m.x < self.sidebar_area.x + self.sidebar_area.width;
        // A preview's ✕ puts it away (the image stays in the prompt).
        if m.kind == MouseKind::LeftDown
            && let Some((_, pane, i)) = self
                .preview_hits
                .iter()
                .find(|(r, ..)| r.contains((m.x, m.y).into()))
                .cloned()
        {
            if let Some(list) = self.previews.get_mut(&pane)
                && i < list.len()
            {
                list.remove(i);
            }
            return;
        }
        if !in_sidebar && self.select_mouse(m) {
            return;
        }
        if matches!(m.kind, MouseKind::WheelUp | MouseKind::WheelDown) {
            self.selection = None;
            // The wheel scrolls the half of the sidebar it's over.
            if in_sidebar {
                let half = self.hits.iter().find_map(|h| match h.click {
                    sidebar::Click::Scroll { half, offset, max } if h.contains(m.x, m.y) => {
                        Some((half, offset, max))
                    }
                    _ => None,
                });
                if let Some((half, offset, max)) = half {
                    let offset = if m.kind == MouseKind::WheelUp {
                        offset.saturating_sub(2)
                    } else {
                        (offset + 2).min(max)
                    };
                    self.sidebar_scroll[half as usize] = Some(offset);
                }
                return;
            }
        }
        // Clicking or scrolling over another tile moves the keyboard there.
        if !in_sidebar
            && matches!(
                m.kind,
                MouseKind::LeftDown | MouseKind::WheelUp | MouseKind::WheelDown
            )
            && let Some(id) = self.tile_at(m.x, m.y)
            && self.focus.as_ref() != Some(&id)
        {
            self.set_focus(id);
        }
        // Programs that read the mouse, or run full screen (most agents),
        // scroll themselves; corral's scrollback is for the rest.
        if !in_sidebar
            && matches!(m.kind, MouseKind::WheelUp | MouseKind::WheelDown)
            && matches!(self.mode, Mode::Normal)
            && let Some(bytes) = self.wheel_bytes(m)
        {
            if let Some(pane) = self.focus.clone() {
                self.outbox.push(ClientMsg::Input { pane, bytes });
            }
            return;
        }
        match m.kind {
            MouseKind::LeftDown if in_sidebar => {
                let click = self
                    .hits
                    .iter()
                    .find(|h| h.contains(m.x, m.y))
                    .map(|h| h.click.clone());
                match click {
                    Some(sidebar::Click::Focus(id)) => self.set_focus(id),
                    Some(sidebar::Click::Scroll { .. }) => {}
                    Some(sidebar::Click::Space(name)) => {
                        if let Some(p) = sidebar::space_pick(&self.panes, &name) {
                            let id = p.id.clone();
                            self.set_focus(id);
                        }
                    }
                    Some(sidebar::Click::New) => self.new_shell(),
                    Some(sidebar::Click::Menu) => self.open_menu(),
                    Some(sidebar::Click::Exit) => self.mode = Mode::Quit,
                    Some(sidebar::Click::Order) => self.toggle_order(),
                    Some(sidebar::Click::Stories) => self.open_stories(),
                    Some(sidebar::Click::Notes) => self.open_notes(),
                    Some(sidebar::Click::Collapse) => self.toggle_sidebar(),
                    Some(sidebar::Click::Archive(id)) => self.archive(id),
                    Some(sidebar::Click::Archived) => self.open_archive(),
                    None => {}
                }
            }
            MouseKind::WheelUp
                if !in_sidebar && matches!(self.mode, Mode::Normal | Mode::Scroll { .. }) =>
            {
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
            Mode::Quit => match key {
                Key::Char('d' | 'q') | Key::Enter => self.detach(),
                Key::Char('s' | 'S') => self.stop_corral(),
                Key::Esc | Key::Ctrl('c') => {}
                _ => self.mode = Mode::Quit,
            },
            Mode::Menu { selected } => self.menu_key(selected, key),
            Mode::Archive { selected } => self.archive_key(selected, key),
            Mode::Restore => match key {
                Key::Enter | Key::Char('y' | 'Y' | 'r') => self.decide_restore(false),
                Key::Char('n' | 'N' | 'f') => self.decide_restore(true),
                Key::Esc | Key::Ctrl('c') | Key::Char('l') => {
                    self.restore_later = true;
                    self.set_status("bring them back later with `corral restore`", false);
                    if self.panes.is_empty() {
                        self.new_shell();
                    }
                }
                _ => self.mode = Mode::Restore,
            },
            Mode::Stories => self.stories_key(key),
            Mode::Notes => self.notes_key(key),
            Mode::Hooks(agent) => match key {
                Key::Char('y' | 'Y') | Key::Enter => match integrate::install_quietly(agent) {
                    Ok(msg) => self.set_status(msg, false),
                    Err(e) => self.set_status(format!("{e:#}"), true),
                },
                Key::Esc | Key::Ctrl('c') | Key::Char('n' | 'N' | 'q') => {}
                _ => self.mode = Mode::Hooks(agent),
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
            Mode::Close { pane } => match key {
                Key::Char('y' | 'Y' | 'x') | Key::Enter => self.kill(pane),
                Key::Esc | Key::Ctrl('c') | Key::Char('n' | 'N') => {}
                _ => self.mode = Mode::Close { pane },
            },
            m @ (Mode::Normal | Mode::Scroll { .. }) => self.mode = m,
        }
    }

    fn paste_into_dialog(&mut self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
        let text = text
            .trim_start_matches("\x1b[200~")
            .trim_end_matches("\x1b[201~");
        if let Mode::Notes = self.mode {
            self.notes.paste(text);
            return;
        }
        let text = text.replace(['\r', '\n'], " ");
        if let Mode::Rename { input, .. } = &mut self.mode {
            input.insert_str(&text);
        }
    }

    pub fn on_stdin(&mut self, bytes: &[u8]) {
        self.dirty = true;
        for chunk in self.splitter.feed(bytes) {
            let in_dialog = !matches!(self.mode, Mode::Normal | Mode::Scroll { .. });
            match chunk {
                Chunk::Mouse(m) => {
                    if in_dialog {
                        self.dialog_mouse(m);
                    } else {
                        self.on_mouse(m);
                    }
                }
                Chunk::Paste(p) => {
                    if in_dialog {
                        self.paste_into_dialog(&p);
                    } else {
                        self.awaiting_command = false;
                        self.mode = Mode::Normal;
                        self.watch_paste(&p);
                        self.forward(&p, true);
                    }
                }
                Chunk::Prefix if in_dialog => match self.mode {
                    Mode::Help => self.mode = Mode::Normal,
                    // The page steps aside for corral's keys.
                    Mode::Stories | Mode::Notes => {
                        self.closed_page = Some(if let Mode::Notes = self.mode {
                            'w'
                        } else {
                            't'
                        });
                        self.mode = Mode::Normal;
                        self.awaiting_command = true;
                    }
                    _ => {}
                },
                Chunk::Prefix => {
                    if self.awaiting_command {
                        // Prefix twice sends it to the pane.
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
                        let mut keys = decode_keys(&b).into_iter();
                        if let Some(k) = keys.next() {
                            self.prefix_command(k);
                        }
                        // Keys typed right after a command that opened a
                        // dialog belong to that dialog.
                        for k in keys {
                            if matches!(self.mode, Mode::Normal | Mode::Scroll { .. }) {
                                break;
                            }
                            self.dialog_key(k);
                        }
                    } else if matches!(self.mode, Mode::Scroll { .. }) {
                        for k in decode_keys(&b) {
                            self.scroll_key(k);
                        }
                    } else {
                        self.watch_typing(&b);
                        self.forward(&b, false);
                    }
                }
            }
        }
        self.sync_view();
    }

    pub fn on_server(&mut self, msg: ServerMsg) {
        self.dirty = true;
        match msg {
            ServerMsg::State {
                panes,
                focus,
                view_generation,
                restorable,
                archived,
            } => {
                self.restorable = restorable;
                self.archived = archived;
                if let Mode::Archive { selected } = &mut self.mode {
                    match self.archived.len() {
                        0 => self.mode = Mode::Normal,
                        n => *selected = (*selected).min(n - 1),
                    }
                }
                if self.restorable.is_empty() {
                    // Decided (here or by another client).
                    if matches!(self.mode, Mode::Restore) {
                        self.mode = Mode::Normal;
                    }
                } else if !self.restore_later && matches!(self.mode, Mode::Normal) {
                    self.mode = Mode::Restore;
                }
                self.panes = panes;
                self.state_at = Instant::now();
                let exists = |id: &PaneId| self.panes.iter().any(|p| &p.id == id);
                self.tiles.retain(|id| exists(id));
                self.previews.retain(|id, _| exists(id));
                if view_generation == self.view_generation {
                    // The server has seen our latest layout, so its focus is
                    // current (it may have moved it: a new or closed pane).
                    self.show(focus);
                } else if self.focus.as_ref().is_none_or(|f| !self.tiles.contains(f)) {
                    // Stale; the server will catch up with what we sent.
                    self.focus = self.tiles.first().cloned();
                }
                self.screens.retain(|id, _| self.tiles.contains(id));
                if !self.seen_state {
                    self.seen_state = true;
                    // Nothing running yet (and nothing to bring back): start
                    // with a shell.
                    if self.panes.is_empty() && self.restorable.is_empty() {
                        self.new_shell();
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
                if self.focus.as_ref() == Some(&pane) && matches!(self.mode, Mode::Scroll { .. }) {
                    self.mode = Mode::Normal;
                }
                self.screens.insert(pane, p);
            }
            ServerMsg::Output { pane, bytes, .. } => {
                if let Some(p) = self.screens.get_mut(&pane) {
                    p.process(&bytes);
                }
            }
            ServerMsg::Notify { message, .. } => {
                if self.notifications {
                    self.term_writes.push(0x07);
                    let clean: String = message.chars().filter(|c| !c.is_control()).collect();
                    self.term_writes
                        .extend_from_slice(format!("\x1b]9;corral: {clean}\x07").as_bytes());
                }
                self.set_status(message, false);
            }
            ServerMsg::Reply { id, result } => {
                let kind = self.pending.remove(&id).unwrap_or(Pending::Other);
                self.on_reply(kind, result);
            }
            ServerMsg::Shutdown { reason } => {
                self.exit = Some(match self.stopping {
                    Some(n) => Exit::Stopped(n),
                    None => Exit::ServerGone(reason),
                });
            }
            ServerMsg::Hello { .. } => {}
        }
        self.sync_view();
    }

    fn on_reply(&mut self, kind: Pending, result: Result<Reply, String>) {
        match (kind, result) {
            (Pending::Restore { discard }, Ok(Reply::Text(t))) => {
                self.set_status(t, false);
                if discard && self.panes.is_empty() {
                    self.new_shell();
                }
            }
            (Pending::Restore { .. }, Err(e)) => self.set_status(e, true),
            (Pending::Archive, Ok(Reply::Text(t))) => self.set_status(
                format!("{t} · {} A brings it back", self.prefix_label),
                false,
            ),
            (Pending::Unarchive, Ok(Reply::Created { pane })) => {
                if matches!(self.mode, Mode::Archive { .. }) {
                    self.mode = Mode::Normal;
                }
                self.set_status(format!("brought back {}", pane.name), false);
            }
            (Pending::New, Ok(Reply::Created { .. })) => self.status = None,
            (Pending::Kill, Ok(Reply::Text(t))) => self.set_status(t, false),
            (_, Err(e)) => self.set_status(e, true),
            (_, Ok(_)) => {}
        }
    }

    /// Time `p` has spent in its current state.
    pub fn age(&self, p: &PaneInfo) -> Duration {
        Duration::from_secs(p.state_secs) + self.state_at.elapsed()
    }

    pub fn tick(&mut self) {
        let working = self.panes.iter().any(|p| p.state == AgentState::Working);
        self.frame = self.frame.wrapping_add(1);
        // Spinners animate; waiting timers in the sidebar tick each second.
        if working || self.frame.is_multiple_of(10) {
            self.dirty = true;
        }
        // An open page keeps its feed fresh.
        if matches!(self.mode, Mode::Stories) {
            self.fetch_stories(false);
        }
        self.notes.autosave(matches!(self.mode, Mode::Notes));
        if let Some((_, at, _)) = &self.status
            && at.elapsed() >= STATUS_FOR
        {
            self.status = None;
            self.dirty = true;
        }
        if let Some((_, at)) = &self.update
            && at.elapsed() >= TOAST_FOR
        {
            self.update = None;
            self.dirty = true;
        }
    }
}

/// Puts the terminal in raw mode on the alternate screen with mouse and
/// bracketed paste reporting, and restores it on drop (and on panic).
struct TerminalGuard;

// Mouse: 1000 clicks and the wheel, 1002 drags (for selecting text), 1006
// the SGR encoding.
const ENTER: &[u8] = b"\x1b[?1049h\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?2004h";
const LEAVE: &[u8] = b"\x1b[?2004l\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?25h\x1b[?1049l";

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

/// Lay out the screen for the terminal's current size; returns the size of
/// the area for panes.
fn fit_terminal(app: &mut App) -> Result<(u16, u16)> {
    let (w, h) = crossterm::terminal::size()?;
    app.term_area = Rect::new(0, 0, w, h);
    app.relayout();
    Ok((app.pane_area.width.max(2), app.pane_area.height.max(1)))
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
    theme::set(theme::by_name(&config.theme)?);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let mut app = App::new(&config, cwd)?;
    app.save_to = Some(paths::config_path());
    app.notes = Notes::new(Some(paths::notes_dir()));
    let conn = Conn::connect_or_start(&paths::socket_path()).await?;

    let exit = {
        let _guard = TerminalGuard::enter()?;
        event_loop(&mut app, conn, workspace).await
    };
    // However the UI ended, notes being typed are kept.
    app.notes.save_all();
    match exit? {
        Exit::Detached => {
            println!("[detached from corral; agents keep running — `corral` to reattach]")
        }
        Exit::Stopped(n) => println!(
            "[corral stopped; {n} {} ended]",
            if n == 1 { "agent" } else { "agents" }
        ),
        Exit::ServerGone(reason) => println!("[corral: {reason}]"),
    }
    Ok(())
}

async fn event_loop(app: &mut App, conn: Conn, workspace: Option<String>) -> Result<Exit> {
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    // Not `term.clear()`, which asks the terminal where its cursor is and
    // fails when it doesn't say; a new `Terminal` has nothing to forget.
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
    )?;

    if app.image_previews {
        app.picker = Some(images::picker());
    }
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
    let mut size = fit_terminal(app)?;
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
    // Story feeds come back from blocking fetches on worker threads.
    let (stories_tx, mut stories_rx) = mpsc::unbounded_channel();
    // So does the daily look for a newer release; a failed one stays quiet.
    let (update_tx, mut update_rx) = mpsc::unbounded_channel();
    // And pasted images, read and encoded off the UI thread too.
    let (image_tx, mut image_rx) = mpsc::unbounded_channel();
    if app.update_check {
        tokio::task::spawn_blocking(move || match update::check() {
            Ok(Some(version)) => {
                let _ = update_tx.send(version);
            }
            Ok(None) => {}
            Err(e) => tracing::debug!("update check: {e:#}"),
        });
    }

    loop {
        tokio::select! {
            Some((source, result)) = stories_rx.recv() => app.on_stories(source, result),
            Some(version) = update_rx.recv() => app.on_update(version),
            Some((pane, preview)) = image_rx.recv() => app.on_image(pane, preview),
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
                        if let Some(ws) = &workspace
                            && !app.panes.iter().any(|p| &p.workspace == ws)
                        {
                            app.set_status(format!("no workspace named {ws}"), true);
                        }
                    }
                }
                Some(Err(e)) => return Err(e).context("reading from the server"),
                None => return Ok(Exit::ServerGone("server closed the connection".into())),
            },
            _ = winch.recv() => {
                app.dirty = true;
                let new = fit_terminal(app)?;
                if new != size {
                    size = new;
                    app.outbox.push(ClientMsg::Resize { cols: size.0, rows: size.1 });
                    app.sync_view();
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
        for source in app.fetches.drain(..) {
            let tx = stories_tx.clone();
            tokio::task::spawn_blocking(move || {
                let result = stories::fetch(source).map_err(|e| format!("{e:#}"));
                let _ = tx.send((source, result));
            });
        }
        if let Some(picker) = &app.picker {
            for (pane, source) in app.image_loads.drain(..) {
                let (tx, picker) = (image_tx.clone(), picker.clone());
                tokio::task::spawn_blocking(move || match images::load(&source, &picker) {
                    Ok(Some(preview)) => {
                        let _ = tx.send((pane, preview));
                    }
                    Ok(None) => {}
                    Err(e) => tracing::debug!("image preview: {e:#}"),
                });
            }
        } else {
            app.image_loads.clear();
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
            git: None,
            cols: 80,
            rows: 24,
            state_secs: 0,
        }
    }

    fn app() -> App {
        let mut a = App::new(&Config::default(), "/tmp".into()).unwrap();
        a.term_area = Rect::new(0, 0, 151, 40);
        a.sidebar_area = Rect::new(0, 0, 30, 39);
        a.pane_area = Rect::new(30, 0, 121, 39);
        a.on_server(ServerMsg::State {
            panes: vec![
                pane("p1", AgentState::Idle),
                pane("p2", AgentState::Blocked),
                pane("p3", AgentState::Working),
            ],
            focus: Some("p1".into()),
            view_generation: 0,
            restorable: vec![],
            archived: vec![],
        });
        a.on_server(ServerMsg::Screen {
            pane: "p1".into(),
            cols: 121,
            rows: 39,
            seq: 0,
            bytes: vec![],
        });
        // The first layout went out once the state arrived.
        assert!(matches!(
            a.outbox[..],
            [ClientMsg::View { generation: 1, .. }]
        ));
        a.outbox.clear();
        a
    }

    /// The layout in the last `View` sent: focus, then `(pane, cols, rows)`.
    fn sent_view(a: &App) -> (Option<String>, Vec<(String, u16, u16)>) {
        a.outbox
            .iter()
            .rev()
            .find_map(|m| match m {
                ClientMsg::View { focus, panes, .. } => Some((
                    focus.clone(),
                    panes
                        .iter()
                        .map(|v| (v.pane.clone(), v.cols, v.rows))
                        .collect(),
                )),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no View in {:?}", a.outbox))
    }

    fn sent_focus(a: &App) -> String {
        sent_view(a).0.unwrap()
    }

    #[test]
    fn typing_is_forwarded() {
        let mut a = app();
        a.on_stdin(b"hi\r");
        assert_eq!(
            a.outbox,
            vec![ClientMsg::Input {
                pane: "p1".into(),
                bytes: b"hi\r".to_vec()
            }]
        );
    }

    #[test]
    fn prefix_commands() {
        let mut a = app();
        // Sidebar order is blocked p2, working p3, idle p1: after p1 comes p2.
        a.on_stdin(b"\x02j");
        assert_eq!(sent_focus(&a), "p2");
        a.outbox.clear();
        a.on_stdin(b"\x023");
        assert_eq!(sent_focus(&a), "p3");
        a.outbox.clear();
        // Prefix split from its key across reads.
        a.on_stdin(b"\x02");
        a.on_stdin(b"b");
        assert_eq!(sent_focus(&a), "p2");
        // One pane on screen, so switching replaces it.
        assert_eq!(a.tiles, vec!["p2"]);
        a.outbox.clear();
        // Prefix twice sends a literal prefix byte.
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

    /// The `New` request in the outbox.
    fn sent_new(a: &App) -> NewPane {
        a.outbox
            .iter()
            .find_map(|m| match m {
                ClientMsg::Command {
                    cmd: Command::New(req),
                    ..
                } => Some(req.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no New in {:?}", a.outbox))
    }

    #[test]
    fn new_opens_a_shell() {
        // ^b n: a plain shell next to the focused agent, no dialog.
        let mut a = app();
        a.on_stdin(b"\x02n");
        assert!(matches!(a.mode, Mode::Normal));
        let req = sent_new(&a);
        assert!(req.command.is_empty() && req.focus);
        assert_eq!(req.cwd.as_deref(), Some(std::path::Path::new("/tmp")));

        // With a default command it runs first, and the shell stays after.
        let mut a = app();
        a.default_command = "claude".into();
        a.on_stdin(b"\x02n");
        let req = sent_new(&a);
        assert_eq!(req.command, vec!["claude"]);
        assert!(req.shell && req.then_shell);
    }

    #[test]
    fn close_asks_first() {
        let mut a = app();
        a.on_stdin(b"\x02x");
        assert!(matches!(&a.mode, Mode::Close { pane } if pane == "p1"));
        a.on_stdin(b"y");
        assert!(matches!(a.mode, Mode::Normal));
        assert!(matches!(
            &a.outbox[..],
            [ClientMsg::Command { cmd: Command::Kill { pane }, .. }] if pane == "p1"
        ));
    }

    fn archived(id: u64, name: &str) -> ArchivedPane {
        ArchivedPane {
            id,
            pane: Restorable {
                name: name.into(),
                workspace: "w".into(),
                agent: Some("claude".into()),
                how: "resumes its conversation".into(),
            },
        }
    }

    fn sent_commands(a: &App) -> Vec<Command> {
        a.outbox
            .iter()
            .filter_map(|m| match m {
                ClientMsg::Command { cmd, .. } => Some(cmd.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn archiving_and_bringing_back() {
        let mut a = app();
        // ^b a archives the focused agent.
        a.on_stdin(b"\x02a");
        assert_eq!(
            sent_commands(&a),
            vec![Command::Archive { pane: "p1".into() }]
        );
        a.outbox.clear();
        // So does its button in the sidebar.
        a.hits = vec![sidebar::Hit {
            area: Rect::new(20, 4, 9, 1),
            click: sidebar::Click::Archive("p2".into()),
        }];
        a.on_stdin(b"\x1b[<0;22;5M");
        assert_eq!(
            sent_commands(&a),
            vec![Command::Archive { pane: "p2".into() }]
        );
        a.outbox.clear();

        // Nothing archived: ^b A just says so.
        a.on_stdin(b"\x02A");
        assert!(matches!(a.mode, Mode::Normal));
        assert!(a.status().unwrap().0.contains("nothing archived"));

        a.on_server(ServerMsg::State {
            panes: a.panes.clone(),
            focus: Some("p1".into()),
            view_generation: a.view_generation,
            restorable: vec![],
            archived: vec![archived(20, "new"), archived(10, "old")],
        });
        a.outbox.clear();
        // ^b A lists them; Enter on the second brings it back.
        a.on_stdin(b"\x02A");
        assert!(matches!(a.mode, Mode::Archive { selected: 0 }));
        a.on_stdin(b"j\r");
        assert_eq!(sent_commands(&a), vec![Command::Unarchive { id: 10 }]);
        a.outbox.clear();
        // x forgets one and stays open on the rest.
        a.on_stdin(b"\x02Ax");
        assert_eq!(sent_commands(&a), vec![Command::ForgetArchived { id: 20 }]);
        assert!(matches!(a.mode, Mode::Archive { selected: 0 }));
        // The last one gone, the list closes.
        a.on_server(ServerMsg::State {
            panes: a.panes.clone(),
            focus: Some("p1".into()),
            view_generation: a.view_generation,
            restorable: vec![],
            archived: vec![],
        });
        assert!(matches!(a.mode, Mode::Normal));
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
    fn starts_a_shell_when_empty() {
        let mut a = App::new(&Config::default(), "/tmp".into()).unwrap();
        a.on_server(ServerMsg::State {
            panes: vec![],
            focus: None,
            view_generation: 0,
            restorable: vec![],
            archived: vec![],
        });
        assert!(matches!(a.mode, Mode::Normal));
        assert!(sent_new(&a).command.is_empty());
    }

    #[test]
    fn exit_button_offers_detach_or_stop() {
        let mut a = app();
        a.sidebar_area = Rect::new(0, 0, 30, 20);
        a.hits = vec![sidebar::Hit {
            area: Rect::new(21, 0, 8, 1),
            click: sidebar::Click::Exit,
        }];
        // Click "✕ exit", then d: detach, agents keep running.
        a.on_stdin(b"\x1b[<0;23;1M");
        assert!(matches!(a.mode, Mode::Quit));
        assert!(a.exit.is_none());
        a.on_stdin(b"d");
        assert_eq!(a.exit, Some(Exit::Detached));
        assert_eq!(a.outbox, vec![ClientMsg::Detach]);

        // Esc cancels.
        let mut a = app();
        a.on_stdin(b"\x02q\x1b");
        assert!(matches!(a.mode, Mode::Normal) && a.exit.is_none());

        // ^b q, then s: stop the server; exit once it has shut down.
        let mut a = app();
        a.on_stdin(b"\x02qs");
        assert!(matches!(
            &a.outbox[0],
            ClientMsg::Command {
                cmd: Command::KillServer,
                ..
            }
        ));
        assert!(a.exit.is_none());
        a.on_server(ServerMsg::Shutdown {
            reason: "server shut down".into(),
        });
        assert_eq!(a.exit, Some(Exit::Stopped(3)));
    }

    #[test]
    fn new_button() {
        let mut a = app();
        a.sidebar_area = Rect::new(0, 0, 30, 20);
        a.hits = vec![sidebar::Hit {
            area: Rect::new(1, 18, 12, 1),
            click: sidebar::Click::New,
        }];
        // Click "+ new".
        a.on_stdin(b"\x1b[<0;3;19M\x1b[<0;3;19m");
        assert!(sent_new(&a).command.is_empty());
        assert!(matches!(a.mode, Mode::Normal));
    }

    #[test]
    fn menu_changes_settings() {
        let mut a = app();
        let before = theme::get().name;
        a.on_stdin(b"\x02m");
        assert!(matches!(a.mode, Mode::Menu { selected: 0 }));
        // Down to the theme, → to the next one.
        a.on_stdin(b"jjj\x1b[C");
        assert!(matches!(a.mode, Mode::Menu { selected: 3 }));
        assert_ne!(theme::get().name, before);
        assert!(a.menu_value(MenuItem::Theme).contains(theme::get().name));
        a.on_stdin(b"\x1b[D");
        assert_eq!(theme::get().name, before);
        // The sidebar order flips, and so does j/k's order.
        a.on_stdin(b"j\r");
        assert_eq!(a.order, Order::Flat);
        a.on_stdin(b"\x1b");
        assert!(matches!(a.mode, Mode::Normal));
        a.on_stdin(b"\x02j");
        assert_eq!(sent_focus(&a), "p2");

        // Notifications off: the status line still says it, no bell.
        a.on_stdin(b"\x02m");
        a.on_stdin(b"jjjjj ");
        assert!(!a.notifications);
        a.on_stdin(b"q");
        a.on_server(ServerMsg::Notify {
            pane: "p2".into(),
            state: AgentState::Blocked,
            message: "p2 needs your input".into(),
        });
        assert!(a.term_writes.is_empty());
        assert!(a.status().unwrap().0.contains("p2"));

        // Clicking a row works; clicking elsewhere closes the menu.
        a.on_stdin(b"\x02m");
        a.menu_hits = vec![(Rect::new(10, 3, 20, 1), 0)];
        a.on_stdin(b"\x1b[<0;12;4M");
        assert!(matches!(a.mode, Mode::Help));
        a.on_stdin(b"q\x02m");
        a.on_stdin(b"\x1b[<0;80;30M");
        assert!(matches!(a.mode, Mode::Normal));
    }

    #[test]
    fn offers_to_bring_panes_back() {
        let fresh = || {
            let mut a = App::new(&Config::default(), "/tmp".into()).unwrap();
            a.term_area = Rect::new(0, 0, 151, 40);
            a.sidebar_area = Rect::new(0, 0, 30, 39);
            a.pane_area = Rect::new(30, 0, 121, 39);
            a.on_server(ServerMsg::State {
                panes: vec![],
                focus: None,
                view_generation: 0,
                restorable: vec![Restorable {
                    name: "claude".into(),
                    workspace: "api".into(),
                    agent: Some("claude".into()),
                    how: "resumes its conversation".into(),
                }],
                archived: vec![],
            });
            a
        };
        let restores = |a: &App| -> Vec<bool> {
            a.outbox
                .iter()
                .filter_map(|m| match m {
                    ClientMsg::Command {
                        cmd: Command::Restore { discard },
                        ..
                    } => Some(*discard),
                    _ => None,
                })
                .collect()
        };
        let news = |a: &App| {
            a.outbox
                .iter()
                .filter(|m| {
                    matches!(
                        m,
                        ClientMsg::Command {
                            cmd: Command::New(_),
                            ..
                        }
                    )
                })
                .count()
        };

        // Asked first, instead of starting a shell.
        let mut a = fresh();
        assert!(matches!(a.mode, Mode::Restore));
        assert_eq!(news(&a), 0);
        a.on_stdin(b"\r");
        assert_eq!(restores(&a), vec![false]);

        // Starting fresh: once the server agrees, a shell.
        let mut a = fresh();
        a.on_stdin(b"n");
        assert_eq!(restores(&a), vec![true]);
        let id = a.pending.keys().copied().max().unwrap();
        a.on_server(ServerMsg::Reply {
            id,
            result: Ok(Reply::Text("started fresh".into())),
        });
        assert_eq!(news(&a), 1);

        // Later: a shell now, and no asking again this time.
        let mut a = fresh();
        a.on_stdin(b"\x1b");
        assert!(matches!(a.mode, Mode::Normal));
        assert_eq!((restores(&a).len(), news(&a)), (0, 1));
        a.on_server(ServerMsg::State {
            panes: vec![],
            focus: None,
            view_generation: a.view_generation,
            restorable: a.restorable.clone(),
            archived: vec![],
        });
        assert!(matches!(a.mode, Mode::Normal));
    }

    #[test]
    fn wheel_goes_to_programs_that_scroll_themselves() {
        let mut a = app();
        let wheel_down = b"\x1b[<65;50;10M";
        let inputs = |a: &App| -> Vec<Vec<u8>> {
            a.outbox
                .iter()
                .filter_map(|m| match m {
                    ClientMsg::Input { bytes, .. } => Some(bytes.clone()),
                    _ => None,
                })
                .collect()
        };
        // A plain shell: the wheel scrolls corral's scrollback.
        a.on_stdin(b"\x1b[<64;50;10M");
        assert!(inputs(&a).is_empty());
        a.on_stdin(b"q");
        a.outbox.clear();
        // A full-screen program: arrow keys, so it scrolls itself.
        a.screens.get_mut("p1").unwrap().process(b"\x1b[?1049h");
        a.on_stdin(wheel_down);
        assert!(matches!(a.mode, Mode::Normal));
        assert_eq!(inputs(&a), vec![b"\x1b[B".repeat(WHEEL_LINES)]);
        a.outbox.clear();
        // One that reads the mouse: the wheel itself, where it happened
        // (column 50 is the pane's 20th).
        a.screens
            .get_mut("p1")
            .unwrap()
            .process(b"\x1b[?1000h\x1b[?1006h");
        a.on_stdin(wheel_down);
        assert_eq!(inputs(&a), vec![b"\x1b[<65;20;10M".to_vec()]);
    }

    #[test]
    fn drag_selects_and_copies() {
        let mut a = app();
        a.screens
            .get_mut("p1")
            .unwrap()
            .process(b"hello world\r\nsecond");
        // The pane's terminal starts at column 30 (31 in mouse terms).
        a.on_stdin(b"\x1b[<0;31;1M\x1b[<32;33;1M\x1b[<32;35;1M");
        let s = a.selection.clone().unwrap();
        assert_eq!((s.anchor, s.head, s.dragging), ((0, 0), (0, 4), true));
        a.on_stdin(b"\x1b[<0;35;1m");
        let osc = String::from_utf8(a.term_writes.clone()).unwrap();
        assert_eq!(osc, "\x1b]52;c;aGVsbG8=\x07"); // "hello"
        assert!(a.status().unwrap().0.contains("copied 5 characters"));
        // Still highlighted; typing clears it.
        assert!(a.selection.is_some());
        a.on_stdin(b"x");
        assert!(a.selection.is_none());

        // A plain click selects nothing and copies nothing.
        a.term_writes.clear();
        a.on_stdin(b"\x1b[<0;40;2M\x1b[<0;40;2m");
        assert!(a.selection.is_none() && a.term_writes.is_empty());

        // Dragging past the pane's edge stops at the edge.
        a.on_stdin(b"\x1b[<0;31;2M\x1b[<32;5;2M");
        assert_eq!(a.selection.as_ref().unwrap().head, (1, 0));
    }

    #[test]
    fn stories_button() {
        let mut a = app();
        a.hits = vec![sidebar::Hit {
            area: Rect::new(1, 38, 13, 1),
            click: sidebar::Click::Stories,
        }];
        a.on_stdin(b"\x1b[<0;3;39M");
        assert!(matches!(a.mode, Mode::Stories));
    }

    #[test]
    fn top_stories_page() {
        let mut a = app();
        a.on_stdin(b"\x02t");
        assert!(matches!(a.mode, Mode::Stories));
        // The feed is fetched, and the agents underneath count as covered.
        assert_eq!(a.fetches, vec![stories::Source::HackerNews]);
        assert!(matches!(
            a.outbox.last(),
            Some(ClientMsg::View { covered: true, .. })
        ));
        a.fetches.clear();
        let story = |n: u64| stories::Story {
            title: format!("story {n}"),
            url: None,
            discussion: format!("https://news.ycombinator.com/item?id={n}"),
            points: 1,
            comments: 0,
            by: String::new(),
            posted: 0,
            tags: vec![],
        };
        a.on_stories(
            stories::Source::HackerNews,
            Ok((1..=5).map(story).collect()),
        );
        a.on_stdin(b"jj");
        assert_eq!(a.stories.selected, 2);
        // Tab switches to lobste.rs, which needs fetching too.
        a.on_stdin(b"\t");
        assert_eq!(a.stories.source, stories::Source::Lobsters);
        assert_eq!(a.fetches, vec![stories::Source::Lobsters]);

        // Esc goes back to the agents, uncovered.
        a.on_stdin(b"\x1b");
        assert!(matches!(a.mode, Mode::Normal));
        assert!(matches!(
            a.outbox.last(),
            Some(ClientMsg::View { covered: false, .. })
        ));

        // On the page, the prefix still drives corral: ^b j moves to the
        // next agent, and ^b t just closes the page.
        a.on_stdin(b"\x02t");
        a.on_stdin(b"\x02j");
        assert!(matches!(a.mode, Mode::Normal));
        assert_eq!(sent_focus(&a), "p2");
        a.on_stdin(b"\x02t\x02t");
        assert!(matches!(a.mode, Mode::Normal));

        // Clicking an agent in the sidebar leaves the page for it.
        a.on_stdin(b"\x02t");
        a.hits = vec![sidebar::Hit {
            area: Rect::new(0, 5, 30, 1),
            click: sidebar::Click::Focus("p3".into()),
        }];
        a.on_stdin(b"\x1b[<0;3;6M");
        assert!(matches!(a.mode, Mode::Normal));
        assert_eq!(sent_focus(&a), "p3");
    }

    #[test]
    fn notes_page() {
        let mut a = app();
        a.on_stdin(b"\x02w");
        assert!(matches!(a.mode, Mode::Notes));
        assert!(matches!(
            a.outbox.last(),
            Some(ClientMsg::View { covered: true, .. })
        ));
        // No notes yet: straight into a new one. Keys that are corral's
        // elsewhere (q, j, x) are just typing here, and pastes keep lines.
        a.on_stdin(b"jot\r- quick x");
        a.on_stdin(b"\x1b[200~one\ntwo\x1b[201~");
        assert_eq!(a.notes.notes[0].lines, vec!["# jot", "- quick xone", "two"]);
        // Esc leaves the note, then the page.
        a.on_stdin(b"\x1b");
        assert!(matches!(a.mode, Mode::Notes));
        assert!(!a.notes.editing);
        a.on_stdin(b"\x1b");
        assert!(matches!(a.mode, Mode::Normal));

        // The prefix still drives corral; ^b w just closes the page.
        a.on_stdin(b"\x02w\x02j");
        assert!(matches!(a.mode, Mode::Normal));
        assert_eq!(sent_focus(&a), "p2");
        a.on_stdin(b"\x02w\x02w");
        assert!(matches!(a.mode, Mode::Normal));
        // The note is still there when it opens again.
        a.on_stdin(b"\x02w");
        assert_eq!(a.notes.notes.len(), 1);
        assert_eq!(a.notes.notes[0].title(), "jot");
    }

    #[test]
    fn order_switch_in_the_sidebar() {
        let mut a = app();
        a.hits = vec![sidebar::Hit {
            area: Rect::new(15, 1, 12, 1),
            click: sidebar::Click::Order,
        }];
        a.on_stdin(b"\x1b[<0;17;2M");
        assert_eq!(a.order, Order::Flat);
        a.on_stdin(b"\x1b[<0;17;2M");
        assert_eq!(a.order, Order::Need);
    }

    #[test]
    fn sidebar_collapses_to_a_rail() {
        let mut a = app();
        a.on_stdin(b"\x02s");
        a.outbox.clear();
        a.hits = vec![sidebar::Hit {
            area: Rect::new(27, 0, 1, 1),
            click: sidebar::Click::Collapse,
        }];
        a.on_stdin(b"\x1b[<0;28;1M");
        assert!(a.sidebar_collapsed);
        assert_eq!(a.sidebar_area.width, sidebar::RAIL_WIDTH);
        assert_eq!(a.pane_area.x, sidebar::RAIL_WIDTH);
        // The panes on screen grow into the room.
        let widths = |a: &App| -> Vec<u16> {
            a.tile_rects()
                .iter()
                .map(|(_, _, body)| body.width)
                .collect()
        };
        let wide = widths(&a);
        assert!(a.outbox.iter().any(|m| matches!(m, ClientMsg::View { .. })));
        a.on_stdin(b"\x02c");
        assert!(!a.sidebar_collapsed);
        assert_eq!(a.sidebar_area.width, 30);
        assert!(widths(&a).iter().zip(&wide).all(|(n, w)| n < w));
    }

    #[test]
    fn sidebar_click_focuses() {
        let mut a = app();
        a.sidebar_area = Rect::new(0, 0, 30, 20);
        let row = |y, id: &str| sidebar::Hit {
            area: Rect::new(0, y, 30, 1),
            click: sidebar::Click::Focus(id.into()),
        };
        a.hits = vec![row(3, "p1"), row(4, "p3")];
        a.on_stdin(b"\x1b[<0;5;5M");
        assert_eq!(sent_focus(&a), "p3");
    }

    #[test]
    fn split_shows_agents_side_by_side() {
        let mut a = app();
        // ^b s brings in the agent that needs you most: blocked p2.
        a.on_stdin(b"\x02s");
        assert_eq!(a.tiles, vec!["p1", "p2"]);
        let (focus, view) = sent_view(&a);
        assert_eq!(focus.as_deref(), Some("p2"));
        // Two columns with a divider; each tile loses a row to its header.
        assert_eq!(view, vec![("p1".into(), 60, 38), ("p2".into(), 60, 38)]);

        // Picking an agent in the sidebar swaps it into the focused tile.
        a.outbox.clear();
        a.on_stdin(b"\x023");
        assert_eq!(a.tiles, vec!["p1", "p3"]);
        assert_eq!(sent_focus(&a), "p3");
        // An agent already on screen just gets the keyboard.
        a.on_stdin(b"\x021");
        assert_eq!(a.tiles, vec!["p1", "p3"]);
        assert_eq!(sent_focus(&a), "p1");
        a.on_stdin(b"\x02l");
        assert_eq!(a.focus.as_deref(), Some("p3"));

        // Clicking a tile focuses it; the click doesn't reach the agent.
        a.outbox.clear();
        a.on_stdin(b"\x1b[<0;35;10M\x1b[<0;35;10m");
        assert_eq!(a.focus.as_deref(), Some("p1"));
        assert!(a.outbox.iter().all(|m| matches!(m, ClientMsg::View { .. })));

        // ^b o keeps only the focused one, and brings the others back.
        a.on_stdin(b"\x02o");
        assert_eq!(a.tiles, vec!["p1"]);
        assert_eq!(sent_view(&a).1, vec![("p1".into(), 121, 39)]);
        a.on_stdin(b"\x02o");
        assert_eq!(a.tiles, vec!["p1", "p3"]);
        // ^b S takes the focused one off screen.
        a.on_stdin(b"\x02S");
        assert_eq!(a.tiles, vec!["p3"]);
        assert_eq!(a.focus.as_deref(), Some("p3"));
    }

    #[test]
    fn at_most_four_tiles() {
        let mut a = app();
        let mut panes = a.panes.clone();
        for i in 4..=6 {
            panes.push(pane(&format!("p{i}"), AgentState::Idle));
        }
        a.on_server(ServerMsg::State {
            panes,
            focus: Some("p1".into()),
            view_generation: a.view_generation,
            restorable: vec![],
            archived: vec![],
        });
        for _ in 0..5 {
            a.on_stdin(b"\x02s");
        }
        assert_eq!(a.tiles.len(), MAX_TILES);
        assert!(a.status().unwrap().0.contains("up to 4"));
        // A 2×2 grid.
        let rects: Vec<Rect> = a.tile_rects().into_iter().map(|(_, r, _)| r).collect();
        assert_eq!(rects[0].y, rects[1].y);
        assert_eq!(rects[0].x, rects[2].x);
        assert!(rects[2].y > rects[0].y);
    }

    #[test]
    fn stale_server_focus_is_ignored() {
        let mut a = app();
        a.on_stdin(b"\x02s");
        let generation = a.view_generation;
        // State sent before the server saw our layout still names p1.
        a.on_server(ServerMsg::State {
            panes: a.panes.clone(),
            focus: Some("p1".into()),
            view_generation: generation - 1,
            restorable: vec![],
            archived: vec![],
        });
        assert_eq!(a.focus.as_deref(), Some("p2"));
        assert_eq!(a.tiles, vec!["p1", "p2"]);
        // Once it has, the server's focus counts: here it moved the
        // keyboard to a new agent, which takes the focused tile.
        let mut panes = a.panes.clone();
        panes.push(pane("p4", AgentState::Working));
        a.on_server(ServerMsg::State {
            panes,
            focus: Some("p4".into()),
            view_generation: generation,
            restorable: vec![],
            archived: vec![],
        });
        assert_eq!(a.tiles, vec!["p1", "p4"]);
        assert_eq!(a.focus.as_deref(), Some("p4"));
    }

    #[test]
    fn closed_agents_leave_the_screen() {
        let mut a = app();
        a.on_stdin(b"\x02s");
        a.outbox.clear();
        // p2 (focused) closed; the server moved the keyboard to p1.
        let panes: Vec<PaneInfo> = a.panes.iter().filter(|p| p.id != "p2").cloned().collect();
        a.on_server(ServerMsg::State {
            panes,
            focus: Some("p1".into()),
            view_generation: a.view_generation,
            restorable: vec![],
            archived: vec![],
        });
        assert_eq!(a.tiles, vec!["p1"]);
        // p1 grows back to the whole area.
        assert_eq!(sent_view(&a).1, vec![("p1".into(), 121, 39)]);
    }

    #[test]
    fn a_newer_release_puts_up_a_toast() {
        let mut a = app();
        a.on_update("9.9.9".into());
        assert_eq!(a.update(), Some("9.9.9"));
        let mut term = Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap();
        term.draw(|f| ui::draw(f, &mut a)).unwrap();
        let screen: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("corral 9.9.9 is out"));
        assert!(screen.contains(update::INSTALL));
        // It goes away on its own.
        a.update.as_mut().unwrap().1 -= TOAST_FOR;
        a.tick();
        assert_eq!(a.update(), None);
    }

    /// p1 as a Claude Code pane.
    fn agent_app() -> App {
        let mut a = app();
        let mut p1 = pane("p1", AgentState::Idle);
        p1.agent = Some("claude".into());
        a.on_server(ServerMsg::State {
            panes: vec![p1, pane("p2", AgentState::Blocked)],
            focus: Some("p1".into()),
            view_generation: a.view_generation,
            restorable: vec![],
            archived: vec![],
        });
        a.outbox.clear();
        a
    }

    fn preview(w: u32, h: u32) -> images::Preview {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("x.png");
        image::DynamicImage::new_rgb8(w, h).save(&png).unwrap();
        images::load(
            &images::Source::File(png),
            &ratatui_image::picker::Picker::halfblocks(),
        )
        .unwrap()
        .unwrap()
    }

    #[test]
    fn ctrl_v_in_an_agent_looks_for_a_pasted_image() {
        let mut a = agent_app();
        a.on_stdin(b"\x16");
        assert_eq!(
            a.image_loads,
            vec![("p1".to_string(), images::Source::Clipboard)]
        );
        // Ctrl-V still reaches the agent, which attaches the image itself.
        assert!(matches!(
            &a.outbox[..],
            [ClientMsg::Input { bytes, .. }] if bytes == b"\x16"
        ));
        // A plain shell's Ctrl-V is its own business.
        let mut a = app();
        a.on_stdin(b"\x16");
        assert!(a.image_loads.is_empty());
        // And previews can be turned off.
        let mut a = agent_app();
        a.image_previews = false;
        a.on_stdin(b"\x16");
        assert!(a.image_loads.is_empty());
    }

    #[test]
    fn a_dropped_image_file_is_previewed() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("my shot.png");
        image::DynamicImage::new_rgb8(4, 4).save(&png).unwrap();
        let mut a = agent_app();
        let escaped = png.to_str().unwrap().replace(' ', "\\ ");
        a.on_stdin(format!("\x1b[200~{escaped}\x1b[201~").as_bytes());
        assert_eq!(
            a.image_loads,
            vec![("p1".to_string(), images::Source::File(png))]
        );
        // Text is only text.
        a.image_loads.clear();
        a.on_stdin(b"\x1b[200~hello there\x1b[201~");
        assert!(a.image_loads.is_empty());
    }

    #[test]
    fn previews_show_until_the_prompt_is_sent() {
        let mut a = agent_app();
        a.on_image("p1".into(), preview(1280, 720));
        a.on_image("p1".into(), preview(300, 600));
        let mut term = Terminal::new(ratatui::backend::TestBackend::new(151, 40)).unwrap();
        term.draw(|f| ui::draw(f, &mut a)).unwrap();
        let screen: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("image 1"), "{screen}");
        assert!(screen.contains("1280×720"));
        assert!(screen.contains("image 2"));
        // Both cards sit at the top right of the pane, inside it.
        assert_eq!(a.preview_hits.len(), 2);
        let (first, second) = (a.preview_hits[0].0, a.preview_hits[1].0);
        assert!(first.x < second.x);
        assert!(second.x + second.width < 151);
        assert!(first.x >= a.pane_area.x);
        // Typing doesn't clear them; Enter sends them with the prompt.
        a.on_stdin(b"describe these");
        assert_eq!(a.previews["p1"].len(), 2);
        a.on_stdin(b"\r");
        assert!(!a.previews.contains_key("p1"));
    }

    #[test]
    fn a_preview_closes_from_its_button() {
        let mut a = agent_app();
        a.on_image("p1".into(), preview(64, 64));
        let mut term = Terminal::new(ratatui::backend::TestBackend::new(151, 40)).unwrap();
        term.draw(|f| ui::draw(f, &mut a)).unwrap();
        let (r, ..) = a.preview_hits[0].clone();
        a.outbox.clear();
        a.on_stdin(format!("\x1b[<0;{};{}M", r.x + 2, r.y + 1).as_bytes());
        assert!(a.previews["p1"].is_empty());
        // The click was corral's, not the agent's.
        assert!(
            !a.outbox
                .iter()
                .any(|m| matches!(m, ClientMsg::Input { .. }))
        );
        // Images for panes that are gone are dropped.
        a.on_image("p9".into(), preview(8, 8));
        assert!(!a.previews.contains_key("p9"));
    }

    #[test]
    fn the_wheel_scrolls_the_sidebar_half_under_it() {
        let mut a = app();
        // Twenty agents in one space: more than the agents half holds.
        let panes: Vec<PaneInfo> = (1..=20)
            .map(|i| pane(&format!("p{i}"), AgentState::Idle))
            .collect();
        a.on_server(ServerMsg::State {
            panes,
            focus: Some("p1".into()),
            view_generation: a.view_generation,
            restorable: vec![],
            archived: vec![],
        });
        let mut term = Terminal::new(ratatui::backend::TestBackend::new(151, 40)).unwrap();
        term.draw(|f| ui::draw(f, &mut a)).unwrap();
        let agents = a
            .hits
            .iter()
            .find_map(|h| match h.click {
                sidebar::Click::Scroll {
                    half: sidebar::Half::Agents,
                    max,
                    ..
                } => Some((h.area, max)),
                _ => None,
            })
            .unwrap();
        assert!(agents.1 > 0);
        a.outbox.clear();
        // Wheel down over the agents half (SGR button 65).
        let (x, y) = (agents.0.x + 3, agents.0.y + 2);
        a.on_stdin(format!("\x1b[<65;{};{}M", x + 1, y + 1).as_bytes());
        assert_eq!(a.sidebar_scroll, [None, Some(2)]);
        // It's the sidebar's, not the agent's.
        assert!(
            !a.outbox
                .iter()
                .any(|m| matches!(m, ClientMsg::Input { .. }))
        );
        // Back up past the top stops there.
        a.on_stdin(
            format!("\x1b[<64;{};{}M\x1b[<64;{};{}M", x + 1, y + 1, x + 1, y + 1).as_bytes(),
        );
        assert_eq!(a.sidebar_scroll, [None, Some(0)]);
        // Moving the focus hands the sidebar back to following it.
        a.on_stdin(b"\x02j");
        assert_eq!(a.sidebar_scroll, [None, None]);
    }
}

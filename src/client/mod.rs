//! The terminal UI client.
//!
//! Stdin goes to the terminal in raw mode and is forwarded byte-for-byte to
//! the focused pane, except for the prefix key, mouse events and corral's
//! dialogs. Up to four panes are on screen at once, tiled next to the
//! sidebar; each is mirrored locally in a vt100 parser (snapshot plus live
//! output) and drawn with tui-term. The layout is the client's: it tells the
//! server which panes it shows, at what size, with `View`.

pub mod browser;
pub mod conn;
pub mod diff;
pub mod input;
pub mod sidebar;
pub mod stories;
pub mod theme;
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
use crate::integrate::{self, Agent, HookStatus};
use crate::paths;
use crate::protocol::{
    self, AgentState, ClientMsg, Command, NewPane, PaneId, PaneInfo, Reply, Restorable, ServerMsg,
    Viewport, WorktreeAction,
};
use browser::{BarClick, Browser, BrowserCmd, BrowserEvent, TILE as BROWSER};
use conn::Conn;
use diff::DiffView;
use input::{Chunk, Key, Mouse, MouseKind, Splitter, decode_keys};
use sidebar::Order;
use stories::Stories;
use ui::{CloseStep, TextInput, WorktreeDialog};

const STATUS_FOR: Duration = Duration::from_secs(6);

/// The menu's rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
    Keys,
    Stories,
    Theme,
    Order,
    Notifications,
    Hooks(Agent),
    Detach,
    Exit,
}

pub const MENU: [MenuItem; 9] = [
    MenuItem::Keys,
    MenuItem::Stories,
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
    /// New worktree: where, and on which branch.
    Worktree(WorktreeDialog),
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
        step: CloseStep,
    },
    Diff(DiffView),
    /// Top stories, over the panes (`App::stories` has the page).
    Stories,
    /// Detach, or stop corral and every agent.
    Quit,
    /// Bring back the panes from before corral stopped?
    Restore,
    /// Open a page: an address or a search.
    Address(TextInput),
}

#[derive(Debug)]
enum Pending {
    New,
    Kill { pane: PaneId },
    Diff { pane: PaneId },
    Restore { discard: bool },
    DiffAction { pane: PaneId },
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
    /// The prefix was pressed on the stories page, which it closed.
    from_stories: bool,
    /// The browser tile's page, once one is open.
    pub browser: Option<Browser>,
    /// Start the browser on this page at this size (the run loop does).
    pub browser_launch: Option<(String, browser::Viewport)>,
    /// For the running browser.
    pub browser_out: Vec<BrowserCmd>,
    /// The terminal can show images (kitty graphics), so pages can be drawn.
    pub graphics: bool,
    /// One cell, in pixels (for sizing pages).
    pub cell_px: (f64, f64),
    /// The terminal reports the mouse in pixels (SGR-Pixels, turned on
    /// where pages can be shown), so clicks land exactly on the page.
    pub pixel_mouse: bool,
    /// `cell_px` came from the terminal, not a guess.
    pub cell_px_known: bool,
    /// The pixel position of the mouse event being handled.
    mouse_px: Option<(f64, f64)>,
    /// What's drawn on the terminal as images.
    pub gfx: browser::kitty::Gfx,
    /// The configured browser program; empty to find one.
    pub browser_path: String,
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
            frame: 0,
            scroll_max: 0,
            splitter: Splitter::new(prefix),
            default_command: config.default_command.trim().to_string(),
            order: Order::from_config(&config.sidebar_order),
            notifications: config.notifications,
            stories: Stories::default(),
            fetches: Vec::new(),
            from_stories: false,
            browser: None,
            browser_launch: None,
            browser_out: Vec::new(),
            graphics: false,
            cell_px: (8.0, 16.0),
            pixel_mouse: false,
            cell_px_known: false,
            mouse_px: None,
            gfx: browser::kitty::Gfx::default(),
            browser_path: config.browser.clone(),
            hooks: Vec::new(),
            menu_hits: Vec::new(),
            save_to: None,
            cwd,
            next_id: 1,
            pending: HashMap::new(),
            seen_state: false,
            outbox: Vec::new(),
            term_writes: Vec::new(),
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
            // Nothing to show but the page, if there is one.
            None => {
                self.tiles.retain(|t| t == BROWSER);
                self.focus = self.tiles.first().cloned();
                return;
            }
            Some(ref p) if self.tiles.contains(p) => {}
            Some(ref p) => {
                // The focused slot takes it, unless that's the page: then
                // the first agent's slot does, or it goes next to the page.
                let focused = self
                    .focus
                    .as_ref()
                    .and_then(|f| self.tiles.iter().position(|t| t == f))
                    .filter(|&i| self.tiles[i] != BROWSER);
                let slot = focused.or_else(|| self.tiles.iter().position(|t| t != BROWSER));
                match slot {
                    Some(i) => self.tiles[i] = p.clone(),
                    None if self.tiles.is_empty() => self.tiles = vec![p.clone()],
                    None => self.tiles.insert(0, p.clone()),
                }
            }
        }
        self.focus = pane;
    }

    fn browser_focused(&self) -> bool {
        self.focus.as_deref() == Some(BROWSER)
    }

    /// The agent the keyboard is on, or (with the page focused) the one
    /// beside it: where `^b j` and friends count from.
    fn agent_focus(&self) -> Option<&PaneId> {
        if self.browser_focused() {
            self.tiles.iter().find(|t| *t != BROWSER)
        } else {
            self.focus.as_ref()
        }
    }

    fn set_focus(&mut self, pane: PaneId) {
        self.show(Some(pane));
        if matches!(self.mode, Mode::Scroll { .. } | Mode::Stories) {
            self.mode = Mode::Normal;
        }
    }

    /// The tiles' areas: `(pane, whole tile, terminal part)`. The page
    /// always has its bar.
    pub fn tile_rects(&self) -> Vec<(PaneId, Rect, Rect)> {
        let n = self.tiles.len();
        ui::tiles(self.pane_area, n)
            .into_iter()
            .zip(&self.tiles)
            .map(|(r, id)| {
                let headed = if id == BROWSER { n.max(2) } else { n };
                (id.clone(), r, ui::tile_body(r, headed))
            })
            .collect()
    }

    /// Tell the server about layout changes (new panes on screen, sizes,
    /// focus). Called after every input and server message.
    pub fn sync_view(&mut self) {
        if !self.seen_state || self.pane_area.is_empty() {
            return;
        }
        self.sync_browser();
        let view: Vec<Viewport> = self
            .tile_rects()
            .into_iter()
            .filter(|(pane, ..)| pane != BROWSER)
            .map(|(pane, _, body)| Viewport {
                pane,
                cols: body.width.max(2),
                rows: body.height.max(1),
            })
            .collect();
        // Pages drawn over the panes leave them unseen.
        let covered = matches!(self.mode, Mode::Diff(_) | Mode::Stories);
        // The server only knows agents: with the page focused, it hears
        // about the agent next to it.
        let focus = match self.focus.as_deref() {
            Some(BROWSER) => view.first().map(|v| v.pane.clone()),
            _ => self.focus.clone(),
        };
        let now = (focus, view, covered);
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
            .agent_focus()
            .and_then(|f| order.iter().position(|p| p == f))
            .unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(order.len() as isize) as usize;
        self.set_focus(order[next].clone());
    }

    fn focus_next_blocked(&mut self) {
        let order = self.order();
        let cur = self
            .agent_focus()
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
            Some(p) => p
                .worktree
                .as_ref()
                .map_or_else(|| p.cwd.clone(), |w| w.repo.clone()),
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
            worktree: false,
            branch: None,
            focus: true,
        }
    }

    /// `^b n` / "+ new shell": a shell next to the focused agent's work.
    fn new_shell(&mut self) {
        let req = self.new_pane_request(self.default_dir());
        self.request(Command::New(req), Pending::New);
    }

    /// `^b w` / "⎇ worktree": ask where and on which branch.
    fn open_worktree(&mut self) {
        let dir = self.default_dir();
        self.mode = Mode::Worktree(WorktreeDialog::new(&dir));
    }

    fn submit_worktree(&mut self, d: WorktreeDialog) {
        let cwd = paths::expand_tilde(d.cwd.text.trim());
        let cwd = if cwd.is_relative() {
            self.cwd.join(cwd)
        } else {
            cwd
        };
        let branch = d.branch.text.trim().to_string();
        let req = NewPane {
            worktree: true,
            branch: (!branch.is_empty()).then_some(branch),
            ..self.new_pane_request(cwd)
        };
        self.set_status("creating worktree…", false);
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
    }

    fn decide_restore(&mut self, discard: bool) {
        self.request(Command::Restore { discard }, Pending::Restore { discard });
    }

    // ---- the browser tile ---------------------------------------------

    /// `^b u`: ask for an address (the current one to start from).
    fn open_address(&mut self) {
        let current = self
            .browser
            .as_ref()
            .map(|b| b.url.clone())
            .filter(|u| u != "about:blank")
            .unwrap_or_default();
        self.mode = Mode::Address(TextInput::new(&current));
    }

    /// Show `input` (an address or a search) in the browser tile, starting
    /// the browser if needed. Without terminal graphics it opens in the
    /// system browser.
    fn open_page(&mut self, input: &str) {
        let url = browser::address(input);
        self.mode = Mode::Normal;
        if !self.graphics {
            self.open_link(url);
            self.set_status(
                "pages show in corral in Ghostty, kitty or WezTerm; opened in your browser",
                false,
            );
            return;
        }
        if !self.tiles.iter().any(|t| t == BROWSER) {
            if self.tiles.len() < MAX_TILES {
                let at = self
                    .focus
                    .as_ref()
                    .and_then(|f| self.tiles.iter().position(|t| t == f))
                    .map_or(self.tiles.len(), |i| i + 1);
                self.tiles.insert(at, BROWSER.to_string());
                self.others.clear();
            } else if let Some(i) = self
                .focus
                .as_ref()
                .and_then(|f| self.tiles.iter().position(|t| t == f))
            {
                self.tiles[i] = BROWSER.to_string();
            }
        }
        self.focus = Some(BROWSER.to_string());
        match &mut self.browser {
            Some(b) if !b.dead => {
                b.url = url.clone();
                self.browser_out.push(BrowserCmd::Navigate(url));
            }
            _ => {
                let viewport = self.browser_body().map_or(
                    browser::Viewport {
                        width: 800,
                        height: 600,
                        scale: 1.0,
                    },
                    |body| browser::viewport_for(body, self.cell_px),
                );
                self.browser = Some(Browser {
                    url: url.clone(),
                    loading: true,
                    viewport: Some(viewport),
                    ..Default::default()
                });
                self.browser_launch = Some((url, viewport));
            }
        }
    }

    /// Enter on a story: read it next to the agents when pages can be
    /// drawn here, else in the system browser.
    fn read_story(&mut self, url: String) {
        if self.graphics {
            self.mode = Mode::Normal;
            self.open_page(&url);
        } else {
            self.open_link(url);
        }
    }

    fn close_browser(&mut self) {
        if self.browser.take().is_some() {
            self.browser_out.push(BrowserCmd::Quit);
        }
        self.browser_launch = None;
        if let Some(i) = self.tiles.iter().position(|t| t == BROWSER) {
            self.tiles.remove(i);
            if self.browser_focused() {
                self.focus = self
                    .tiles
                    .get(i.min(self.tiles.len().saturating_sub(1)))
                    .cloned();
            }
        }
    }

    /// The page's part of the browser tile, when it's on screen.
    fn browser_body(&self) -> Option<Rect> {
        self.tile_rects()
            .into_iter()
            .find(|(id, ..)| id == BROWSER)
            .map(|(_, _, body)| body)
    }

    /// Keep the page the size of its tile.
    fn sync_browser(&mut self) {
        let Some(body) = self.browser_body() else {
            return;
        };
        let v = browser::viewport_for(body, self.cell_px);
        if let Some(b) = &mut self.browser
            && b.viewport != Some(v)
        {
            b.viewport = Some(v);
            match &mut self.browser_launch {
                Some((_, launch)) => *launch = v,
                None => self.browser_out.push(BrowserCmd::Resize(v)),
            }
        }
    }

    pub fn on_browser(&mut self, ev: BrowserEvent) {
        if let Some(b) = &mut self.browser {
            if let BrowserEvent::Failed(e) = &ev {
                self.status = Some((format!("browser: {e}"), Instant::now(), true));
            }
            b.on_event(ev);
            self.dirty = true;
        }
    }

    fn forward_to_page(&mut self, bytes: &[u8], is_paste: bool) {
        if is_paste {
            let text = String::from_utf8_lossy(bytes)
                .trim_start_matches("\x1b[200~")
                .trim_end_matches("\x1b[201~")
                .to_string();
            self.browser_out.push(BrowserCmd::Text(text));
            return;
        }
        for k in decode_keys(bytes) {
            if let Some(k) = browser::KeyInput::from_key(k) {
                self.browser_out.push(BrowserCmd::Key(k));
            }
        }
    }

    /// A click or the wheel over the browser tile.
    fn browser_mouse(&mut self, m: Mouse) {
        let Some(body) = self.browser_body() else {
            return;
        };
        let bar = self.browser.as_ref().and_then(|b| b.click_at(m.x, m.y));
        match (m.kind, bar) {
            (MouseKind::LeftDown, Some(click)) => match click {
                BarClick::Back => self.browser_out.push(BrowserCmd::Back),
                BarClick::Forward => self.browser_out.push(BrowserCmd::Forward),
                BarClick::Reload => self.browser_out.push(BrowserCmd::Reload),
                BarClick::Address => self.open_address(),
                BarClick::Close => self.close_browser(),
            },
            (kind, _) if body.contains((m.x, m.y).into()) => {
                let Some(v) = self.browser.as_ref().and_then(|b| b.viewport) else {
                    return;
                };
                let (x, y) = match self.mouse_px {
                    Some(px) => browser::page_point_px(body, px, self.cell_px, v),
                    None => browser::page_point(body, m.x, m.y, self.cell_px, v),
                };
                let dy = match kind {
                    MouseKind::WheelUp => -120.0,
                    MouseKind::WheelDown => 120.0,
                    _ => 0.0,
                };
                match kind {
                    MouseKind::LeftDown => self.browser_out.push(BrowserCmd::Click { x, y }),
                    MouseKind::WheelUp | MouseKind::WheelDown => {
                        self.browser_out.push(BrowserCmd::Scroll { x, y, dy })
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Escapes that put the page's latest frame on the terminal (or take
    /// it off while something is drawn over it). Written after each frame.
    pub fn graphics_output(&mut self) -> Vec<u8> {
        let visible = matches!(self.mode, Mode::Normal | Mode::Scroll { .. });
        let area = self.browser_body().filter(|r| visible && !r.is_empty());
        let frame = self
            .browser
            .as_ref()
            .and_then(|b| b.frame.as_deref().map(|f| (b.frame_seq, f)));
        self.gfx.update(area, frame)
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
            Key::Enter => {
                if let Some(url) = self.stories.current().map(|s| s.link().to_string()) {
                    self.read_story(url);
                    // Read here: the page took over from the list.
                    if self.graphics {
                        return;
                    }
                }
            }
            Key::Char('o') => {
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
                        && matches!(
                            h.click,
                            sidebar::Click::Focus(_) | sidebar::Click::New { .. }
                        )
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
                        self.read_story(url);
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

    fn open_close(&mut self) {
        let Some(p) = self.focused() else { return };
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

    fn open_diff(&mut self) {
        let Some(p) = self.focused() else { return };
        if p.worktree.is_none() {
            self.set_status(
                format!(
                    "{} has no worktree; start agents with {} w to review their diffs",
                    p.name, self.prefix_label
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

    fn forward(&mut self, bytes: &[u8], is_paste: bool) {
        let Some(pane) = self.focus.clone() else {
            return;
        };
        if pane == BROWSER {
            self.forward_to_page(bytes, is_paste);
            return;
        }
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
        let from_stories = std::mem::take(&mut self.from_stories);
        match key {
            // `^b t` on the page closes it (the prefix already did).
            Key::Char('t') if from_stories => {}
            Key::Char('t') => self.open_stories(),
            Key::Char('u') => self.open_address(),
            Key::Char('x') if self.browser_focused() => self.close_browser(),
            Key::Char('n') => self.new_shell(),
            Key::Char('w') => self.open_worktree(),
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
            Key::Char('D') => self.open_diff(),
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

    fn on_mouse(&mut self, m: Mouse) {
        let in_sidebar = m.x < self.sidebar_area.x + self.sidebar_area.width;
        if !in_sidebar && self.tile_at(m.x, m.y).as_deref() == Some(BROWSER) {
            if matches!(m.kind, MouseKind::LeftDown) && !self.browser_focused() {
                self.set_focus(BROWSER.to_string());
            }
            self.browser_mouse(m);
            return;
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
        match m.kind {
            MouseKind::LeftDown if in_sidebar => {
                let click = self
                    .hits
                    .iter()
                    .find(|h| h.contains(m.x, m.y))
                    .map(|h| h.click.clone());
                match click {
                    Some(sidebar::Click::Focus(id)) => self.set_focus(id),
                    Some(sidebar::Click::New { worktree: false }) => self.new_shell(),
                    Some(sidebar::Click::New { worktree: true }) => self.open_worktree(),
                    Some(sidebar::Click::Menu) => self.open_menu(),
                    Some(sidebar::Click::Exit) => self.mode = Mode::Quit,
                    Some(sidebar::Click::Order) => self.toggle_order(),
                    Some(sidebar::Click::Page) => self.open_address(),
                    Some(sidebar::Click::Stories) => self.open_stories(),
                    Some(sidebar::Click::Collapse) => self.toggle_sidebar(),
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
            Mode::Worktree(mut d) => match d.handle(key) {
                Some(true) => self.submit_worktree(d),
                Some(false) => {}
                None => self.mode = Mode::Worktree(d),
            },
            Mode::Menu { selected } => self.menu_key(selected, key),
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
            Mode::Address(mut input) => match key {
                Key::Esc | Key::Ctrl('c') => {}
                Key::Enter => self.open_page(&input.text),
                k => {
                    input.handle(k);
                    self.mode = Mode::Address(input);
                }
            },
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
            Mode::Worktree(d) => d.paste(&text),
            Mode::Rename { input, .. } | Mode::Address(input) => input.insert_str(&text),
            Mode::Diff(v) => v.paste(&text),
            _ => {}
        }
    }

    pub fn on_stdin(&mut self, bytes: &[u8]) {
        self.dirty = true;
        for chunk in self.splitter.feed(bytes) {
            let in_dialog = !matches!(self.mode, Mode::Normal | Mode::Scroll { .. });
            match chunk {
                Chunk::Mouse(mut m) => {
                    // In pixels: keep them for the page, use cells for the rest.
                    self.mouse_px = None;
                    if self.pixel_mouse {
                        let (px, py) = (m.x as f64, m.y as f64);
                        self.mouse_px = Some((px, py));
                        m.x = (px / self.cell_px.0) as u16;
                        m.y = (py / self.cell_px.1) as u16;
                    }
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
                        self.forward(&p, true);
                    }
                }
                Chunk::Prefix if in_dialog => match self.mode {
                    Mode::Help => self.mode = Mode::Normal,
                    // The page steps aside for corral's keys.
                    Mode::Stories => {
                        self.mode = Mode::Normal;
                        self.awaiting_command = true;
                        self.from_stories = true;
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
            } => {
                self.restorable = restorable;
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
                let exists = |id: &PaneId| id == BROWSER || self.panes.iter().any(|p| &p.id == id);
                self.tiles.retain(|id| exists(id));
                let ours = self.sent_view.as_ref().map(|v| v.0.clone());
                if view_generation == self.view_generation
                    && self.browser_focused()
                    && Some(focus.clone()) == ours
                {
                    // The page has the keyboard and the server agrees with
                    // what we told it.
                } else if view_generation == self.view_generation {
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
                    // Keep the user's choice of deleting the branch too.
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
const LEAVE: &[u8] = b"\x1b[?1016l\x1b[?2004l\x1b[?1006l\x1b[?1000l\x1b[?25h\x1b[?1049l";
/// SGR-Pixels: mouse positions in pixels.
const PIXEL_MOUSE: &[u8] = b"\x1b[?1016h";

fn restore_terminal() {
    let mut out = std::io::stdout();
    // Take the page off the screen.
    if browser::kitty::supported() {
        let _ = out.write_all(&browser::kitty::Gfx::default().clear());
    }
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
    if let Ok(ws) = crossterm::terminal::window_size()
        && ws.width > 0
        && ws.height > 0
        && ws.columns > 0
        && ws.rows > 0
    {
        app.cell_px = (
            ws.width as f64 / ws.columns as f64,
            ws.height as f64 / ws.rows as f64,
        );
        app.cell_px_known = true;
    }
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
    let conn = Conn::connect_or_start(&paths::socket_path()).await?;

    let exit = {
        let _guard = TerminalGuard::enter()?;
        event_loop(&mut app, conn, workspace).await
    };
    match exit? {
        Exit::Detached => {
            println!("[detached from corral; agents keep running — `corral` to reattach]")
        }
        Exit::Stopped(n) => println!(
            "[corral stopped; {n} {} ended. Worktrees are kept: `corral worktree ls`]",
            if n == 1 { "agent" } else { "agents" }
        ),
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
    app.graphics = browser::kitty::supported();
    let mut size = fit_terminal(app)?;
    // Pixel-exact clicks on pages, where the terminal says how big a cell is.
    if app.graphics && app.cell_px_known {
        let mut out = std::io::stdout();
        out.write_all(PIXEL_MOUSE)?;
        out.flush()?;
        app.pixel_mouse = true;
    }
    // The browser runs as a task; its frames and news come back here.
    let mut browser_tx: Option<mpsc::UnboundedSender<BrowserCmd>> = None;
    let (browser_events, mut browser_rx) = mpsc::unbounded_channel::<BrowserEvent>();
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

    loop {
        tokio::select! {
            Some((source, result)) = stories_rx.recv() => app.on_stories(source, result),
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
            Some(ev) = browser_rx.recv() => {
                if matches!(ev, BrowserEvent::Failed(_)) {
                    browser_tx = None;
                }
                app.on_browser(ev);
            }
            _ = winch.recv() => {
                app.dirty = true;
                // The terminal clears the screen on resize, images too.
                app.gfx.forget_placement();
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
                    if app.graphics {
                        let images = app.graphics_output();
                        if !images.is_empty() {
                            let mut out = std::io::stdout();
                            out.write_all(&images)?;
                            out.flush()?;
                        }
                    }
                }
            }
            _ = anim.tick() => app.tick(),
        }

        for msg in app.outbox.drain(..) {
            sink.send(protocol::encode(&msg)?).await?;
        }
        for cmd in app.browser_out.drain(..) {
            let quit = cmd == BrowserCmd::Quit;
            if let Some(tx) = &browser_tx {
                let _ = tx.send(cmd);
            }
            if quit {
                browser_tx = None;
            }
        }
        if let Some((url, viewport)) = app.browser_launch.take() {
            match browser::cdp::find_chrome(&app.browser_path) {
                Ok(chrome) => {
                    let (tx, rx) = mpsc::unbounded_channel();
                    let profile = paths::corral_home().join("browser");
                    tokio::spawn(browser::cdp::run(
                        chrome,
                        profile,
                        url,
                        viewport,
                        rx,
                        browser_events.clone(),
                    ));
                    browser_tx = Some(tx);
                }
                Err(e) => app.on_browser(BrowserEvent::Failed(format!("{e:#}"))),
            }
        }
        for source in app.fetches.drain(..) {
            let tx = stories_tx.clone();
            tokio::task::spawn_blocking(move || {
                let result = stories::fetch(source).map_err(|e| format!("{e:#}"));
                let _ = tx.send((source, result));
            });
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
    fn new_opens_a_shell_and_worktree_asks_for_a_branch() {
        // ^b n: a plain shell next to the focused agent, no dialog.
        let mut a = app();
        a.on_stdin(b"\x02n");
        assert!(matches!(a.mode, Mode::Normal));
        let req = sent_new(&a);
        assert!(req.command.is_empty() && req.focus && !req.worktree);
        assert_eq!(req.cwd.as_deref(), Some(std::path::Path::new("/tmp")));

        // With a default command it runs first, and the shell stays after.
        let mut a = app();
        a.default_command = "claude".into();
        a.on_stdin(b"\x02n");
        let req = sent_new(&a);
        assert_eq!(req.command, vec!["claude"]);
        assert!(req.shell && req.then_shell);

        // ^b w: the worktree dialog starts on the branch field.
        let mut a = app();
        a.on_stdin(b"\x02w");
        assert!(matches!(&a.mode, Mode::Worktree(d) if d.field == 1));
        a.on_stdin(b"fix-login\r");
        let req = sent_new(&a);
        assert!(req.worktree && req.command.is_empty());
        assert_eq!(req.branch.as_deref(), Some("fix-login"));
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
    fn prefix_shift_d_opens_diff_review() {
        let mut a = app();
        a.on_stdin(b"\x02D");
        assert!(matches!(a.mode, Mode::Normal));
        assert!(a.status().unwrap().0.contains("no worktree"));
        let mut panes = a.panes.clone();
        panes[0].worktree = Some(Default::default());
        a.on_server(ServerMsg::State {
            panes,
            focus: Some("p1".into()),
            view_generation: a.view_generation,
            restorable: vec![],
        });
        a.on_stdin(b"\x02D");
        assert!(matches!(a.mode, Mode::Diff(_)));
        let ClientMsg::Command {
            id,
            cmd: Command::Diff { pane },
        } = &a.outbox[0]
        else {
            panic!("{:?}", a.outbox)
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
        // Keys go to the diff view, not the agent.
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
    fn new_buttons() {
        let mut a = app();
        a.sidebar_area = Rect::new(0, 0, 30, 20);
        let button = |x, worktree| sidebar::Hit {
            area: Rect::new(x, 18, 12, 1),
            click: sidebar::Click::New { worktree },
        };
        a.hits = vec![button(1, false), button(14, true)];
        // Click "+ new shell".
        a.on_stdin(b"\x1b[<0;3;19M\x1b[<0;3;19m");
        assert!(sent_new(&a).command.is_empty());
        assert!(matches!(a.mode, Mode::Normal));
        // Click "⎇ worktree".
        a.on_stdin(b"\x1b[<0;16;19M");
        assert!(matches!(&a.mode, Mode::Worktree(_)));
    }

    #[test]
    fn menu_changes_settings() {
        let mut a = app();
        let before = theme::get().name;
        a.on_stdin(b"\x02m");
        assert!(matches!(a.mode, Mode::Menu { selected: 0 }));
        // Down to the theme, → to the next one.
        a.on_stdin(b"jj\x1b[C");
        assert!(matches!(a.mode, Mode::Menu { selected: 2 }));
        assert_ne!(theme::get().name, before);
        assert!(a.menu_value(MenuItem::Theme).contains(theme::get().name));
        a.on_stdin(b"\x1b[D");
        assert_eq!(theme::get().name, before);
        // The sidebar order flips, and so does j/k's order.
        a.on_stdin(b"j\r");
        assert_eq!(a.order, Order::Project);
        a.on_stdin(b"\x1b");
        assert!(matches!(a.mode, Mode::Normal));
        a.on_stdin(b"\x02j");
        assert_eq!(sent_focus(&a), "p2");

        // Notifications off: the status line still says it, no bell.
        a.on_stdin(b"\x02m");
        a.on_stdin(b"jjjj ");
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
                    worktree: true,
                }],
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
        });
        assert!(matches!(a.mode, Mode::Normal));
    }

    #[test]
    fn page_and_stories_buttons() {
        let mut a = app();
        let button = |x, click| sidebar::Hit {
            area: Rect::new(x, 38, 10, 1),
            click,
        };
        a.hits = vec![
            button(1, sidebar::Click::Page),
            button(15, sidebar::Click::Stories),
        ];
        a.on_stdin(b"\x1b[<0;3;39M");
        assert!(matches!(a.mode, Mode::Address(_)));
        a.on_stdin(b"\x1b");
        a.on_stdin(b"\x1b[<0;17;39M");
        assert!(matches!(a.mode, Mode::Stories));
        // From the stories page, the page button asks for an address.
        a.on_stdin(b"\x1b[<0;3;39M");
        assert!(matches!(a.mode, Mode::Address(_)));
    }

    #[test]
    fn browser_tile_next_to_the_agents() {
        let mut a = app();
        a.graphics = true;
        a.cell_px = (9.0, 18.0);
        a.on_stdin(b"\x02u");
        assert!(matches!(a.mode, Mode::Address(_)));
        a.on_stdin(b"example.com\r");
        // The page opens beside the agent and takes the keyboard.
        assert_eq!(a.tiles, vec!["p1".to_string(), BROWSER.to_string()]);
        assert!(a.browser_focused());
        let (url, v) = a.browser_launch.clone().unwrap();
        assert_eq!(url, "https://example.com");
        let body = a.browser_body().unwrap();
        assert_eq!(v.width, body.width as u32 * 9);
        // The server hears only about the agent.
        let (focus, view) = sent_view(&a);
        assert_eq!(focus.as_deref(), Some("p1"));
        assert_eq!(view.len(), 1);
        // Its answer doesn't take the keyboard from the page.
        a.on_server(ServerMsg::State {
            panes: a.panes.clone(),
            focus: Some("p1".into()),
            view_generation: a.view_generation,
            restorable: vec![],
        });
        assert!(a.browser_focused());

        // Typing goes to the page, not to an agent.
        a.outbox.clear();
        a.browser_launch = None;
        a.on_stdin(b"hi");
        assert!(
            !a.outbox
                .iter()
                .any(|m| matches!(m, ClientMsg::Input { .. }))
        );
        assert!(
            matches!(&a.browser_out[..], [BrowserCmd::Key(h), BrowserCmd::Key(i)] if h.key == "h" && i.key == "i")
        );
        a.browser_out.clear();

        // A click on the page lands at the right spot.
        let body = a.browser_body().unwrap();
        a.on_stdin(format!("\x1b[<0;{};{}M", body.x + 3, body.y + 2).as_bytes());
        let v = a.browser.as_ref().unwrap().viewport.unwrap();
        assert_eq!(
            a.browser_out.last(),
            Some(&BrowserCmd::Click {
                x: 2.5 * 9.0 / v.scale,
                y: 1.5 * 18.0 / v.scale
            })
        );

        // Another agent replaces the agent beside the page, never the page.
        a.on_stdin(b"\x02j");
        assert_eq!(a.tiles, vec!["p2".to_string(), BROWSER.to_string()]);

        // Frames are drawn, and hidden under dialogs.
        a.on_browser(BrowserEvent::Frame("iVBORw0KGgo".into()));
        let out = String::from_utf8(a.graphics_output()).unwrap();
        assert!(out.contains("a=t") && out.contains("iVBORw0KGgo"));
        a.on_stdin(b"\x02?");
        assert!(
            String::from_utf8(a.graphics_output())
                .unwrap()
                .contains("a=d")
        );
        a.on_stdin(b"q");

        // ^b x on the page closes it.
        a.on_stdin(b"\x02l");
        assert!(a.browser_focused());
        a.browser_out.clear();
        a.on_stdin(b"\x02x");
        assert_eq!(a.browser_out, vec![BrowserCmd::Quit]);
        assert_eq!(a.tiles, vec!["p2".to_string()]);
        assert!(a.browser.is_none());
    }

    #[test]
    fn without_graphics_pages_open_in_the_system_browser() {
        let mut a = app();
        a.on_stdin(b"\x02u");
        a.on_stdin(b"example.com\r");
        assert!(a.browser.is_none());
        assert_eq!(a.tiles, vec!["p1".to_string()]);
        assert!(a.status().unwrap().0.contains("Ghostty"));
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
    fn order_switch_in_the_sidebar() {
        let mut a = app();
        a.hits = vec![sidebar::Hit {
            area: Rect::new(15, 1, 12, 1),
            click: sidebar::Click::Order,
        }];
        a.on_stdin(b"\x1b[<0;17;2M");
        assert_eq!(a.order, Order::Project);
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
        });
        assert_eq!(a.tiles, vec!["p1"]);
        // p1 grows back to the whole area.
        assert_eq!(sent_view(&a).1, vec![("p1".into(), 121, 39)]);
    }
}

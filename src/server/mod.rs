//! The corral server: owns every PTY so agents outlive their clients.
//!
//! State lives in a [`Hub`] behind a std mutex (never held across an
//! await). PTY reader threads feed output into the hub, which updates the
//! pane's emulator and broadcasts the bytes. Each client connection is a
//! tokio task that filters the broadcast down to what that client needs.

pub mod conn;
pub mod pane;
pub mod restore;

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use tokio::net::UnixListener;
use tokio::sync::{Notify, broadcast};

use crate::config::Config;
use crate::detect::agents::{Identifier, Running, Seen};
use crate::detect::rules::{Rules, View};
use crate::paths;
use crate::protocol::{
    AgentState, Command, DiffReport, GitInfo, NewPane, PaneId, PaneInfo, PullRequest, Restorable,
    ServerMsg, Viewport, WorktreeAction, WorktreeInfo,
};
use crate::worktree::{self, RemoveOpts, WorktreeError};
use pane::{Pane, SpawnSpec};
use restore::{Record, Saved, SavedAgent};

pub const DEFAULT_SIZE: (u16, u16) = (120, 36);
const TICK: Duration = Duration::from_millis(250);
const IDENTIFY_EVERY: Duration = Duration::from_secs(2);
const WORKTREE_POLL_EVERY: Duration = Duration::from_secs(3);
const GIT_POLL_EVERY: Duration = Duration::from_secs(3);
/// Pull requests are asked of GitHub every this many git polls (a minute),
/// and right away when a pane's branch changes.
const PR_POLL_TICKS: u32 = 20;

pub type ClientId = u64;

#[derive(Debug, Clone)]
pub enum Event {
    Output {
        pane: PaneId,
        seq: u64,
        bytes: Bytes,
    },
    /// Pane list or a pane's state changed.
    State,
    Resized {
        pane: PaneId,
        cols: u16,
        rows: u16,
    },
    Removed {
        pane: PaneId,
    },
    Notify {
        pane: PaneId,
        state: AgentState,
        message: String,
    },
    Shutdown,
}

#[derive(Debug, Default)]
struct Client {
    attached: bool,
    /// The whole area the client has for panes.
    size: Option<(u16, u16)>,
    /// The pane that gets the keyboard; always one of `view`.
    focus: Option<PaneId>,
    /// Panes on the client's screen, at the size each is drawn.
    view: Vec<Viewport>,
    /// The client is drawing something over its panes.
    covered: bool,
}

impl Client {
    /// The pane is in view and not covered: the user can see it.
    fn shows(&self, id: &str) -> bool {
        self.attached && !self.covered && self.view.iter().any(|v| v.pane == id)
    }

    fn size_for(&self, id: &str) -> Option<(u16, u16)> {
        self.view
            .iter()
            .find(|v| v.pane == id)
            .map(|v| (v.cols, v.rows))
            .or(self.size)
    }
}

#[derive(Default)]
struct Inner {
    panes: Vec<Pane>,
    next_pane: u64,
    next_client: ClientId,
    clients: HashMap<ClientId, Client>,
    active_client: Option<ClientId>,
    last_size: Option<(u16, u16)>,
    last_focus: Option<PaneId>,
    /// Panes from before the last stop, until restored or let go.
    restorable: Option<Record>,
    /// Where the session record is kept: `None` until `run` sets it, so a
    /// Hub built in a unit test never writes one.
    record_path: Option<PathBuf>,
    last_saved: Option<Record>,
    /// Shutting down: the record keeps the panes as they were.
    stopping: bool,
}

impl Inner {
    fn pane(&mut self, id: &str) -> Result<&mut Pane> {
        self.panes
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| anyhow!("no pane {id:?}"))
    }

    /// On some client's screen (so a finished agent counts as seen).
    fn shown_to_anyone(&self, id: &str) -> bool {
        self.clients.values().any(|c| c.shows(id))
    }

    fn refresh_focus_flags(&mut self) {
        let focused: Vec<bool> = self
            .panes
            .iter()
            .map(|p| self.shown_to_anyone(&p.id))
            .collect();
        for (p, f) in self.panes.iter_mut().zip(focused) {
            p.detector.set_focused(f);
        }
    }
}

pub struct Hub {
    inner: Mutex<Inner>,
    events: broadcast::Sender<Event>,
    pub config: Config,
    pub socket: PathBuf,
    rules: Rules,
    shutdown: Notify,
}

impl Hub {
    pub fn new(config: Config, socket: PathBuf) -> Arc<Hub> {
        let (events, _) = broadcast::channel(8192);
        Arc::new(Hub {
            inner: Mutex::new(Inner {
                next_pane: 1,
                next_client: 1,
                ..Default::default()
            }),
            events,
            config,
            socket,
            rules: Rules::builtin(),
            shutdown: Notify::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit(&self, ev: Event) {
        let _ = self.events.send(ev);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn infos(&self) -> Vec<PaneInfo> {
        self.lock().panes.iter().map(Pane::info).collect()
    }

    pub fn pane_info(&self, id: &str) -> Option<PaneInfo> {
        self.lock()
            .panes
            .iter()
            .find(|p| p.id == id)
            .map(Pane::info)
    }

    pub fn pane_exists(&self, id: &str) -> bool {
        self.lock().panes.iter().any(|p| p.id == id)
    }

    // ---- clients -------------------------------------------------------

    pub fn register_client(&self) -> ClientId {
        let mut g = self.lock();
        let id = g.next_client;
        g.next_client += 1;
        g.clients.insert(id, Client::default());
        id
    }

    pub fn unregister_client(&self, client: ClientId) {
        let mut g = self.lock();
        g.clients.remove(&client);
        if g.active_client == Some(client) {
            g.active_client = None;
        }
        g.refresh_focus_flags();
    }

    pub fn attach(&self, client: ClientId, cols: u16, rows: u16) {
        tracing::info!(client, cols, rows, "client attached");
        let mut g = self.lock();
        if let Some(c) = g.clients.get_mut(&client) {
            c.attached = true;
            c.size = Some((cols, rows));
        }
        g.active_client = Some(client);
        g.last_size = Some((cols, rows));
    }

    pub fn detach(&self, client: ClientId) {
        tracing::info!(client, "client detached");
        let mut g = self.lock();
        if let Some(c) = g.clients.get_mut(&client) {
            c.attached = false;
            c.focus = None;
            c.view.clear();
        }
        g.refresh_focus_flags();
    }

    /// The pane a newly attached client looks at: the first in
    /// `workspace`, else the last one anyone looked at, else the first.
    pub fn initial_focus(&self, workspace: Option<&str>) -> Option<PaneId> {
        let g = self.lock();
        if let Some(ws) = workspace
            && let Some(p) = g.panes.iter().find(|p| p.workspace == ws)
        {
            return Some(p.id.clone());
        }
        g.last_focus
            .clone()
            .filter(|id| g.panes.iter().any(|p| &p.id == id))
            .or_else(|| g.panes.first().map(|p| p.id.clone()))
    }

    pub fn workspace_exists(&self, ws: &str) -> bool {
        self.lock().panes.iter().any(|p| p.workspace == ws)
    }

    /// Mark `client` as the most recently active one and size `panes` to
    /// how it draws them (tmux's "latest" policy).
    fn touch_and_fit(g: &mut Inner, client: ClientId, panes: &[&str]) -> Vec<Event> {
        g.active_client = Some(client);
        let Some(c) = g.clients.get(&client) else {
            return Vec::new();
        };
        if let Some(size) = c.size {
            g.last_size = Some(size);
        }
        let sizes: Vec<(String, (u16, u16))> = panes
            .iter()
            .filter_map(|id| Some((id.to_string(), c.size_for(id)?)))
            .collect();
        let mut events = Vec::new();
        for (id, (cols, rows)) in sizes {
            let Ok(p) = g.pane(&id) else { continue };
            if p.resize(cols, rows) {
                p.detector.on_resize(Instant::now());
                events.push(Event::Resized {
                    pane: p.id.clone(),
                    cols: p.cols,
                    rows: p.rows,
                });
            }
        }
        events
    }

    fn emit_all(&self, events: Vec<Event>) {
        for ev in events {
            self.emit(ev);
        }
    }

    /// Show `pane` and give it the keyboard. If the client isn't showing it
    /// yet, it takes the place of the focused pane in the client's layout
    /// (the whole area when there's no layout). `None` shows nothing.
    /// Clients make the same substitution on their side.
    pub fn focus(&self, client: ClientId, pane: Option<&str>) -> Result<()> {
        let events = {
            let mut g = self.lock();
            if let Some(p) = pane {
                g.pane(p)?;
                g.last_focus = Some(p.to_string());
            }
            let Some(c) = g.clients.get_mut(&client) else {
                return Ok(());
            };
            match pane {
                None => c.view.clear(),
                Some(p) if c.view.iter().any(|v| v.pane == p) => {}
                Some(p) => match c
                    .view
                    .iter_mut()
                    .find(|v| Some(&v.pane) == c.focus.as_ref())
                {
                    Some(slot) => slot.pane = p.to_string(),
                    None => {
                        let (cols, rows) = c.size.unwrap_or(DEFAULT_SIZE);
                        c.view = vec![Viewport {
                            pane: p.to_string(),
                            cols,
                            rows,
                        }];
                    }
                },
            }
            c.focus = pane.map(str::to_string);
            g.refresh_focus_flags();
            Self::touch_and_fit(&mut g, client, &pane.into_iter().collect::<Vec<_>>())
        };
        self.emit_all(events);
        // The pane may have gone done → idle now that it's been seen.
        self.tick();
        Ok(())
    }

    /// The client's layout: which panes it shows, at what sizes, and which
    /// one has the keyboard. Panes that no longer exist are dropped; if the
    /// focus is one of them, the first remaining pane gets it.
    pub fn set_view(
        &self,
        client: ClientId,
        focus: Option<&str>,
        panes: Vec<Viewport>,
        covered: bool,
    ) -> Vec<Viewport> {
        let (events, view) = {
            let mut g = self.lock();
            let mut seen: Vec<String> = Vec::new();
            let view: Vec<Viewport> = panes
                .into_iter()
                .filter(|v| g.panes.iter().any(|p| p.id == v.pane))
                .filter(|v| {
                    let new = !seen.contains(&v.pane);
                    seen.push(v.pane.clone());
                    new
                })
                .collect();
            let focus = focus
                .filter(|f| view.iter().any(|v| v.pane == *f))
                .map(str::to_string)
                .or_else(|| view.first().map(|v| v.pane.clone()));
            if focus.is_some() {
                g.last_focus = focus.clone();
            }
            let Some(c) = g.clients.get_mut(&client) else {
                return Vec::new();
            };
            c.focus = focus;
            c.view = view.clone();
            c.covered = covered;
            g.refresh_focus_flags();
            let ids: Vec<&str> = view.iter().map(|v| v.pane.as_str()).collect();
            (Self::touch_and_fit(&mut g, client, &ids), view)
        };
        self.emit_all(events);
        self.tick();
        view
    }

    /// The client's layout as the server has it.
    pub fn view_of(&self, client: ClientId) -> (Option<PaneId>, Vec<Viewport>) {
        let g = self.lock();
        g.clients
            .get(&client)
            .map(|c| (c.focus.clone(), c.view.clone()))
            .unwrap_or_default()
    }

    /// The client's whole pane area changed size. A single pane fills it;
    /// with several, the client follows up with a new `View`.
    pub fn resize_client(&self, client: ClientId, cols: u16, rows: u16) {
        let events = {
            let mut g = self.lock();
            let single = match g.clients.get_mut(&client) {
                Some(c) => {
                    c.size = Some((cols, rows));
                    match c.view.as_mut_slice() {
                        [only] => {
                            (only.cols, only.rows) = (cols, rows);
                            Some(only.pane.clone())
                        }
                        _ => None,
                    }
                }
                None => return,
            };
            Self::touch_and_fit(
                &mut g,
                client,
                &single.as_deref().into_iter().collect::<Vec<_>>(),
            )
        };
        self.emit_all(events);
    }

    /// Keyboard input from an attached client.
    pub fn input(&self, client: ClientId, pane: &str, bytes: Vec<u8>) -> Result<()> {
        let events = {
            let mut g = self.lock();
            let events = Self::touch_and_fit(&mut g, client, &[pane]);
            let p = g.pane(pane)?;
            p.detector.on_input(Instant::now());
            p.write(bytes);
            events
        };
        self.emit_all(events);
        Ok(())
    }

    /// Input from the CLI (`corral send`).
    pub fn send(&self, pane: &str, bytes: Vec<u8>) -> Result<()> {
        let mut g = self.lock();
        let p = g.pane(pane)?;
        if p.exited {
            bail!("pane {pane} has exited");
        }
        p.detector.on_input(Instant::now());
        p.write(bytes);
        Ok(())
    }

    pub fn screen_msg(&self, pane: &str) -> Option<ServerMsg> {
        let mut g = self.lock();
        let p = g.pane(pane).ok()?;
        Some(ServerMsg::Screen {
            pane: p.id.clone(),
            cols: p.cols,
            rows: p.rows,
            seq: p.seq,
            bytes: p.snapshot(),
        })
    }

    pub fn read(&self, pane: &str, lines: Option<usize>, scrollback: bool) -> Result<String> {
        Ok(self.lock().pane(pane)?.text(lines, scrollback))
    }

    pub fn dump(&self, pane: &str) -> Result<Vec<u8>> {
        Ok(self.lock().pane(pane)?.parser.screen().state_formatted())
    }

    pub fn rename(&self, pane: &str, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            bail!("name can't be empty");
        }
        {
            let mut g = self.lock();
            let p = g.pane(pane)?;
            p.name = name.to_string();
            p.named = true;
        }
        self.emit(Event::State);
        Ok(())
    }

    pub fn report(&self, pane: &str, state: AgentState, session: Option<String>) -> Result<()> {
        if state == AgentState::Exited {
            bail!("exited can't be reported; it's detected when the process ends");
        }
        {
            let mut g = self.lock();
            let p = g.pane(pane)?;
            p.detector.report(state, Instant::now());
            if let Some(s) = session.filter(|s| !s.trim().is_empty()) {
                p.hook_session = Some((s, unix_now()));
            }
        }
        tracing::info!(pane, %state, "state reported");
        self.tick();
        Ok(())
    }

    pub fn state_of(&self, pane: &str) -> Option<AgentState> {
        self.lock()
            .panes
            .iter()
            .find(|p| p.id == pane)
            .map(|p| p.detector.state())
    }

    // ---- pane lifecycle ------------------------------------------------

    /// Create a pane. Blocking (may run git); call from `spawn_blocking`.
    pub fn new_pane(
        self: &Arc<Self>,
        req: NewPane,
        client: Option<ClientId>,
    ) -> Result<(PaneInfo, Option<String>)> {
        self.spawn_pane(req, client, None)
    }

    /// `new_pane`, optionally in a worktree that already exists (restore).
    fn spawn_pane(
        self: &Arc<Self>,
        req: NewPane,
        client: Option<ClientId>,
        existing_worktree: Option<PathBuf>,
    ) -> Result<(PaneInfo, Option<String>)> {
        let origin = match &req.cwd {
            Some(c) => c.clone(),
            None => dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")),
        };
        let origin = origin
            .canonicalize()
            .with_context(|| format!("directory {} doesn't exist", origin.display()))?;
        if !origin.is_dir() {
            bail!("{} is not a directory", origin.display());
        }

        let (argv, display) = resolve_command(&req);
        let hint = display
            .first()
            .map(|c| program_name(c))
            .unwrap_or_else(|| "shell".into());

        let mut warning = None;
        let mut cwd = origin.clone();
        let mut wt = None;
        if let Some(path) = existing_worktree {
            match worktree::info(&path) {
                Ok(info) => {
                    cwd = info.path.clone();
                    wt = Some(info);
                }
                Err(_) => {
                    warning = Some(format!(
                        "its worktree {} is gone; started in {}",
                        path.display(),
                        origin.display()
                    ))
                }
            }
        }
        if req.worktree {
            // A plain shell's branch is named for the work, not the shell.
            let branch_hint = if argv.is_empty() { "agent" } else { &hint };
            match worktree::create(
                &origin,
                &self.config.worktree_root(),
                req.branch.as_deref(),
                branch_hint,
            ) {
                Ok(info) => {
                    cwd = info.path.clone();
                    wt = Some(info);
                }
                Err(e @ WorktreeError::NotARepo(_)) => {
                    warning = Some(format!("{e}; started without a worktree"));
                }
                Err(e) => return Err(anyhow!(e).context("creating worktree")),
            }
        }

        let workspace = req
            .workspace
            .clone()
            .filter(|w| !w.trim().is_empty())
            .unwrap_or_else(|| workspace_name(wt.as_ref().map_or(&origin, |w| &w.repo)));

        let (id, size) = {
            let mut g = self.lock();
            let id = format!("p{}", g.next_pane);
            g.next_pane += 1;
            // A focused new pane takes the place of the client's focused one.
            let size = client
                .and_then(|c| g.clients.get(&c))
                .and_then(|c| c.focus.as_deref().and_then(|f| c.size_for(f)).or(c.size))
                .or(g.last_size)
                .unwrap_or(DEFAULT_SIZE);
            (id, size)
        };

        let named = req.name.as_deref().is_some_and(|n| !n.trim().is_empty());
        let spec = SpawnSpec {
            id: id.clone(),
            name: req
                .name
                .clone()
                .filter(|_| named)
                .unwrap_or_else(|| hint.clone()),
            named,
            workspace,
            argv,
            display,
            cwd,
            origin,
            socket: self.socket.clone(),
            cols: size.0,
            rows: size.1,
            worktree: wt.clone().map(|mut w| {
                w.pane = Some(id.clone());
                w
            }),
        };
        let spawned = match pane::spawn(spec) {
            Ok(s) => s,
            Err(e) => {
                if let Some(w) = &wt {
                    let _ = worktree::remove(
                        &w.path,
                        RemoveOpts {
                            force: true,
                            delete_branch: true,
                        },
                    );
                }
                return Err(e);
            }
        };
        let pane::Spawned {
            pane,
            reader,
            child,
        } = spawned;
        let info = pane.info();
        tracing::info!(
            pane = %info.id,
            command = ?info.command,
            cwd = %info.cwd.display(),
            worktree = ?info.worktree.as_ref().map(|w| &w.branch),
            "pane started"
        );
        self.lock().panes.push(pane);
        self.start_io(id, reader, child);
        self.emit(Event::State);
        Ok((info, warning))
    }

    fn start_io(
        self: &Arc<Self>,
        id: PaneId,
        mut reader: Box<dyn Read + Send>,
        mut child: Box<dyn portable_pty::Child + Send + Sync>,
    ) {
        let hub = Arc::clone(self);
        let rid = id.clone();
        let _ = std::thread::Builder::new()
            .name(format!("pty-reader-{id}"))
            .spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => hub.on_output(&rid, &buf[..n]),
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            });
        let hub = Arc::clone(self);
        let _ = std::thread::Builder::new()
            .name(format!("pty-wait-{id}"))
            .spawn(move || {
                let code = child.wait().ok().map(|s| s.exit_code() as i32);
                hub.on_exit(&id, code);
            });
    }

    fn on_output(&self, id: &str, bytes: &[u8]) {
        let mut g = self.lock();
        let Ok(p) = g.pane(id) else { return };
        p.process(bytes);
        p.detector.on_output(Instant::now());
        let ev = Event::Output {
            pane: p.id.clone(),
            seq: p.seq,
            bytes: Bytes::copy_from_slice(bytes),
        };
        // Sent under the lock so output order matches `seq` order.
        let _ = self.events.send(ev);
    }

    fn on_exit(&self, id: &str, code: Option<i32>) {
        {
            let mut g = self.lock();
            let Ok(p) = g.pane(id) else { return };
            p.exited = true;
            p.exit_code = code;
            p.detector.set_exited();
        }
        tracing::info!(pane = id, ?code, "pane exited");
        self.tick();
        self.emit(Event::State);
    }

    /// Close a pane: refuse first if its worktree would lose work, then end
    /// the process and apply the worktree action. Blocking.
    pub fn kill_pane(&self, id: &str, action: WorktreeAction, force: bool) -> Result<String> {
        tracing::info!(pane = id, ?action, force, "closing pane");
        let wt = self.lock().pane(id)?.worktree.clone();
        let opts = RemoveOpts {
            force,
            delete_branch: action == WorktreeAction::RemoveAndDeleteBranch,
        };
        if let (Some(w), true) = (&wt, action != WorktreeAction::Keep) {
            worktree::check_removable(&w.path, opts)?;
        }

        let mut pane = {
            let mut g = self.lock();
            let idx = g
                .panes
                .iter()
                .position(|p| p.id == id)
                .ok_or_else(|| anyhow!("no pane {id:?}"))?;
            let mut p = g.panes.remove(idx);
            p.kill();
            if g.last_focus.as_deref() == Some(id) {
                g.last_focus = None;
            }
            for c in g.clients.values_mut() {
                c.view.retain(|v| v.pane != id);
                if c.focus.as_deref() == Some(id) {
                    c.focus = None;
                }
            }
            p
        };
        self.emit(Event::Removed {
            pane: id.to_string(),
        });
        self.emit(Event::State);

        // Give the process a moment to exit before touching its directory.
        let deadline = Instant::now() + Duration::from_secs(2);
        while pane.pid.is_some_and(process_alive) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        pane.kill();
        drop(pane);

        let Some(w) = wt else {
            return Ok(format!("closed {id}"));
        };
        match action {
            WorktreeAction::Keep => Ok(format!(
                "closed {id}; worktree kept at {} (branch {})",
                w.path.display(),
                w.branch
            )),
            _ => match worktree::remove(&w.path, opts) {
                Ok(()) if opts.delete_branch => Ok(format!(
                    "closed {id}; removed worktree and deleted branch {}",
                    w.branch
                )),
                Ok(()) => Ok(format!(
                    "closed {id}; removed worktree (branch {} kept)",
                    w.branch
                )),
                Err(e) => Err(anyhow!(
                    "closed {id} but kept its worktree at {}: {e}",
                    w.path.display()
                )),
            },
        }
    }

    /// Worktrees under the configured root, annotated with their panes.
    pub fn worktrees(&self) -> Vec<WorktreeInfo> {
        let by_path: HashMap<PathBuf, PaneId> = self
            .lock()
            .panes
            .iter()
            .filter_map(|p| p.worktree.as_ref().map(|w| (w.path.clone(), p.id.clone())))
            .collect();
        let mut list = worktree::list(&self.config.worktree_root());
        for w in &mut list {
            w.pane = by_path.get(&w.path).cloned();
        }
        list
    }

    /// `corral worktree rm <pane|path>`. Blocking.
    pub fn remove_worktree(
        &self,
        target: &str,
        force: bool,
        delete_branch: bool,
    ) -> Result<String> {
        let (path, pane) = {
            let g = self.lock();
            match g.panes.iter().find(|p| p.id == target) {
                Some(p) => match &p.worktree {
                    Some(w) => (w.path.clone(), Some(p.id.clone())),
                    None => bail!("pane {target} has no worktree"),
                },
                None => {
                    let path = Path::new(target)
                        .canonicalize()
                        .with_context(|| format!("{target} is neither a pane nor a path"))?;
                    let pane = g
                        .panes
                        .iter()
                        .find(|p| p.worktree.as_ref().is_some_and(|w| w.path == path))
                        .map(|p| p.id.clone());
                    (path, pane)
                }
            }
        };
        if let Some(p) = pane {
            let info = self.pane_info(&p);
            if info.is_some_and(|i| i.state != AgentState::Exited) {
                bail!(
                    "pane {p} is still running in that worktree; close it with `corral kill {p} --rm-worktree`"
                );
            }
        }
        let opts = RemoveOpts {
            force,
            delete_branch,
        };
        worktree::remove(&path, opts)?;
        {
            let mut g = self.lock();
            for p in g.panes.iter_mut() {
                if p.worktree.as_ref().is_some_and(|w| w.path == path) {
                    p.worktree = None;
                }
            }
        }
        self.emit(Event::State);
        Ok(format!("removed {}", path.display()))
    }

    // ---- session restore -----------------------------------------------

    /// Keep the session record at `path`, picking up the one already there:
    /// its panes can be restored until someone decides.
    pub fn keep_record(&self, path: PathBuf) {
        let found = match restore::load(&path) {
            Ok(r) => r.filter(|r| !r.panes.is_empty()),
            Err(e) => {
                tracing::warn!("ignoring the session record: {e:#}");
                None
            }
        };
        if let Some(r) = &found {
            tracing::info!(
                panes = r.panes.len(),
                "panes from before the last stop can be restored"
            );
        }
        let mut g = self.lock();
        g.restorable = found;
        g.record_path = Some(path);
    }

    pub fn restorable(&self) -> Vec<Restorable> {
        self.lock()
            .restorable
            .as_ref()
            .map(Record::summary)
            .unwrap_or_default()
    }

    /// Write the session record if it changed. Blocking: finding an agent's
    /// session reads its files.
    pub fn save_record(&self) {
        struct Live {
            saved: Saved,
            running: Option<Running>,
            hook_session: Option<(String, u64)>,
        }
        let (path, live, pending) = {
            let g = self.lock();
            let Some(path) = g.record_path.clone() else {
                return;
            };
            if g.stopping {
                return;
            }
            let live: Vec<Live> = g
                .panes
                .iter()
                .filter(|p| !p.exited)
                .map(|p| Live {
                    saved: Saved {
                        name: p.name.clone(),
                        named: p.named,
                        workspace: p.workspace.clone(),
                        cwd: p.cwd.clone(),
                        origin: p.origin.clone(),
                        worktree: p.worktree.as_ref().map(|w| w.path.clone()),
                        agent: None,
                    },
                    running: p.running.clone(),
                    hook_session: p.hook_session.clone(),
                })
                .collect();
            (path, live, g.restorable.clone())
        };
        // Agents of one kind sharing a folder can't be told apart by the
        // files they write; only their hooks can say.
        let shares_folder = |l: &Live| {
            live.iter()
                .filter(|o| {
                    o.saved.cwd == l.saved.cwd
                        && o.running.as_ref().map(|r| r.name) == l.running.as_ref().map(|r| r.name)
                })
                .count()
                > 1
        };
        let mut panes: Vec<Saved> = live
            .iter()
            .map(|l| {
                let mut s = l.saved.clone();
                s.agent = l.running.as_ref().map(|r| {
                    let from_hook = l
                        .hook_session
                        .as_ref()
                        .filter(|(_, at)| *at >= r.started)
                        .map(|(id, _)| id.clone());
                    let session = from_hook.or_else(|| {
                        let started = UNIX_EPOCH + Duration::from_secs(r.started);
                        (!shares_folder(l))
                            .then(|| restore::find_session(r.name, &l.saved.cwd, started))
                            .flatten()
                    });
                    SavedAgent {
                        name: r.name.to_string(),
                        argv: r.argv.clone(),
                        session,
                    }
                });
                s
            })
            .collect();
        // Not brought back yet: still kept.
        if let Some(p) = pending {
            panes.extend(p.panes);
        }
        let record = Record::new(panes);
        {
            let mut g = self.lock();
            if g.stopping || g.last_saved.as_ref() == Some(&record) {
                return;
            }
            g.last_saved = Some(record.clone());
        }
        if let Err(e) = restore::save(&path, &record) {
            tracing::warn!("saving the session record: {e:#}");
        }
    }

    /// Bring back the panes from before the last stop, or (`discard`) let
    /// them go. Agents resume their conversations; the rest come back as a
    /// shell in the same folder. Blocking.
    pub fn restore(self: &Arc<Self>, discard: bool, client: Option<ClientId>) -> Result<String> {
        let record = {
            let mut g = self.lock();
            let r = g.restorable.take();
            g.last_saved = None;
            r
        };
        let Some(record) = record else {
            bail!("there's nothing to restore");
        };
        if discard {
            let kept = record.panes.iter().filter(|p| p.worktree.is_some()).count() as u32;
            self.save_record();
            tracing::info!(panes = record.panes.len(), "let the old panes go");
            return Ok(match kept {
                0 => "started fresh".into(),
                n => format!(
                    "started fresh; {n} {} kept (`corral worktree ls`)",
                    worktree::plural(n, "worktree")
                ),
            });
        }
        let mut back = 0;
        let mut problems = Vec::new();
        for (i, p) in record.panes.iter().enumerate() {
            let argv = match &p.agent {
                Some(a) => record
                    .resume_for(i)
                    .and_then(|how| restore::resume_argv(a, &how))
                    .map(restore::with_full_path)
                    .unwrap_or_else(|| restore::with_full_path(restore::restart_argv(a))),
                None => Vec::new(),
            };
            let req = NewPane {
                name: p.named.then(|| p.name.clone()),
                workspace: Some(p.workspace.clone()),
                // Worktree panes start from their repo, then move in.
                cwd: Some(if p.worktree.is_some() {
                    p.origin.clone()
                } else {
                    p.cwd.clone()
                }),
                command: if argv.is_empty() {
                    Vec::new()
                } else {
                    vec![restore::shell_line(&argv)]
                },
                shell: true,
                then_shell: true,
                ..Default::default()
            };
            match self.spawn_pane(req, client, p.worktree.clone()) {
                Ok((_, warning)) => {
                    back += 1;
                    if let Some(w) = warning {
                        problems.push(format!("{}: {w}", p.name));
                    }
                }
                Err(e) => problems.push(format!("{}: {e:#}", p.name)),
            }
        }
        tracing::info!(back, "restored panes");
        self.save_record();
        let mut msg = format!("brought back {back} {}", worktree::plural(back, "pane"));
        if !problems.is_empty() {
            msg.push_str(&format!("; {}", problems.join("; ")));
        }
        Ok(msg)
    }

    // ---- periodic work -------------------------------------------------

    /// Re-run detection on every pane; publish state and notifications.
    pub fn tick(&self) {
        let now = Instant::now();
        let mut changed = false;
        let mut notes = Vec::new();
        {
            let mut g = self.lock();
            let focus: Vec<bool> = g.panes.iter().map(|p| g.shown_to_anyone(&p.id)).collect();
            for (p, focused) in g.panes.iter_mut().zip(focus) {
                if p.dirty {
                    p.dirty = false;
                    let screen = p.parser.screen();
                    let text = screen.contents();
                    let mut line = String::new();
                    let view = View::of_screen(screen, &text, &mut line);
                    let verdict = self.rules.classify(p.agent.as_deref(), view);
                    p.detector.set_verdict(verdict);
                }
                if let Some((_, new)) = p.detector.evaluate(now) {
                    changed = true;
                    if matches!(new, AgentState::Blocked | AgentState::Done) && !focused {
                        let what = match new {
                            AgentState::Blocked => "needs your input",
                            _ => "is done",
                        };
                        notes.push(Event::Notify {
                            pane: p.id.clone(),
                            state: new,
                            message: format!("{} ({}) {what}", p.name, p.id),
                        });
                    }
                }
            }
        }
        if changed {
            self.emit(Event::State);
        }
        // Clients decide whether to ring (their `notifications` setting).
        for n in notes {
            self.emit(n);
        }
    }

    /// Find which agent runs in each pane. Blocking (process table scan).
    pub fn identify(&self, ident: &mut Identifier) {
        let leaders: Vec<(PaneId, u32)> = self
            .lock()
            .panes
            .iter()
            .filter(|p| !p.exited)
            .filter_map(|p| p.foreground_pid().map(|pid| (p.id.clone(), pid)))
            .collect();
        if leaders.is_empty() {
            return;
        }
        let found = ident.identify(&leaders);
        let mut changed = false;
        {
            let mut g = self.lock();
            for p in g.panes.iter_mut() {
                let Some(seen) = found.get(&p.id) else {
                    continue;
                };
                match seen {
                    Seen::Agent(r) => {
                        p.running = Some(r.clone());
                        // Keep the last agent seen: a shell that ran claude
                        // is still "the claude pane" after it quits.
                        if p.agent.as_deref() != Some(r.name) {
                            p.agent = Some(r.name.to_string());
                            if !p.named {
                                p.name = r.name.to_string();
                            }
                            p.dirty = true;
                            changed = true;
                        }
                    }
                    Seen::Shell => p.running = None,
                    // Something the agent runs, or unreadable: no change.
                    Seen::Other => {}
                }
            }
        }
        if changed {
            self.emit(Event::State);
        }
    }

    /// Refresh changed-file counts for panes with worktrees. Blocking.
    pub fn poll_worktrees(&self) {
        let targets: Vec<(PaneId, PathBuf)> = self
            .lock()
            .panes
            .iter()
            .filter_map(|p| p.worktree.as_ref().map(|w| (p.id.clone(), w.path.clone())))
            .collect();
        if targets.is_empty() {
            return;
        }
        let stats: Vec<(PaneId, u32, (u32, u32, u32))> = targets
            .into_iter()
            .filter_map(|(id, path)| {
                let w = worktree::info(&path).ok()?;
                Some((id, w.changed_files, (w.diff_files, w.added, w.removed)))
            })
            .collect();
        let mut changed = false;
        {
            let mut g = self.lock();
            for (id, n, (files, added, removed)) in stats {
                if let Ok(p) = g.pane(&id)
                    && let Some(w) = p.worktree.as_mut()
                {
                    let new = (n, files, added, removed);
                    if (w.changed_files, w.diff_files, w.added, w.removed) != new {
                        (w.changed_files, w.diff_files, w.added, w.removed) = new;
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.emit(Event::State);
        }
    }

    /// Refresh the branch each pane runs on and, with `prs` or when the
    /// branch changed, its pull request. Blocking: the lookup runs `gh`.
    pub fn poll_git(&self, prs: bool) {
        let targets: Vec<(PaneId, PathBuf, Option<GitInfo>)> = self
            .lock()
            .panes
            .iter()
            .filter(|p| !p.exited)
            .map(|p| {
                let dir = p.worktree.as_ref().map_or(&p.cwd, |w| &w.path);
                (p.id.clone(), dir.clone(), p.git.clone())
            })
            .collect();
        // Panes in the same checkout share one lookup.
        let mut found: HashMap<(PathBuf, String), Option<PullRequest>> = HashMap::new();
        let fresh: Vec<(PaneId, Option<GitInfo>)> = targets
            .into_iter()
            .map(|(id, dir, old)| {
                let Some(branch) = worktree::current_branch(&dir) else {
                    return (id, None);
                };
                let old = old.filter(|g| g.branch == branch);
                let pr = if prs || old.is_none() {
                    found
                        .entry((dir.clone(), branch.clone()))
                        .or_insert_with(|| worktree::pull_request(&dir, &branch))
                        .clone()
                } else {
                    old.and_then(|g| g.pr)
                };
                (id, Some(GitInfo { branch, pr }))
            })
            .collect();
        let mut changed = false;
        {
            let mut g = self.lock();
            for (id, git) in fresh {
                if let Ok(p) = g.pane(&id)
                    && p.git != git
                {
                    p.git = git;
                    changed = true;
                }
            }
        }
        if changed {
            self.emit(Event::State);
        }
    }

    fn worktree_path(&self, pane: &str) -> Result<PathBuf> {
        let mut g = self.lock();
        let p = g.pane(pane)?;
        p.worktree.as_ref().map(|w| w.path.clone()).ok_or_else(|| {
            anyhow!("pane {pane} doesn't have a worktree (start agents with --worktree)")
        })
    }

    /// The pane's worktree diff. Blocking.
    pub fn diff(&self, pane: &str) -> Result<DiffReport> {
        let mut d = worktree::diff(&self.worktree_path(pane)?)?;
        d.pane = Some(pane.to_string());
        Ok(d)
    }

    /// Commit, merge or discard in a pane's worktree. Blocking.
    pub fn worktree_action(&self, pane: &str, action: &Command) -> Result<String> {
        let path = self.worktree_path(pane)?;
        let msg = match action {
            Command::Commit { message, .. } => worktree::commit(&path, message)?,
            Command::Merge { .. } => worktree::merge(&path)?,
            Command::Discard { .. } => worktree::discard(&path)?,
            other => bail!("not a worktree action: {other:?}"),
        };
        tracing::info!(pane, %msg, "worktree action");
        self.poll_worktrees();
        Ok(msg)
    }

    pub fn request_shutdown(&self) {
        self.shutdown.notify_one();
    }

    fn shutdown_all(&self) {
        // The record keeps the panes as they were, to bring back next time.
        self.save_record();
        self.lock().stopping = true;
        let mut g = self.lock();
        for p in g.panes.iter_mut() {
            p.kill();
        }
        g.panes.clear();
        drop(g);
        self.emit(Event::Shutdown);
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn process_alive(pid: u32) -> bool {
    // Signal 0 checks for existence without sending anything.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// argv to spawn and argv to display for a request.
fn resolve_command(req: &NewPane) -> (Vec<String>, Vec<String>) {
    if req.command.is_empty() {
        return (Vec::new(), Vec::new());
    }
    if req.shell {
        let line = req.command.join(" ");
        if line.trim().is_empty() {
            return (Vec::new(), Vec::new());
        }
        let shell = std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".into());
        let display = line.split_whitespace().map(String::from).collect();
        let script = if req.then_shell {
            format!("{line}\nexec '{}' -l", shell.replace('\'', r"'\''"))
        } else {
            line
        };
        return (vec![shell, "-lc".into(), script], display);
    }
    (req.command.clone(), req.command.clone())
}

fn program_name(cmd: &str) -> String {
    Path::new(cmd)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| cmd.to_string())
}

fn workspace_name(dir: &Path) -> String {
    dir.file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".into())
}

/// Bind the socket, refusing if a live server already owns it.
async fn bind(socket: &Path) -> Result<UnixListener> {
    paths::ensure_socket_dir(socket)?;
    if socket.exists() {
        if tokio::net::UnixStream::connect(socket).await.is_ok() {
            bail!("a corral server is already running on {}", socket.display());
        }
        std::fs::remove_file(socket)
            .with_context(|| format!("removing stale socket {}", socket.display()))?;
    }
    let listener =
        UnixListener::bind(socket).with_context(|| format!("binding {}", socket.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Run the server until `kill-server` or a termination signal.
pub async fn run(socket: PathBuf, config: Config) -> Result<()> {
    let listener = bind(&socket).await?;
    tracing::info!(socket = %socket.display(), pid = std::process::id(), "server started");
    let hub = Hub::new(config, socket.clone());
    hub.keep_record(restore::record_path(&socket));

    let h = hub.clone();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(TICK);
        loop {
            t.tick().await;
            h.tick();
        }
    });
    let h = hub.clone();
    tokio::spawn(async move {
        let ident = Arc::new(Mutex::new(Identifier::new()));
        let mut t = tokio::time::interval(IDENTIFY_EVERY);
        loop {
            t.tick().await;
            let (h, ident) = (h.clone(), ident.clone());
            let _ = tokio::task::spawn_blocking(move || {
                let mut ident = ident.lock().unwrap_or_else(|e| e.into_inner());
                h.identify(&mut ident);
                h.save_record();
            })
            .await;
        }
    });
    let h = hub.clone();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(WORKTREE_POLL_EVERY);
        loop {
            t.tick().await;
            let h = h.clone();
            let _ = tokio::task::spawn_blocking(move || h.poll_worktrees()).await;
        }
    });
    let h = hub.clone();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(GIT_POLL_EVERY);
        for n in 0u32.. {
            t.tick().await;
            let h = h.clone();
            let prs = n % PR_POLL_TICKS == 0;
            let _ = tokio::task::spawn_blocking(move || h.poll_git(prs)).await;
        }
    });

    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut hup = signal(SignalKind::hangup())?;

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let h = hub.clone();
                    tokio::spawn(async move {
                        if let Err(e) = conn::handle(h, stream).await {
                            tracing::debug!("connection ended: {e:#}");
                        }
                    });
                }
                Err(e) => tracing::warn!("accept failed: {e}"),
            },
            _ = hub.shutdown.notified() => break,
            _ = term.recv() => break,
            _ = int.recv() => break,
            // The daemon has no terminal; ignore hangups.
            _ = hup.recv() => {}
        }
    }

    tracing::info!("server shutting down");
    hub.shutdown_all();
    // Let connections flush their shutdown notice.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_command_lines_run_through_the_shell() {
        let req = NewPane {
            command: vec!["claude --model opus".into()],
            shell: true,
            ..Default::default()
        };
        let (argv, display) = resolve_command(&req);
        assert_eq!(argv[1], "-lc");
        assert_eq!(argv[2], "claude --model opus");
        assert_eq!(display, vec!["claude", "--model", "opus"]);
        assert_eq!(resolve_command(&NewPane::default()).0, Vec::<String>::new());

        // then_shell: the shell takes over when the command exits.
        let req = NewPane {
            then_shell: true,
            ..req
        };
        let (argv, display) = resolve_command(&req);
        assert!(
            argv[2].starts_with("claude --model opus\nexec '"),
            "{}",
            argv[2]
        );
        assert!(argv[2].ends_with("' -l"), "{}", argv[2]);
        assert_eq!(display[0], "claude");
    }

    #[test]
    fn names() {
        assert_eq!(program_name("/usr/bin/claude"), "claude");
        assert_eq!(workspace_name(Path::new("/x/corral")), "corral");
        assert_eq!(workspace_name(Path::new("/")), "default");
    }
}

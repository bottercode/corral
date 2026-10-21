//! One client connection.
//!
//! Interactive clients `Attach` and then get the pane list, a snapshot of
//! every pane in their view and its live output. Snapshot and stream line up
//! via the pane's output sequence number: output chunks already contained in
//! the snapshot are dropped. CLI clients just send commands.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, bail};
use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc};

use super::{ClientId, Event, Hub};
use crate::protocol::{
    self, AgentState, ClientMsg, Command, PROTOCOL_VERSION, PaneId, Reply, ServerMsg,
};

pub async fn handle(hub: Arc<Hub>, stream: UnixStream) -> Result<()> {
    let mut t = protocol::framed(stream);
    let version = match protocol::recv::<ClientMsg>(&mut t).await? {
        Some(ClientMsg::Hello { protocol_version }) => protocol_version,
        Some(_) => bail!("expected Hello as the first message"),
        None => return Ok(()),
    };
    protocol::send(
        &mut t,
        &ServerMsg::Hello {
            protocol_version: PROTOCOL_VERSION,
            server_version: env!("CARGO_PKG_VERSION").into(),
        },
    )
    .await?;
    protocol::check_version(version)?;

    let client = hub.register_client();
    let (mut sink, mut stream) = t.split();
    let (out, mut out_rx) = mpsc::unbounded_channel::<ServerMsg>();
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let last = matches!(msg, ServerMsg::Shutdown { .. });
            let Ok(frame) = protocol::encode(&msg) else {
                continue;
            };
            if sink.send(frame).await.is_err() || last {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let (internal, mut internal_rx) = mpsc::unbounded_channel::<PaneId>();
    let mut s = Session {
        hub: hub.clone(),
        client,
        out,
        internal,
        events: None,
        focus: None,
        shown: HashMap::new(),
        view_generation: 0,
        covered: false,
    };

    let result: Result<()> = async {
        loop {
            tokio::select! {
                frame = stream.next() => match frame {
                    None => return Ok(()),
                    Some(frame) => {
                        let msg: ClientMsg = protocol::decode(&frame?)?;
                        s.on_client(msg)?;
                    }
                },
                ev = next_event(&mut s.events) => {
                    if !s.on_event(ev) {
                        return Ok(());
                    }
                }
                Some(pane) = internal_rx.recv() => s.set_focus(Some(&pane)),
            }
        }
    }
    .await;

    hub.unregister_client(client);
    drop(s);
    let _ = writer.await;
    result
}

async fn next_event(
    rx: &mut Option<broadcast::Receiver<Event>>,
) -> Result<Event, broadcast::error::RecvError> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

struct Session {
    hub: Arc<Hub>,
    client: ClientId,
    out: mpsc::UnboundedSender<ServerMsg>,
    /// Panes to focus once a command (e.g. New) finishes.
    internal: mpsc::UnboundedSender<PaneId>,
    events: Option<broadcast::Receiver<Event>>,
    focus: Option<PaneId>,
    /// Panes in view → (last output seq sent, size of the last snapshot).
    shown: HashMap<PaneId, (u64, (u16, u16))>,
    view_generation: u64,
    /// The client draws over its panes (see `ClientMsg::View`).
    covered: bool,
}

impl Session {
    fn send(&self, msg: ServerMsg) {
        let _ = self.out.send(msg);
    }

    fn send_state(&self) {
        self.send(ServerMsg::State {
            panes: self.hub.infos(),
            focus: self.focus.clone(),
            view_generation: self.view_generation,
        });
    }

    fn send_screen(&mut self, pane: &str) {
        if let Some(msg) = self.hub.screen_msg(pane) {
            if let ServerMsg::Screen {
                seq, cols, rows, ..
            } = &msg
            {
                self.shown.insert(pane.to_string(), (*seq, (*cols, *rows)));
            }
            self.send(msg);
        }
    }

    /// Bring `shown` in line with the hub's view of this client: snapshot
    /// panes that just came into view or changed size, forget the rest.
    fn sync_view(&mut self) {
        let (focus, view) = self.hub.view_of(self.client);
        self.focus = focus;
        self.shown
            .retain(|id, _| view.iter().any(|v| &v.pane == id));
        for v in view {
            let fresh = self.shown.get(&v.pane).is_none_or(|(_, size)| {
                self.hub
                    .pane_info(&v.pane)
                    .is_some_and(|p| (p.cols, p.rows) != *size)
            });
            if fresh {
                self.send_screen(&v.pane);
            }
        }
        self.send_state();
    }

    fn set_focus(&mut self, pane: Option<&str>) {
        if self.events.is_none() {
            return;
        }
        if self.hub.focus(self.client, pane).is_ok() {
            self.sync_view();
        }
    }

    fn on_client(&mut self, msg: ClientMsg) -> Result<()> {
        match msg {
            ClientMsg::Hello { .. } => bail!("duplicate Hello"),
            ClientMsg::Attach {
                cols,
                rows,
                workspace,
            } => {
                self.hub.attach(self.client, cols, rows);
                // Subscribe before any snapshot so no output falls in between.
                self.events = Some(self.hub.subscribe());
                let focus = self.hub.initial_focus(workspace.as_deref());
                self.set_focus(focus.as_deref());
            }
            ClientMsg::Detach => {
                self.hub.detach(self.client);
                self.events = None;
                self.focus = None;
                self.shown.clear();
            }
            ClientMsg::View {
                generation,
                focus,
                panes,
                covered,
            } => {
                self.view_generation = generation;
                self.covered = covered;
                if self.events.is_some() {
                    self.hub
                        .set_view(self.client, focus.as_deref(), panes, covered);
                    self.sync_view();
                }
            }
            ClientMsg::Input { pane, bytes } => {
                let _ = self.hub.input(self.client, &pane, bytes);
            }
            ClientMsg::Resize { cols, rows } => self.hub.resize_client(self.client, cols, rows),
            ClientMsg::Command { id, cmd } => self.command(id, cmd),
        }
        Ok(())
    }

    /// Returns false when the connection should close.
    fn on_event(&mut self, ev: Result<Event, broadcast::error::RecvError>) -> bool {
        use broadcast::error::RecvError;
        match ev {
            Ok(Event::Output { pane, seq, bytes }) => {
                if let Some((sent, _)) = self.shown.get_mut(&pane)
                    && seq > *sent
                {
                    *sent = seq;
                    self.send(ServerMsg::Output {
                        pane,
                        seq,
                        bytes: bytes.to_vec(),
                    });
                }
            }
            Ok(Event::State) => {
                // Attached with nothing to show, and a pane just appeared
                // (e.g. `corral new` from another shell): show it.
                match self.focus.is_none().then(|| self.hub.initial_focus(None)) {
                    Some(Some(pane)) => self.set_focus(Some(&pane)),
                    _ => self.send_state(),
                }
            }
            Ok(Event::Resized { pane, cols, rows }) => {
                if self
                    .shown
                    .get(&pane)
                    .is_some_and(|(_, size)| *size != (cols, rows))
                {
                    self.send_screen(&pane);
                }
            }
            Ok(Event::Removed { pane }) => {
                if self.shown.remove(&pane).is_some() || self.focus.as_ref() == Some(&pane) {
                    // Keep the keyboard on something: the focus if it's still
                    // there, else another pane in view, else the hub's pick.
                    let (focus, view) = self.hub.view_of(self.client);
                    let next = focus
                        .or_else(|| view.first().map(|v| v.pane.clone()))
                        .or_else(|| self.hub.initial_focus(None));
                    self.set_focus(next.as_deref());
                }
            }
            Ok(Event::Notify {
                pane,
                state,
                message,
            }) => {
                if self.covered || !self.shown.contains_key(&pane) {
                    self.send(ServerMsg::Notify {
                        pane,
                        state,
                        message,
                    });
                }
            }
            Ok(Event::Shutdown) | Err(RecvError::Closed) => {
                self.send(ServerMsg::Shutdown {
                    reason: "server shut down".into(),
                });
                return false;
            }
            Err(RecvError::Lagged(_)) => {
                // Missed output: resynchronise from fresh snapshots.
                self.shown.clear();
                self.sync_view();
            }
        }
        true
    }

    fn command(&mut self, id: u64, cmd: Command) {
        let hub = self.hub.clone();
        let out = self.out.clone();
        let reply = move |result: Result<Reply>| {
            let _ = out.send(ServerMsg::Reply {
                id,
                result: result.map_err(|e| format!("{e:#}")),
            });
        };
        match cmd {
            Command::KillServer => {
                reply(Ok(Reply::Ok));
                hub.request_shutdown();
            }
            Command::Wait { pane, state } => {
                let closed = self.out.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        r = wait_for(&hub, &pane, state) => reply(r),
                        _ = closed.closed() => {}
                    }
                });
            }
            Command::New(req) => {
                let client = self.client;
                let internal = self.internal.clone();
                let focus = req.focus;
                tokio::task::spawn_blocking(move || {
                    let r = hub.new_pane(req, Some(client));
                    if let (Ok((info, _)), true) = (&r, focus) {
                        let _ = internal.send(info.id.clone());
                    }
                    reply(r.map(|(pane, warning)| Reply::Created { pane, warning }));
                });
            }
            cmd => {
                tokio::task::spawn_blocking(move || reply(run_blocking(&hub, cmd)));
            }
        }
    }
}

fn run_blocking(hub: &Hub, cmd: Command) -> Result<Reply> {
    Ok(match cmd {
        Command::List => Reply::List { panes: hub.infos() },
        Command::Kill {
            pane,
            worktree,
            force,
        } => Reply::Text(hub.kill_pane(&pane, worktree, force)?),
        Command::Rename { pane, name } => {
            hub.rename(&pane, &name)?;
            Reply::Ok
        }
        Command::Send { pane, bytes } => {
            hub.send(&pane, bytes)?;
            Reply::Ok
        }
        Command::Read {
            pane,
            lines,
            scrollback,
        } => Reply::Text(hub.read(&pane, lines, scrollback)?),
        Command::Report { pane, state } => {
            hub.report(&pane, state)?;
            Reply::Ok
        }
        Command::WorktreeList => Reply::Worktrees(hub.worktrees()),
        Command::WorktreeRemove {
            target,
            force,
            delete_branch,
        } => Reply::Text(hub.remove_worktree(&target, force, delete_branch)?),
        Command::DebugDump { pane } => Reply::Bytes(hub.dump(&pane)?),
        Command::Diff { pane } => Reply::Diff(hub.diff(&pane)?),
        Command::Commit { ref pane, .. }
        | Command::Merge { ref pane }
        | Command::Discard { ref pane } => Reply::Text(hub.worktree_action(pane, &cmd)?),
        Command::New(_) | Command::Wait { .. } | Command::KillServer => {
            unreachable!("handled asynchronously")
        }
    })
}

async fn wait_for(hub: &Hub, pane: &str, want: AgentState) -> Result<Reply> {
    let mut rx = hub.subscribe();
    loop {
        match hub.state_of(pane) {
            None if want == AgentState::Exited => return Ok(Reply::State(AgentState::Exited)),
            None => bail!("pane {pane} was closed"),
            Some(s) if s == want => return Ok(Reply::State(s)),
            Some(AgentState::Exited) => bail!("pane {pane} exited"),
            Some(_) => {}
        }
        // Only re-check when something that could change the state happens.
        loop {
            use broadcast::error::RecvError;
            match rx.recv().await {
                Ok(Event::State | Event::Removed { .. }) | Err(RecvError::Lagged(_)) => break,
                Ok(Event::Shutdown) | Err(RecvError::Closed) => bail!("server shut down"),
                Ok(_) => {}
            }
        }
    }
}

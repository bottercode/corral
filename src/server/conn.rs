//! One client connection.
//!
//! Interactive clients `Attach` and then get the pane list, a snapshot of
//! their focused pane and its live output. Snapshot and stream line up via
//! the pane's output sequence number: output chunks already contained in the
//! snapshot are dropped. CLI clients just send commands.

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
        focus_seq: 0,
        focus_size: (0, 0),
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
                Some(pane) = internal_rx.recv() => s.set_focus(&pane),
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
    focus_seq: u64,
    focus_size: (u16, u16),
}

impl Session {
    fn send(&self, msg: ServerMsg) {
        let _ = self.out.send(msg);
    }

    fn send_state(&self) {
        self.send(ServerMsg::State {
            panes: self.hub.infos(),
            focus: self.focus.clone(),
        });
    }

    fn send_screen(&mut self) {
        let Some(pane) = self.focus.clone() else {
            return;
        };
        if let Some(msg) = self.hub.screen_msg(&pane) {
            if let ServerMsg::Screen {
                seq, cols, rows, ..
            } = &msg
            {
                self.focus_seq = *seq;
                self.focus_size = (*cols, *rows);
            }
            self.send(msg);
        }
    }

    fn set_focus(&mut self, pane: &str) {
        if self.events.is_none() {
            return;
        }
        if self.hub.focus(self.client, pane).is_ok() {
            self.focus = Some(pane.to_string());
            self.send_screen();
            self.send_state();
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
                match self.hub.initial_focus(workspace.as_deref()) {
                    Some(p) => self.set_focus(&p),
                    None => self.send_state(),
                }
            }
            ClientMsg::Detach => {
                self.hub.detach(self.client);
                self.events = None;
                self.focus = None;
            }
            ClientMsg::Focus { pane } => self.set_focus(&pane),
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
                if self.focus.as_ref() == Some(&pane) && seq > self.focus_seq {
                    self.focus_seq = seq;
                    self.send(ServerMsg::Output {
                        pane,
                        seq,
                        bytes: bytes.to_vec(),
                    });
                }
            }
            Ok(Event::State) => self.send_state(),
            Ok(Event::Resized { pane, cols, rows }) => {
                if self.focus.as_ref() == Some(&pane) && self.focus_size != (cols, rows) {
                    self.send_screen();
                }
            }
            Ok(Event::Removed { pane }) => {
                if self.focus.as_ref() == Some(&pane) {
                    self.focus = None;
                    match self.hub.initial_focus(None) {
                        Some(p) => self.set_focus(&p),
                        None => self.send_state(),
                    }
                }
            }
            Ok(Event::Notify {
                pane,
                state,
                message,
            }) => {
                if self.focus.as_ref() != Some(&pane) {
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
                // Missed output: resynchronise from a fresh snapshot.
                self.send_screen();
                self.send_state();
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

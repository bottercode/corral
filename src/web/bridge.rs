//! Between the phone and the corral server. One connection, never
//! attached, lists the panes every second and runs the phone's actions; it
//! stays unattached so it never resizes anyone's panes. Each terminal the
//! phone opens gets a connection of its own that attaches like the TUI.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use super::App;
use crate::client::conn::Conn;
use crate::client::sidebar::Need;
use crate::paths;
use crate::protocol::{
    AgentState, ClientMsg, Command, PaneId, PaneInfo, Reply, ServerMsg, Viewport,
};

const POLL: Duration = Duration::from_secs(1);
/// Lines of a blocked agent's screen shown above its Approve button.
const EXCERPT_LINES: usize = 14;

/// What the phone's home screen shows.
#[derive(Serialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct Snapshot {
    /// Connected to the corral server.
    pub online: bool,
    pub sections: Vec<Section>,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// `needs`, `working` or `quiet`.
    pub need: &'static str,
    pub title: &'static str,
    pub agents: Vec<Agent>,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub id: PaneId,
    pub name: String,
    pub workspace: String,
    pub agent: Option<String>,
    pub state: AgentState,
    /// When it entered its state, in Unix seconds.
    pub since: u64,
    pub branch: Option<String>,
    pub pr: Option<u32>,
    /// The bottom of its screen, when it's blocked.
    pub excerpt: Option<String>,
}

impl Snapshot {
    /// Group like the sidebar does: what needs you (blocked before done),
    /// then what's working, then the quiet ones.
    pub fn new(
        panes: &[PaneInfo],
        since: &HashMap<PaneId, u64>,
        excerpts: &HashMap<PaneId, String>,
    ) -> Snapshot {
        let sections = [
            (Need::NeedsYou, "needs", "Needs you"),
            (Need::Working, "working", "Working"),
            (Need::Quiet, "quiet", "Quiet"),
        ]
        .into_iter()
        .filter_map(|(need, key, title)| {
            let mut members: Vec<&PaneInfo> =
                panes.iter().filter(|p| Need::of(p.state) == need).collect();
            members.sort_by_key(|p| p.state != AgentState::Blocked);
            let agents: Vec<Agent> = members
                .into_iter()
                .map(|p| Agent {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    workspace: p.workspace.clone(),
                    agent: p.agent.clone(),
                    state: p.state,
                    since: since.get(&p.id).copied().unwrap_or(0),
                    branch: p.git.as_ref().map(|g| g.branch.clone()),
                    pr: p
                        .git
                        .as_ref()
                        .and_then(|g| g.pr.as_ref())
                        .map(|pr| pr.number),
                    excerpt: excerpts.get(&p.id).cloned(),
                })
                .collect();
            (!agents.is_empty()).then_some(Section {
                need: key,
                title,
                agents,
            })
        })
        .collect();
        Snapshot {
            online: true,
            sections,
        }
    }

    /// The agents that need you, most recent first (for notifications).
    pub fn needs(&self) -> Vec<&Agent> {
        let mut v: Vec<&Agent> = self
            .sections
            .iter()
            .filter(|s| s.need == "needs")
            .flat_map(|s| &s.agents)
            .collect();
        v.sort_by_key(|a| std::cmp::Reverse(a.since));
        v
    }
}

/// Something the phone asks for.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Action {
    /// Say yes to what a blocked agent asks.
    Approve {
        pane: PaneId,
    },
    Deny {
        pane: PaneId,
    },
    /// Paste text into the agent and press Enter.
    Note {
        pane: PaneId,
        text: String,
    },
    Kill {
        pane: PaneId,
    },
    Archive {
        pane: PaneId,
    },
}

pub type ActionTx = mpsc::Sender<(Action, oneshot::Sender<Result<String>>)>;
pub type ActionRx = mpsc::Receiver<(Action, oneshot::Sender<Result<String>>)>;

/// The keys that answer a blocked agent. Agents with a menu of choices
/// (Claude Code, Codex, opencode, Gemini) highlight "yes" and take Esc for
/// no; anything else asked a `[y/n]` question.
pub fn answer(agent: Option<&str>, yes: bool) -> &'static [u8] {
    let menu = matches!(agent, Some("claude" | "codex" | "opencode" | "gemini"));
    match (menu, yes) {
        (true, true) => b"\r",
        (true, false) => b"\x1b",
        (false, true) => b"y\r",
        (false, false) => b"n\r",
    }
}

/// A note as a bracketed paste. Escapes are dropped so the text can't end
/// the paste early and type keys of its own.
pub fn paste(text: &str) -> Vec<u8> {
    let clean: String = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|&c| c != '\x1b')
        .collect();
    format!("\x1b[200~{}\x1b[201~", clean.trim_end()).into_bytes()
}

/// Keep the server's view in a [`Snapshot`], push to phones when an agent
/// starts needing you, and run actions. Reconnects (and starts the server)
/// when the connection drops.
pub async fn run(app: Arc<App>, mut actions: ActionRx) {
    loop {
        match Conn::connect_or_start(&paths::socket_path()).await {
            Ok(conn) => {
                if let Err(e) = serve(&app, conn, &mut actions).await {
                    tracing::warn!("lost the corral server: {e:#}");
                }
            }
            Err(e) => tracing::warn!("can't reach the corral server: {e:#}"),
        }
        app.feed.send_modify(|s| s.online = false);
        // Answer what's waiting rather than leave the phone hanging.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while let Ok(Some((_, reply))) = tokio::time::timeout_at(deadline, actions.recv()).await {
            let _ = reply.send(Err(anyhow!("corral's server isn't running")));
        }
    }
}

struct Seen {
    state: AgentState,
    agent: Option<String>,
    since: u64,
}

async fn serve(app: &App, mut conn: Conn, actions: &mut ActionRx) -> Result<()> {
    let mut seen: Option<HashMap<PaneId, Seen>> = None;
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let panes = match conn.request(Command::List).await? {
                    Reply::List { panes } => panes,
                    other => bail!("unexpected reply {other:?}"),
                };
                let mut excerpts = HashMap::new();
                for p in panes.iter().filter(|p| p.state == AgentState::Blocked) {
                    let read = Command::Read { pane: p.id.clone(), lines: Some(EXCERPT_LINES), scrollback: false };
                    if let Ok(Reply::Text(t)) = conn.request(read).await {
                        excerpts.insert(p.id.clone(), trim_excerpt(&t));
                    }
                }
                let now = unix_now();
                let prev = seen.take();
                let mut next = HashMap::new();
                let mut calls = Vec::new();
                for p in &panes {
                    let before = prev.as_ref().and_then(|m| m.get(&p.id));
                    // `state_secs` ticks with the clock; keep the earlier
                    // start unless the state changed, so the snapshot only
                    // changes when something did.
                    let since = match before {
                        Some(b) if b.state == p.state => b.since,
                        _ => now.saturating_sub(p.state_secs),
                    };
                    let starts_needing = Need::of(p.state) == Need::NeedsYou
                        && before.is_none_or(|b| b.state != p.state);
                    // Nothing to tell on the first look: it all happened before.
                    if prev.is_some() && starts_needing {
                        calls.push(p.id.clone());
                    }
                    next.insert(p.id.clone(), Seen { state: p.state, agent: p.agent.clone(), since });
                }
                let since = next.iter().map(|(k, v)| (k.clone(), v.since)).collect();
                seen = Some(next);
                let snap = Snapshot::new(&panes, &since, &excerpts);
                app.feed.send_if_modified(|s| {
                    let changed = *s != snap;
                    *s = snap;
                    changed
                });
                if !calls.is_empty() {
                    app.notify();
                }
                app.refresh_devices();
            }
            Some((action, reply)) = actions.recv() => {
                let agents = seen.as_ref();
                let r = act(&mut conn, action, |id| agents.and_then(|m| m.get(id))).await;
                let _ = reply.send(r);
                // Show the effect right away.
                tick.reset_immediately();
            }
        }
    }
}

async fn act<'a>(
    conn: &mut Conn,
    action: Action,
    seen: impl Fn(&str) -> Option<&'a Seen>,
) -> Result<String> {
    let send = |pane: PaneId, bytes: Vec<u8>| Command::Send { pane, bytes };
    match action {
        Action::Approve { pane } | Action::Deny { pane }
            if seen(&pane).is_none_or(|s| s.state != AgentState::Blocked) =>
        {
            bail!("{pane} isn't waiting for an answer any more")
        }
        Action::Approve { pane } => {
            let keys = answer(seen(&pane).and_then(|s| s.agent.as_deref()), true);
            conn.request(send(pane, keys.to_vec())).await?;
            Ok("approved".into())
        }
        Action::Deny { pane } => {
            let keys = answer(seen(&pane).and_then(|s| s.agent.as_deref()), false);
            conn.request(send(pane, keys.to_vec())).await?;
            Ok("denied".into())
        }
        Action::Note { pane, text } => {
            if text.trim().is_empty() {
                bail!("the note is empty");
            }
            conn.request(send(pane.clone(), paste(&text))).await?;
            // Let the agent take the paste in before Enter.
            tokio::time::sleep(Duration::from_millis(120)).await;
            conn.request(send(pane, b"\r".to_vec())).await?;
            Ok("sent".into())
        }
        Action::Kill { pane } => reply_text(conn.request(Command::Kill { pane }).await?),
        Action::Archive { pane } => reply_text(conn.request(Command::Archive { pane }).await?),
    }
}

fn reply_text(r: Reply) -> Result<String> {
    Ok(match r {
        Reply::Text(t) => t,
        _ => "done".into(),
    })
}

/// The last lines of a screen with the blank ones at the end dropped.
fn trim_excerpt(s: &str) -> String {
    let lines: Vec<&str> = s.lines().map(str::trim_end).collect();
    let end = lines
        .iter()
        .rposition(|l| !l.is_empty())
        .map_or(0, |i| i + 1);
    let start = lines[..end]
        .iter()
        .position(|l| !l.is_empty())
        .unwrap_or(end);
    lines[start..end].join("\n")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// From the phone's terminal view.
#[derive(Deserialize, Debug, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum TermIn {
    Input { data: String },
    Resize { cols: u16, rows: u16 },
}

/// To the phone's terminal view, as binary WebSocket frames.
pub enum TermOut {
    /// Redraw from scratch at this size.
    Screen {
        cols: u16,
        rows: u16,
        bytes: Vec<u8>,
    },
    Output(Vec<u8>),
    /// The pane closed or the server stopped.
    End,
}

impl TermOut {
    /// `0, cols, rows, bytes…` for a screen (sizes big-endian u16),
    /// `1, bytes…` for output, `2` at the end.
    pub fn frame(self) -> Vec<u8> {
        match self {
            TermOut::Screen { cols, rows, bytes } => {
                let mut v = Vec::with_capacity(bytes.len() + 5);
                v.push(0);
                v.extend_from_slice(&cols.to_be_bytes());
                v.extend_from_slice(&rows.to_be_bytes());
                v.extend_from_slice(&bytes);
                v
            }
            TermOut::Output(bytes) => {
                let mut v = Vec::with_capacity(bytes.len() + 1);
                v.push(1);
                v.extend_from_slice(&bytes);
                v
            }
            TermOut::End => vec![2],
        }
    }
}

/// A terminal on one pane, sized to the phone: attach like the TUI does,
/// showing just that pane.
pub struct Term {
    conn: Conn,
    pane: PaneId,
}

impl Term {
    pub async fn open(pane: PaneId, cols: u16, rows: u16) -> Result<Term> {
        let cols = cols.clamp(20, 500);
        let rows = rows.clamp(5, 300);
        let mut conn = Conn::connect(&paths::socket_path()).await?;
        conn.send(&ClientMsg::Attach {
            cols,
            rows,
            workspace: None,
            pane: Some(pane.clone()),
        })
        .await?;
        conn.send(&ClientMsg::View {
            generation: 1,
            focus: Some(pane.clone()),
            panes: vec![Viewport {
                pane: pane.clone(),
                cols,
                rows,
            }],
            covered: false,
        })
        .await?;
        Ok(Term { conn, pane })
    }

    /// The next thing to show; `End` once there's nothing more.
    pub async fn next(&mut self) -> Result<TermOut> {
        loop {
            match self.conn.recv().await.context("reading from the server")? {
                Some(ServerMsg::Screen {
                    pane,
                    cols,
                    rows,
                    bytes,
                    ..
                }) if pane == self.pane => return Ok(TermOut::Screen { cols, rows, bytes }),
                Some(ServerMsg::Output { pane, bytes, .. }) if pane == self.pane => {
                    return Ok(TermOut::Output(bytes));
                }
                Some(ServerMsg::State { panes, .. })
                    if !panes.iter().any(|p| p.id == self.pane) =>
                {
                    return Ok(TermOut::End);
                }
                Some(ServerMsg::Shutdown { .. }) | None => return Ok(TermOut::End),
                Some(_) => {}
            }
        }
    }

    pub async fn input(&mut self, msg: TermIn) -> Result<()> {
        let msg = match msg {
            TermIn::Input { data } => ClientMsg::Input {
                pane: self.pane.clone(),
                bytes: data.into_bytes(),
            },
            TermIn::Resize { cols, rows } => ClientMsg::Resize {
                cols: cols.clamp(20, 500),
                rows: rows.clamp(5, 300),
            },
        };
        self.conn.send(&msg).await
    }

    pub async fn close(mut self) {
        let _ = self.conn.send(&ClientMsg::Detach).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::GitInfo;

    fn pane(id: &str, state: AgentState) -> PaneInfo {
        PaneInfo {
            id: id.into(),
            name: format!("n{id}"),
            workspace: "w".into(),
            command: vec![],
            cwd: "/".into(),
            agent: Some("claude".into()),
            state,
            pid: None,
            exit_code: None,
            title: None,
            git: Some(GitInfo {
                branch: "main".into(),
                pr: None,
            }),
            cols: 80,
            rows: 24,
            state_secs: 5,
        }
    }

    #[test]
    fn sections_follow_the_sidebar() {
        let panes = [
            pane("p1", AgentState::Idle),
            pane("p2", AgentState::Done),
            pane("p3", AgentState::Working),
            pane("p4", AgentState::Blocked),
            pane("p5", AgentState::Exited),
        ];
        let since = HashMap::from([("p4".to_string(), 100)]);
        let excerpts = HashMap::from([("p4".to_string(), "Allow? 1. Yes".to_string())]);
        let s = Snapshot::new(&panes, &since, &excerpts);
        let got: Vec<(&str, Vec<&str>)> = s
            .sections
            .iter()
            .map(|s| (s.need, s.agents.iter().map(|a| a.id.as_str()).collect()))
            .collect();
        assert_eq!(
            got,
            [
                ("needs", vec!["p4", "p2"]),
                ("working", vec!["p3"]),
                ("quiet", vec!["p1", "p5"]),
            ]
        );
        let blocked = &s.sections[0].agents[0];
        assert_eq!(blocked.since, 100);
        assert_eq!(blocked.excerpt.as_deref(), Some("Allow? 1. Yes"));
        assert_eq!(blocked.branch.as_deref(), Some("main"));
        assert_eq!(
            serde_json::to_value(blocked).unwrap()["state"],
            serde_json::json!("blocked")
        );
        assert!(Snapshot::new(&[], &since, &excerpts).sections.is_empty());
    }

    #[test]
    fn needs_newest_first() {
        let panes = [
            pane("p1", AgentState::Done),
            pane("p2", AgentState::Blocked),
        ];
        let since = HashMap::from([("p1".to_string(), 50), ("p2".to_string(), 10)]);
        let s = Snapshot::new(&panes, &since, &HashMap::new());
        let ids: Vec<&str> = s.needs().iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["p1", "p2"]);
    }

    #[test]
    fn answers_per_agent() {
        assert_eq!(answer(Some("claude"), true), b"\r");
        assert_eq!(answer(Some("codex"), false), b"\x1b");
        assert_eq!(answer(None, true), b"y\r");
        assert_eq!(answer(Some("aider"), false), b"n\r");
    }

    #[test]
    fn notes_paste_safely() {
        assert_eq!(
            paste("fix it\r\nplease\n"),
            b"\x1b[200~fix it\nplease\x1b[201~"
        );
        assert_eq!(paste("a\x1b[201~\x03b"), b"\x1b[200~a[201~\x03b\x1b[201~");
    }

    #[test]
    fn actions_parse() {
        let a: Action = serde_json::from_str(r#"{"t":"approve","pane":"p1"}"#).unwrap();
        assert_eq!(a, Action::Approve { pane: "p1".into() });
        let a: Action = serde_json::from_str(r#"{"t":"note","pane":"p2","text":"hi"}"#).unwrap();
        assert_eq!(
            a,
            Action::Note {
                pane: "p2".into(),
                text: "hi".into()
            }
        );
        assert!(serde_json::from_str::<Action>(r#"{"t":"rm","pane":"p1"}"#).is_err());
        let t: TermIn = serde_json::from_str(r#"{"t":"resize","cols":40,"rows":20}"#).unwrap();
        assert_eq!(t, TermIn::Resize { cols: 40, rows: 20 });
    }

    #[test]
    fn excerpts_drop_blank_edges() {
        assert_eq!(trim_excerpt("\n\n  a  \nb\n\n   \n"), "  a\nb");
        assert_eq!(trim_excerpt("\n \n"), "");
    }

    #[test]
    fn term_frames() {
        assert_eq!(
            TermOut::Screen {
                cols: 300,
                rows: 2,
                bytes: b"x".to_vec()
            }
            .frame(),
            [0, 1, 44, 0, 2, b'x']
        );
        assert_eq!(TermOut::Output(b"y".to_vec()).frame(), [1, b'y']);
        assert_eq!(TermOut::End.frame(), [2]);
    }
}

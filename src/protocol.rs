//! Wire protocol between corral clients and the server.
//!
//! Frames are length-delimited; each body is a MessagePack-encoded
//! [`ClientMsg`] or [`ServerMsg`]. The first frame in each direction is a
//! `Hello` carrying [`PROTOCOL_VERSION`], which must match exactly.

use std::path::PathBuf;

use anyhow::{Context, Result};
use bytes::{Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

pub const PROTOCOL_VERSION: u32 = 8;

/// Largest frame either side will accept (scrollback snapshots can be large).
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

pub type PaneId = String;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
#[derive(clap::ValueEnum)]
pub enum AgentState {
    Working,
    Blocked,
    Done,
    Idle,
    Exited,
}

impl AgentState {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
            AgentState::Done => "done",
            AgentState::Idle => "idle",
            AgentState::Exited => "exited",
        }
    }
}

impl std::fmt::Display for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for AgentState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "working" => Ok(AgentState::Working),
            "blocked" => Ok(AgentState::Blocked),
            "done" => Ok(AgentState::Done),
            "idle" => Ok(AgentState::Idle),
            "exited" => Ok(AgentState::Exited),
            _ => Err(format!(
                "unknown state {s:?} (expected working, blocked, done, idle or exited)"
            )),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct GitInfo {
    /// Branch name, or a short commit hash when `HEAD` is detached.
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<PullRequest>,
}

/// The branch's most recent pull request on GitHub (found with `gh`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct PullRequest {
    pub number: u32,
    /// `OPEN`, `MERGED` or `CLOSED`, as `gh` reports it.
    pub state: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PaneInfo {
    pub id: PaneId,
    pub name: String,
    pub workspace: String,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub agent: Option<String>,
    pub state: AgentState,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub title: Option<String>,
    /// The branch checked out where the pane runs, and its pull request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitInfo>,
    pub cols: u16,
    pub rows: u16,
    /// Seconds spent in the current state when this was sent.
    #[serde(default)]
    pub state_secs: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct NewPane {
    pub name: Option<String>,
    pub cwd: Option<PathBuf>,
    pub workspace: Option<String>,
    /// argv; empty means the user's login shell.
    pub command: Vec<String>,
    /// Run `command[0]` through `$SHELL -c` (used for command lines typed in the TUI).
    pub shell: bool,
    /// With `shell`: when the command exits, the pane carries on as an
    /// interactive shell instead of ending.
    #[serde(default)]
    pub then_shell: bool,
    /// Make the new pane the focus of the requesting client.
    pub focus: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Command {
    List,
    New(NewPane),
    Kill {
        pane: PaneId,
    },
    Rename {
        pane: PaneId,
        name: String,
    },
    Send {
        pane: PaneId,
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
    },
    Read {
        pane: PaneId,
        lines: Option<usize>,
        scrollback: bool,
    },
    Wait {
        pane: PaneId,
        state: AgentState,
    },
    Report {
        pane: PaneId,
        state: AgentState,
        /// The agent's session id, when its hook says (Claude's do).
        #[serde(default)]
        session: Option<String>,
    },
    /// Bring back the panes from before the server stopped, or (`discard`)
    /// let them go.
    Restore {
        discard: bool,
    },
    /// End the pane's process but keep what it takes to bring it back
    /// (its folder and agent conversation).
    Archive {
        pane: PaneId,
    },
    /// Bring an archived pane back as a new pane.
    Unarchive {
        id: u64,
    },
    /// Drop an archived pane.
    ForgetArchived {
        id: u64,
    },
    DebugDump {
        pane: PaneId,
    },
    KillServer,
}

#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Ok,
    List { panes: Vec<PaneInfo> },
    Created { pane: PaneInfo },
    Text(String),
    State(AgentState),
    Bytes(#[serde(with = "serde_bytes")] Vec<u8>),
}

/// A pane that can be brought back, as a client lists it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Restorable {
    pub name: String,
    pub workspace: String,
    pub agent: Option<String>,
    /// "resumes its conversation", "shell", …
    pub how: String,
}

/// A pane put away with `Command::Archive`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ArchivedPane {
    /// When it was archived, in Unix milliseconds; names it in commands.
    pub id: u64,
    pub pane: Restorable,
}

/// One pane in a client's layout, at the size it's drawn.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Viewport {
    pub pane: PaneId,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ClientMsg {
    Hello {
        protocol_version: u32,
    },
    /// Become an interactive client: receive state, the screens of the panes
    /// in view and their live output. `cols`/`rows` is the area for panes.
    Attach {
        cols: u16,
        rows: u16,
        workspace: Option<String>,
    },
    Detach,
    /// The panes this client shows (side by side) and which one gets the
    /// keyboard. `generation` increases with every View; the server echoes the
    /// last one it applied in `State` so stale focus can be told apart.
    View {
        generation: u64,
        focus: Option<PaneId>,
        panes: Vec<Viewport>,
        /// Something of the client's own (top stories, a diff) is drawn over
        /// the panes: they keep streaming, but don't count as seen, so a
        /// finished agent stays "done" and its notification still comes.
        #[serde(default)]
        covered: bool,
    },
    Input {
        pane: PaneId,
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    Command {
        id: u64,
        cmd: Command,
    },
}

#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ServerMsg {
    Hello {
        protocol_version: u32,
        server_version: String,
    },
    State {
        panes: Vec<PaneInfo>,
        focus: Option<PaneId>,
        /// The last `View` generation applied when this was sent.
        view_generation: u64,
        /// Panes from before the server last stopped, waiting for someone
        /// to bring them back (`Command::Restore`) or let them go.
        #[serde(default)]
        restorable: Vec<Restorable>,
        /// Archived panes, newest first.
        #[serde(default)]
        archived: Vec<ArchivedPane>,
    },
    /// A full redraw of a pane: scrollback history, screen contents and
    /// terminal modes. `seq` is the last output chunk it includes.
    Screen {
        pane: PaneId,
        cols: u16,
        rows: u16,
        seq: u64,
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
    },
    Output {
        pane: PaneId,
        seq: u64,
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
    },
    Notify {
        pane: PaneId,
        state: AgentState,
        message: String,
    },
    Reply {
        id: u64,
        result: Result<Reply, String>,
    },
    Shutdown {
        reason: String,
    },
}

pub fn encode<T: Serialize>(msg: &T) -> Result<Bytes> {
    let v = rmp_serde::to_vec_named(msg).context("encoding message")?;
    Ok(Bytes::from(v))
}

pub fn decode<T: DeserializeOwned>(buf: &[u8]) -> Result<T> {
    rmp_serde::from_slice(buf).context("decoding message")
}

pub fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME)
        .new_codec()
}

pub type Transport = Framed<UnixStream, LengthDelimitedCodec>;

pub fn framed(stream: UnixStream) -> Transport {
    Framed::new(stream, codec())
}

pub async fn send<T: Serialize>(t: &mut Transport, msg: &T) -> Result<()> {
    t.send(encode(msg)?).await.context("writing to socket")
}

pub async fn recv<T: DeserializeOwned>(t: &mut Transport) -> Result<Option<T>> {
    match t.next().await {
        None => Ok(None),
        Some(frame) => {
            let frame: BytesMut = frame.context("reading from socket")?;
            Ok(Some(decode(&frame)?))
        }
    }
}

/// Error for a peer speaking a different protocol version.
/// The two sides speak different protocol versions (corral was updated
/// while a server from the old version runs).
#[derive(Debug, thiserror::Error)]
#[error(
    "protocol version mismatch: this corral speaks v{ours}, the server v{theirs}. \
     Restart the server with `corral kill-server` (this ends running agents; the next \
     `corral` offers to bring them back)."
)]
pub struct VersionMismatch {
    pub ours: u32,
    pub theirs: u32,
}

pub fn check_version(theirs: u32) -> Result<()> {
    if theirs != PROTOCOL_VERSION {
        return Err(VersionMismatch {
            ours: PROTOCOL_VERSION,
            theirs,
        }
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(msg: T)
    where
        T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let bytes = encode(&msg).unwrap();
        let back: T = decode(&bytes).unwrap();
        assert_eq!(back, msg);
    }

    fn pane() -> PaneInfo {
        PaneInfo {
            id: "p1".into(),
            name: "claude".into(),
            workspace: "corral".into(),
            command: vec!["claude".into()],
            cwd: "/tmp".into(),
            agent: Some("claude".into()),
            state: AgentState::Blocked,
            pid: Some(42),
            exit_code: None,
            title: Some("✳ Fix bug".into()),
            git: None,
            cols: 80,
            rows: 24,
            state_secs: 12,
        }
    }

    #[test]
    fn client_messages_roundtrip() {
        roundtrip(ClientMsg::Hello {
            protocol_version: PROTOCOL_VERSION,
        });
        roundtrip(ClientMsg::Attach {
            cols: 100,
            rows: 30,
            workspace: Some("w".into()),
        });
        roundtrip(ClientMsg::Detach);
        roundtrip(ClientMsg::View {
            generation: 3,
            focus: Some("p2".into()),
            panes: vec![
                Viewport {
                    pane: "p2".into(),
                    cols: 60,
                    rows: 20,
                },
                Viewport {
                    pane: "p1".into(),
                    cols: 59,
                    rows: 20,
                },
            ],
            covered: false,
        });
        roundtrip(ClientMsg::View {
            generation: 0,
            focus: None,
            panes: vec![],
            covered: true,
        });
        roundtrip(ClientMsg::Input {
            pane: "p1".into(),
            bytes: b"\x1b[A\x00\xff\xe2\x9c\xb3".to_vec(),
        });
        roundtrip(ClientMsg::Resize { cols: 1, rows: 2 });
    }

    #[test]
    fn commands_roundtrip() {
        let cmds = vec![
            Command::List,
            Command::New(NewPane {
                name: Some("a".into()),
                cwd: Some("/tmp".into()),
                workspace: Some("w".into()),
                command: vec!["sh".into(), "-c".into(), "echo hi".into()],
                shell: false,
                then_shell: true,
                focus: true,
            }),
            Command::Kill { pane: "p1".into() },
            Command::Rename {
                pane: "p1".into(),
                name: "n".into(),
            },
            Command::Send {
                pane: "p1".into(),
                bytes: b"hello\r".to_vec(),
            },
            Command::Read {
                pane: "p1".into(),
                lines: Some(20),
                scrollback: true,
            },
            Command::Wait {
                pane: "p1".into(),
                state: AgentState::Exited,
            },
            Command::Report {
                pane: "p1".into(),
                state: AgentState::Done,
                session: Some("s".into()),
            },
            Command::Restore { discard: true },
            Command::DebugDump { pane: "p1".into() },
            Command::KillServer,
        ];
        for (id, cmd) in cmds.into_iter().enumerate() {
            roundtrip(ClientMsg::Command { id: id as u64, cmd });
        }
    }

    #[test]
    fn server_messages_roundtrip() {
        roundtrip(ServerMsg::Hello {
            protocol_version: PROTOCOL_VERSION,
            server_version: "0.1.0".into(),
        });
        roundtrip(ServerMsg::State {
            panes: vec![pane()],
            focus: Some("p1".into()),
            view_generation: 7,
            restorable: vec![Restorable {
                name: "claude".into(),
                workspace: "api".into(),
                agent: Some("claude".into()),
                how: "resumes its conversation".into(),
            }],
            archived: vec![],
        });
        roundtrip(ServerMsg::Screen {
            pane: "p1".into(),
            cols: 80,
            rows: 24,
            seq: 7,
            bytes: vec![0x1b, b'[', b'H'],
        });
        roundtrip(ServerMsg::Output {
            pane: "p1".into(),
            seq: 8,
            bytes: b"hi".to_vec(),
        });
        roundtrip(ServerMsg::Notify {
            pane: "p1".into(),
            state: AgentState::Blocked,
            message: "claude is blocked".into(),
        });
        roundtrip(ServerMsg::Shutdown {
            reason: "bye".into(),
        });
        let replies = vec![
            Ok(Reply::Ok),
            Ok(Reply::List {
                panes: vec![pane()],
            }),
            Ok(Reply::Created { pane: pane() }),
            Ok(Reply::Text("x".into())),
            Ok(Reply::State(AgentState::Idle)),
            Ok(Reply::Bytes(vec![1, 2, 3])),
            Err("no such pane".into()),
        ];
        for (id, result) in replies.into_iter().enumerate() {
            roundtrip(ServerMsg::Reply {
                id: id as u64,
                result,
            });
        }
    }

    #[test]
    fn raw_bytes_are_compact() {
        // serde_bytes keeps input as a msgpack bin, not an array of ints.
        let msg = ClientMsg::Input {
            pane: "p1".into(),
            bytes: vec![0xff; 1000],
        };
        assert!(encode(&msg).unwrap().len() < 1100);
    }

    #[test]
    fn version_mismatch_is_rejected() {
        assert!(check_version(PROTOCOL_VERSION).is_ok());
        let err = check_version(PROTOCOL_VERSION + 1).unwrap_err();
        // Typed, so `kill-server` can tell it apart from other failures.
        assert!(err.is::<VersionMismatch>());
        let msg = err.to_string();
        assert!(msg.contains("protocol version mismatch"), "{msg}");
        assert!(msg.contains("corral kill-server"), "{msg}");
    }

    #[test]
    fn state_parses_and_displays() {
        for s in ["working", "blocked", "done", "idle", "exited"] {
            let st: AgentState = s.parse().unwrap();
            assert_eq!(st.to_string(), s);
        }
        assert!("sleepy".parse::<AgentState>().is_err());
    }

    #[tokio::test]
    async fn framed_transport_roundtrip() {
        let (a, b) = UnixStream::pair().unwrap();
        let (mut a, mut b) = (framed(a), framed(b));
        let big = ServerMsg::Output {
            pane: "p1".into(),
            seq: 1,
            bytes: vec![b'x'; 5 * 1024 * 1024],
        };
        let sent = big.clone();
        let writer = tokio::spawn(async move {
            send(&mut a, &sent).await.unwrap();
        });
        let got: ServerMsg = recv(&mut b).await.unwrap().unwrap();
        assert_eq!(got, big);
        writer.await.unwrap();
        assert!(recv::<ServerMsg>(&mut b).await.unwrap().is_none());
    }
}

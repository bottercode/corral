//! Wire protocol between corral clients and the server.
//!
//! Frames are length-delimited; each body is a MessagePack-encoded
//! [`ClientMsg`] or [`ServerMsg`]. The first frame in each direction is a
//! `Hello` carrying [`PROTOCOL_VERSION`], which must match exactly.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use bytes::{Bytes, BytesMut};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

pub const PROTOCOL_VERSION: u32 = 3;

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
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: String,
    pub repo: PathBuf,
    /// Branch (or commit) the worktree was created from.
    #[serde(default)]
    pub base: Option<String>,
    /// Uncommitted changed or untracked files (what removal would lose).
    pub changed_files: u32,
    /// Files, added and removed lines compared with where the branch
    /// started, committed work and untracked files included.
    #[serde(default)]
    pub diff_files: u32,
    #[serde(default)]
    pub added: u32,
    #[serde(default)]
    pub removed: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<PaneId>,
}

/// One file in a worktree's diff.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct FileDiff {
    pub path: String,
    /// `M`odified, `A`dded, `D`eleted, or `?` for untracked.
    pub status: String,
    pub added: u32,
    pub removed: u32,
    pub binary: bool,
    /// Unified diff text (hunks only, no file header).
    pub patch: String,
}

/// Everything an agent changed in its worktree since its branch started.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffReport {
    pub pane: Option<PaneId>,
    pub branch: String,
    pub base: String,
    /// Commits on the branch that aren't on the base yet.
    pub commits: u32,
    /// Files with uncommitted changes.
    pub uncommitted: u32,
    pub files: Vec<FileDiff>,
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
    pub worktree: Option<WorktreeInfo>,
    pub cols: u16,
    pub rows: u16,
    /// Seconds spent in the current state when this was sent.
    #[serde(default)]
    pub state_secs: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum WorktreeAction {
    #[default]
    Keep,
    Remove,
    RemoveAndDeleteBranch,
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
    pub worktree: bool,
    pub branch: Option<String>,
    /// Make the new pane the focus of the requesting client.
    pub focus: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Command {
    List,
    New(NewPane),
    Kill {
        pane: PaneId,
        worktree: WorktreeAction,
        force: bool,
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
    },
    WorktreeList,
    WorktreeRemove {
        target: String,
        force: bool,
        delete_branch: bool,
    },
    DebugDump {
        pane: PaneId,
    },
    /// The pane's worktree diff against where its branch started.
    Diff {
        pane: PaneId,
    },
    /// Commit everything in the pane's worktree.
    Commit {
        pane: PaneId,
        message: String,
    },
    /// Merge the pane's branch into the base branch in the main checkout.
    Merge {
        pane: PaneId,
    },
    /// Throw away uncommitted changes in the pane's worktree.
    Discard {
        pane: PaneId,
    },
    KillServer,
}

#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Ok,
    List {
        panes: Vec<PaneInfo>,
    },
    Created {
        pane: PaneInfo,
        warning: Option<String>,
    },
    Text(String),
    State(AgentState),
    Worktrees(Vec<WorktreeInfo>),
    Bytes(#[serde(with = "serde_bytes")] Vec<u8>),
    Diff(DiffReport),
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ClientMsg {
    Hello {
        protocol_version: u32,
    },
    /// Become an interactive client: receive state, the focused pane's screen
    /// and its live output. `cols`/`rows` is the area available for a pane.
    Attach {
        cols: u16,
        rows: u16,
        workspace: Option<String>,
    },
    Detach,
    /// Look at one pane, or at none (`None`).
    Focus {
        pane: Option<PaneId>,
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
pub fn check_version(theirs: u32) -> Result<()> {
    if theirs != PROTOCOL_VERSION {
        bail!(
            "protocol version mismatch: this corral speaks v{PROTOCOL_VERSION}, the other side v{theirs}. \
             Restart the server with `corral kill-server` (this ends running agents)."
        );
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
            worktree: Some(WorktreeInfo {
                path: "/w/x".into(),
                branch: "corral/x".into(),
                repo: "/r".into(),
                base: Some("main".into()),
                changed_files: 3,
                diff_files: 4,
                added: 40,
                removed: 2,
                pane: Some("p1".into()),
            }),
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
        roundtrip(ClientMsg::Focus {
            pane: Some("p2".into()),
        });
        roundtrip(ClientMsg::Focus { pane: None });
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
                worktree: true,
                branch: Some("feature/x".into()),
                focus: true,
            }),
            Command::Kill {
                pane: "p1".into(),
                worktree: WorktreeAction::RemoveAndDeleteBranch,
                force: true,
            },
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
            },
            Command::WorktreeList,
            Command::WorktreeRemove {
                target: "p1".into(),
                force: false,
                delete_branch: true,
            },
            Command::DebugDump { pane: "p1".into() },
            Command::Diff { pane: "p1".into() },
            Command::Commit {
                pane: "p1".into(),
                message: "wip".into(),
            },
            Command::Merge { pane: "p1".into() },
            Command::Discard { pane: "p1".into() },
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
            Ok(Reply::Created {
                pane: pane(),
                warning: Some("not a repo".into()),
            }),
            Ok(Reply::Text("x".into())),
            Ok(Reply::State(AgentState::Idle)),
            Ok(Reply::Worktrees(vec![pane().worktree.unwrap()])),
            Ok(Reply::Bytes(vec![1, 2, 3])),
            Ok(Reply::Diff(DiffReport {
                pane: Some("p1".into()),
                branch: "corral/x".into(),
                base: "main".into(),
                commits: 1,
                uncommitted: 2,
                files: vec![FileDiff {
                    path: "a.rs".into(),
                    status: "M".into(),
                    added: 1,
                    removed: 1,
                    binary: false,
                    patch: "@@ -1 +1 @@\n-a\n+b\n".into(),
                }],
            })),
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
        let err = check_version(PROTOCOL_VERSION + 1).unwrap_err().to_string();
        assert!(err.contains("protocol version mismatch"), "{err}");
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

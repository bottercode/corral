//! Client connection to the server, with auto-start.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use tokio::net::UnixStream;

use crate::paths;
use crate::protocol::{self, ClientMsg, Command, PROTOCOL_VERSION, Reply, ServerMsg, Transport};

pub struct Conn {
    pub transport: Transport,
    next_id: u64,
}

impl Conn {
    /// Connect and handshake; fails if no server is listening.
    pub async fn connect(socket: &Path) -> Result<Conn> {
        let stream = UnixStream::connect(socket).await.with_context(|| {
            format!(
                "no corral server is running on {} (start one with `corral` or `corral server`)",
                socket.display()
            )
        })?;
        Self::handshake(stream).await
    }

    /// Connect, starting a background server first if none is running.
    pub async fn connect_or_start(socket: &Path) -> Result<Conn> {
        if let Ok(stream) = UnixStream::connect(socket).await {
            return Self::handshake(stream).await;
        }
        start_daemon(socket)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match UnixStream::connect(socket).await {
                Ok(stream) => return Self::handshake(stream).await,
                Err(e) if Instant::now() >= deadline => {
                    return Err(anyhow!(e)).with_context(|| {
                        format!(
                            "started a server but couldn't connect to {}; see {}",
                            socket.display(),
                            paths::log_dir().join("server.log").display()
                        )
                    });
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(40)).await,
            }
        }
    }

    async fn handshake(stream: UnixStream) -> Result<Conn> {
        let mut t = protocol::framed(stream);
        protocol::send(
            &mut t,
            &ClientMsg::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
        )
        .await?;
        match protocol::recv::<ServerMsg>(&mut t).await? {
            Some(ServerMsg::Hello {
                protocol_version, ..
            }) => protocol::check_version(protocol_version)?,
            Some(other) => bail!("unexpected first message from server: {other:?}"),
            None => bail!("server closed the connection during the handshake"),
        }
        Ok(Conn {
            transport: t,
            next_id: 1,
        })
    }

    pub async fn send(&mut self, msg: &ClientMsg) -> Result<()> {
        protocol::send(&mut self.transport, msg).await
    }

    pub async fn recv(&mut self) -> Result<Option<ServerMsg>> {
        protocol::recv(&mut self.transport).await
    }

    /// Send a command and wait for its reply, skipping unrelated messages.
    pub async fn request(&mut self, cmd: Command) -> Result<Reply> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&ClientMsg::Command { id, cmd }).await?;
        loop {
            match self.recv().await? {
                Some(ServerMsg::Reply { id: rid, result }) if rid == id => {
                    return result.map_err(|e| anyhow!(e));
                }
                Some(ServerMsg::Shutdown { reason }) => bail!("{reason}"),
                Some(_) => {}
                None => bail!("server closed the connection"),
            }
        }
    }
}

/// Start `corral server --daemon` in its own session, detached from this
/// terminal so it survives the client, the terminal and SSH hangups.
pub fn start_daemon(socket: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("finding the corral executable")?;
    // Panics and early errors land in the server log rather than nowhere.
    let log_dir = paths::log_dir();
    let stderr = std::fs::create_dir_all(&log_dir)
        .and_then(|_| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_dir.join("server.log"))
        })
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("server")
        .arg("--daemon")
        .env(paths::SOCKET_ENV, socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    // SAFETY: setsid is async-signal-safe and touches no Rust state.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("starting the corral server")?;
    Ok(())
}

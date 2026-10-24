//! Client connection to the server, with auto-start.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use tokio::net::UnixStream;

use crate::paths;
use crate::protocol::{self, ClientMsg, Command, PROTOCOL_VERSION, Reply, ServerMsg, Transport};

/// Stop the server listening on `socket` with a signal. For a server from
/// another corral version, which won't take `KillServer` from this one: the
/// system says which process is on the other end of the socket and, if it's
/// a corral of this user's, it gets SIGTERM, which every version handles by
/// saving its panes for restore and exiting. Returns its pid.
pub fn stop_by_signal(socket: &Path) -> Result<u32> {
    let stream = std::os::unix::net::UnixStream::connect(socket)
        .with_context(|| format!("no corral server is running on {}", socket.display()))?;
    let pid = peer_pid(&stream)?;
    drop(stream);
    if !is_our_corral(pid) {
        bail!(
            "the process on {} (pid {pid}) isn't a corral server of yours; not stopping it",
            socket.display()
        );
    }
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .with_context(|| format!("stopping the server (pid {pid})"))?;
    Ok(pid)
}

/// The pid of the process on the other end of a Unix socket.
fn peer_pid(stream: &std::os::unix::net::UnixStream) -> Result<u32> {
    use nix::sys::socket::{getsockopt, sockopt};
    #[cfg(target_os = "macos")]
    let pid = getsockopt(stream, sockopt::LocalPeerPid)?;
    #[cfg(not(target_os = "macos"))]
    let pid = getsockopt(stream, sockopt::PeerCredentials)?.pid();
    u32::try_from(pid).context("no process on the other end of the socket")
}

/// A process named corral, run by this user.
pub fn is_our_corral(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let pid = Pid::from_u32(pid);
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::Always)
            .with_user(UpdateKind::Always),
    );
    let Some(p) = sys.process(pid) else {
        return false;
    };
    let ours = p
        .user_id()
        .is_some_and(|u| **u == nix::unistd::getuid().as_raw());
    let named = |s: &std::ffi::OsStr| s.to_string_lossy().starts_with("corral");
    let corral = named(p.name()) || p.exe().and_then(|e| e.file_name()).is_some_and(named);
    ours && corral
}

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
        // Fail fast with the real reason instead of waiting on a server that can't start.
        paths::ensure_socket_dir(socket)?;
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

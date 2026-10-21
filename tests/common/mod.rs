//! Shared helpers: a real `corral server` on a private socket.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use corral::client::conn::Conn;
use corral::protocol::{ClientMsg, Command as Cmd, NewPane, PaneInfo, Reply, ServerMsg};

pub const BIN: &str = env!("CARGO_BIN_EXE_corral");

pub struct TestServer {
    pub dir: tempfile::TempDir,
    pub socket: PathBuf,
    pub home: PathBuf,
    child: Option<Child>,
}

impl TestServer {
    /// Start `corral server` in the foreground on a temp socket.
    pub fn start() -> Self {
        Self::start_with_config("")
    }

    pub fn start_with_config(config: &str) -> Self {
        let mut s = Self::unstarted(config);
        let child = Command::new(BIN)
            .arg("server")
            .envs(s.env())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawning corral server");
        s.child = Some(child);
        s.wait_for_socket();
        s
    }

    /// Paths and env without a running server (for auto-start tests).
    pub fn unstarted(config: &str) -> Self {
        let dir = tempfile::Builder::new().prefix("c").tempdir().unwrap();
        let socket = dir.path().join("s.sock");
        let home = dir.path().join("home");
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        TestServer {
            dir,
            socket,
            home,
            child: None,
        }
    }

    pub fn env(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            ("CORRAL_SOCKET", self.socket.clone()),
            ("CORRAL_HOME", self.home.clone()),
            ("CORRAL_CONFIG", self.dir.path().join("config.toml")),
        ]
    }

    pub fn wait_for_socket(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.socket.exists() {
            assert!(Instant::now() < deadline, "server didn't create its socket");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub async fn conn(&self) -> Conn {
        Conn::connect(&self.socket).await.expect("connecting")
    }

    /// Run the corral CLI against this server.
    pub fn cli(&self, args: &[&str]) -> Output {
        self.cli_in(args, None)
    }

    pub fn cli_in(&self, args: &[&str], cwd: Option<&Path>) -> Output {
        let mut c = Command::new(BIN);
        c.args(args)
            .envs(self.env())
            .env_remove("CORRAL_PANE")
            .stdin(Stdio::null());
        if let Some(d) = cwd {
            c.current_dir(d);
        }
        c.output().expect("running corral")
    }

    /// Run the CLI and return stdout, failing the test on a non-zero exit.
    pub fn ok(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(
            out.status.success(),
            "corral {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    pub fn panes(&self) -> Vec<PaneInfo> {
        serde_json::from_str(&self.ok(&["ls", "--json"])).unwrap()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = Command::new(BIN)
            .arg("kill-server")
            .envs(self.env())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Some(mut c) = self.child.take() {
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = c.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

pub fn sh(script: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), script.into()]
}

pub async fn new_pane(conn: &mut Conn, command: Vec<String>, cwd: &Path) -> PaneInfo {
    match conn
        .request(Cmd::New(NewPane {
            command,
            cwd: Some(cwd.to_path_buf()),
            ..Default::default()
        }))
        .await
        .unwrap()
    {
        Reply::Created { pane, .. } => pane,
        other => panic!("{other:?}"),
    }
}

/// A headless interactive client: attaches and mirrors its focused pane.
pub struct Headless {
    pub conn: Conn,
    pub parser: Option<vt100::Parser>,
    pub focus: Option<String>,
    pub panes: Vec<PaneInfo>,
    pub notes: Vec<String>,
    pub last_screen: Option<Vec<u8>>,
}

impl Headless {
    pub async fn attach(server: &TestServer, cols: u16, rows: u16) -> Self {
        let mut conn = server.conn().await;
        conn.send(&ClientMsg::Attach {
            cols,
            rows,
            workspace: None,
        })
        .await
        .unwrap();
        Headless {
            conn,
            parser: None,
            focus: None,
            panes: vec![],
            notes: vec![],
            last_screen: None,
        }
    }

    /// Look at one pane (clients start on the overview).
    pub async fn focus_on(&mut self, pane: &str) {
        self.parser = None;
        self.conn
            .send(&ClientMsg::Focus {
                pane: Some(pane.into()),
            })
            .await
            .unwrap();
        let p = pane.to_string();
        self.until("focus", move |c| {
            c.focus.as_deref() == Some(p.as_str()) && c.parser.is_some()
        })
        .await;
    }

    pub fn apply(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::State { panes, focus } => {
                self.panes = panes;
                self.focus = focus;
            }
            ServerMsg::Screen {
                cols, rows, bytes, ..
            } => {
                let mut p = vt100::Parser::new(rows, cols, 10_000);
                p.process(&bytes);
                self.parser = Some(p);
                self.last_screen = Some(bytes);
            }
            ServerMsg::Output { bytes, .. } => {
                if let Some(p) = self.parser.as_mut() {
                    p.process(&bytes);
                }
            }
            ServerMsg::Notify { message, .. } => self.notes.push(message),
            _ => {}
        }
    }

    pub fn contents(&self) -> String {
        self.parser
            .as_ref()
            .map(|p| p.screen().contents())
            .unwrap_or_default()
    }

    /// Process messages until `cond` holds.
    pub async fn until(&mut self, what: &str, mut cond: impl FnMut(&Self) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !cond(self) {
            let msg = tokio::time::timeout_at(deadline, self.conn.recv())
                .await
                .unwrap_or_else(|_| {
                    panic!("timed out waiting for {what}; screen:\n{}", self.contents())
                })
                .unwrap()
                .expect("server closed the connection");
            self.apply(msg);
        }
    }
}

/// Poll a CLI-visible condition.
pub fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

//! Shared helpers: a real `corral server` on a private socket.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use corral::client::conn::Conn;
use corral::protocol::{
    ArchivedPane, ClientMsg, Command as Cmd, NewPane, PaneInfo, Reply, Restorable, ServerMsg,
    Viewport,
};

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
        s.spawn(&[]);
        s
    }

    /// Start (or start again) the server, with extra environment.
    pub fn spawn(&mut self, extra: &[(&str, std::ffi::OsString)]) {
        let child = Command::new(BIN)
            .arg("server")
            .envs(self.env())
            .envs(extra.iter().map(|(k, v)| (*k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawning corral server");
        self.child = Some(child);
        self.wait_for_socket();
    }

    /// `corral kill-server`, waiting for the server to exit.
    pub fn stop(&mut self) {
        self.ok(&["kill-server"]);
        if let Some(mut c) = self.child.take() {
            let _ = c.wait();
        }
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
    /// The focused pane's screen.
    pub parser: Option<vt100::Parser>,
    parser_pane: Option<String>,
    /// Other panes on screen.
    pub others: HashMap<String, vt100::Parser>,
    pub size: (u16, u16),
    pub generation: u64,
    /// The last view generation the server says it applied.
    pub applied: u64,
    /// Sent with every view: something is drawn over the panes.
    pub covered: bool,
    pub focus: Option<String>,
    pub panes: Vec<PaneInfo>,
    /// Panes the server could bring back.
    pub restorable: Vec<Restorable>,
    /// Panes put away with `Archive`.
    pub archived: Vec<ArchivedPane>,
    pub seen_state: bool,
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
            parser_pane: None,
            others: HashMap::new(),
            size: (cols, rows),
            generation: 0,
            applied: 0,
            covered: false,
            focus: None,
            panes: vec![],
            restorable: vec![],
            archived: vec![],
            seen_state: false,
            notes: vec![],
            last_screen: None,
        }
    }

    /// Show one pane, filling the client.
    pub async fn focus_on(&mut self, pane: &str) {
        let (cols, rows) = self.size;
        self.view(pane, &[(pane, cols, rows)]).await;
    }

    /// Keep `pane` in view, but say whether something is drawn over it.
    pub async fn cover(&mut self, pane: &str, covered: bool) {
        let (cols, rows) = self.size;
        self.covered = covered;
        self.generation += 1;
        self.conn
            .send(&ClientMsg::View {
                generation: self.generation,
                focus: Some(pane.into()),
                panes: vec![Viewport {
                    pane: pane.into(),
                    cols,
                    rows,
                }],
                covered,
            })
            .await
            .unwrap();
        // Other connections (reports, commands) must see the new view.
        let generation = self.generation;
        self.until("view applied", move |c| c.applied >= generation)
            .await;
    }

    /// Show several panes at the given sizes, with the keyboard on `focus`.
    pub async fn view(&mut self, focus: &str, panes: &[(&str, u16, u16)]) {
        self.parser = None;
        self.parser_pane = None;
        self.others.clear();
        self.focus = Some(focus.into());
        self.generation += 1;
        self.conn
            .send(&ClientMsg::View {
                generation: self.generation,
                focus: Some(focus.into()),
                panes: panes
                    .iter()
                    .map(|(p, cols, rows)| Viewport {
                        pane: p.to_string(),
                        cols: *cols,
                        rows: *rows,
                    })
                    .collect(),
                covered: self.covered,
            })
            .await
            .unwrap();
        let (f, n) = (focus.to_string(), panes.len() - 1);
        self.until("view", move |c| {
            c.focus.as_deref() == Some(f.as_str()) && c.parser.is_some() && c.others.len() == n
        })
        .await;
    }

    pub fn apply(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::State {
                panes,
                focus,
                restorable,
                archived,
                view_generation,
                ..
            } => {
                self.applied = view_generation;
                self.restorable = restorable;
                self.archived = archived;
                self.seen_state = true;
                self.panes = panes;
                if focus.is_some() && self.parser_pane != focus {
                    // The server moved the keyboard (e.g. to a new pane).
                    if let (Some(old), Some(p)) = (self.parser_pane.take(), self.parser.take()) {
                        self.others.insert(old, p);
                    }
                    self.parser = focus.as_ref().and_then(|f| self.others.remove(f));
                    self.parser_pane = focus.clone().filter(|_| self.parser.is_some());
                }
                self.focus = focus;
            }
            ServerMsg::Screen {
                pane,
                cols,
                rows,
                bytes,
                ..
            } => {
                let mut p = vt100::Parser::new(rows, cols, 10_000);
                p.process(&bytes);
                if self.focus.is_none() || self.focus.as_ref() == Some(&pane) {
                    self.parser = Some(p);
                    self.parser_pane = Some(pane);
                    self.last_screen = Some(bytes);
                } else {
                    self.others.insert(pane, p);
                }
            }
            ServerMsg::Output { pane, bytes, .. } => {
                let target = if self.parser_pane.as_ref() == Some(&pane) {
                    self.parser.as_mut()
                } else {
                    self.others.get_mut(&pane)
                };
                if let Some(p) = target {
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

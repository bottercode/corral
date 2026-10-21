//! Drive the real TUI inside a PTY and read its screen back through vt100.

mod common;

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

struct Tui {
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl Tui {
    fn start(server: &TestServer, cols: u16, rows: u16) -> Tui {
        let pty = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut cmd = CommandBuilder::new(BIN);
        for (k, v) in server.env() {
            cmd.env(k, v);
        }
        cmd.env_remove("CORRAL_PANE");
        cmd.env("TERM", "xterm-256color");
        cmd.cwd(server.dir.path());
        let child = pty.slave.spawn_command(cmd).unwrap();
        drop(pty.slave);
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let mut reader = pty.master.try_clone_reader().unwrap();
        let p = parser.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                p.lock().unwrap().process(&buf[..n]);
            }
        });
        let writer = pty.master.take_writer().unwrap();
        Tui {
            parser,
            writer,
            child,
            _master: pty.master,
        }
    }

    fn screen(&self) -> String {
        self.parser.lock().unwrap().screen().contents()
    }

    fn type_(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    fn wait_for(&self, what: &str, cond: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let s = self.screen();
            if cond(&s) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; screen:\n{s}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[test]
fn overview_zoom_and_detach() {
    let s = TestServer::start();
    let dir = s.dir.path().to_str().unwrap().to_string();
    s.ok(&[
        "new",
        "--cwd",
        &dir,
        "--name",
        "first",
        "--",
        "sh",
        "-c",
        "echo hello-from-pane; cat",
    ]);
    s.ok(&[
        "new",
        "--cwd",
        &dir,
        "--name",
        "second",
        "--",
        "sh",
        "-c",
        "echo second-pane; cat",
    ]);

    let mut t = Tui::start(&s, 120, 32);
    // The overview: both agents as cards with live previews.
    t.wait_for("cards", |x| {
        x.contains("first")
            && x.contains("second")
            && x.contains("hello-from-pane")
            && x.contains("second-pane")
    });
    println!("{}", t.screen());

    // Keys on the overview drive corral; typing doesn't reach agents.
    t.type_(b"zzz");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!s.ok(&["read", "p1"]).contains("zzz"));

    // Enter opens the selected agent; typing goes to it.
    t.type_(b"\r");
    t.wait_for("zoomed", |x| x.contains("^b g grid"));
    t.type_(b"typed-in-tui\r");
    t.wait_for("echo", |x| x.matches("typed-in-tui").count() >= 2);

    // ^b j: next agent, still zoomed. ^b g: back to the overview.
    t.type_(b"\x02j");
    t.wait_for("second zoomed", |x| {
        x.contains("second-pane") && !x.contains("hello-from-pane")
    });
    t.type_(b"\x02g");
    t.wait_for("overview again", |x| {
        x.contains("hello-from-pane") && x.contains("second-pane")
    });

    // n opens the new-agent dialog; starting one opens it zoomed.
    t.type_(b"n");
    t.wait_for("dialog", |x| {
        x.contains("New agent") && x.contains("Command")
    });
    t.type_(b"\x1b");
    t.wait_for("dialog closed", |x| !x.contains("New agent"));
    t.type_(b"n");
    t.wait_for("dialog", |x| x.contains("New agent"));
    t.type_(b"\x15echo from-dialog; cat\r");
    t.wait_for("third agent zoomed", |x| {
        x.contains("from-dialog") && x.contains("^b g grid")
    });
    assert_eq!(s.panes().len(), 3);

    // Help, then close the zoomed agent: the client returns to the overview.
    t.type_(b"\x02?");
    t.wait_for("help", |x| x.contains("corral help"));
    t.type_(b"q");
    t.wait_for("help closed", |x| !x.contains("corral help"));
    t.type_(b"\x02x");
    t.wait_for("close dialog", |x| x.contains("Close p3"));
    t.type_(b"y");
    eventually("pane closed", || s.panes().len() == 2);
    t.wait_for("overview", |x| {
        x.contains("hello-from-pane") && !x.contains("from-dialog")
    });

    // q detaches from the overview; agents keep running.
    t.type_(b"q");
    let deadline = Instant::now() + Duration::from_secs(10);
    while t.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "client didn't exit on detach");
        std::thread::sleep(Duration::from_millis(50));
    }
    t.wait_for("detach message", |x| x.contains("detached from corral"));
    assert_eq!(s.panes().len(), 2);
    assert!(s.panes().iter().all(|p| p.exit_code.is_none()));
}

#[test]
fn clicking_a_card_selects_then_opens_it() {
    let s = TestServer::start();
    let dir = s.dir.path().to_str().unwrap().to_string();
    s.ok(&["new", "--cwd", &dir, "--", "sh", "-c", "echo pane-one; cat"]);
    s.ok(&["new", "--cwd", &dir, "--", "sh", "-c", "echo pane-two; cat"]);
    let mut t = Tui::start(&s, 120, 30);
    t.wait_for("cards", |x| {
        x.contains("pane-one") && x.contains("pane-two")
    });
    // Click inside the second card twice: select, then open.
    let screen = t.screen();
    let (row, col) = screen
        .lines()
        .enumerate()
        .find_map(|(r, l)| l.find("pane-two").map(|c| (r, l[..c].chars().count())))
        .unwrap();
    let click = format!(
        "\x1b[<0;{};{}M\x1b[<0;{};{}m",
        col + 2,
        row + 1,
        col + 2,
        row + 1
    );
    t.type_(click.as_bytes());
    std::thread::sleep(Duration::from_millis(200));
    t.type_(click.as_bytes());
    t.wait_for("pane two zoomed", |x| {
        x.contains("pane-two") && x.contains("^b g grid") && !x.contains("pane-one")
    });
}

#[test]
fn diff_review_commit_and_merge() {
    let s = TestServer::start();
    let repo = s.dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.name", "t"]);
    git(&["config", "user.email", "t@t.invalid"]);
    git(&["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("README"), "hello\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    // An "agent" that edits a file and waits.
    s.ok(&[
        "new",
        "--worktree",
        "--cwd",
        repo.to_str().unwrap(),
        "--name",
        "fixer",
        "--",
        "sh",
        "-c",
        "echo 'fixed line' >> README; echo agent-done; cat",
    ]);
    eventually("worktree stats", || {
        s.panes()[0].worktree.as_ref().is_some_and(|w| w.added == 1)
    });

    let mut t = Tui::start(&s, 120, 30);
    t.wait_for("card with diff stats", |x| x.contains("1 file +1 −0"));
    t.type_(b"d");
    t.wait_for("diff view", |x| {
        x.contains("README") && x.contains("+fixed line")
    });
    println!("{}", t.screen());
    // Commit with the suggested message, then merge into main.
    t.type_(b"c");
    t.wait_for("commit prompt", |x| x.contains("Work from fixer"));
    t.type_(b"\r");
    t.wait_for("committed", |x| {
        x.contains("committed") && x.contains("0 uncommitted")
    });
    t.type_(b"m");
    t.wait_for("merge prompt", |x| x.contains("Merge corral/"));
    t.type_(b"y");
    t.wait_for("merged", |x| x.contains("into main (1 commit)"));
    assert_eq!(
        std::fs::read_to_string(repo.join("README")).unwrap(),
        "hello\nfixed line\n"
    );
    t.type_(b"q");
    t.wait_for("overview", |x| x.contains("agent-done"));
}

/// Needs `claude` on PATH; run with `cargo test --test tui -- --ignored --nocapture`.
#[test]
#[ignore]
fn tui_renders_a_real_claude_pane() {
    let s = TestServer::start();
    let proj = s.dir.path().join("proj");
    std::fs::create_dir(&proj).unwrap();
    s.ok(&["new", "--cwd", proj.to_str().unwrap(), "--", "claude"]);
    let t = Tui::start(&s, 130, 32);
    t.wait_for("claude trust prompt", |x| x.contains("trust this folder"));
    eventually("detected", || {
        let p = &s.panes()[0];
        p.agent.as_deref() == Some("claude") && p.state == corral::protocol::AgentState::Blocked
    });
    std::thread::sleep(Duration::from_millis(500));
    println!("{}", t.screen());
    s.ok(&["kill", "p1"]);
}

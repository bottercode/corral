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
fn tui_shows_panes_forwards_input_and_detaches() {
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

    let mut t = Tui::start(&s, 110, 30);
    t.wait_for("sidebar and pane", |x| {
        x.contains("corral") && x.contains("hello-from-pane")
    });
    t.wait_for("both panes listed", |x| {
        x.contains("first") && x.contains("second")
    });
    println!("{}", t.screen());

    // Typing goes to the focused pane (cat echoes it back).
    t.type_(b"typed-in-tui\r");
    t.wait_for("echo", |x| x.matches("typed-in-tui").count() >= 2);

    // Prefix j switches to the next pane.
    t.type_(b"\x02j");
    t.wait_for("second pane", |x| x.contains("second-pane"));

    // Prefix s shows the other agent alongside: both on screen (stacked, as
    // 80 columns is too narrow for side by side), each sized to its half,
    // and typing goes to the focused one.
    t.type_(b"\x02s");
    t.wait_for("side by side", |x| {
        x.contains("second-pane") && x.contains("hello-from-pane") && x.contains("typed-in-tui")
    });
    eventually("panes sized to their tiles", || {
        s.panes().iter().all(|p| p.cols == 80 && p.rows < 15)
    });
    println!("{}", t.screen());
    t.type_(b"in-split\r");
    eventually("typed into the focused tile", || {
        s.ok(&["read", "p1"]).matches("in-split").count() >= 2
    });
    // Prefix o: back to one pane.
    t.type_(b"\x02o");
    eventually("pane fills the screen again", || {
        s.panes().iter().any(|p| p.rows > 25)
    });

    // Prefix n opens the new-agent dialog; Esc closes it.
    t.type_(b"\x02n");
    t.wait_for("dialog", |x| {
        x.contains("New agent") && x.contains("Command")
    });
    println!("{}", t.screen());
    t.type_(b"\x1b");
    t.wait_for("dialog closed", |x| !x.contains("New agent"));

    // Start a pane from the dialog: it gets focus.
    t.type_(b"\x02n");
    t.wait_for("dialog", |x| x.contains("New agent"));
    t.type_(b"\x15echo from-dialog; cat\r");
    t.wait_for("third pane", |x| x.contains("from-dialog"));
    assert_eq!(s.panes().len(), 3);

    // Help overlay.
    t.type_(b"\x02?");
    t.wait_for("help", |x| x.contains("corral keys"));
    t.type_(b"q");
    t.wait_for("help closed", |x| !x.contains("corral keys"));

    // Close the focused pane with prefix x, confirm with y.
    t.type_(b"\x02x");
    t.wait_for("close dialog", |x| x.contains("Close p3"));
    t.type_(b"y");
    eventually("pane closed", || s.panes().len() == 2);

    // Detach: the client exits, the agents keep running.
    t.type_(b"\x02d");
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
fn tui_mouse_click_focuses_a_sidebar_row() {
    let s = TestServer::start();
    let dir = s.dir.path().to_str().unwrap().to_string();
    s.ok(&["new", "--cwd", &dir, "--", "sh", "-c", "echo pane-one; cat"]);
    s.ok(&["new", "--cwd", &dir, "--", "sh", "-c", "echo pane-two; cat"]);
    let mut t = Tui::start(&s, 110, 30);
    t.wait_for("pane one", |x| x.contains("pane-one"));
    // Find the row of the second pane in the sidebar ("2 sh").
    let screen = t.screen();
    let row = screen
        .lines()
        .position(|l| l.trim_start().starts_with("2 sh"))
        .unwrap_or_else(|| panic!("no second row:\n{screen}"));
    t.type_(format!("\x1b[<0;5;{}M\x1b[<0;5;{}m", row + 1, row + 1).as_bytes());
    t.wait_for("pane two", |x| x.contains("pane-two"));
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
    t.wait_for("sidebar with diff stats", |x| {
        x.contains("+1 −0") && x.contains("agent-done")
    });
    t.type_(b"\x02D");
    t.wait_for("diff view", |x| {
        x.contains("README") && x.contains("+fixed line")
    });
    println!("{}", t.screen());
    // Commit with the suggested message, then merge into main.
    t.type_(b"c");
    t.wait_for("commit prompt", |x| x.contains("Work from fixer"));
    t.type_(b"\r");
    t.wait_for("committed", |x| x.contains("0 uncommitted"));
    t.type_(b"m");
    t.wait_for("merge prompt", |x| x.contains("Merge corral/"));
    t.type_(b"y");
    t.wait_for("merged", |x| x.contains("into main (1 commit)"));
    assert_eq!(
        std::fs::read_to_string(repo.join("README")).unwrap(),
        "hello\nfixed line\n"
    );
    // q closes the diff; typing goes to the agent again.
    t.type_(b"q");
    t.wait_for("agent again", |x| {
        x.contains("agent-done") && !x.contains("+fixed line")
    });
}

#[test]
fn new_agent_button_starts_an_agent() {
    let s = TestServer::start();
    let dir = s.dir.path().to_str().unwrap().to_string();
    s.ok(&[
        "new",
        "--cwd",
        &dir,
        "--",
        "sh",
        "-c",
        "echo first-agent; cat",
    ]);
    let mut t = Tui::start(&s, 120, 30);
    t.wait_for("button", |x| {
        x.contains("+ new agent") && x.contains("first-agent")
    });
    // Click the "+ new agent" button where it's drawn.
    let screen = t.screen();
    let (row, col) = screen
        .lines()
        .enumerate()
        .find_map(|(r, l)| l.find("+ new agent").map(|c| (r, l[..c].chars().count())))
        .unwrap();
    t.type_(
        format!(
            "\x1b[<0;{};{}M\x1b[<0;{};{}m",
            col + 2,
            row + 1,
            col + 2,
            row + 1
        )
        .as_bytes(),
    );
    t.wait_for("dialog", |x| {
        x.contains("New agent") && x.contains("Command")
    });
    t.type_(b"\x15echo from-button; cat\r");
    t.wait_for("new agent focused", |x| x.contains("from-button"));
    assert_eq!(s.panes().len(), 2);
}

fn click_text(t: &mut Tui, text: &str) {
    let screen = t.screen();
    let (row, col) = screen
        .lines()
        .enumerate()
        .find_map(|(r, l)| l.find(text).map(|c| (r, l[..c].chars().count())))
        .unwrap_or_else(|| panic!("{text:?} not on screen:\n{screen}"));
    t.type_(
        format!(
            "\x1b[<0;{};{}M\x1b[<0;{};{}m",
            col + 2,
            row + 1,
            col + 2,
            row + 1
        )
        .as_bytes(),
    );
}

fn wait_exit(t: &mut Tui) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while t.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "client didn't exit");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn exit_button_detaches_or_stops_corral() {
    let s = TestServer::start();
    let dir = s.dir.path().to_str().unwrap().to_string();
    s.ok(&[
        "new",
        "--cwd",
        &dir,
        "--",
        "sh",
        "-c",
        "echo agent-one; cat",
    ]);

    // ✕ exit → d: the window closes, the agent keeps running.
    let mut t = Tui::start(&s, 120, 30);
    t.wait_for("exit button", |x| {
        x.contains("✕ exit") && x.contains("agent-one")
    });
    click_text(&mut t, "✕ exit");
    t.wait_for("exit menu", |x| {
        x.contains("Exit corral") && x.contains("stop corral")
    });
    t.type_(b"d");
    wait_exit(&mut t);
    t.wait_for("detach message", |x| x.contains("detached from corral"));
    assert_eq!(s.panes().len(), 1);

    // ✕ exit → s: the server and its agents stop.
    let mut t = Tui::start(&s, 120, 30);
    t.wait_for("exit button", |x| x.contains("✕ exit"));
    click_text(&mut t, "✕ exit");
    t.wait_for("exit menu", |x| x.contains("Exit corral"));
    t.type_(b"s");
    wait_exit(&mut t);
    t.wait_for("stopped message", |x| {
        x.contains("corral stopped; 1 agent ended")
    });
    eventually("socket removed", || !s.socket.exists());
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

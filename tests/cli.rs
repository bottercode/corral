//! The CLI is the API for scripts and agents: ls/new/send/read/wait/kill.

mod common;

use common::*;
use corral::protocol::AgentState;

#[test]
fn new_send_read_roundtrip() {
    let s = TestServer::start();
    let dir = s.dir.path().to_str().unwrap().to_string();
    assert_eq!(
        s.ok(&["new", "--cwd", &dir, "--name", "echoer", "--", "cat"])
            .trim(),
        "p1"
    );

    s.ok(&["send", "p1", "hello corral", "--enter"]);
    eventually("echo", || {
        s.ok(&["read", "p1"]).matches("hello corral").count() >= 2
    });

    // Panes resolve by bare number or by name too.
    s.ok(&["send", "1", "again", "--enter"]);
    eventually("again", || {
        s.ok(&["read", "echoer", "--lines", "2"]).contains("again")
    });
    let last = s.ok(&["read", "p1", "--lines", "1"]);
    assert_eq!(last.lines().count(), 1, "{last:?}");

    let json: serde_json::Value = serde_json::from_str(&s.ok(&["read", "p1", "--json"])).unwrap();
    assert_eq!(json["pane"], "p1");
    assert!(json["text"].as_str().unwrap().contains("hello corral"));
}

#[test]
fn ls_json_describes_panes() {
    let s = TestServer::start();
    let dir = s.dir.path().to_str().unwrap().to_string();
    s.ok(&[
        "new",
        "--cwd",
        &dir,
        "--workspace",
        "ws",
        "--",
        "sh",
        "-c",
        "read x",
    ]);
    let panes = s.panes();
    assert_eq!(panes.len(), 1);
    let p = &panes[0];
    assert_eq!(p.id, "p1");
    assert_eq!(p.workspace, "ws");
    assert_eq!(p.name, "sh");
    assert_eq!(p.command, vec!["sh", "-c", "read x"]);
    assert!(p.pid.is_some());
    let table = s.ok(&["ls"]);
    assert!(table.starts_with("ID"), "{table}");
}

#[test]
fn wait_for_exit_and_timeout() {
    let s = TestServer::start();
    let dir = s.dir.path().to_str().unwrap().to_string();
    s.ok(&["new", "--cwd", &dir, "--", "sh", "-c", "sleep 0.5; exit 3"]);
    let out = s.ok(&["wait", "p1", "--state", "exited", "--timeout", "10"]);
    assert_eq!(out.trim(), "exited");
    assert_eq!(s.panes()[0].exit_code, Some(3));

    s.ok(&["new", "--cwd", &dir, "--", "sh", "-c", "read x"]);
    let out = s.cli(&["wait", "p2", "--state", "exited", "--timeout", "0.3"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn kill_rename_and_errors() {
    let s = TestServer::start();
    let dir = s.dir.path().to_str().unwrap().to_string();
    s.ok(&["new", "--cwd", &dir, "--", "sh", "-c", "read x"]);
    s.ok(&["rename", "p1", "worker"]);
    assert_eq!(s.panes()[0].name, "worker");
    assert!(s.ok(&["kill", "worker"]).contains("closed p1"));
    assert!(s.panes().is_empty());

    let out = s.cli(&["read", "p9"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no pane"));

    // Sending to an exited pane is an error, not silence.
    s.ok(&["new", "--cwd", &dir, "--", "true"]);
    s.ok(&["wait", "p2", "--state", "exited", "--timeout", "10"]);
    assert!(!s.cli(&["send", "p2", "x"]).status.success());
}

#[test]
fn report_overrides_heuristics_and_wait_sees_it() {
    let s = TestServer::start();
    let dir = s.dir.path().to_str().unwrap().to_string();
    s.ok(&["new", "--cwd", &dir, "--", "sh", "-c", "read x"]);
    s.ok(&["report", "--state", "blocked", "--pane", "p1"]);
    assert_eq!(s.panes()[0].state, AgentState::Blocked);
    assert_eq!(
        s.ok(&["wait", "p1", "--state", "blocked", "--timeout", "5"])
            .trim(),
        "blocked"
    );
    s.ok(&["report", "--state", "done", "--pane", "p1"]);
    assert_eq!(s.panes()[0].state, AgentState::Done);

    // A hook outside corral (no $CORRAL_PANE) is a silent no-op.
    assert!(
        s.cli(&["report", "--hook", "--state", "blocked"])
            .status
            .success()
    );
    // Without --hook it's an error.
    assert!(!s.cli(&["report", "--state", "blocked"]).status.success());
}

#[test]
fn report_from_inside_a_pane_uses_its_environment() {
    let s = TestServer::start();
    let dir = s.dir.path().to_str().unwrap().to_string();
    // The hook command corral installs, run by the pane itself.
    let hook = corral::integrate::hook_command("blocked");
    let script = format!("{hook}; read x");
    s.ok(&["new", "--cwd", &dir, "--", "sh", "-c", &script]);
    eventually("hook report", || s.panes()[0].state == AgentState::Blocked);
}

#[test]
fn new_autostarts_a_daemon() {
    let s = TestServer::unstarted("");
    let dir = s.dir.path().to_str().unwrap().to_string();
    // No server yet: read fails, new starts one.
    assert!(!s.cli(&["ls"]).status.success());
    // stdout is only the id, so `p=$(corral new …)` works in scripts.
    let out = s.ok(&["new", "--cwd", &dir, "--", "sh", "-c", "read x"]);
    assert_eq!(out.trim(), "p1");
    assert_eq!(s.panes().len(), 1);
    assert!(s.home.join("log").join("server.log").exists());
    s.ok(&["kill-server"]);
    assert!(!s.socket.exists());
}

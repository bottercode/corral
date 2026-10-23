//! The server owns the PTYs: clients can attach, detach and reattach.

mod common;

use common::*;
use corral::protocol::{AgentState, ClientMsg, Command, Reply};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn attach_detach_reattach_keeps_the_screen() {
    let server = TestServer::start();
    let mut conn = server.conn().await;
    let pane = new_pane(&mut conn, sh("echo hi; read x"), server.dir.path()).await;
    assert_eq!(pane.id, "p1");

    let mut a = Headless::attach(&server, 80, 24).await;
    a.focus_on("p1").await;
    a.until("hi on screen", |c| c.contents().contains("hi"))
        .await;
    let first = a.contents();

    // Detach: the process keeps running.
    a.conn.send(&ClientMsg::Detach).await.unwrap();
    drop(a);
    assert!(server.panes()[0].exit_code.is_none());

    let mut b = Headless::attach(&server, 80, 24).await;
    b.focus_on("p1").await;
    assert_eq!(b.contents(), first);

    // The pane is still interactive after reattaching.
    b.conn
        .send(&ClientMsg::Input {
            pane: "p1".into(),
            bytes: b"bye\r".to_vec(),
        })
        .await
        .unwrap();
    b.until("pane exits", |c| {
        c.panes.first().is_some_and(|p| p.exit_code == Some(0))
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_includes_scrollback_and_resize_follows_latest_client() {
    let server = TestServer::start();
    let mut conn = server.conn().await;
    new_pane(
        &mut conn,
        sh("i=0; while [ $i -lt 100 ]; do echo line$i; i=$((i+1)); done; read x"),
        server.dir.path(),
    )
    .await;

    let mut a = Headless::attach(&server, 60, 10).await;
    a.focus_on("p1").await;
    a.until("all lines", |c| c.contents().contains("line99"))
        .await;
    // History came along with the snapshot or the stream.
    let p = a.parser.as_mut().unwrap();
    p.screen_mut().set_scrollback(usize::MAX);
    assert!(
        p.screen().scrollback() >= 90,
        "scrollback {}",
        p.screen().scrollback()
    );

    // A second, larger client becomes active by typing; the pane follows it.
    let mut b = Headless::attach(&server, 100, 30).await;
    b.focus_on("p1").await;
    b.conn
        .send(&ClientMsg::Input {
            pane: "p1".into(),
            bytes: vec![],
        })
        .await
        .unwrap();
    b.until("pane resized", |c| {
        c.panes
            .first()
            .is_some_and(|p| p.cols == 100 && p.rows == 30)
    })
    .await;
    a.until("first client gets a resized snapshot", |c| {
        c.parser
            .as_ref()
            .is_some_and(|p| p.screen().size() == (30, 100))
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn answers_cursor_position_queries() {
    let server = TestServer::start();
    let mut conn = server.conn().await;
    // Ask for the cursor position like crossterm does and print the reply.
    let script = r#"printf 'ab'; stty raw -echo; printf '\033[6n'; r=$(dd bs=1 count=6 2>/dev/null); stty sane; printf '\nreply:%s\n' "$(printf %s "$r" | tr -d '\033')"; read x"#;
    new_pane(&mut conn, sh(script), server.dir.path()).await;
    eventually("cursor report", || {
        server.ok(&["read", "p1"]).contains("reply:[1;3R")
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn panes_get_their_id_and_socket_in_the_environment() {
    let server = TestServer::start();
    let mut conn = server.conn().await;
    new_pane(
        &mut conn,
        sh("echo id=$CORRAL_PANE; echo sock=$CORRAL_SOCKET; echo cc=${CLAUDECODE:-unset}; read x"),
        server.dir.path(),
    )
    .await;
    eventually("env printed", || {
        server.ok(&["read", "p1"]).contains("id=p1")
    });
    let text = server.ok(&["read", "p1"]);
    assert!(
        text.contains(&format!("sock={}", server.socket.display())),
        "{text}"
    );
    // Agent session markers from whoever started the server don't leak in.
    assert!(text.contains("cc=unset"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn version_mismatch_is_refused() {
    use corral::protocol::{self, ServerMsg};
    let server = TestServer::start();
    let stream = tokio::net::UnixStream::connect(&server.socket)
        .await
        .unwrap();
    let mut t = protocol::framed(stream);
    protocol::send(
        &mut t,
        &ClientMsg::Hello {
            protocol_version: 999,
        },
    )
    .await
    .unwrap();
    // The server says who it is, then hangs up.
    let hello: ServerMsg = protocol::recv(&mut t).await.unwrap().unwrap();
    assert!(matches!(
        hello,
        ServerMsg::Hello {
            protocol_version: protocol::PROTOCOL_VERSION,
            ..
        }
    ));
    assert!(protocol::recv::<ServerMsg>(&mut t).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn notifications_only_for_unfocused_panes() {
    let server = TestServer::start();
    let mut conn = server.conn().await;
    new_pane(&mut conn, sh("read x"), server.dir.path()).await;
    new_pane(&mut conn, sh("read x"), server.dir.path()).await;

    let mut a = Headless::attach(&server, 80, 24).await;
    a.focus_on("p1").await;

    // Blocked on the focused pane: no notification.
    let r = conn
        .request(Command::Report {
            pane: "p1".into(),
            state: AgentState::Blocked,
            session: None,
        })
        .await
        .unwrap();
    assert_eq!(r, Reply::Ok);
    // Blocked on the other pane: notification.
    conn.request(Command::Report {
        pane: "p2".into(),
        state: AgentState::Blocked,
        session: None,
    })
    .await
    .unwrap();
    a.until("notification", |c| !c.notes.is_empty()).await;
    assert_eq!(a.notes.len(), 1);
    assert!(a.notes[0].contains("p2"), "{:?}", a.notes);
}

#[tokio::test(flavor = "multi_thread")]
async fn covered_panes_still_need_you() {
    let server = TestServer::start();
    let mut conn = server.conn().await;
    new_pane(&mut conn, sh("echo hi; read x"), server.dir.path()).await;

    let mut a = Headless::attach(&server, 80, 24).await;
    a.focus_on("p1").await;
    // The client draws its own page over p1.
    a.cover("p1", true).await;

    // p1 finishes: it isn't seen, so it stays done and the client is told.
    conn.request(Command::Report {
        pane: "p1".into(),
        state: AgentState::Done,
        session: None,
    })
    .await
    .unwrap();
    a.until("notification", |c| !c.notes.is_empty()).await;
    a.until("p1 done", |c| {
        c.panes
            .iter()
            .any(|p| p.id == "p1" && p.state == AgentState::Done)
    })
    .await;

    // Uncovered, the user sees it.
    a.cover("p1", false).await;
    a.until("p1 seen", |c| {
        c.panes
            .iter()
            .any(|p| p.id == "p1" && p.state == AgentState::Idle)
    })
    .await;
}

/// The session record, once it mentions `what`.
fn wait_for_record(server: &TestServer, what: &str) -> String {
    let dir = server.home.join("sessions");
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let text: String = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .filter_map(|e| std::fs::read_to_string(e.ok()?.path()).ok())
            .collect();
        if text.contains(what) {
            return text;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "record never mentioned {what}: {text}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A stand-in for claude that prints how it was started, then waits.
fn fake_agents(server: &TestServer) -> Vec<(&'static str, std::ffi::OsString)> {
    let bin = server.dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    std::fs::write(
        &claude,
        "#!/bin/sh\necho \"claude started: $*\"\nwhile read x; do :; done\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    vec![
        ("PATH", path.into()),
        ("SHELL", "/bin/sh".into()),
        // Never look at the real ~/.claude.
        (
            "CLAUDE_CONFIG_DIR",
            server.dir.path().join("claude-home").into(),
        ),
        ("CODEX_HOME", server.dir.path().join("codex-home").into()),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn panes_come_back_after_a_restart() {
    let mut server = TestServer::unstarted("");
    let env = fake_agents(&server);
    server.spawn(&env);
    let (a, b) = (server.dir.path().join("a"), server.dir.path().join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let mut conn = server.conn().await;
    new_pane(
        &mut conn,
        vec!["claude".into(), "--model".into(), "haiku".into()],
        &a,
    )
    .await;
    new_pane(&mut conn, sh("echo some job; read x"), &b).await;
    wait_for_record(&server, "--model");
    drop(conn);

    server.stop();
    server.spawn(&env);

    // A client hears what can come back.
    let mut c = Headless::attach(&server, 80, 24).await;
    c.until("restorable", |c| c.restorable.len() == 2).await;
    assert_eq!(c.restorable[0].agent.as_deref(), Some("claude"));
    assert_eq!(c.restorable[0].how, "continues its latest conversation");
    assert_eq!(c.restorable[1].how, "shell");

    let mut conn = server.conn().await;
    let r = conn
        .request(Command::Restore { discard: false })
        .await
        .unwrap();
    assert_eq!(r, Reply::Text("brought back 2 panes".into()));
    c.until("decided", |c| c.restorable.is_empty()).await;

    let panes = server.panes();
    assert_eq!(panes.len(), 2);
    // claude resumes with the flags it had; the job comes back as a shell
    // in its folder, not run again.
    let claude = panes
        .iter()
        .find(|p| p.cwd == a.canonicalize().unwrap())
        .unwrap();
    let shell = panes
        .iter()
        .find(|p| p.cwd == b.canonicalize().unwrap())
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let out = server.ok(&["read", &claude.id]);
        if out.contains("claude started: --model haiku --continue") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "claude screen: {out}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!server.ok(&["read", &shell.id]).contains("some job"));

    // Asking twice: nothing left.
    let r = conn.request(Command::Restore { discard: false }).await;
    assert!(r.unwrap_err().to_string().contains("nothing to restore"));
}

#[tokio::test(flavor = "multi_thread")]
async fn archived_panes_keep_through_a_restart_and_come_back() {
    let mut server = TestServer::unstarted("");
    let env = fake_agents(&server);
    server.spawn(&env);
    let a = server.dir.path().join("a");
    std::fs::create_dir_all(&a).unwrap();
    let mut conn = server.conn().await;
    let p = new_pane(
        &mut conn,
        vec!["claude".into(), "--model".into(), "haiku".into()],
        &a,
    )
    .await;
    // Once corral has seen claude in it.
    wait_for_record(&server, "--model");

    let r = conn.request(Command::Archive { pane: p.id }).await.unwrap();
    assert_eq!(r, Reply::Text("archived claude".into()));
    assert!(server.panes().is_empty());
    let mut c = Headless::attach(&server, 80, 24).await;
    c.until("archived", |c| c.archived.len() == 1).await;
    assert_eq!(c.archived[0].pane.agent.as_deref(), Some("claude"));
    assert_eq!(c.archived[0].pane.how, "continues its latest conversation");
    drop(c);

    // A restart keeps it archived, not something to restore.
    server.stop();
    server.spawn(&env);
    let mut c = Headless::attach(&server, 80, 24).await;
    c.until("archived", |c| c.seen_state && c.archived.len() == 1)
        .await;
    assert!(c.restorable.is_empty());

    let mut conn = server.conn().await;
    let id = c.archived[0].id;
    let pane = match conn.request(Command::Unarchive { id }).await.unwrap() {
        Reply::Created { pane, .. } => pane,
        other => panic!("{other:?}"),
    };
    assert_eq!(pane.cwd, a.canonicalize().unwrap());
    c.until("unarchived", |c| c.archived.is_empty()).await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let out = server.ok(&["read", &pane.id]);
        if out.contains("claude started: --model haiku --continue") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "claude screen: {out}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Archive again and forget it: gone for good.
    conn.request(Command::Archive { pane: pane.id })
        .await
        .unwrap();
    c.until("archived", |c| c.archived.len() == 1).await;
    let id = c.archived[0].id;
    let r = conn.request(Command::ForgetArchived { id }).await.unwrap();
    assert_eq!(r, Reply::Text("forgot claude".into()));
    c.until("forgotten", |c| c.archived.is_empty()).await;
    let r = conn.request(Command::Unarchive { id }).await;
    assert!(r.unwrap_err().to_string().contains("nothing archived"));
}

#[tokio::test(flavor = "multi_thread")]
async fn starting_fresh_lets_the_old_panes_go() {
    let mut server = TestServer::unstarted("");
    let env = fake_agents(&server);
    server.spawn(&env);
    let mut conn = server.conn().await;
    new_pane(&mut conn, sh("read x"), server.dir.path()).await;
    wait_for_record(&server, "\"name\"");
    drop(conn);
    server.stop();
    server.spawn(&env);

    assert_eq!(server.ok(&["restore", "--discard"]).trim(), "started fresh");
    assert!(server.panes().is_empty());
    // And they stay gone after another restart.
    wait_for_record(&server, "\"panes\": []");
    server.stop();
    server.spawn(&env);
    let mut c = Headless::attach(&server, 80, 24).await;
    c.until("state", |c| c.seen_state).await;
    assert!(c.restorable.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_from_another_version_can_still_be_stopped() {
    // `kill-server` falls back to this when the server speaks another
    // protocol version: find it through the socket, and signal it.
    let server = TestServer::start();
    let mut conn = server.conn().await;
    new_pane(&mut conn, sh("read x"), server.dir.path()).await;
    wait_for_record(&server, "\"name\"");
    drop(conn);
    let pid = corral::client::conn::stop_by_signal(&server.socket).unwrap();
    assert_eq!(Some(pid), server.pid());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while server.socket.exists() {
        assert!(std::time::Instant::now() < deadline, "server didn't stop");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // It shut down as usual: its pane is kept to bring back.
    wait_for_record(&server, "\"name\": \"sh\"");
}

#[test]
fn only_corral_servers_get_signalled() {
    // Something else listening on the socket (here, this test itself).
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("other.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let err = corral::client::conn::stop_by_signal(&socket).unwrap_err();
    assert!(err.to_string().contains("isn't a corral server"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn client_attached_to_an_empty_server_shows_the_first_new_pane() {
    let server = TestServer::start();
    let mut a = Headless::attach(&server, 80, 24).await;
    a.until("state", |c| c.focus.is_none()).await;

    // Another shell starts a pane; the idle client picks it up on its own.
    let mut conn = server.conn().await;
    new_pane(&mut conn, sh("echo started; read x"), server.dir.path()).await;
    a.until("focus moves to p1 with its screen", |c| {
        c.focus.as_deref() == Some("p1") && c.contents().contains("started")
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn several_panes_on_screen_stream_side_by_side() {
    let server = TestServer::start();
    let mut conn = server.conn().await;
    new_pane(
        &mut conn,
        sh("read x; echo left-$x; read y"),
        server.dir.path(),
    )
    .await;
    new_pane(
        &mut conn,
        sh("read x; echo right-$x; read y"),
        server.dir.path(),
    )
    .await;

    let mut a = Headless::attach(&server, 121, 30).await;
    a.view("p1", &[("p1", 60, 29), ("p2", 60, 29)]).await;
    // Each pane is sized to its tile.
    a.until("tile sizes", |c| {
        c.panes.iter().all(|p| (p.cols, p.rows) == (60, 29))
    })
    .await;

    // Output from both arrives live.
    server.ok(&["send", "p1", "a", "--enter"]);
    server.ok(&["send", "p2", "b", "--enter"]);
    a.until("both outputs", |c| {
        c.contents().contains("left-a") && c.others["p2"].screen().contents().contains("right-b")
    })
    .await;

    // Neither is notified about: both are on screen.
    for pane in ["p1", "p2"] {
        conn.request(Command::Report {
            pane: pane.into(),
            state: AgentState::Blocked,
            session: None,
        })
        .await
        .unwrap();
    }
    a.until("blocked shown", |c| {
        c.panes.iter().all(|p| p.state == AgentState::Blocked)
    })
    .await;
    assert!(a.notes.is_empty(), "{:?}", a.notes);

    // Closing the focused pane moves the keyboard to the other one.
    server.ok(&["kill", "p1"]);
    a.until("focus moves to p2", |c| c.focus.as_deref() == Some("p2"))
        .await;
    assert!(
        a.parser
            .as_ref()
            .unwrap()
            .screen()
            .contents()
            .contains("right-b")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_can_hand_over_to_the_shell() {
    use corral::protocol::NewPane;
    let server = TestServer::start();
    let mut conn = server.conn().await;
    let r = conn
        .request(Command::New(NewPane {
            command: vec!["echo ran-$((1+2))".into()],
            shell: true,
            then_shell: true,
            cwd: Some(server.dir.path().to_path_buf()),
            ..Default::default()
        }))
        .await
        .unwrap();
    assert!(matches!(r, Reply::Created { .. }), "{r:?}");
    eventually("command ran", || {
        server.ok(&["read", "p1"]).contains("ran-3")
    });
    // The command is done but the pane isn't: a shell took over.
    server.ok(&["send", "p1", "echo still-$((3+3))", "--enter"]);
    eventually("shell answers", || {
        server.ok(&["read", "p1"]).contains("still-6")
    });
    assert!(server.panes()[0].exit_code.is_none());
}

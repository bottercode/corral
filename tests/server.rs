//! The server owns the PTYs: clients can attach, detach and reattach.

mod common;

use common::*;
use corral::protocol::{AgentState, ClientMsg, Command, Reply};

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
        })
        .await
        .unwrap();
    assert_eq!(r, Reply::Ok);
    // Blocked on the other pane: notification.
    conn.request(Command::Report {
        pane: "p2".into(),
        state: AgentState::Blocked,
    })
    .await
    .unwrap();
    a.until("notification", |c| !c.notes.is_empty()).await;
    assert_eq!(a.notes.len(), 1);
    assert!(a.notes[0].contains("p2"), "{:?}", a.notes);
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

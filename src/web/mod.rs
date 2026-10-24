//! `corral web`: corral on your phone. A small web app, served from this
//! binary, that you add to the phone's home screen: who needs you, Approve
//! and Deny for blocked agents, notes, a terminal for any agent, and push
//! notifications. It talks to the corral server like any other client.
//!
//! It listens on 127.0.0.1 only; phones reach it through `tailscale serve`
//! (or another HTTPS proxy), which also gives it the HTTPS that home-screen
//! apps and notifications need. Every request but the pairing one needs a
//! paired device's cookie (see [`auth`]).

pub mod auth;
pub mod bridge;
pub mod push;

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use base64::Engine;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};

use crate::paths;
use auth::Devices;
use bridge::{Action, ActionTx, Snapshot, Term, TermIn, TermOut};
use push::Vapid;

pub const DEFAULT_PORT: u16 = 7777;
const MAX_HEAD: usize = 16 * 1024;
const MAX_BODY: usize = 64 * 1024;
/// How often open connections check that their device is still paired.
const RECHECK: Duration = Duration::from_secs(5);

pub struct Options {
    pub addr: SocketAddr,
    /// What phones open, e.g. `https://mac.tail1234.ts.net`.
    pub url: String,
}

/// Shared by every connection.
pub struct App {
    feed: watch::Sender<Snapshot>,
    actions: ActionTx,
    dir: PathBuf,
    devices: Mutex<(Devices, Option<SystemTime>)>,
    vapid: Vapid,
    /// The host in `Options::url`, which `Origin` may name.
    public_host: String,
}

impl App {
    fn device(&self, token: &str) -> Option<String> {
        let g = self.devices.lock().unwrap_or_else(|e| e.into_inner());
        g.0.find(token).map(|d| d.name.clone())
    }

    /// Change the device list and save it.
    fn edit_devices<T>(&self, f: impl FnOnce(&mut Devices) -> T) -> Result<T> {
        let mut g = self.devices.lock().unwrap_or_else(|e| e.into_inner());
        let out = f(&mut g.0);
        g.0.save(&self.dir)?;
        g.1 = modified(&Devices::path(&self.dir));
        Ok(out)
    }

    /// Pick up changes made by `corral web revoke`.
    pub fn refresh_devices(&self) {
        let path = Devices::path(&self.dir);
        let mtime = modified(&path);
        let mut g = self.devices.lock().unwrap_or_else(|e| e.into_inner());
        if mtime != g.1 {
            match Devices::load(&self.dir) {
                Ok(d) => *g = (d, mtime),
                Err(e) => tracing::warn!("{e:#}"),
            }
        }
    }

    /// Wake every phone that turned notifications on.
    pub fn notify(self: &App) {
        let endpoints: Vec<String> = {
            let g = self.devices.lock().unwrap_or_else(|e| e.into_inner());
            g.0.list.iter().filter_map(|d| d.push.clone()).collect()
        };
        for endpoint in endpoints {
            let vapid = self.vapid.clone();
            let dir = self.dir.clone();
            tokio::task::spawn_blocking(move || match vapid.send(&endpoint) {
                Ok(true) => {}
                Ok(false) => {
                    // The phone dropped the subscription.
                    if let Ok(mut d) = Devices::load(&dir) {
                        for dev in d
                            .list
                            .iter_mut()
                            .filter(|d| d.push.as_deref() == Some(&endpoint))
                        {
                            dev.push = None;
                        }
                        let _ = d.save(&dir);
                    }
                }
                Err(e) => tracing::warn!("push failed: {e:#}"),
            });
        }
    }

    async fn act(&self, action: Action) -> Result<String> {
        let (tx, rx) = oneshot::channel();
        self.actions
            .send((action, tx))
            .await
            .context("the bridge stopped")?;
        rx.await.context("the bridge stopped")?
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Serve the phone app until the process is stopped.
pub async fn run(opts: Options) -> Result<()> {
    let dir = paths::web_dir();
    auth::private_dir(&dir)?;
    let vapid = Vapid::load_or_create(&dir)?;
    let devices = Devices::load(&dir)?;
    let mtime = modified(&Devices::path(&dir));
    let listener = TcpListener::bind(opts.addr)
        .await
        .with_context(|| format!("listening on {}", opts.addr))?;
    let (actions, actions_rx) = tokio::sync::mpsc::channel(16);
    let app = Arc::new(App {
        feed: watch::Sender::new(Snapshot::default()),
        actions,
        dir: dir.clone(),
        devices: Mutex::new((devices, mtime)),
        vapid,
        public_host: host_of(&opts.url).to_string(),
    });
    tokio::spawn(bridge::run(app.clone(), actions_rx));
    tracing::info!(addr = %opts.addr, url = %opts.url, "corral web listening");

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let app = app.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle(app, stream).await {
                            tracing::debug!("request failed: {e:#}");
                        }
                    });
                }
                Err(e) => tracing::warn!("accept failed: {e}"),
            },
            _ = tokio::signal::ctrl_c() => break,
            _ = term.recv() => break,
        }
    }
    Ok(())
}

// ---- HTTP ------------------------------------------------------------------

struct Request {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn query(&self, key: &str) -> Option<&str> {
        self.query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    }

    fn token(&self) -> Option<&str> {
        self.header("cookie").and_then(auth::cookie_token)
    }

    /// Came from a page of ours, not another site.
    fn same_origin(&self, app: &App) -> bool {
        let hosts = [
            self.header("host").unwrap_or(""),
            self.header("x-forwarded-host").unwrap_or(""),
            app.public_host.as_str(),
        ];
        auth::origin_allowed(self.header("origin"), &hosts)
    }

    /// The phone reached us over HTTPS (through a proxy).
    fn https(&self) -> bool {
        self.header("x-forwarded-proto") == Some("https")
            || self
                .header("origin")
                .is_some_and(|o| o.starts_with("https://"))
    }
}

/// Read one request. `None` if the client went away or sent nonsense.
async fn read_request(stream: &mut TcpStream) -> Result<Option<Request>> {
    let mut buf = Vec::with_capacity(2048);
    let head_len = loop {
        let mut chunk = [0u8; 2048];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_HEAD {
            return Ok(None);
        }
    };
    let mut headers = [httparse::EMPTY_HEADER; 48];
    let mut parsed = httparse::Request::new(&mut headers);
    if !matches!(
        parsed.parse(&buf[..head_len]),
        Ok(httparse::Status::Complete(_))
    ) {
        return Ok(None);
    }
    let target = parsed.path.unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let headers: Vec<(String, String)> = parsed
        .headers
        .iter()
        .map(|h| {
            (
                h.name.to_string(),
                String::from_utf8_lossy(h.value).into_owned(),
            )
        })
        .collect();
    let mut req = Request {
        method: parsed.method.unwrap_or("").to_string(),
        path: path.to_string(),
        query: query.to_string(),
        headers,
        body: buf[head_len..].to_vec(),
    };
    let len: usize = req
        .header("content-length")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        return Ok(None);
    }
    while req.body.len() < len {
        let mut chunk = vec![0u8; len - req.body.len()];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        req.body.extend_from_slice(&chunk[..n]);
    }
    req.body.truncate(len);
    Ok(Some(req))
}

struct Response {
    status: &'static str,
    content_type: &'static str,
    headers: Vec<String>,
    body: Vec<u8>,
}

impl Response {
    fn new(status: &'static str, content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Response {
            status,
            content_type,
            headers: Vec::new(),
            body: body.into(),
        }
    }

    fn json(v: serde_json::Value) -> Self {
        Self::new("200 OK", "application/json", v.to_string())
    }

    fn error(status: &'static str, message: &str) -> Self {
        Self::new(
            status,
            "application/json",
            serde_json::json!({ "error": message }).to_string(),
        )
    }

    fn header(mut self, h: String) -> Self {
        self.headers.push(h);
        self
    }

    fn bytes(&self) -> Vec<u8> {
        let mut head = format!(
            "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\
             Cache-Control: no-cache\r\nX-Content-Type-Options: nosniff\r\n\
             Referrer-Policy: no-referrer\r\nX-Frame-Options: DENY\r\n\
             Content-Security-Policy: default-src 'self'; style-src 'self' 'unsafe-inline'; \
             img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'\r\n",
            self.status,
            self.content_type,
            self.body.len()
        );
        for h in &self.headers {
            head.push_str(h);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

/// The app's files, built into the binary.
fn asset(path: &str) -> Option<(&'static str, &'static [u8])> {
    Some(match path {
        "/" | "/index.html" | "/pair" => (
            "text/html; charset=utf-8",
            include_bytes!("assets/index.html"),
        ),
        "/app.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("assets/app.js"),
        ),
        "/app.css" => ("text/css; charset=utf-8", include_bytes!("assets/app.css")),
        "/sw.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("assets/sw.js"),
        ),
        "/manifest.webmanifest" => (
            "application/manifest+json",
            include_bytes!("assets/manifest.webmanifest"),
        ),
        "/icon-192.png" => ("image/png", include_bytes!("assets/icon-192.png")),
        "/icon-512.png" => ("image/png", include_bytes!("assets/icon-512.png")),
        "/apple-touch-icon.png" => ("image/png", include_bytes!("assets/apple-touch-icon.png")),
        "/vendor/xterm.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("assets/vendor/xterm.js"),
        ),
        "/vendor/xterm.css" => (
            "text/css; charset=utf-8",
            include_bytes!("assets/vendor/xterm.css"),
        ),
        "/vendor/addon-fit.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("assets/vendor/addon-fit.js"),
        ),
        _ => return None,
    })
}

async fn handle(app: Arc<App>, mut stream: TcpStream) -> Result<()> {
    let Ok(Ok(Some(req))) =
        tokio::time::timeout(Duration::from_secs(15), read_request(&mut stream)).await
    else {
        return Ok(());
    };
    let device = req.token().and_then(|t| app.device(t));
    let res = match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/ws") | ("GET", "/ws/term") => {
            if device.is_none() {
                Response::error("401 Unauthorized", "not paired")
            } else if !req.same_origin(&app) {
                Response::error("403 Forbidden", "wrong origin")
            } else {
                return upgrade(app, req, stream).await;
            }
        }
        ("GET", path) if asset(path).is_some() => {
            let (ty, body) = asset(path).unwrap_or(("text/plain", b""));
            Response::new("200 OK", ty, body)
        }
        ("GET", "/api/me") => Response::json(serde_json::json!({
            "paired": device.is_some(),
            "name": device,
            "push_key": app.vapid.public_key(),
            "version": env!("CARGO_PKG_VERSION"),
        })),
        ("GET", "/api/needs") if device.is_some() => {
            let snap = app.feed.borrow();
            Response::json(serde_json::json!({ "agents": snap.needs() }))
        }
        ("POST", _) if !req.same_origin(&app) => Response::error("403 Forbidden", "wrong origin"),
        ("POST", "/api/pair") => pair(&app, &req),
        ("POST", "/api/unpair") if device.is_some() => {
            let name = device.unwrap_or_default();
            app.edit_devices(|d| d.revoke(&name))?;
            Response::json(serde_json::json!({})).header(format!(
                "Set-Cookie: {}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict",
                auth::COOKIE
            ))
        }
        ("POST", "/api/push") if device.is_some() => subscribe(&app, &req),
        (_, p) if p.starts_with("/api/") => Response::error("401 Unauthorized", "not paired"),
        _ => Response::error("404 Not Found", "not found"),
    };
    stream.write_all(&res.bytes()).await?;
    let _ = stream.shutdown().await;
    Ok(())
}

fn pair(app: &App, req: &Request) -> Response {
    #[derive(Deserialize)]
    struct Body {
        code: String,
        #[serde(default)]
        name: String,
    }
    let Ok(body) = serde_json::from_slice::<Body>(&req.body) else {
        return Response::error("400 Bad Request", "bad request");
    };
    if !auth::redeem(&app.dir, body.code.trim(), SystemTime::now()) {
        return Response::error(
            "403 Forbidden",
            "That code is used or expired. Run `corral web pair` for a new one.",
        );
    }
    let added = app.edit_devices(|d| {
        let token = d.add(&body.name, SystemTime::now())?;
        let name = d.find(&token).map(|d| d.name.clone()).unwrap_or_default();
        anyhow::Ok((token, name))
    });
    let (token, name) = match added.and_then(|r| r) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("pairing failed: {e:#}");
            return Response::error("500 Internal Server Error", "couldn't save the device");
        }
    };
    tracing::info!(device = %name, "paired");
    let secure = if req.https() { "; Secure" } else { "" };
    Response::json(serde_json::json!({ "name": name })).header(format!(
        "Set-Cookie: {}={token}; Path=/; Max-Age=31536000; HttpOnly; SameSite=Strict{secure}",
        auth::COOKIE
    ))
}

fn subscribe(app: &App, req: &Request) -> Response {
    #[derive(Deserialize)]
    struct Body {
        endpoint: Option<String>,
    }
    let Ok(body) = serde_json::from_slice::<Body>(&req.body) else {
        return Response::error("400 Bad Request", "bad request");
    };
    if let Some(e) = &body.endpoint
        && push::audience(e).is_err()
    {
        return Response::error("400 Bad Request", "not a push endpoint");
    }
    let token = req.token().unwrap_or("");
    match app.edit_devices(|d| {
        d.find_mut(token)
            .map(|dev| dev.push = body.endpoint.clone())
    }) {
        Ok(_) => Response::json(serde_json::json!({})),
        Err(e) => {
            tracing::warn!("{e:#}");
            Response::error("500 Internal Server Error", "couldn't save")
        }
    }
}

// ---- WebSockets --------------------------------------------------------------

fn accept_key(key: &str) -> String {
    let mut h = Sha1::new();
    h.update(key.trim().as_bytes());
    h.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64::engine::general_purpose::STANDARD.encode(h.finalize())
}

async fn upgrade(app: Arc<App>, req: Request, mut stream: TcpStream) -> Result<()> {
    let is_ws = req
        .header("upgrade")
        .is_some_and(|u| u.eq_ignore_ascii_case("websocket"));
    let Some(key) = req.header("sec-websocket-key").filter(|_| is_ws) else {
        let res = Response::error("400 Bad Request", "expected a WebSocket");
        stream.write_all(&res.bytes()).await?;
        return Ok(());
    };
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n",
        accept_key(key)
    );
    stream.write_all(head.as_bytes()).await?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(1 << 20))
        .max_frame_size(Some(1 << 20));
    let ws = WebSocketStream::from_raw_socket(stream, Role::Server, Some(config)).await;
    let token = req.token().unwrap_or("").to_string();
    if req.path == "/ws" {
        control(app, ws, token).await
    } else {
        let pane = req.query("pane").unwrap_or("").to_string();
        let num = |k: &str, d: u16| req.query(k).and_then(|v| v.parse().ok()).unwrap_or(d);
        terminal(app, ws, token, pane, num("cols", 80), num("rows", 24)).await
    }
}

/// The home screen's connection: the latest snapshot whenever it changes,
/// and actions with their results.
async fn control(app: Arc<App>, mut ws: WebSocketStream<TcpStream>, token: String) -> Result<()> {
    #[derive(Deserialize)]
    struct Req {
        id: u64,
        #[serde(flatten)]
        action: Action,
    }
    let state = |s: &Snapshot| {
        let mut v = serde_json::to_value(s).unwrap_or_default();
        v["t"] = "state".into();
        Message::text(v.to_string())
    };
    let mut feed = app.feed.subscribe();
    let first = state(&feed.borrow_and_update());
    ws.send(first).await?;
    let mut check = tokio::time::interval(RECHECK);
    loop {
        tokio::select! {
            changed = feed.changed() => {
                if changed.is_err() {
                    break;
                }
                let msg = state(&feed.borrow_and_update());
                ws.send(msg).await?;
            }
            msg = ws.next() => match msg {
                Some(Ok(Message::Text(text))) => {
                    let reply = match serde_json::from_str::<Req>(text.as_str()) {
                        Ok(r) => {
                            let result = app.act(r.action).await;
                            serde_json::json!({
                                "t": "done",
                                "id": r.id,
                                "ok": result.is_ok(),
                                "message": match &result { Ok(m) => m.clone(), Err(e) => format!("{e:#}") },
                            })
                        }
                        Err(e) => serde_json::json!({ "t": "done", "ok": false, "message": e.to_string() }),
                    };
                    ws.send(Message::text(reply.to_string())).await?;
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
            _ = check.tick() => {
                if app.device(&token).is_none() {
                    break;
                }
            }
        }
    }
    let _ = ws.close(None).await;
    Ok(())
}

/// One pane's terminal: screen and output as binary frames, keys and
/// resizes back.
async fn terminal(
    app: Arc<App>,
    mut ws: WebSocketStream<TcpStream>,
    token: String,
    pane: String,
    cols: u16,
    rows: u16,
) -> Result<()> {
    let mut term = match Term::open(pane, cols, rows).await {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!("terminal: {e:#}");
            ws.send(Message::binary(TermOut::End.frame())).await?;
            let _ = ws.close(None).await;
            return Ok(());
        }
    };
    let mut check = tokio::time::interval(RECHECK);
    loop {
        tokio::select! {
            out = term.next() => {
                let out = out.unwrap_or(TermOut::End);
                let end = matches!(out, TermOut::End);
                ws.send(Message::binary(out.frame())).await?;
                if end {
                    break;
                }
            }
            msg = ws.next() => match msg {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(m) = serde_json::from_str::<TermIn>(text.as_str()) {
                        term.input(m).await?;
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
            _ = check.tick() => {
                if app.device(&token).is_none() {
                    break;
                }
            }
        }
    }
    term.close().await;
    let _ = ws.close(None).await;
    Ok(())
}

// ---- CLI ---------------------------------------------------------------------

/// `https://host:port/x` → `host:port`.
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split('/').next().unwrap_or("")
}

/// `https://<this machine>.<tailnet>.ts.net`, if Tailscale is installed and up.
pub fn tailscale_url() -> Option<String> {
    for bin in [
        "tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    ] {
        let Ok(out) = std::process::Command::new(bin)
            .args(["status", "--json"])
            .stderr(std::process::Stdio::null())
            .output()
        else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        let name = v["Self"]["DNSName"].as_str()?.trim_end_matches('.');
        if !name.is_empty() {
            return Some(format!("https://{name}"));
        }
    }
    None
}

/// The address phones open: `--url`, else Tailscale's name for this
/// machine, else the address we listen on.
pub fn public_url(url: Option<String>, addr: SocketAddr) -> String {
    url.map(|u| u.trim_end_matches('/').to_string())
        .or_else(tailscale_url)
        .unwrap_or_else(|| format!("http://{addr}"))
}

fn url_path() -> PathBuf {
    paths::web_dir().join("url")
}

fn pid_path() -> PathBuf {
    paths::web_dir().join("pid")
}

/// A QR code drawn with half blocks, black on white whatever the
/// terminal's colours, so phone cameras read it.
pub fn qr(text: &str) -> Result<String> {
    let code = qrcode::QrCode::new(text.as_bytes()).context("making the QR code")?;
    let w = code.width() as isize;
    let colors = code.to_colors();
    let dark = |x: isize, y: isize| {
        (0..w).contains(&x)
            && (0..w).contains(&y)
            && colors[(y * w + x) as usize] == qrcode::Color::Dark
    };
    let quiet = 2;
    let shade = |d: bool| if d { 16 } else { 231 };
    let mut out = String::new();
    let mut y = -quiet;
    while y < w + quiet {
        out.push_str("  ");
        for x in -quiet..w + quiet {
            out.push_str(&format!(
                "\x1b[38;5;{}m\x1b[48;5;{}m▀",
                shade(dark(x, y)),
                shade(dark(x, y + 1))
            ));
        }
        out.push_str("\x1b[0m\n");
        y += 2;
    }
    Ok(out)
}

/// Make a pairing code and show how to use it.
pub fn print_pairing(url: &str) -> Result<()> {
    let dir = paths::web_dir();
    let code = auth::new_pairing(&dir, SystemTime::now())?;
    let link = format!("{url}/pair#{code}");
    println!("\nScan with your phone's camera, or open:\n  {link}\n");
    print!("{}", qr(&link)?);
    println!(
        "\nThe code works once, for {} minutes. `corral web pair` makes another.",
        auth::PAIR_TTL.as_secs() / 60
    );
    Ok(())
}

pub fn print_banner(addr: SocketAddr, url: &str) -> Result<()> {
    println!("corral web is listening on http://{addr}");
    if url.starts_with("https://") && url.ends_with(".ts.net") {
        println!(
            "\nPhones reach it through Tailscale. If you haven't already, run once:\n  tailscale serve --bg {}",
            addr.port()
        );
    } else if url.starts_with("http://") {
        println!(
            "\nWithout HTTPS a phone can open the page but can't install it or get\n\
             notifications. The easy way to get HTTPS is Tailscale (free):\n\
             install it on this machine and the phone, then run\n  tailscale serve --bg {}\n\
             and start `corral web` again.",
            addr.port()
        );
    }
    print_pairing(url)
}

/// Is a `corral web` from this user running? Its pid.
pub fn running_pid() -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(pid_path())
        .ok()?
        .trim()
        .parse()
        .ok()?;
    crate::client::conn::is_our_corral(pid).then_some(pid)
}

/// Run in the foreground; the pid file lets `corral web stop` find a
/// detached one.
pub fn serve(addr: SocketAddr, url: String, detached: bool) -> Result<()> {
    let dir = paths::web_dir();
    auth::private_dir(&dir)?;
    if let Some(pid) = running_pid().filter(|&p| p != std::process::id()) {
        bail!("corral web is already running (pid {pid}); `corral web stop` stops it");
    }
    auth::write_private(&url_path(), url.as_bytes())?;
    auth::write_private(&pid_path(), std::process::id().to_string().as_bytes())?;
    if !detached {
        print_banner(addr, &url)?;
        println!("\nCtrl-C stops it.");
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = rt.block_on(run(Options { addr, url }));
    let _ = std::fs::remove_file(pid_path());
    result
}

/// Start `corral web` in the background with the same settings.
pub fn spawn_daemon(args: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().context("finding the corral executable")?;
    let log_dir = paths::log_dir();
    std::fs::create_dir_all(&log_dir)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("web.log"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("web")
        .args(args)
        .arg("--detached")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe and touches no Rust state.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().context("starting corral web")?;
    Ok(())
}

pub fn stop() -> Result<()> {
    let Some(pid) = running_pid() else {
        bail!("corral web isn't running");
    };
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .with_context(|| format!("stopping corral web (pid {pid})"))?;
    println!("stopped corral web (pid {pid})");
    Ok(())
}

/// The URL the running (or last) `corral web` gave phones.
pub fn last_url() -> Option<String> {
    std::fs::read_to_string(url_path())
        .ok()
        .map(|s| s.trim().to_string())
}

pub fn list_devices() -> Result<()> {
    let d = Devices::load(&paths::web_dir())?;
    if d.list.is_empty() {
        println!("No phones paired. Run `corral web` to pair one.");
    }
    for dev in &d.list {
        let notes = if dev.push.is_some() {
            "notifications on"
        } else {
            ""
        };
        println!("{}  {notes}", dev.name);
    }
    Ok(())
}

pub fn revoke(name: &str) -> Result<()> {
    let dir = paths::web_dir();
    let mut d = Devices::load(&dir)?;
    if !d.revoke(name) {
        bail!("no phone named {name:?} (see `corral web devices`)");
    }
    d.save(&dir)?;
    println!("unpaired {name}");
    Ok(())
}

/// Non-loopback addresses reach the network without TLS.
pub fn check_bind(ip: IpAddr, expose: bool) -> Result<()> {
    if !ip.is_loopback() && !expose {
        bail!(
            "listening on {ip} lets anyone on that network open the pairing page, over plain HTTP.\n\
             Prefer `tailscale serve`; pass --expose if you mean it"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_accept_key() {
        // RFC 6455's example.
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn hosts() {
        assert_eq!(host_of("https://mac.tail.ts.net"), "mac.tail.ts.net");
        assert_eq!(host_of("http://127.0.0.1:7777/x"), "127.0.0.1:7777");
    }

    #[test]
    fn binds() {
        assert!(check_bind("127.0.0.1".parse().unwrap(), false).is_ok());
        assert!(check_bind("0.0.0.0".parse().unwrap(), false).is_err());
        assert!(check_bind("0.0.0.0".parse().unwrap(), true).is_ok());
    }

    #[test]
    fn qr_draws_square_halves() {
        let q = qr("https://x/pair#abc").unwrap();
        assert!(q.lines().count() > 10);
        assert!(q.contains('▀'));
    }

    #[test]
    fn assets_are_served() {
        for p in [
            "/",
            "/pair",
            "/app.js",
            "/sw.js",
            "/manifest.webmanifest",
            "/vendor/xterm.js",
        ] {
            assert!(asset(p).is_some_and(|(_, b)| !b.is_empty()), "{p}");
        }
        assert!(asset("/../Cargo.toml").is_none());
    }
}

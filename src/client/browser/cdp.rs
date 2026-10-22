//! The browser engine: the user's own Chrome (or Chromium, Edge, Brave),
//! headless, driven over the Chrome DevTools Protocol. The page streams
//! frames as PNGs (`Page.startScreencast`) whenever it changes; clicks,
//! scrolling and keys go back as input events.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use super::{BrowserCmd, BrowserEvent, KeyInput};

/// Chrome's usual homes, most likely first.
const CANDIDATES: &[&str] = &[
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
    "brave-browser",
    "microsoft-edge",
];

/// The browser to run: `configured` if set, else the first one installed.
pub fn find_chrome(configured: &str) -> Result<PathBuf> {
    if !configured.trim().is_empty() {
        let p = crate::paths::expand_tilde(configured.trim());
        return resolve(&p).ok_or_else(|| anyhow!("browser {configured:?} wasn't found"));
    }
    CANDIDATES
        .iter()
        .find_map(|c| resolve(Path::new(c)))
        .ok_or_else(|| {
            anyhow!(
                "no Chrome, Chromium, Brave or Edge found; install one, or set `browser = \"/path/to/chrome\"` in the config"
            )
        })
}

/// An absolute path that exists, or a bare name found on `$PATH`.
fn resolve(p: &Path) -> Option<PathBuf> {
    if p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(p))
        .find(|c| c.is_file())
}

/// `DevTools listening on ws://…` is how Chrome says where to connect.
pub fn devtools_url(line: &str) -> Option<&str> {
    line.trim()
        .strip_prefix("DevTools listening on ")
        .filter(|u| u.starts_with("ws://"))
}

/// A DevTools connection: calls get their reply; events arrive on a channel.
struct Cdp {
    out: mpsc::UnboundedSender<Message>,
    pending: std::sync::Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    next_id: u64,
}

impl Cdp {
    async fn connect(url: &str) -> Result<(Cdp, mpsc::UnboundedReceiver<Value>)> {
        // Frames are big PNGs.
        let config = WebSocketConfig::default()
            .max_message_size(Some(256 << 20))
            .max_frame_size(Some(256 << 20));
        let (ws, _) = tokio_tungstenite::connect_async_with_config(url, Some(config), false)
            .await
            .context("connecting to the browser")?;
        let (mut sink, mut stream) = ws.split();
        let (out, mut out_rx) = mpsc::unbounded_channel::<Message>();
        let (events_tx, events) = mpsc::unbounded_channel();
        let pending: std::sync::Arc<
            std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>,
        > = Default::default();
        tokio::spawn(async move {
            while let Some(m) = out_rx.recv().await {
                if sink.send(m).await.is_err() {
                    break;
                }
            }
        });
        let p = pending.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let Message::Text(text) = msg else { continue };
                let Ok(v) = serde_json::from_str::<Value>(text.as_str()) else {
                    continue;
                };
                match v["id"].as_u64() {
                    Some(id) => {
                        let reply = match v.get("error") {
                            Some(e) => Err(anyhow!("{}", e["message"].as_str().unwrap_or("error"))),
                            None => Ok(v["result"].clone()),
                        };
                        if let Some(tx) = p.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) {
                            let _ = tx.send(reply);
                        }
                    }
                    None => {
                        if events_tx.send(v).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Ok((
            Cdp {
                out,
                pending,
                next_id: 1,
            },
            events,
        ))
    }

    fn send(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> oneshot::Receiver<Result<Value>> {
        let id = self.next_id;
        self.next_id += 1;
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        let _ = self.out.send(Message::text(msg.to_string()));
        rx
    }

    /// Send and wait for the reply.
    async fn call(&mut self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        let rx = self.send(method, params, session);
        match tokio::time::timeout(Duration::from_secs(20), rx).await {
            Ok(Ok(r)) => r.with_context(|| method.to_string()),
            Ok(Err(_)) => bail!("the browser went away"),
            Err(_) => bail!("the browser didn't answer {method}"),
        }
    }

    /// Send without waiting (input, acks).
    fn fire(&mut self, method: &str, params: Value, session: Option<&str>) {
        drop(self.send(method, params, session));
    }
}

/// The page's size: CSS pixels and how many device pixels each has.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
    pub scale: f64,
}

/// Run a browser until `Quit` or the channel closes. Events (frames, the
/// page's address and title, errors) go to `events`.
pub async fn run(
    chrome: PathBuf,
    profile: PathBuf,
    url: String,
    viewport: Viewport,
    mut cmds: mpsc::UnboundedReceiver<BrowserCmd>,
    events: mpsc::UnboundedSender<BrowserEvent>,
) {
    if let Err(e) = drive(chrome, profile, url, viewport, &mut cmds, &events).await {
        let _ = events.send(BrowserEvent::Failed(format!("{e:#}")));
    }
}

async fn launch(chrome: &Path, profile: &Path) -> Result<(tokio::process::Child, String)> {
    std::fs::create_dir_all(profile).with_context(|| format!("creating {}", profile.display()))?;
    let mut child = tokio::process::Command::new(chrome)
        .args([
            "--headless=new",
            "--remote-debugging-port=0",
            "--no-first-run",
            "--no-default-browser-check",
            "--mute-audio",
            "--hide-scrollbars",
        ])
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {}", chrome.display()))?;
    let stderr = child.stderr.take().context("no stderr from the browser")?;
    let mut lines = BufReader::new(stderr).lines();
    let find = async {
        let mut seen = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(u) = devtools_url(&line) {
                return Ok((u.to_string(), lines));
            }
            seen.push(line);
        }
        bail!("the browser quit: {}", seen.join(" / "))
    };
    let (url, mut lines) = tokio::time::timeout(Duration::from_secs(20), find)
        .await
        .map_err(|_| anyhow!("the browser didn't start within 20s"))??;
    // Keep draining its log so it never blocks on a full pipe.
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    Ok((child, url))
}

async fn drive(
    chrome: PathBuf,
    profile: PathBuf,
    url: String,
    mut viewport: Viewport,
    cmds: &mut mpsc::UnboundedReceiver<BrowserCmd>,
    events: &mpsc::UnboundedSender<BrowserEvent>,
) -> Result<()> {
    let (_child, ws) = match launch(&chrome, &profile).await {
        Ok(x) => x,
        // Another corral has the profile open: use a throwaway one.
        Err(_) if profile.join("SingletonLock").exists() => {
            let tmp = std::env::temp_dir().join(format!("corral-browser-{}", std::process::id()));
            launch(&chrome, &tmp).await?
        }
        Err(e) => return Err(e),
    };
    let (mut cdp, mut cdp_events) = Cdp::connect(&ws).await?;
    cdp.call(
        "Target.setDiscoverTargets",
        json!({ "discover": true }),
        None,
    )
    .await?;
    let target = cdp
        .call("Target.createTarget", json!({ "url": "about:blank" }), None)
        .await?["targetId"]
        .as_str()
        .context("no target id")?
        .to_string();
    let session = cdp
        .call(
            "Target.attachToTarget",
            json!({ "targetId": target, "flatten": true }),
            None,
        )
        .await?["sessionId"]
        .as_str()
        .context("no session id")?
        .to_string();
    let s = Some(session.as_str());
    cdp.call("Page.enable", json!({}), s).await?;
    // A person is driving this browser: it says it's the Chrome it is, not
    // "HeadlessChrome" (which sites take for a bot).
    if let Ok(v) = cdp.call("Browser.getVersion", json!({}), None).await
        && let Some(ua) = v["userAgent"].as_str()
    {
        let ua = ua.replace("HeadlessChrome", "Chrome");
        cdp.call(
            "Emulation.setUserAgentOverride",
            json!({ "userAgent": ua }),
            s,
        )
        .await?;
    }
    resize(&mut cdp, s, viewport).await?;
    cdp.call("Page.navigate", json!({ "url": url }), s).await?;
    // Titles set by scripts aren't announced: ask after the page changes.
    let mut changed = false;
    let mut ask = tokio::time::interval(Duration::from_millis(500));
    let mut known = (String::new(), String::new());

    loop {
        tokio::select! {
            _ = ask.tick(), if changed => {
                changed = false;
                let r = cdp
                    .call(
                        "Runtime.evaluate",
                        json!({ "expression": "[location.href, document.title]", "returnByValue": true }),
                        s,
                    )
                    .await?;
                let v = &r["result"]["value"];
                let now = (
                    v[0].as_str().unwrap_or("").to_string(),
                    v[1].as_str().unwrap_or("").to_string(),
                );
                if now != known {
                    known = now.clone();
                    let _ = events.send(BrowserEvent::Page { url: now.0, title: now.1 });
                }
            }
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    BrowserCmd::Quit => break,
                    BrowserCmd::Navigate(url) => {
                        cdp.call("Page.navigate", json!({ "url": url }), s).await?;
                    }
                    BrowserCmd::Back | BrowserCmd::Forward => {
                        let back = matches!(cmd, BrowserCmd::Back);
                        let h = cdp.call("Page.getNavigationHistory", json!({}), s).await?;
                        let i = h["currentIndex"].as_i64().unwrap_or(0) + if back { -1 } else { 1 };
                        if let Some(entry) = h["entries"].as_array().and_then(|e| e.get(i.max(0) as usize))
                            && i >= 0
                        {
                            cdp.fire("Page.navigateToHistoryEntry", json!({ "entryId": entry["id"] }), s);
                        }
                    }
                    BrowserCmd::Reload => cdp.fire("Page.reload", json!({}), s),
                    BrowserCmd::Resize(v) if v != viewport => {
                        viewport = v;
                        resize(&mut cdp, s, viewport).await?;
                    }
                    BrowserCmd::Resize(_) => {}
                    BrowserCmd::Click { x, y } => {
                        for kind in ["mousePressed", "mouseReleased"] {
                            cdp.fire(
                                "Input.dispatchMouseEvent",
                                json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1 }),
                                s,
                            );
                        }
                    }
                    BrowserCmd::Scroll { x, y, dy } => cdp.fire(
                        "Input.dispatchMouseEvent",
                        json!({ "type": "mouseWheel", "x": x, "y": y, "deltaX": 0, "deltaY": dy }),
                        s,
                    ),
                    BrowserCmd::Text(text) => cdp.fire("Input.insertText", json!({ "text": text }), s),
                    BrowserCmd::Key(k) => {
                        for e in key_events(&k) {
                            cdp.fire("Input.dispatchKeyEvent", e, s);
                        }
                    }
                }
            }
            ev = cdp_events.recv() => {
                let Some(ev) = ev else { bail!("the browser closed") };
                let params = &ev["params"];
                match ev["method"].as_str().unwrap_or("") {
                    "Page.screencastFrame" => {
                        cdp.fire(
                            "Page.screencastFrameAck",
                            json!({ "sessionId": params["sessionId"] }),
                            s,
                        );
                        if let Some(data) = params["data"].as_str() {
                            let _ = events.send(BrowserEvent::Frame(data.to_string()));
                        }
                        changed = true;
                    }
                    "Target.targetInfoChanged" => {
                        let info = &params["targetInfo"];
                        if info["targetId"].as_str() == Some(target.as_str()) {
                            known = (
                                info["url"].as_str().unwrap_or("").to_string(),
                                info["title"].as_str().unwrap_or("").to_string(),
                            );
                            let _ = events.send(BrowserEvent::Page {
                                url: known.0.clone(),
                                title: known.1.clone(),
                            });
                        } else if info["openerId"].as_str() == Some(target.as_str())
                            && let Some(u) = info["url"].as_str().filter(|u| *u != "about:blank" && !u.is_empty())
                        {
                            // A link that opens a new window: open it here
                            // instead.
                            cdp.fire("Target.closeTarget", json!({ "targetId": info["targetId"] }), None);
                            cdp.fire("Page.navigate", json!({ "url": u }), s);
                        }
                    }
                    "Page.frameStartedLoading" => { let _ = events.send(BrowserEvent::Loading(true)); }
                    "Page.frameStoppedLoading" | "Page.loadEventFired" => {
                        let _ = events.send(BrowserEvent::Loading(false));
                    }
                    // alert/confirm would stop the page; accept them.
                    "Page.javascriptDialogOpening" => cdp.fire(
                        "Page.handleJavaScriptDialog",
                        json!({ "accept": true }),
                        s,
                    ),
                    "Target.detachedFromTarget" if params["sessionId"].as_str() == s => {
                        bail!("the page closed");
                    }
                    _ => {}
                }
            }
        }
    }
    let _ = tokio::time::timeout(
        Duration::from_secs(2),
        cdp.call("Browser.close", json!({}), None),
    )
    .await;
    Ok(())
}

/// Size the page and (re)start streaming it at that size.
async fn resize(cdp: &mut Cdp, s: Option<&str>, v: Viewport) -> Result<()> {
    cdp.call(
        "Emulation.setDeviceMetricsOverride",
        json!({ "width": v.width, "height": v.height, "deviceScaleFactor": v.scale, "mobile": false }),
        s,
    )
    .await?;
    let _ = cdp.call("Page.stopScreencast", json!({}), s).await;
    cdp.call(
        "Page.startScreencast",
        json!({
            "format": "png",
            "maxWidth": (v.width as f64 * v.scale) as u32,
            "maxHeight": (v.height as f64 * v.scale) as u32,
            "everyNthFrame": 1,
        }),
        s,
    )
    .await?;
    Ok(())
}

/// DevTools key events for one key press.
pub fn key_events(k: &KeyInput) -> Vec<Value> {
    let modifiers =
        (if k.alt { 1 } else { 0 }) | (if k.ctrl { 2 } else { 0 }) | (if k.shift { 8 } else { 0 });
    let mut down = json!({
        "type": "keyDown",
        "key": k.key,
        "code": k.code,
        "windowsVirtualKeyCode": k.vk,
        "nativeVirtualKeyCode": k.vk,
        "modifiers": modifiers,
    });
    if let Some(t) = &k.text {
        down["text"] = json!(t);
        down["unmodifiedText"] = json!(t);
    }
    let mut up = down.clone();
    up["type"] = json!("keyUp");
    up.as_object_mut().map(|o| {
        o.remove("text");
        o.remove("unmodifiedText")
    });
    vec![down, up]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_devtools_address() {
        assert_eq!(
            devtools_url("DevTools listening on ws://127.0.0.1:53211/devtools/browser/ab-cd\n"),
            Some("ws://127.0.0.1:53211/devtools/browser/ab-cd")
        );
        assert_eq!(devtools_url("[1021/WARNING:something]"), None);
    }

    #[test]
    fn configured_browser_must_exist() {
        let e = find_chrome("/nope/chrome").unwrap_err().to_string();
        assert!(e.contains("wasn't found"), "{e}");
        let tmp = tempfile::tempdir().unwrap();
        let fake = tmp.path().join("chrome");
        std::fs::write(&fake, "").unwrap();
        assert_eq!(find_chrome(fake.to_str().unwrap()).unwrap(), fake);
    }

    #[test]
    fn enter_types_a_return() {
        let k = KeyInput::special("Enter", "Enter", 13, Some("\r"));
        let e = key_events(&k);
        assert_eq!(e[0]["type"], "keyDown");
        assert_eq!(e[0]["windowsVirtualKeyCode"], 13);
        assert_eq!(e[0]["text"], "\r");
        assert_eq!(e[1]["type"], "keyUp");
        assert!(e[1].get("text").is_none());
    }

    /// Against a real browser, when one is installed: a page streams
    /// frames, and a click reaches it.
    #[tokio::test(flavor = "multi_thread")]
    async fn drives_a_real_browser() {
        let Ok(chrome) = find_chrome("") else {
            eprintln!("no browser installed; skipping");
            return;
        };
        let profile = tempfile::tempdir().unwrap();
        let page = "data:text/html,<body style='margin:0;background:%23e3b25b'>\
                    <button style='width:100vw;height:100vh' \
                    onclick=\"document.title='clicked'\">go</button></body>";
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let v = Viewport {
            width: 320,
            height: 200,
            scale: 1.0,
        };
        let task = tokio::spawn(run(
            chrome,
            profile.path().to_path_buf(),
            page.into(),
            v,
            cmd_rx,
            ev_tx,
        ));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let mut framed = false;
        let mut clicked = false;
        while !(framed && clicked) {
            let ev = tokio::time::timeout_at(deadline, ev_rx.recv())
                .await
                .expect("timed out")
                .expect("browser task ended");
            match ev {
                BrowserEvent::Frame(png) if !framed => {
                    // Base64 of a PNG's signature.
                    assert!(png.starts_with("iVBORw0KGgo"), "{}", &png[..20]);
                    framed = true;
                    cmd_tx
                        .send(BrowserCmd::Click { x: 100.0, y: 100.0 })
                        .unwrap();
                }
                BrowserEvent::Page { title, .. } if title == "clicked" => clicked = true,
                BrowserEvent::Failed(e) => panic!("{e}"),
                _ => {}
            }
        }
        cmd_tx.send(BrowserCmd::Quit).unwrap();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
    }
}

// corral on your phone. Talks to `corral web`: /ws streams who needs you
// and takes actions, /ws/term is one agent's terminal.
"use strict";

const app = document.getElementById("app");
const wsBase = `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}`;

let me = null;          // GET /api/me
let snapshot = null;    // the last state from /ws
let control = null;     // the /ws socket
let nextId = 1;
const pending = new Map();
let term = null;        // the open terminal, if any
let retry = 0;

// ---- little helpers --------------------------------------------------------

function h(tag, attrs, ...kids) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v == null || v === false) continue;
    if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else if (k === "class") el.className = v;
    else el.setAttribute(k, v === true ? "" : v);
  }
  for (const kid of kids.flat()) {
    if (kid == null || kid === false) continue;
    el.append(kid instanceof Node ? kid : document.createTextNode(String(kid)));
  }
  return el;
}

function svg(path) {
  const s = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  s.setAttribute("viewBox", "0 0 24 24");
  s.setAttribute("fill", "none");
  s.setAttribute("stroke", "currentColor");
  s.setAttribute("stroke-width", "2");
  s.setAttribute("stroke-linecap", "round");
  s.setAttribute("stroke-linejoin", "round");
  s.innerHTML = path; // constant icon paths only
  return s;
}
const ICON = {
  menu: '<circle cx="12" cy="5" r="1"/><circle cx="12" cy="12" r="1"/><circle cx="12" cy="19" r="1"/>',
  back: '<path d="M15 18l-6-6 6-6"/>',
  note: '<path d="M12 20h9"/><path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4Z"/>',
  fit: '<path d="M8 3H5a2 2 0 0 0-2 2v3m18 0V5a2 2 0 0 0-2-2h-3m0 18h3a2 2 0 0 0 2-2v-3M3 16v3a2 2 0 0 0 2 2h3"/>',
  kbd: '<rect x="2" y="6" width="20" height="12" rx="2"/><path d="M6 10h.01M10 10h.01M14 10h.01M18 10h.01M7 14h10"/>',
};

let toastTimer;
function toast(text, bad) {
  const t = document.getElementById("toast");
  t.textContent = text;
  t.className = `show${bad ? " bad" : ""}`;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (t.className = bad ? "bad" : ""), 2600);
}

function ago(since) {
  if (!since) return "";
  const s = Math.max(0, Math.floor(Date.now() / 1000) - since);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}

const STATE_LABEL = { blocked: "Needs answer", done: "Done", working: "Working", idle: "Idle", exited: "Exited" };

const standalone = () =>
  matchMedia("(display-mode: standalone)").matches || navigator.standalone === true;
const isIOS = () => /iPhone|iPad|iPod/.test(navigator.userAgent);

// ---- routing ---------------------------------------------------------------

async function start() {
  if ("serviceWorker" in navigator && isSecureContext) {
    navigator.serviceWorker.register("/sw.js").catch(() => {});
  }
  try {
    me = await (await fetch("/api/me", { cache: "no-store" })).json();
  } catch (_) {
    me = null;
  }
  if (location.pathname === "/pair") {
    const code = location.hash.slice(1);
    if (me && me.paired) {
      history.replaceState(null, "", "/");
    } else {
      return showPair(code);
    }
  }
  if (!me || !me.paired) return showPair("");
  connect();
  route();
}

window.addEventListener("hashchange", () => route());

function route() {
  const m = location.hash.match(/^#\/t\/(.+)$/);
  if (m) openTerm(decodeURIComponent(m[1]));
  else {
    closeTerm();
    renderHome();
  }
}

// ---- pairing ---------------------------------------------------------------

function guessName() {
  const ua = navigator.userAgent;
  if (/iPhone/.test(ua)) return "iPhone";
  if (/iPad/.test(ua)) return "iPad";
  if (/Android/.test(ua)) return "Android phone";
  return "Browser";
}

function showPair(code) {
  const name = h("input", { class: "field", id: "devname", value: guessName(), autocomplete: "off", maxlength: "40" });
  const err = h("div", { class: "error", hidden: true });
  const go = h("button", { class: "btn primary" }, "Pair this phone");
  go.addEventListener("click", async () => {
    go.disabled = true;
    err.hidden = true;
    try {
      const r = await fetch("/api/pair", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ code, name: name.value }),
      });
      const body = await r.json();
      if (!r.ok) throw new Error(body.error || "Pairing failed");
      history.replaceState(null, "", "/");
      me = await (await fetch("/api/me", { cache: "no-store" })).json();
      toast(`Paired as ${body.name}`);
      connect();
      route();
    } catch (e) {
      err.textContent = e.message;
      err.hidden = false;
      go.disabled = false;
    }
  });
  const logo = h("img", { src: "/icon-192.png", alt: "" });
  app.replaceChildren(
    code
      ? h("div", { class: "pair" }, logo,
          h("h1", {}, "Pair with corral"),
          h("p", {}, "This phone will see your agents and can answer them."),
          h("label", { for: "devname" }, "Name this phone"), name, go, err)
      : h("div", { class: "pair" }, logo,
          h("h1", {}, "Not paired yet"),
          h("p", {}, "On your computer, run ", h("code", {}, "corral web pair"),
            " and scan the code it shows with this phone's camera.")),
  );
}

// ---- the control socket ----------------------------------------------------

function connect() {
  if (control) return;
  const ws = new WebSocket(`${wsBase}/ws`);
  control = ws;
  ws.onopen = () => (retry = 0);
  ws.onmessage = (e) => {
    const msg = JSON.parse(e.data);
    if (msg.t === "state") {
      snapshot = msg;
      if (!term) renderHome();
      else term.onState?.();
    } else if (msg.t === "done") {
      const p = pending.get(msg.id);
      pending.delete(msg.id);
      p?.(msg);
    }
  };
  ws.onclose = async () => {
    control = null;
    for (const p of pending.values()) p({ ok: false, message: "Lost the connection" });
    pending.clear();
    snapshot = snapshot && { ...snapshot, online: false, lost: true };
    if (!term) renderHome();
    // Unpaired (revoked on the computer)? Then pair again rather than retry.
    try {
      me = await (await fetch("/api/me", { cache: "no-store" })).json();
      if (!me.paired) return showPair("");
    } catch (_) {}
    retry = Math.min(retry + 1, 6);
    setTimeout(connect, 500 * 2 ** retry);
  };
}

function act(action) {
  return new Promise((resolve) => {
    if (!control || control.readyState !== WebSocket.OPEN) {
      return resolve({ ok: false, message: "Not connected" });
    }
    const id = nextId++;
    pending.set(id, resolve);
    control.send(JSON.stringify({ id, ...action }));
  });
}

document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible" && !control && me?.paired) connect();
});

// ---- home ------------------------------------------------------------------

function topBar() {
  const dot = h("span", { class: `dot${snapshot?.online ? " on" : ""}`, title: snapshot?.online ? "Connected" : "Offline" });
  return h("header", { class: "top" },
    h("div", { class: "brand" }, h("img", { src: "/icon-192.png", alt: "" }), "corral"),
    dot,
    h("div", { class: "spacer" }),
    h("button", { class: "icon-btn", "aria-label": "Settings", onclick: showSettings }, svg(ICON.menu)));
}

function renderHome() {
  if (term) return;
  const main = h("main", {});
  if (!snapshot) {
    main.append(h("div", { class: "empty" }, "Connecting…"));
  } else {
    if (snapshot.lost) main.append(h("div", { class: "banner" }, "Reconnecting to corral web…"));
    else if (!snapshot.online) main.append(h("div", { class: "banner" }, "corral's server isn't running on your computer."));
    if (!snapshot.sections.length && snapshot.online) {
      main.append(h("div", { class: "empty" },
        h("h2", {}, "No agents running"),
        "Start one on your computer, e.g. ", h("code", {}, "corral new -- claude")));
    }
    for (const s of snapshot.sections) {
      main.append(h("section", { class: `section ${s.need}` },
        h("h2", {}, s.title, h("span", { class: "count" }, s.agents.length)),
        s.agents.map(card)));
    }
  }
  app.replaceChildren(topBar(), main);
  // The question is at the bottom of the screen.
  for (const ex of main.querySelectorAll(".excerpt")) ex.scrollTop = ex.scrollHeight;
}

function card(a) {
  const meta = [a.agent || "shell", a.workspace].filter(Boolean).join(" · ");
  const branch = a.branch ? h("span", { class: "branch" }, ` · ${a.branch}${a.pr ? ` #${a.pr}` : ""}`) : null;
  const row = h("button", { class: "row", onclick: () => (location.hash = `#/t/${encodeURIComponent(a.id)}`) },
    h("div", { class: "who" },
      h("div", { class: "name" }, a.name),
      h("div", { class: "meta" }, meta, branch)),
    h("div", { class: "state" },
      h("span", { class: `pill ${a.state}` }, STATE_LABEL[a.state] || a.state),
      h("span", { class: "ago", "data-since": a.since }, ago(a.since))));
  const el = h("article", { class: `card ${a.state}` }, row);
  if (a.state === "blocked") {
    if (a.excerpt) el.append(h("pre", { class: "excerpt" }, a.excerpt));
    const deny = h("button", { class: "btn ghost" }, "Deny");
    const yes = h("button", { class: "btn primary" }, "Approve");
    const answer = async (t, btn) => {
      deny.disabled = yes.disabled = true;
      btn.textContent = "…";
      const r = await act({ t, pane: a.id });
      toast(r.ok ? (t === "approve" ? `Approved ${a.name}` : `Denied ${a.name}`) : r.message, !r.ok);
      if (!r.ok) {
        deny.disabled = yes.disabled = false;
        deny.textContent = "Deny";
        yes.textContent = "Approve";
      }
    };
    deny.addEventListener("click", () => answer("deny", deny));
    yes.addEventListener("click", () => answer("approve", yes));
    el.append(h("div", { class: "actions" }, deny, yes));
  } else if (a.state === "done" || a.state === "idle") {
    el.append(h("div", { class: "actions" },
      h("button", { class: "btn ghost small", onclick: () => noteSheet(a) }, svg(ICON.note), "Note"),
      h("div", { class: "spacer" })));
  }
  return el;
}

setInterval(() => {
  for (const el of document.querySelectorAll("[data-since]")) el.textContent = ago(+el.dataset.since);
}, 5000);

// ---- sheets ----------------------------------------------------------------

function sheet(...content) {
  const scrim = h("div", { class: "scrim" });
  const close = () => scrim.remove();
  scrim.addEventListener("click", (e) => e.target === scrim && close());
  scrim.append(h("div", { class: "sheet", role: "dialog", "aria-modal": "true" }, h("div", { class: "grab" }), ...content));
  document.body.append(scrim);
  return close;
}

function noteSheet(a) {
  const text = h("textarea", { placeholder: `Tell ${a.name} what to do next…`, enterkeyhint: "send" });
  const send = h("button", { class: "btn primary" }, "Send");
  const close = sheet(
    h("h3", {}, `Note to ${a.name}`),
    h("p", { class: "sub" }, "Pasted into the agent, then Enter."),
    text,
    h("div", { class: "actions" }, h("button", { class: "btn ghost", onclick: () => close() }, "Cancel"), send));
  send.addEventListener("click", async () => {
    if (!text.value.trim()) return text.focus();
    send.disabled = true;
    const r = await act({ t: "note", pane: a.id, text: text.value });
    toast(r.ok ? `Sent to ${a.name}` : r.message, !r.ok);
    if (r.ok) close();
    else send.disabled = false;
  });
  setTimeout(() => text.focus(), 50);
}

function confirmSheet(title, sub, label, run) {
  const go = h("button", { class: "btn danger" }, label);
  const close = sheet(h("h3", {}, title), h("p", { class: "sub" }, sub),
    h("div", { class: "actions" }, h("button", { class: "btn ghost", onclick: () => close() }, "Cancel"), go));
  go.addEventListener("click", async () => {
    go.disabled = true;
    await run();
    close();
  });
}

async function pushState() {
  if (!("serviceWorker" in navigator) || !("PushManager" in window) || !isSecureContext) return "unsupported";
  if (Notification.permission === "denied") return "denied";
  const reg = await navigator.serviceWorker.ready;
  return (await reg.pushManager.getSubscription()) ? "on" : "off";
}

function b64ToBytes(s) {
  const b = atob(s.replace(/-/g, "+").replace(/_/g, "/") + "===".slice((s.length + 3) % 4));
  return Uint8Array.from(b, (c) => c.charCodeAt(0));
}

async function setPush(on) {
  const reg = await navigator.serviceWorker.ready;
  let endpoint = null;
  if (on) {
    if ((await Notification.requestPermission()) !== "granted") throw new Error("Notifications are blocked for this app");
    const sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: b64ToBytes(me.push_key) });
    endpoint = sub.endpoint;
  } else {
    await (await reg.pushManager.getSubscription())?.unsubscribe();
  }
  const r = await fetch("/api/push", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ endpoint }),
  });
  if (!r.ok) throw new Error((await r.json()).error || "Couldn't save");
}

async function showSettings() {
  const state = await pushState();
  const pushRow = h("div", { class: "item" });
  const drawPush = (st) => {
    let hint, btn = null;
    if (state === "unsupported") {
      hint = !isSecureContext
        ? "Needs HTTPS. Use tailscale serve (see corral web's output)."
        : isIOS() && !standalone()
          ? "On iPhone, add corral to the Home Screen first (Share → Add to Home Screen)."
          : "This browser can't do push notifications.";
    } else if (st === "denied") {
      hint = "Blocked in the phone's settings.";
    } else {
      hint = st === "on" ? "On: you'll hear when an agent needs you." : "Off";
      btn = h("button", { class: "btn ghost small" }, st === "on" ? "Turn off" : "Turn on");
      btn.addEventListener("click", async () => {
        btn.disabled = true;
        try {
          await setPush(st !== "on");
          drawPush(st === "on" ? "off" : "on");
          toast(st === "on" ? "Notifications off" : "Notifications on");
        } catch (e) {
          toast(e.message, true);
          btn.disabled = false;
        }
      });
    }
    pushRow.replaceChildren(h("div", { class: "grow" }, h("div", {}, "Notifications"), h("div", { class: "hint" }, hint)), btn);
  };
  drawPush(state);
  const install = !standalone()
    ? h("div", { class: "item" }, h("div", { class: "grow" }, h("div", {}, "Install"),
        h("div", { class: "hint" }, isIOS()
          ? "Share → Add to Home Screen, then open corral from there."
          : "Browser menu → Install app (or Add to Home screen).")))
    : null;
  const unpair = h("button", { class: "btn danger small" }, "Unpair");
  const close = sheet(
    h("h3", {}, "corral"),
    h("p", { class: "sub" }, `${me.name || "This phone"} · corral ${me.version}`),
    h("div", { class: "list" }, pushRow, install,
      h("div", { class: "item" }, h("div", { class: "grow" }, h("div", {}, "This phone"),
        h("div", { class: "hint" }, "Unpairing signs it out; pair again with corral web pair.")), unpair)));
  unpair.addEventListener("click", () => {
    close();
    confirmSheet("Unpair this phone?", "It won't see or control your agents until you pair it again.", "Unpair", async () => {
      try { await setPush(false); } catch (_) {}
      await fetch("/api/unpair", { method: "POST", headers: { "Content-Type": "application/json" }, body: "{}" });
      control?.close();
      me.paired = false;
      showPair("");
    });
  });
}

// ---- terminal --------------------------------------------------------------

const KEYS = [
  ["esc", "\x1b"], ["tab", "\t"], ["^C", "\x03"],
  ["↑", "A"], ["↓", "B"], ["←", "D"], ["→", "C"],
  ["^D", "\x04"], ["^R", "\x12"], ["enter", "\r", "accent"],
];
const FONT_SIZES = [11, 13, 15];

function findAgent(id) {
  for (const s of snapshot?.sections || []) for (const a of s.agents) if (a.id === id) return a;
  return null;
}

function openTerm(pane) {
  if (term && term.pane === pane) return;
  closeTerm();
  const agent = findAgent(pane);
  const name = h("div", { class: "name" }, agent ? agent.name : pane);
  const pill = h("span", { class: `pill ${agent?.state || "idle"}` }, STATE_LABEL[agent?.state] || "");
  const wrap = h("div", { class: "term-wrap" });
  const gone = h("div", { class: "gone", hidden: true }, "This agent's pane closed.");
  const keys = h("div", { class: "keys" });
  const back = h("button", { class: "icon-btn", "aria-label": "Back", onclick: () => (location.hash = "") }, svg(ICON.back));
  const fitBtn = h("button", { class: "icon-btn", "aria-label": "Fit to screen" }, svg(ICON.fit));
  const noteBtn = h("button", { class: "icon-btn", "aria-label": "Note" }, svg(ICON.note));
  const view = h("div", { class: "term-view" },
    h("header", { class: "top" }, back, h("div", { class: "title" }, name, pill), fitBtn, noteBtn),
    wrap, gone, keys);
  app.replaceChildren(view);

  let fontSize = FONT_SIZES[+(localStorage.getItem("corral.font") ?? 1)] || 13;
  const xterm = new Terminal({
    fontFamily: getComputedStyle(document.documentElement).getPropertyValue("--mono"),
    fontSize,
    cursorBlink: true,
    scrollback: 5000,
    allowProposedApi: true,
    theme: { background: "#0c0d0d", foreground: "#e4e1e9", cursor: "#caea72", selectionBackground: "#caea7255" },
  });
  const fit = new FitAddon.FitAddon();
  xterm.loadAddon(fit);
  xterm.open(wrap);
  const dims = () => fit.proposeDimensions() || { cols: 80, rows: 24 };
  const d = dims();
  xterm.resize(d.cols, d.rows);

  const ws = new WebSocket(`${wsBase}/ws/term?pane=${encodeURIComponent(pane)}&cols=${d.cols}&rows=${d.rows}`);
  ws.binaryType = "arraybuffer";
  const send = (m) => ws.readyState === WebSocket.OPEN && ws.send(JSON.stringify(m));
  ws.onmessage = (e) => {
    const b = new Uint8Array(e.data);
    if (b[0] === 0) {
      const cols = (b[1] << 8) | b[2], rows = (b[3] << 8) | b[4];
      xterm.reset();
      if (xterm.cols !== cols || xterm.rows !== rows) xterm.resize(cols, rows);
      xterm.write(b.subarray(5));
    } else if (b[0] === 1) {
      xterm.write(b.subarray(1));
    } else if (b[0] === 2) {
      gone.hidden = false;
    }
  };
  ws.onclose = () => { if (term?.ws === ws) gone.hidden = false; };
  xterm.onData((data) => send({ t: "input", data }));

  // Sized to this phone. The computer takes the pane back when you type
  // there; Fit takes it back here.
  const refit = () => {
    const d = dims();
    if (d.cols !== xterm.cols || d.rows !== xterm.rows) xterm.resize(d.cols, d.rows);
    send({ t: "resize", cols: d.cols, rows: d.rows });
  };
  fitBtn.addEventListener("click", refit);
  fitBtn.addEventListener("contextmenu", (e) => e.preventDefault());
  // Long-press-free font cycling: double tap Fit.
  fitBtn.addEventListener("dblclick", () => {
    const i = (FONT_SIZES.indexOf(fontSize) + 1) % FONT_SIZES.length;
    fontSize = FONT_SIZES[i];
    try { localStorage.setItem("corral.font", String(i)); } catch (_) {}
    xterm.options.fontSize = fontSize;
    refit();
  });
  noteBtn.addEventListener("click", () => noteSheet(findAgent(pane) || { id: pane, name: name.textContent }));

  for (const [label, seq, cls] of KEYS) {
    const k = h("button", { class: `key${cls ? ` ${cls}` : ""}` }, label);
    // Keep the keyboard up: don't let the button take focus.
    k.addEventListener("pointerdown", (e) => e.preventDefault());
    k.addEventListener("click", () => {
      let data = seq;
      if (seq.length === 1 && "ABCD".includes(seq)) {
        data = (xterm.modes.applicationCursorKeysMode ? "\x1bO" : "\x1b[") + seq;
      }
      send({ t: "input", data });
    });
    keys.append(k);
  }
  const kb = h("button", { class: "key", "aria-label": "Keyboard" }, svg(ICON.kbd));
  kb.addEventListener("pointerdown", (e) => e.preventDefault());
  kb.addEventListener("click", () => xterm.focus());
  keys.prepend(kb);

  // Follow the on-screen keyboard (iOS doesn't resize the layout for it).
  const vv = window.visualViewport;
  let resizeTimer;
  const onViewport = () => {
    if (vv) view.style.height = `${vv.height}px`;
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(refit, 120);
  };
  vv?.addEventListener("resize", onViewport);
  window.addEventListener("orientationchange", onViewport);

  term = {
    pane, ws,
    onState() {
      const a = findAgent(pane);
      if (!a) return;
      name.textContent = a.name;
      pill.className = `pill ${a.state}`;
      pill.textContent = STATE_LABEL[a.state] || a.state;
    },
    close() {
      vv?.removeEventListener("resize", onViewport);
      window.removeEventListener("orientationchange", onViewport);
      ws.close();
      xterm.dispose();
    },
  };
}

function closeTerm() {
  if (!term) return;
  const t = term;
  term = null;
  t.close();
}

start();

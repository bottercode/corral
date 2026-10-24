// corral's service worker: it only shows notifications. Pushes arrive
// empty, so it asks corral who needs you and says so.

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (e) => e.waitUntil(self.clients.claim()));

self.addEventListener("push", (e) => e.waitUntil(show()));

async function show() {
  let title = "corral";
  let body = "An agent needs you";
  let pane = null;
  let blocked = false;
  try {
    const r = await fetch("/api/needs", { credentials: "same-origin", cache: "no-store" });
    if (r.ok) {
      const { agents } = await r.json();
      if (agents.length) {
        const a = agents[0];
        pane = a.id;
        blocked = a.state === "blocked";
        title = a.name;
        body = blocked ? "is waiting for your answer" : "finished";
        if (agents.length > 1) body += ` · ${agents.length - 1} more need you`;
      }
    }
  } catch (_) {
    // Offline: the generic notification still tells you to look.
  }
  // Phones require every push to show something, so this always does.
  return self.registration.showNotification(title, {
    body,
    tag: "corral",
    renotify: true,
    icon: "/icon-192.png",
    badge: "/icon-192.png",
    data: { pane, blocked },
  });
}

self.addEventListener("notificationclick", (e) => {
  e.notification.close();
  const { pane, blocked } = e.notification.data || {};
  // A blocked agent is answered from the home screen; a finished one is read
  // in its terminal.
  const url = pane && !blocked ? `/#/t/${encodeURIComponent(pane)}` : "/";
  e.waitUntil(
    (async () => {
      const wins = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
      for (const w of wins) {
        if ("focus" in w) {
          if ("navigate" in w) await w.navigate(url).catch(() => {});
          return w.focus();
        }
      }
      return self.clients.openWindow(url);
    })(),
  );
});

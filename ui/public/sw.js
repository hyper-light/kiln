/*
 * kiln's service worker: it shows the notifications kiln's server pushes (docs/notifications.md)
 * and opens the agent one is about when it's tapped. It has no fetch handler: it never caches or
 * intercepts the app's requests, so the app loads exactly as it would without it.
 *
 * A push's data is JSON: { title, body, tag, url, agent?, kind, at }. One notification per agent at
 * a time: the tag is the agent's id, so a newer state replaces the older one. None shows while a
 * focused kiln window is already on that agent.
 */

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));

/** Safari (and every browser on iOS) takes a push away from a page that doesn't show one: there it is shown and closed at once. */
const webkit = /AppleWebKit/.test(self.navigator.userAgent) && !/Chrome|Chromium|Edg\/|Firefox/.test(self.navigator.userAgent);

function read(data) {
  if (!data) return {};
  try {
    return data.json();
  } catch {
    return { body: data.text() };
  }
}

/** A focused window of kiln on `path` (the page you're looking at). */
async function looking(path) {
  const windows = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
  return windows.some((w) => w.focused && new URL(w.url).pathname.replace(/\/+$/, "") === path);
}

async function show(msg) {
  const tag = msg.tag || "kiln";
  const options = {
    body: msg.body || "",
    tag,
    renotify: true,
    icon: "/icon-192.png",
    badge: "/badge-96.png",
    timestamp: msg.at || Date.now(),
    data: { url: msg.url || "/" },
  };
  if (msg.agent && (await looking(`/agents/${msg.agent}`))) {
    // You're on it already: nothing to tell you (and an older note of it goes too).
    for (const n of await self.registration.getNotifications({ tag })) n.close();
    if (!webkit) return;
    await self.registration.showNotification(msg.title || "kiln", { ...options, silent: true, renotify: false });
    for (const n of await self.registration.getNotifications({ tag })) n.close();
    return;
  }
  await self.registration.showNotification(msg.title || "kiln", options);
}

self.addEventListener("push", (event) => {
  event.waitUntil(show(read(event.data)));
});

/** Tapped: a kiln window there is, at the page the note is about; else a new one. */
self.addEventListener("notificationclick", (event) => {
  event.notification.close();
  const url = new URL((event.notification.data && event.notification.data.url) || "/", self.location.origin).href;
  event.waitUntil(
    (async () => {
      const windows = await self.clients.matchAll({ type: "window", includeUncontrolled: true });
      const mine = windows.find((w) => new URL(w.url).origin === self.location.origin);
      if (mine) {
        const focused = await mine.focus();
        if ("navigate" in focused && focused.url !== url) await focused.navigate(url);
        return;
      }
      await self.clients.openWindow(url);
    })(),
  );
});

/** The push service changed this device's subscription: the server is told of the new one, in place of the old. */
self.addEventListener("pushsubscriptionchange", (event) => {
  event.waitUntil(
    (async () => {
      const old = event.oldSubscription;
      let sub = event.newSubscription;
      const key = old && old.options && old.options.applicationServerKey;
      if (!sub && key) sub = await self.registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key });
      if (!sub) return;
      await fetch("/api/push/subscriptions", {
        method: "POST",
        credentials: "same-origin",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ subscription: sub.toJSON(), replaces: old ? old.endpoint : undefined }),
      });
    })(),
  );
});

// kiln's capture extension, its worker (crate::browser::stream): a tab's picture and sound handed
// to the offscreen document, which encodes them. kiln calls these over the DevTools protocol.

const pause = (ms) => new Promise((r) => setTimeout(r, ms));

// A tab captured for a stream (`ask`: what the offscreen document's to do with it), tried again a
// while: a capture just let go of is let go of a moment later (Chrome says the tab still has one,
// or that its state is wrong), and each try needs an id of its own.
async function captured(tabId, ask) {
  let last = { ok: false, error: "it wasn't tried" };
  const until = Date.now() + 6000;
  for (let i = 0; i === 0 || Date.now() < until; i++) {
    if (i) await pause(Math.min(200, 25 * i));
    let mediaId;
    try {
      mediaId = await chrome.tabCapture.getMediaStreamId({ targetTabId: tabId });
    } catch (e) {
      last = { ok: false, error: String(e) };
      continue;
    }
    last = await chrome.runtime.sendMessage({ ...ask, mediaId });
    if (last && last.ok) return last;
  }
  return last;
}
globalThis.kiln = {
  // The tab whose address ends in `#kiln-<mark>` (kiln opens each streamed tab so), its id.
  async tab(mark) {
    const tabs = await chrome.tabs.query({});
    const t = tabs.find((t) => (t.url || t.pendingUrl || "").endsWith(`#kiln-${mark}`));
    return t ? t.id : null;
  },
  // A tab's id, by its DevTools target's (a tab a page opened: a link to a new tab, a popup), and
  // whether it's the one its window shows.
  async tabOf(targetId) {
    const t = (await chrome.debugger.getTargets()).find((t) => t.id === targetId);
    if (!t || typeof t.tabId !== "number") return null;
    const tab = await chrome.tabs.get(t.tabId).catch(() => null);
    return { tab: t.tabId, active: Boolean(tab && tab.active) };
  },
  async capture(tabId, stream, opts) {
    const mediaId = await chrome.tabCapture.getMediaStreamId({ targetTabId: tabId });
    if (!(await chrome.offscreen.hasDocument())) {
      await chrome.offscreen.createDocument({ url: "offscreen.html", reasons: ["USER_MEDIA", "AUDIO_PLAYBACK"], justification: "kiln streams a page it shows" });
    }
    return await chrome.runtime.sendMessage({ kind: "start", stream, mediaId, opts });
  },
  // Another of a stream's tabs captured for it from now on (its viewer went to it).
  async switchTo(tabId, stream) {
    return await captured(tabId, { kind: "switch", stream });
  },
  // A stream's tab captured again at its new size, in pixels (a capture keeps the size it began
  // at): its capture let go first (Chrome captures a tab once at a time), then taken again.
  async recapture(tabId, stream, width, height) {
    await chrome.runtime.sendMessage({ kind: "release", stream });
    return await captured(tabId, { kind: "switch", stream, width, height });
  },
  // A stream whose pictures kiln brings (another engine's page, taken as it changes): encoded in
  // the offscreen document as a captured tab's are.
  async pushed(stream, opts) {
    if (!(await chrome.offscreen.hasDocument())) {
      await chrome.offscreen.createDocument({ url: "offscreen.html", reasons: ["USER_MEDIA", "AUDIO_PLAYBACK"], justification: "kiln streams a page it shows" });
    }
    return await chrome.runtime.sendMessage({ kind: "pushed", stream, opts });
  },
};

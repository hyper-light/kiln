// kiln's capture extension, its worker (crate::browser::stream): a tab's picture and sound handed
// to the offscreen document, which encodes them. kiln calls these over the DevTools protocol.
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
    const mediaId = await chrome.tabCapture.getMediaStreamId({ targetTabId: tabId });
    return await chrome.runtime.sendMessage({ kind: "switch", stream, mediaId });
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

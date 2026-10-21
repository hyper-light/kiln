//! A page in the machine's browser (`super::Browser`), in a context of its own: shown at a size
//! and in an appearance, loaded, asked things, pictured, and closed with its context.

use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Browser;
use super::cdp::Event;

/// What a page printed: its console and what it threw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Said {
    /// "log", "info", "warning", "error".
    pub level: String,
    pub text: String,
}

pub struct Page {
    browser: Arc<Browser>,
    /// Its own context (none: the browser's own).
    context: Option<String>,
    target: String,
    session: String,
    events: async_channel::Receiver<Event>,
    said: Vec<Said>,
}

impl Page {
    pub(super) async fn open(browser: Arc<Browser>) -> anyhow::Result<Self> {
        let ctx = browser.cdp.call(None, "Target.createBrowserContext", json!({"disposeOnDetach": true})).await?;
        let context = field(&ctx, "browserContextId")?;
        Self::open_in(browser, Some(context), "about:blank").await
    }

    /// A page at `url`, in `context` (none: the browser's own).
    pub(super) async fn open_in(browser: Arc<Browser>, context: Option<String>, url: &str) -> anyhow::Result<Self> {
        let cdp = browser.cdp.clone();
        let made = match &context {
            Some(c) => cdp.call(None, "Target.createTarget", json!({"url": url, "browserContextId": c})).await?,
            None => cdp.call(None, "Target.createTarget", json!({"url": url})).await?,
        };
        let target = field(&made, "targetId")?;
        Self::attach(browser, context, target).await
    }

    /// The page `target` is (one kiln opened, or one a page opened itself: a link to a new tab,
    /// a popup), attached to as it is.
    pub(super) async fn attach(browser: Arc<Browser>, context: Option<String>, target: String) -> anyhow::Result<Self> {
        let cdp = browser.cdp.clone();
        let attached = cdp.call(None, "Target.attachToTarget", json!({"targetId": target, "flatten": true})).await?;
        let session = field(&attached, "sessionId")?;
        let events = cdp.listen(Some(&session));
        for m in ["Page.enable", "Runtime.enable", "Log.enable"] {
            cdp.call(Some(&session), m, json!({})).await?;
        }
        cdp.call(Some(&session), "Page.setLifecycleEventsEnabled", json!({"enabled": true})).await?;
        browser.opened();
        Ok(Self { browser, context, target, session, events, said: Vec::new() })
    }

    /// A command to this page.
    pub(crate) async fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.browser.cdp.call(Some(&self.session), method, params).await
    }

    /// What this page says from now on, to someone else besides it (it reads its own).
    pub(crate) fn listen(&self) -> async_channel::Receiver<Event> {
        self.browser.cdp.listen(Some(&self.session))
    }

    pub(crate) fn cdp(&self) -> Arc<super::cdp::Cdp> {
        self.browser.cdp.clone()
    }

    pub(crate) fn session(&self) -> &str {
        &self.session
    }

    /// Its target's id (the browser closes it by that).
    pub(crate) fn target(&self) -> &str {
        &self.target
    }

    /// The window's size in CSS pixels, its pixel density, and whether it's a phone's (touch, a
    /// phone's viewport rules).
    pub async fn viewport(&self, width: u32, height: u32, scale: f64, mobile: bool) -> anyhow::Result<()> {
        self.call("Emulation.setDeviceMetricsOverride", json!({"width": width, "height": height, "deviceScaleFactor": scale, "mobile": mobile})).await?;
        self.call("Emulation.setTouchEmulationEnabled", json!({"enabled": mobile, "maxTouchPoints": if mobile { 5 } else { 1 }})).await?;
        Ok(())
    }

    /// Light or dark, and whether motion is reduced, as the page's media queries see them.
    pub async fn appearance(&self, dark: bool, reduced_motion: bool) -> anyhow::Result<()> {
        let features = json!([
            {"name": "prefers-color-scheme", "value": if dark { "dark" } else { "light" }},
            {"name": "prefers-reduced-motion", "value": if reduced_motion { "reduce" } else { "no-preference" }},
        ]);
        self.call("Emulation.setEmulatedMedia", json!({"features": features})).await?;
        Ok(())
    }

    /// A script run in every document this page loads from now on, before the page's own.
    pub async fn before_load(&self, source: &str) -> anyhow::Result<()> {
        self.call("Page.addScriptToEvaluateOnNewDocument", json!({"source": source})).await?;
        Ok(())
    }

    /// Go to `url` and wait for it to load, then for its network to settle (at most `settle`).
    pub async fn go(&mut self, url: &str, settle: Duration) -> anyhow::Result<()> {
        let nav = self.call("Page.navigate", json!({"url": url})).await?;
        if let Some(e) = nav.get("errorText").and_then(Value::as_str).filter(|e| !e.is_empty()) {
            anyhow::bail!("{url} didn't load: {e}");
        }
        let loaded = Instant::now() + Duration::from_secs(30);
        self.until(loaded, |e| e.method == "Page.loadEventFired").await;
        let idle = Instant::now() + settle;
        self.until(idle, |e| e.method == "Page.lifecycleEvent" && e.params.get("name").and_then(Value::as_str) == Some("networkIdle")).await;
        Ok(())
    }

    /// Take what the page said until `deadline` or an event `done` says; true when it came.
    async fn until(&mut self, deadline: Instant, done: impl Fn(&Event) -> bool) -> bool {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            match crate::rt::timeout(left, self.events.recv()).await {
                Ok(Ok(e)) => {
                    let hit = done(&e);
                    self.heard(&e);
                    if hit {
                        return true;
                    }
                }
                Ok(Err(_)) | Err(_) => return false,
            }
        }
    }

    /// Keep what an event says the page printed.
    fn heard(&mut self, e: &Event) {
        let line = match e.method.as_str() {
            "Runtime.consoleAPICalled" => {
                let kind = e.params.get("type").and_then(Value::as_str).unwrap_or("log");
                let text = e.params.get("args").and_then(Value::as_array).map(|a| a.iter().map(arg_text).collect::<Vec<_>>().join(" ")).unwrap_or_default();
                let level = match kind {
                    "error" | "assert" => "error",
                    "warning" | "warn" => "warning",
                    "info" => "info",
                    _ => "log",
                };
                Some(Said { level: level.to_owned(), text })
            }
            "Runtime.exceptionThrown" => {
                let d = e.params.get("exceptionDetails");
                let text = d
                    .and_then(|d| d.get("exception").and_then(|x| x.get("description")).and_then(Value::as_str).or_else(|| d.get("text").and_then(Value::as_str)))
                    .unwrap_or("an exception")
                    .to_owned();
                Some(Said { level: "error".to_owned(), text })
            }
            "Log.entryAdded" => {
                let entry = e.params.get("entry");
                let level = entry.and_then(|x| x.get("level")).and_then(Value::as_str).unwrap_or("info");
                let text = entry.and_then(|x| x.get("text")).and_then(Value::as_str).unwrap_or("").to_owned();
                let url = entry.and_then(|x| x.get("url")).and_then(Value::as_str).unwrap_or("");
                Some(Said { level: if level == "verbose" { "log".to_owned() } else { level.to_owned() }, text: if url.is_empty() { text } else { format!("{text} ({url})") } })
            }
            _ => None,
        };
        if let Some(l) = line
            && self.said.len() < 500
        {
            self.said.push(l);
        }
    }

    /// What it printed so far (and since, up to now).
    pub async fn said(&mut self) -> Vec<Said> {
        while let Ok(e) = self.events.try_recv() {
            self.heard(&e);
        }
        self.said.clone()
    }

    /// Run `expression` in the page; its value (a promise waited for).
    pub async fn eval(&self, expression: &str) -> anyhow::Result<Value> {
        let r = self.call("Runtime.evaluate", json!({"expression": expression, "returnByValue": true, "awaitPromise": true})).await?;
        if let Some(d) = r.get("exceptionDetails") {
            let text = d.get("exception").and_then(|x| x.get("description")).and_then(Value::as_str).or_else(|| d.get("text").and_then(Value::as_str)).unwrap_or("it threw");
            anyhow::bail!("{text}");
        }
        Ok(r.get("result").and_then(|x| x.get("value")).cloned().unwrap_or(Value::Null))
    }

    /// Two frames on, so what was just changed is laid out and painted.
    pub async fn settle(&self) -> anyhow::Result<()> {
        self.eval("new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)))").await?;
        Ok(())
    }

    /// A PNG of what it shows: the window, or the whole page (`full`).
    pub async fn picture(&self, full: bool) -> anyhow::Result<Vec<u8>> {
        let r = self.call("Page.captureScreenshot", json!({"format": "png", "captureBeyondViewport": full, "fromSurface": true})).await?;
        let data = r.get("data").and_then(Value::as_str).ok_or_else(|| anyhow::anyhow!("the browser sent no picture"))?;
        Ok(base64::engine::general_purpose::STANDARD.decode(data)?)
    }

    /// Close it, and its context with it.
    pub async fn close(self) {
        let cdp = self.browser.cdp.clone();
        let _ = cdp.call(None, "Target.closeTarget", json!({"targetId": self.target})).await;
        if let Some(c) = &self.context {
            let _ = cdp.call(None, "Target.disposeBrowserContext", json!({"browserContextId": c})).await;
        }
        self.browser.closed();
    }
}

fn field(v: &Value, name: &str) -> anyhow::Result<String> {
    v.get(name).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| anyhow::anyhow!("the browser's answer had no {name}"))
}

/// A console argument as words.
fn arg_text(a: &Value) -> String {
    if let Some(v) = a.get("value") {
        return match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    }
    a.get("description").or_else(|| a.get("unserializableValue")).and_then(Value::as_str).unwrap_or("").to_owned()
}

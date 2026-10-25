//! One push to one device (RFC 8030): encrypted for it ([`super::ece`]), signed by the server
//! ([`super::vapid`]) and posted to its push service. A 404 or 410 says the device is gone; 429 and
//! 5xx (and a network that fails) are tried again, backing off; anything else is the device's last
//! error, for Settings to show.

use std::time::Duration;

use crate::http::client::Client;
use crate::http::io::Reach;

use super::vapid::{self, Keys};

/// Seconds to wait before each attempt: at once, then backing off (or as long as the push
/// service's `Retry-After` asks, up to two minutes).
pub const WAITS: &[u64] = &[0, 2, 10, 30];
const MAX_RETRY_AFTER: u64 = 120;

/// What goes to a device.
#[derive(Debug, Clone, Copy)]
pub struct Message<'a> {
    /// The JSON the service worker reads (at most [`super::ece::MAX_PLAINTEXT`] bytes).
    pub payload: &'a [u8],
    /// `high` (needs you) or `normal`.
    pub urgency: &'static str,
    /// A newer message with the same topic replaces this one while it waits to be delivered.
    pub topic: Option<&'a str>,
    /// Seconds the push service keeps it for a device that's away.
    pub ttl: u32,
}

/// Where a device's pushes go, and with which keys.
#[derive(Debug, Clone)]
pub struct Device<'a> {
    pub endpoint: &'a str,
    /// Its P-256 public key (65 bytes) and auth secret (16).
    pub p256dh: &'a [u8],
    pub auth: &'a [u8],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Delivered,
    /// The push service no longer knows the subscription (its HTTP status): forget the device.
    Gone(u16),
    /// Why it wasn't delivered, in a line.
    Failed(String),
}

/// How the server sends: its client, key, contact and where pushes may go.
pub struct Sender<'a> {
    pub client: &'a Client,
    pub keys: &'a Keys,
    pub contact: &'a str,
    pub reach: &'a Reach,
}

impl Sender<'_> {
    /// Sends `m` to `d`, waiting `waits` seconds before each attempt until one is taken.
    pub async fn deliver(&self, d: &Device<'_>, m: &Message<'_>, waits: &[u64]) -> Outcome {
        let body = match super::ece::encrypt(m.payload, d.p256dh, d.auth) {
            Ok(b) => b,
            Err(e) => return Outcome::Failed(format!("it can't be encrypted for this device: {e}")),
        };
        let service = vapid::origin(d.endpoint).map(|o| o.trim_start_matches("https://").to_owned()).unwrap_or_else(|_| "its push service".to_owned());
        let mut last = String::from("not sent");
        let mut asked = 0u64;
        for wait in waits {
            let wait = (*wait).max(asked);
            if wait > 0 {
                crate::rt::sleep(Duration::from_secs(wait)).await;
            }
            let authorization = match self.keys.authorization(d.endpoint, self.contact, crate::util::now_unix()) {
                Ok(a) => a,
                Err(e) => return Outcome::Failed(format!("{e:#}")),
            };
            let mut req = self
                .client
                .post(d.endpoint)
                .reaching(self.reach)
                .header("TTL", &m.ttl.to_string())
                .header("Urgency", m.urgency)
                .header("Content-Encoding", "aes128gcm")
                .header("Content-Type", "application/octet-stream")
                .header("Authorization", &authorization)
                .body(body.clone())
                .timeout(Duration::from_secs(15));
            if let Some(t) = m.topic {
                req = req.header("Topic", t);
            }
            match req.send().await {
                Ok(r) if r.is_success() => return Outcome::Delivered,
                Ok(r) => {
                    let status = r.status();
                    asked = r.header("retry-after").and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0).min(MAX_RETRY_AFTER);
                    let said = r.bytes(4096).await.map(|b| String::from_utf8_lossy(&b).split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_default();
                    last = if said.is_empty() { format!("{service} answered HTTP {status}") } else { format!("{service} answered HTTP {status}: {}", super::watch::clip(&said, 160)) };
                    match status {
                        404 | 410 => return Outcome::Gone(status),
                        429 | 500..=599 => continue,
                        _ => return Outcome::Failed(last),
                    }
                }
                Err(e) => {
                    asked = 0;
                    last = format!("{service} couldn't be reached: {e:#}");
                }
            }
        }
        Outcome::Failed(last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Arc;

    use aws_lc_rs::agreement;
    use aws_lc_rs::encoding::{AsBigEndian as _, EcPrivateKeyBin};
    use parking_lot::Mutex;

    /// A push service: answers each path with the statuses it's given (then 201), keeps what came.
    #[derive(Default)]
    pub(crate) struct FakePush {
        pub answers: Mutex<std::collections::HashMap<String, VecDeque<(u16, Option<&'static str>)>>>,
        pub got: Mutex<Vec<(String, Vec<(String, String)>, Vec<u8>)>>,
    }

    impl crate::http::server::Handler for FakePush {
        async fn handle(self: Arc<Self>, req: crate::http::server::Request) -> crate::http::server::Response {
            use crate::http::server as srv;
            use http_body_util::BodyExt as _;
            let path = req.uri().path().to_owned();
            let headers: Vec<(String, String)> = req.headers().iter().filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned()))).collect();
            let body = req.into_body().collect().await.map(|b| b.to_bytes().to_vec()).unwrap_or_default();
            self.got.lock().push((path.clone(), headers, body));
            let (status, retry) = self.answers.lock().get_mut(&path).and_then(VecDeque::pop_front).unwrap_or((201, None));
            let r = srv::response(http::StatusCode::from_u16(status).unwrap_or(http::StatusCode::IM_A_TEAPOT), "text/plain", srv::full(if status >= 400 { "{\"reason\":\"no\"}" } else { "" }));
            match retry {
                Some(s) => srv::with_header(r, http::header::RETRY_AFTER, s),
                None => r,
            }
        }
    }

    pub(crate) async fn serve(fake: Arc<FakePush>) -> anyhow::Result<String> {
        let tcp = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = tcp.local_addr()?;
        let tcp = hyper_rt::tcp::TcpListener::from_owned(hyper_rt::tcp::OwnedStream::from(tcp), 16).map_err(|e| anyhow::anyhow!("{e}"))?;
        crate::rt::spawn(async move {
            while let Ok(stream) = tcp.accept().await {
                let (h, remote) = (Arc::clone(&fake), stream.peer_addr().ok());
                crate::rt::spawn(crate::http::server::serve_connection(crate::http::io::RtIo::new(stream), h, remote));
            }
        });
        Ok(format!("http://{addr}"))
    }

    /// A browser's keys: private (raw), public, auth.
    pub(crate) fn browser_keys() -> anyhow::Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
        let k = agreement::PrivateKey::generate(&agreement::ECDH_P256).map_err(|e| anyhow::anyhow!("{e}"))?;
        let public = k.compute_public_key().map_err(|e| anyhow::anyhow!("{e}"))?.as_ref().to_vec();
        let private: EcPrivateKeyBin<'static> = k.as_be_bytes().map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut auth = vec![0u8; 16];
        aws_lc_rs::rand::fill(&mut auth).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok((private.as_ref().to_vec(), public, auth))
    }

    fn header<'a>(h: &'a [(String, String)], name: &str) -> Option<&'a str> {
        h.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    #[test]
    fn a_push_reaches_the_device_and_only_it_reads_it() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            crate::rt::timeout(Duration::from_secs(60), async {
                let dir = tempfile::tempdir()?;
                let keys = Keys::load_or_create(&dir.path().join("vapid.pk8"))?;
                let fake = Arc::new(FakePush::default());
                let base = serve(Arc::clone(&fake)).await?;
                let client = Client::public()?;
                let reach = Reach { allow: vec!["127.0.0.1".into()] };
                let sender = Sender { client: &client, keys: &keys, contact: "mailto:ops@example.com", reach: &reach };
                let (private, public, auth) = browser_keys()?;
                let payload = br#"{"title":"Mantle","body":"Asks: which database?","tag":"ag1","url":"/agents/ag1"}"#;
                let m = Message { payload, urgency: "high", topic: Some("abcdefghijklmnopqrstuvwxyz012345"), ttl: 86_400 };

                let endpoint = format!("{base}/push/ok");
                let d = Device { endpoint: &endpoint, p256dh: &public, auth: &auth };
                assert_eq!(sender.deliver(&d, &m, &[0]).await, Outcome::Delivered);
                let (path, headers, body) = fake.got.lock().first().cloned().ok_or_else(|| anyhow::anyhow!("nothing came"))?;
                assert_eq!(path, "/push/ok");
                assert_eq!(header(&headers, "ttl"), Some("86400"));
                assert_eq!(header(&headers, "urgency"), Some("high"));
                assert_eq!(header(&headers, "topic"), Some("abcdefghijklmnopqrstuvwxyz012345"));
                assert_eq!(header(&headers, "content-encoding"), Some("aes128gcm"));
                // Signed for the push service's origin, with the server's key.
                let auth_h = header(&headers, "authorization").unwrap_or_default();
                assert!(auth_h.starts_with("vapid t=") && auth_h.ends_with(&format!(", k={}", keys.public)), "{auth_h}");
                let claims = auth_h.trim_start_matches("vapid t=").split('.').nth(1).and_then(vapid::unb64).unwrap_or_default();
                let claims: serde_json::Value = serde_json::from_slice(&claims)?;
                assert_eq!(claims.get("aud").and_then(|v| v.as_str()), Some(base.as_str()));
                // The device reads it; the push service, with nothing but the body, can't.
                assert_eq!(super::super::ece::decrypt(&body, &private, &public, &auth)?, payload);
                assert!(!body.windows(7).any(|w| w == b"Mantle\""), "the body is encrypted");

                // Gone: 410 (and 404) say forget the device.
                let gone = format!("{base}/push/gone");
                fake.answers.lock().insert("/push/gone".into(), VecDeque::from([(410, None)]));
                assert_eq!(sender.deliver(&Device { endpoint: &gone, ..d.clone() }, &m, &[0, 0]).await, Outcome::Gone(410));

                // Busy, then taken: tried again.
                let busy = format!("{base}/push/busy");
                fake.answers.lock().insert("/push/busy".into(), VecDeque::from([(503, None), (429, Some("0"))]));
                assert_eq!(sender.deliver(&Device { endpoint: &busy, ..d.clone() }, &m, &[0, 0, 0]).await, Outcome::Delivered);
                assert_eq!(fake.got.lock().iter().filter(|g| g.0 == "/push/busy").count(), 3);

                // Refused: not tried again; the reason is kept for Settings.
                let bad = format!("{base}/push/bad");
                fake.answers.lock().insert("/push/bad".into(), VecDeque::from([(403, None)]));
                match sender.deliver(&Device { endpoint: &bad, ..d.clone() }, &m, &[0, 0]).await {
                    Outcome::Failed(why) => assert!(why.contains("HTTP 403") && why.contains("reason"), "{why}"),
                    other => anyhow::bail!("{other:?}"),
                }
                assert_eq!(fake.got.lock().iter().filter(|g| g.0 == "/push/bad").count(), 1);

                // Busy every time: given up, bounded.
                let down = format!("{base}/push/down");
                fake.answers.lock().insert("/push/down".into(), VecDeque::from([(500, None), (500, None)]));
                assert!(matches!(sender.deliver(&Device { endpoint: &down, ..d.clone() }, &m, &[0, 0]).await, Outcome::Failed(w) if w.contains("HTTP 500")));

                // A push service on this machine that `allow_private` doesn't name: never reached.
                let shut = Reach::default();
                let strict = Sender { reach: &shut, ..sender };
                let before = fake.got.lock().len();
                assert!(matches!(strict.deliver(&d, &m, &[0]).await, Outcome::Failed(_)));
                assert_eq!(fake.got.lock().len(), before);
                Ok(())
            })
            .await
            .map_err(|_| anyhow::anyhow!("timed out"))?
        })
    }
}

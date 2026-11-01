//! A host's fake for githost's own tests (docs/ci.md W29-R11, R12): it answers each request as its
//! test says, and keeps what it was sent.

use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::Value;

use crate::http::server::{self as srv, Request, Response};

/// How a fake answers a request (its method, its path with `%2F` read as `/` and without its
/// query, the query, and its JSON body): a status and JSON.
pub(crate) type Answer = Box<dyn Fn(&str, &str, &str, &Value) -> (u16, Value) + Send + Sync>;

/// A fake host, and what it was sent: each request's method, path, query and JSON body, in order.
pub(crate) struct Fake {
    seen: Mutex<Vec<(String, String, String, Value)>>,
    answer: Answer,
}

impl Fake {
    /// The bodies of the requests sent as `method` to `path`, in order.
    pub(crate) fn sent(&self, method: &str, path: &str) -> Vec<Value> {
        self.seen.lock().iter().filter(|(m, p, ..)| m == method && p == path).map(|(.., b)| b.clone()).collect()
    }
}

impl srv::Handler for Fake {
    async fn handle(self: Arc<Self>, req: Request) -> Response {
        let method = req.method().as_str().to_owned();
        let path = req.uri().path().replace("%2F", "/").replace("%2f", "/");
        let query = req.uri().query().map(srv::decode).unwrap_or_default();
        let raw = srv::body_bytes(req.into_body(), 1 << 20).await.unwrap_or_default();
        let body: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
        let (st, v) = (self.answer)(&method, &path, &query, &body);
        self.seen.lock().push((method, path, query, body));
        srv::json(http::StatusCode::from_u16(st).unwrap_or(http::StatusCode::INTERNAL_SERVER_ERROR), &v)
    }
}

/// Serves a fake host on a free port: its base URL, and the fake.
pub(crate) async fn start(answer: Answer) -> anyhow::Result<(String, Arc<Fake>)> {
    let tcp = hyper_rt::tcp::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)), 64).map_err(|e| anyhow::anyhow!("{e}"))?;
    let addr = tcp.local_addr().map_err(|e| anyhow::anyhow!("{e}"))?;
    let fake = Arc::new(Fake { seen: Mutex::default(), answer });
    let f = Arc::clone(&fake);
    crate::rt::spawn(async move {
        while let Ok(s) = tcp.accept().await {
            let (h, r) = (Arc::clone(&f), s.peer_addr().ok());
            crate::rt::spawn(srv::serve_connection(crate::http::io::RtIo::new(s), h, r));
        }
    });
    Ok((format!("http://{addr}"), fake))
}

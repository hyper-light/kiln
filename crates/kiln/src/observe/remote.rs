//! Prometheus's remote write, taken (`docs/observe.md` §8.5): `POST /api/v1/write` on the server
//! (the ingest token as a bearer token, or as basic auth's password: the two ways Prometheus's
//! `remote_write` sends credentials) and on every runner's local receiver (127.0.0.1:4318), as the
//! remote write specifications have a receiver answer: 204 once taken (2.0's counts in its
//! headers), 400 for what does not decode (not sent again), 415 for a message of neither protocol,
//! 503 with `Retry-After` while the node is behind (sent again, as Prometheus retries 5xx).

use std::sync::OnceLock;

use http::{StatusCode, header};
use kiln_obs::promwrite::{Families, MAX_BODY, Version};

use super::Observe;
use crate::http::server as srv;

/// The metric families' types this node has learned from 1.0's metadata.
fn families() -> &'static Families {
    static F: OnceLock<Families> = OnceLock::new();
    F.get_or_init(Families::new)
}

/// What a request came to.
enum Answer {
    Taken(kiln_obs::promwrite::Taken),
    Bad(String),
    Unsupported,
    Busy,
}

/// The ingest token a writer sends: `Authorization: Bearer <token>`, or basic auth with the token
/// as its password (any user).
pub fn token(headers: &http::HeaderMap) -> Option<String> {
    use base64::Engine as _;
    let auth = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    if let Some(t) = auth.strip_prefix("Bearer ") {
        return Some(t.trim().to_owned());
    }
    let basic = auth.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(basic.trim()).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    text.split_once(':').map(|(_, password)| password.to_owned())
}

fn take(o: &Observe, content_type: &str, snappy: bool, body: &[u8]) -> Answer {
    let Some(version) = Version::of(content_type) else { return Answer::Unsupported };
    if o.busy() {
        return Answer::Busy;
    }
    let mut plain = o.bytes();
    let answer = (|| {
        let msg: &[u8] = if snappy {
            if let Err(e) = kiln_obs::promwrite::unsnappy(body, &mut plain, MAX_BODY) {
                return Answer::Bad(e.to_string());
            }
            &plain
        } else {
            body
        };
        let mut d = o.decoding();
        match kiln_obs::promwrite::decode_into(msg, version, super::otlp::keys(), families(), &mut d) {
            Ok(taken) => {
                if o.offer_samples(d) {
                    Answer::Taken(taken)
                } else {
                    Answer::Busy
                }
            }
            Err(e) => {
                o.give_decoding(d);
                Answer::Bad(e.to_string())
            }
        }
    })();
    o.give_bytes(plain);
    answer
}

/// A remote write request answered: read whole (at most [`MAX_BODY`]), taken on the blocking pool.
pub async fn answer(o: &Observe, req: srv::Request) -> srv::Response {
    let content_type = srv::header_str(&req, &header::CONTENT_TYPE).unwrap_or("application/x-protobuf").to_owned();
    let snappy = srv::header_str(&req, &header::CONTENT_ENCODING).is_some_and(|e| e.trim().eq_ignore_ascii_case("snappy"));
    let body = match srv::body_bytes(req.into_body(), MAX_BODY + 1).await {
        Ok(b) => b,
        Err(_) => return srv::text(StatusCode::PAYLOAD_TOO_LARGE, "a remote write request is taken up to 32 MiB"),
    };
    let o = o.clone();
    let taken = crate::rt::blocking(move || take(&o, &content_type, snappy, &body)).await;
    match taken {
        Ok(Answer::Taken(t)) => {
            // 2.0's counts of what was written (1.0's senders pass them over).
            let r = srv::status(StatusCode::NO_CONTENT);
            let r = srv::with_header(r, header::HeaderName::from_static("x-prometheus-remote-write-samples-written"), &t.samples.to_string());
            let r = srv::with_header(r, header::HeaderName::from_static("x-prometheus-remote-write-histograms-written"), "0");
            srv::with_header(r, header::HeaderName::from_static("x-prometheus-remote-write-exemplars-written"), "0")
        }
        Ok(Answer::Bad(why)) => srv::text(StatusCode::BAD_REQUEST, &why),
        Ok(Answer::Unsupported) => srv::text(StatusCode::UNSUPPORTED_MEDIA_TYPE, "remote write 1.0 (application/x-protobuf) or 2.0 (application/x-protobuf;proto=io.prometheus.write.v2.Request)"),
        Ok(Answer::Busy) => srv::with_header(srv::text(StatusCode::SERVICE_UNAVAILABLE, "behind on writes: try again"), header::RETRY_AFTER, "1"),
        Err(e) => srv::text(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_writer_s_token_is_its_bearer_or_its_basic_password() {
        use base64::Engine as _;
        let mut h = http::HeaderMap::new();
        h.insert(header::AUTHORIZATION, http::HeaderValue::from_static("Bearer t0k"));
        assert_eq!(token(&h).as_deref(), Some("t0k"));
        let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("prometheus:s3cret:with:colons"));
        h.insert(header::AUTHORIZATION, http::HeaderValue::from_str(&basic).unwrap_or(http::HeaderValue::from_static("")));
        assert_eq!(token(&h).as_deref(), Some("s3cret:with:colons"));
        h.insert(header::AUTHORIZATION, http::HeaderValue::from_static("Basic !!!"));
        assert_eq!(token(&h), None);
    }
}

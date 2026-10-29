//! AWS Signature Version 4: what signs every request kiln makes of AWS (CloudWatch's, STS's, S3's).
//! Checked against AWS's own test suite (`aws4_testsuite`, as botocore carries it): each case's
//! canonical request, string to sign and signature, to the byte; S3's ways (its path encoded once,
//! its payload's hash a signed header) checked live against MinIO, which signs as S3 does
//! (`tests/observe_archives.rs`).

use sha2::Digest as _;
use time::OffsetDateTime;

/// AWS credentials: an access key, its secret, and a session's token for temporary ones.
#[derive(Clone, PartialEq, Eq)]
pub struct Keys {
    pub access_key: String,
    pub secret: String,
    pub token: Option<String>,
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keys").field("access_key", &self.access_key).field("secret", &"…").field("token", &self.token.as_ref().map(|_| "…")).finish()
    }
}

/// A request to sign: its method, path, query (names and values as they are, unencoded),
/// headers (`host` among them) and body.
pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a [(&'a str, &'a str)],
    pub headers: &'a [(&'a str, &'a str)],
    pub body: &'a [u8],
}

/// RFC 3986's unreserved characters left, every other byte `%XX` (AWS's `UriEncode`); `/` kept
/// when `slash` (a path's): an S3 object's path in its URL is encoded so, as it is signed.
pub(crate) fn uri_encode(s: &str, slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b'/' if slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn sha256_hex(b: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(b))
}

fn hmac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    use hmac::{KeyInit as _, Mac as _};
    match hmac::Hmac::<sha2::Sha256>::new_from_slice(key) {
        Ok(mut m) => {
            m.update(msg);
            m.finalize().into_bytes().to_vec()
        }
        Err(_) => Vec::new(),
    }
}

/// `20150830T123600Z` and `20150830`.
fn stamps(at: OffsetDateTime) -> (String, String) {
    let at = at.to_offset(time::UtcOffset::UTC);
    let date = format!("{:04}{:02}{:02}", at.year(), u8::from(at.month()), at.day());
    (format!("{date}T{:02}{:02}{:02}Z", at.hour(), at.minute(), at.second()), date)
}

/// The canonical request (its lines joined by `\n`) and its signed headers; `s3`'s path encoded
/// once, as S3 wants (twice for every other service).
fn canonical(req: &Request<'_>, s3: bool) -> (String, String) {
    let path = match req.path {
        "" => "/".to_owned(),
        p if s3 => uri_encode(p, true),
        p => uri_encode(&uri_encode(p, true), true),
    };
    let mut query: Vec<(String, String)> = req.query.iter().map(|(k, v)| (uri_encode(k, false), uri_encode(v, false))).collect();
    query.sort();
    let query = query.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    // Names lower-cased and sorted; values trimmed, their runs of spaces one; a name twice, its
    // values joined by commas.
    let mut headers: Vec<(String, String)> = req.headers.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.split_whitespace().collect::<Vec<_>>().join(" "))).collect();
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    let mut merged: Vec<(String, String)> = Vec::with_capacity(headers.len());
    for (k, v) in headers {
        match merged.last_mut() {
            Some((lk, lv)) if *lk == k => {
                lv.push(',');
                lv.push_str(&v);
            }
            _ => merged.push((k, v)),
        }
    }
    let signed = merged.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let lines = merged.iter().map(|(k, v)| format!("{k}:{v}\n")).collect::<String>();
    (format!("{}\n{path}\n{query}\n{lines}\n{signed}\n{}", req.method, sha256_hex(req.body)), signed)
}

/// The headers to send besides the request's own: `x-amz-date`, `x-amz-security-token` with a
/// session's credentials, S3's `x-amz-content-sha256` (the payload's hash, which S3 requires), each
/// signed, and `authorization`.
pub fn sign(keys: &Keys, region: &str, service: &str, req: &Request<'_>, at: OffsetDateTime) -> Vec<(String, String)> {
    let (amz_date, date) = stamps(at);
    let s3 = service == "s3";
    let payload = if s3 { sha256_hex(req.body) } else { String::new() };
    let mut headers: Vec<(&str, &str)> = req.headers.to_vec();
    headers.push(("x-amz-date", &amz_date));
    if let Some(t) = &keys.token {
        headers.push(("x-amz-security-token", t));
    }
    if s3 {
        headers.push(("x-amz-content-sha256", &payload));
    }
    let signed_req = Request { headers: &headers, ..*req };
    let (creq, signed) = canonical(&signed_req, s3);
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(creq.as_bytes()));
    let k = [date.as_str(), region, service, "aws4_request"].iter().fold(format!("AWS4{}", keys.secret).into_bytes(), |k, part| hmac(&k, part.as_bytes()));
    let signature = hex::encode(hmac(&k, to_sign.as_bytes()));
    let mut out = vec![("x-amz-date".to_owned(), amz_date.clone())];
    if let Some(t) = &keys.token {
        out.push(("x-amz-security-token".to_owned(), t.clone()));
    }
    if s3 {
        out.push(("x-amz-content-sha256".to_owned(), payload));
    }
    out.push(("authorization".to_owned(), format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}", keys.access_key)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Keys {
        Keys { access_key: "AKIDEXAMPLE".into(), secret: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(), token: None }
    }

    fn at() -> OffsetDateTime {
        time::macros::datetime!(2015-08-30 12:36:00 UTC)
    }

    /// AWS's test suite (`aws4_testsuite`, as botocore carries it): get-vanilla, post-vanilla and
    /// get-vanilla-query-order-key-case, each its canonical request, string to sign and signature.
    #[test]
    fn aws_s_own_test_suite_signs_alike() {
        let cases: [(&str, &[(&str, &str)], &str, &str, &str); 3] = [
            (
                "GET",
                &[],
                "GET\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                "bb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63",
                "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31",
            ),
            (
                "POST",
                &[],
                "POST\n/\n\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                "553f88c9e4d10fc9e109e2aeb65f030801b70c2f6468faca261d401ae622fc87",
                "5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b",
            ),
            (
                "GET",
                &[("Param2", "value2"), ("Param1", "value1")],
                "GET\n/\nParam1=value1&Param2=value2\nhost:example.amazonaws.com\nx-amz-date:20150830T123600Z\n\nhost;x-amz-date\ne3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                "816cd5b414d056048ba4f7c5386d6e0533120fb1fcfa93762cf0fc39e2cf19e0",
                "b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500",
            ),
        ];
        for (method, query, creq, creq_hash, signature) in cases {
            let headers = [("Host", "example.amazonaws.com"), ("X-Amz-Date", "20150830T123600Z")];
            let req = Request { method, path: "/", query, headers: &headers, body: b"" };
            let (got, _) = canonical(&req, false);
            assert_eq!(got, creq, "{method} {query:?}");
            assert_eq!(sha256_hex(got.as_bytes()), creq_hash);
            let signed = sign(&keys(), "us-east-1", "service", &Request { headers: &[("Host", "example.amazonaws.com")], ..req }, at());
            let authz = signed.iter().find(|(k, _)| k == "authorization").map(|(_, v)| v.as_str()).unwrap_or_default();
            assert_eq!(authz, format!("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature={signature}"));
        }
    }

    #[test]
    fn a_session_s_token_is_sent_and_signed_and_paths_and_values_are_encoded() {
        let k = Keys { token: Some("SESSION".into()), ..keys() };
        let headers = [("host", "monitoring.us-east-1.amazonaws.com")];
        let signed = sign(&k, "us-east-1", "monitoring", &Request { method: "POST", path: "/", query: &[], headers: &headers, body: b"{}" }, at());
        let authz = signed.iter().find(|(h, _)| h == "authorization").map(|(_, v)| v.clone()).unwrap_or_default();
        assert!(authz.contains("SignedHeaders=host;x-amz-date;x-amz-security-token"), "{authz}");
        assert!(signed.iter().any(|(h, v)| h == "x-amz-security-token" && v == "SESSION"));
        assert_eq!(uri_encode("a b/c=d", false), "a%20b%2Fc%3Dd");
        assert_eq!(uri_encode("/a b", true), "/a%20b");
    }

    /// S3's ways: its path encoded once (`$` as `%24`, not `%2524`), every other service's twice;
    /// the payload's hash sent as `x-amz-content-sha256`, signed, and the canonical request's last
    /// line.
    #[test]
    fn s3_s_path_is_encoded_once_and_its_payload_s_hash_is_signed() {
        let body = b"Welcome to Amazon S3.";
        let headers = [("Host", "examplebucket.s3.amazonaws.com")];
        let req = Request { method: "PUT", path: "/test$file.text", query: &[], headers: &headers, body };
        let (s3, _) = canonical(&req, true);
        let (other, _) = canonical(&req, false);
        assert_eq!(s3.lines().nth(1), Some("/test%24file.text"));
        assert_eq!(other.lines().nth(1), Some("/test%2524file.text"));
        let signed = sign(&keys(), "us-east-1", "s3", &req, at());
        let hash = sha256_hex(body);
        assert!(signed.iter().any(|(h, v)| h == "x-amz-content-sha256" && *v == hash), "{signed:?}");
        let authz = signed.iter().find(|(h, _)| h == "authorization").map(|(_, v)| v.clone()).unwrap_or_default();
        assert!(authz.contains("/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, "), "{authz}");
        let monitoring = sign(&keys(), "us-east-1", "monitoring", &req, at());
        assert!(!monitoring.iter().any(|(h, _)| h == "x-amz-content-sha256"), "only S3's");
    }
}

//! The server's VAPID key pair (RFC 8292): made once, kept with the server's data, and the
//! `Authorization: vapid` header that signs each push with it. Browsers subscribe with the public
//! key; push services take only what the private key signed. The private key is never logged and
//! no API returns it.

use std::path::Path;

use anyhow::Context as _;
use aws_lc_rs::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _};
use base64::Engine as _;

/// How long a signature lasts (push services take at most 24 hours).
const VALID_SECONDS: i64 = 12 * 3600;

pub fn b64(bytes: impl AsRef<[u8]>) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// base64url, as browsers give keys (padded or not; standard base64 too).
pub fn unb64(s: &str) -> Option<Vec<u8>> {
    let clean: String = s.trim().trim_end_matches('=').chars().map(|c| match c {
        '+' => '-',
        '/' => '_',
        c => c,
    }).collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(clean).ok()
}

pub struct Keys {
    pair: EcdsaKeyPair,
    /// The public key, uncompressed (65 bytes), base64url: what browsers subscribe with.
    pub public: String,
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keys").field("public", &self.public).field("private", &"<redacted>").finish()
    }
}

impl Keys {
    /// The key pair kept in `path` (PKCS#8, written 0600), made there first when there's none. A
    /// file that's there but can't be read is an error, never replaced: a new key would leave
    /// every device subscribed to the old one.
    pub fn load_or_create(path: &Path) -> anyhow::Result<Keys> {
        match std::fs::read(path) {
            Ok(der) => Self::from_pkcs8(&der).with_context(|| format!("{} isn't a key kiln can use: remove it, and kiln makes a new one (each device then turns notifications on again)", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let pair = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_FIXED_SIGNING).map_err(|_| anyhow::anyhow!("making a P-256 key pair failed"))?;
                let doc = pair.to_pkcs8v1().map_err(|_| anyhow::anyhow!("writing the key pair as PKCS#8 failed"))?;
                crate::util::write_secret(path, doc.as_ref(), 0o600).with_context(|| format!("keeping the push key in {}", path.display()))?;
                tracing::info!(path = %path.display(), "made the server's push key (VAPID)");
                Self::from_pkcs8(doc.as_ref())
            }
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    fn from_pkcs8(der: &[u8]) -> anyhow::Result<Keys> {
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, der).map_err(|e| anyhow::anyhow!("not a P-256 key: {e}"))?;
        let public = b64(pair.public_key().as_ref());
        Ok(Keys { pair, public })
    }

    /// `Authorization` for a push to `endpoint` at `now` (Unix seconds): a JWT for the endpoint's
    /// origin, signed with ES256, valid 12 hours, naming `contact`; then the public key.
    pub fn authorization(&self, endpoint: &str, contact: &str, now: i64) -> anyhow::Result<String> {
        let aud = origin(endpoint)?;
        let header = b64(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = b64(serde_json::to_vec(&serde_json::json!({ "aud": aud, "exp": now + VALID_SECONDS, "sub": contact }))?);
        let input = format!("{header}.{claims}");
        let sig = self.pair.sign(&aws_lc_rs::rand::SystemRandom::new(), input.as_bytes()).map_err(|_| anyhow::anyhow!("signing failed"))?;
        Ok(format!("vapid t={input}.{}, k={}", b64(sig.as_ref()), self.public))
    }
}

/// `scheme://host[:port]` of a URL, its default port left out (RFC 6454).
pub fn origin(url: &str) -> anyhow::Result<String> {
    let uri: http::Uri = url.parse().map_err(|e| anyhow::anyhow!("not a URL ({e})"))?;
    let scheme = uri.scheme_str().ok_or_else(|| anyhow::anyhow!("a URL without a scheme"))?.to_ascii_lowercase();
    let host = uri.host().ok_or_else(|| anyhow::anyhow!("a URL without a host"))?.to_ascii_lowercase();
    Ok(match uri.port_u16() {
        Some(p) if !(scheme == "https" && p == 443) && !(scheme == "http" && p == 80) => format!("{scheme}://{host}:{p}"),
        _ => format!("{scheme}://{host}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_made_once_and_kept_private() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("push").join("vapid.pk8");
        let made = Keys::load_or_create(&path)?;
        let again = Keys::load_or_create(&path)?;
        assert_eq!(made.public, again.public, "kept, not made again");
        assert_eq!(unb64(&made.public).map(|k| k.len()), Some(65));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        }
        assert!(!format!("{made:?}").contains(&b64(std::fs::read(&path)?)), "Debug shows no private key");
        // A file that isn't a key is an error, never replaced.
        std::fs::write(&path, b"not a key")?;
        assert!(Keys::load_or_create(&path).is_err());
        assert_eq!(std::fs::read(&path)?, b"not a key");
        Ok(())
    }

    /// RFC 8292: an ES256 JWT for the push service's origin, under 24 hours, with the key after.
    #[test]
    fn rfc8292_authorization() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let keys = Keys::load_or_create(&dir.path().join("vapid.pk8"))?;
        let now = 1_760_000_000;
        let auth = keys.authorization("https://fcm.googleapis.com:443/fcm/send/abc:def", "mailto:ops@example.com", now)?;
        let (t, k) = auth.strip_prefix("vapid t=").and_then(|r| r.split_once(", k=")).ok_or_else(|| anyhow::anyhow!("{auth}"))?;
        assert_eq!(k, keys.public);
        let parts: Vec<&str> = t.split('.').collect();
        let [header, claims, sig] = parts.as_slice() else { anyhow::bail!("not a JWT: {t}") };
        let header: serde_json::Value = serde_json::from_slice(&unb64(header).unwrap_or_default())?;
        assert_eq!(header, serde_json::json!({ "typ": "JWT", "alg": "ES256" }));
        let claims: serde_json::Value = serde_json::from_slice(&unb64(claims).unwrap_or_default())?;
        assert_eq!(claims.get("aud").and_then(|v| v.as_str()), Some("https://fcm.googleapis.com"));
        assert_eq!(claims.get("sub").and_then(|v| v.as_str()), Some("mailto:ops@example.com"));
        let exp = claims.get("exp").and_then(serde_json::Value::as_i64).unwrap_or_default();
        assert!(exp > now && exp - now <= 24 * 3600);
        // The signature checks with the public key, as a push service checks it.
        let public = unb64(k).unwrap_or_default();
        let key = aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::ECDSA_P256_SHA256_FIXED, public);
        let signed = format!("{}.{}", parts.first().copied().unwrap_or_default(), parts.get(1).copied().unwrap_or_default());
        assert!(key.verify(signed.as_bytes(), &unb64(sig).unwrap_or_default()).is_ok());
        assert!(key.verify(b"something else", &unb64(sig).unwrap_or_default()).is_err());
        Ok(())
    }

    #[test]
    fn origins() -> anyhow::Result<()> {
        assert_eq!(origin("https://web.push.apple.com/QGz3")?, "https://web.push.apple.com");
        assert_eq!(origin("https://Updates.Push.Services.Mozilla.com:443/wpush/v2/x")?, "https://updates.push.services.mozilla.com");
        assert_eq!(origin("http://127.0.0.1:9001/push/1")?, "http://127.0.0.1:9001");
        assert!(origin("/relative").is_err());
        assert_eq!(unb64("BTBZMqHH6r4Tts7J_aSIgg"), unb64("BTBZMqHH6r4Tts7J/aSIgg=="));
        Ok(())
    }
}

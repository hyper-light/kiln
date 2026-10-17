//! A leg's token (docs/ci.md W10's precursor): what `GITHUB_TOKEN`, `ACTIONS_RUNTIME_TOKEN` and the
//! action downloads carry. `kiln.<leg>.<hmac>`: it says which leg it is, so a git client that
//! presents only the token (the facade, W14) can be checked, and it is verified without state.
//! W10's job tokens (JWTs with claims, expiry and permissions) replace it.

use crate::hub::Hub;

const PREFIX: &str = "kiln.";

pub fn leg_token(hub: &Hub, leg: &str) -> String {
    format!("{PREFIX}{leg}.{}", crate::util::hmac_hex(hub.admin_token.as_bytes(), format!("leg:{leg}").as_bytes()))
}

/// The leg a token is for, when it is genuine.
pub fn verify(hub: &Hub, token: &str) -> Option<String> {
    let rest = token.strip_prefix(PREFIX)?;
    let (leg, _) = rest.rsplit_once('.')?;
    crate::util::ct_eq(token.as_bytes(), leg_token(hub, leg).as_bytes()).then(|| leg.to_owned())
}

/// The token in a request: `Authorization: Bearer <token>` or Basic with the token as the
/// password (git's `x-access-token:<token>`, as `actions/checkout` sends it).
pub fn from_headers(headers: &http::HeaderMap) -> Option<String> {
    let v = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, value) = v.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") || scheme.eq_ignore_ascii_case("token") {
        return Some(value.trim().to_owned());
    }
    if scheme.eq_ignore_ascii_case("basic") {
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD.decode(value.trim()).ok()?;
        let s = String::from_utf8(decoded).ok()?;
        return s.split_once(':').map(|(_, p)| p.to_owned());
    }
    None
}

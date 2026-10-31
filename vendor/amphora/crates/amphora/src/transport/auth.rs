//! Direction-separated HMAC proofs authenticate both peers without transmitting the capability.
use super::{common::unavailable, wire::Hello};
use amphora_api::{self as api, Refusal};
use hmac::{Hmac, Mac};
use sha2::Sha256;
pub(super) fn nonce() -> Result<[u8; 32], Refusal> {
    let mut nonce = [0; 32];
    getrandom::fill(&mut nonce).map_err(unavailable)?;
    Ok(nonce)
}
fn context(
    token: &[u8; 32],
    server: bool,
    hello: &Hello,
    nonce: &[u8; 32],
) -> Result<Hmac<Sha256>, Refusal> {
    let mut mac = Hmac::<Sha256>::new_from_slice(token).map_err(unavailable)?;
    mac.update(if server {
        b"amphora.owner.server.v2"
    } else {
        b"amphora.owner.client.v2"
    });
    mac.update(&hello.version.to_be_bytes());
    mac.update(&hello.millis.to_be_bytes());
    mac.update(&hello.nonce);
    mac.update(nonce);
    Ok(mac)
}
pub(super) fn proof(
    token: &[u8; 32],
    server: bool,
    hello: &Hello,
    nonce: &[u8; 32],
) -> Result<[u8; 32], Refusal> {
    Ok(context(token, server, hello, nonce)?
        .finalize()
        .into_bytes()
        .into())
}
pub(super) fn verify(
    token: &[u8; 32],
    server: bool,
    hello: &Hello,
    nonce: &[u8; 32],
    proof: &[u8; 32],
) -> Result<(), Refusal> {
    context(token, server, hello, nonce)?
        .verify_slice(proof)
        .map_err(|_| api::invalid("owner authentication", "peer proof did not match"))
}

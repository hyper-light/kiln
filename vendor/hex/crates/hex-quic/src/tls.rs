//! TLS for Hex's links (docs/design/hex-quic.md Q5): TLS 1.3 alone, `TLS13_AES_256_GCM_SHA384` alone, the
//! hybrid post-quantum key exchange groups alone, and both sides presenting a certificate kiln's CA issued and
//! verifying the other's under its roots. Ada, 2026-11-11: "all such communications should be AESGCM256
//! bidirectional encrypted." TLS 1.3 negotiates one suite for both directions, each keyed apart (RFC 8446
//! §4.1.3, §7.3), so a peer that offers no AES-256-GCM is refused at its handshake. QUIC's Initial packets keep
//! AES-128-GCM under keys any observer derives from the connection ID (RFC 9001 §5.2); they carry no
//! application data, and hyper-quic takes their suite from aws-lc-rs whatever is offered here.

use std::sync::LazyLock;

use hyper_quic::rustls::{self, RootCertStore, server::WebPkiClientVerifier};
use hyper_transport::tls::{CertificateDer, PrivateKeyDer, Tls};

use crate::Refused;

/// The application protocol both sides negotiate (RFC 7301): Hex's lanes ([`hex_core::lanes`]), version 1. A
/// peer that speaks another is refused at the handshake; a change to the lanes' protocol that isn't
/// append-only names a new one.
pub const ALPN: &[u8] = b"hex/1";

/// The one suite every Hex link takes.
pub const SUITE: rustls::CipherSuite = rustls::CipherSuite::TLS13_AES_256_GCM_SHA384;

/// hyper-quic's node provider (aws-lc-rs; the hybrid post-quantum groups alone, SecP384r1MLKEM1024 first)
/// narrowed to `TLS13_AES_256_GCM_SHA384`, as kiln's reach links and Shards' QUIC links narrow it (kiln
/// `crates/kiln/src/reach/e2e.rs`, Shards e12d5b96 `crates/remote/src/tls.rs`): the node provider allows
/// ChaCha20-Poly1305 too.
static PROVIDER: LazyLock<rustls::crypto::CryptoProvider> =
    LazyLock::new(|| rustls::crypto::CryptoProvider {
        cipher_suites: vec![rustls::crypto::aws_lc_rs::cipher_suite::TLS13_AES_256_GCM_SHA384],
        ..hyper_quic::crypto::rustls::node_provider().clone()
    });

/// The provider every Hex link's configurations are built under.
pub fn provider() -> &'static rustls::crypto::CryptoProvider {
    &PROVIDER
}

/// A peer's credentials: its certificate chain, kiln's CA issued from its own key's request (the key never
/// left it: docs/design/hex-quic.md Q6), its private key, and the CA's roots its peers' certificates chain to.
pub struct Credentials {
    /// Its chain, its own certificate first.
    pub chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
    pub roots: Vec<CertificateDer<'static>>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("chain", &self.chain.len())
            .field("key", &"<redacted>")
            .field("roots", &self.roots.len())
            .finish()
    }
}

fn configuration(what: &'static str) -> impl Fn(rustls::Error) -> Refused {
    move |e| Refused::Configuration {
        what,
        why: e.to_string(),
    }
}

fn roots(certificates: &[CertificateDer<'static>]) -> Result<RootCertStore, Refused> {
    let mut roots = RootCertStore::empty();
    for certificate in certificates {
        roots
            .add(certificate.clone())
            .map_err(configuration("the CA's roots"))?;
    }
    if roots.is_empty() {
        return Err(Refused::Configuration {
            what: "the CA's roots",
            why: "none was given".to_owned(),
        });
    }
    Ok(roots)
}

/// The TLS a link runs under, built from `credentials` under [`provider`] on [`ALPN`]: the client side every
/// dial presents, and with `listen` the server side, which requires a certificate under the roots from every
/// peer. hyper-transport turns 0-RTT off on both sides (its `Tls::prepare`).
pub fn tls(credentials: &Credentials, listen: bool) -> Result<Tls, Refused> {
    tls_under(provider(), credentials, listen)
}

/// [`tls`] under another provider: what a test of a peer limited to another suite builds.
pub(crate) fn tls_under(
    provider: &'static rustls::crypto::CryptoProvider,
    credentials: &Credentials,
    listen: bool,
) -> Result<Tls, Refused> {
    let server = if listen {
        let verifier =
            WebPkiClientVerifier::builder_with_provider(roots(&credentials.roots)?, provider)
                .build()
                .map_err(|e| Refused::Configuration {
                    what: "the peers' verifier",
                    why: e.to_string(),
                })?;
        let mut server = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(configuration("the server side"))?
            .with_client_cert_verifier(verifier)
            .with_single_cert(credentials.chain.clone(), credentials.key.clone_key())
            .map_err(configuration("this peer's certificate"))?;
        server.alpn_protocols = vec![ALPN.to_vec()];
        Some(server)
    } else {
        None
    };
    let mut client = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(configuration("the client side"))?
        .with_root_certificates(roots(&credentials.roots)?)
        .with_client_auth_cert(credentials.chain.clone(), credentials.key.clone_key())
        .map_err(configuration("this peer's certificate"))?;
    client.alpn_protocols = vec![ALPN.to_vec()];
    Ok(Tls { server, client })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HEX-QUIC-8 (CP-T7.3; Ada 2026-11-11): every Hex link's provider offers `TLS13_AES_256_GCM_SHA384`
    /// alone, and only hybrid post-quantum key exchange groups.
    #[test]
    fn hex_quic_8_the_provider_offers_aes_256_gcm_alone_and_hybrid_groups_alone() {
        let suites: Vec<rustls::CipherSuite> =
            provider().cipher_suites.iter().map(|s| s.suite()).collect();
        assert_eq!(suites, vec![SUITE]);
        let groups: Vec<rustls::NamedGroup> =
            provider().kx_groups.iter().map(|g| g.name()).collect();
        assert!(!groups.is_empty());
        for group in groups {
            assert!(
                matches!(
                    group,
                    rustls::NamedGroup::secp384r1MLKEM1024
                        | rustls::NamedGroup::X25519MLKEM768
                        | rustls::NamedGroup::secp256r1MLKEM768
                ),
                "{group:?} isn't a hybrid post-quantum group"
            );
        }
    }
}

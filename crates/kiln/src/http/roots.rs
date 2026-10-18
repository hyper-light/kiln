//! The machine's own trusted certificates, trusted beside the public roots for HTTPS: where a
//! company installs the root of a TLS-inspecting proxy (Zscaler, and the like), which re-signs
//! every HTTPS connection with it. Without them kiln's HTTPS (joining a server, the runner's
//! tunnel to it, artifacts, GitHub) fails on such a machine. kiln's own connections to its server
//! stay pinned to kiln's CA inside, whatever the outer TLS: QUIC runs end to end through the
//! tunnel.
//!
//! Read once: `SSL_CERT_FILE` and `SSL_CERT_DIR` (PEM, as OpenSSL takes them), and the system's
//! store: on macOS the keychains (`security`), on Windows the Root stores, elsewhere the system's
//! CA bundle.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use hyper_quic::rustls::pki_types::CertificateDer;
use hyper_quic::rustls::pki_types::pem::PemObject as _;

/// The system's certificates (DER), read once.
pub fn system() -> &'static [CertificateDer<'static>] {
    static ROOTS: OnceLock<Vec<CertificateDer<'static>>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let mut pem = Vec::new();
        let file = std::env::var_os("SSL_CERT_FILE").map(PathBuf::from);
        if let Some(f) = &file {
            pem.extend(std::fs::read(f).unwrap_or_default());
        }
        if let Some(dirs) = std::env::var_os("SSL_CERT_DIR") {
            for dir in std::env::split_paths(&dirs) {
                pem.extend(read_dir_pem(&dir));
            }
        }
        let mut certs = from_pem(&pem);
        certs.extend(platform(file.is_some()));
        if !certs.is_empty() {
            tracing::debug!(certificates = certs.len(), "trusting this machine's certificates beside the public roots");
        }
        certs
    })
}

/// Every certificate in a PEM bundle (others in it, and broken ones, skipped).
pub fn from_pem(pem: &[u8]) -> Vec<CertificateDer<'static>> {
    CertificateDer::pem_slice_iter(pem).filter_map(Result::ok).collect()
}

fn read_dir_pem(dir: &Path) -> Vec<u8> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_file() {
            out.extend(std::fs::read(&p).unwrap_or_default());
            out.push(b'\n');
        }
    }
    out
}

/// Output of a program that lists certificates, or nothing (it isn't there, or failed).
fn listed(program: &str, args: &[&str]) -> Vec<u8> {
    std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| o.stdout)
        .unwrap_or_default()
}

/// The system store's certificates. On Linux and the BSDs, the CA bundle unless `SSL_CERT_FILE`
/// named one already.
fn platform(have_file: bool) -> Vec<CertificateDer<'static>> {
    if cfg!(target_os = "macos") {
        // The user's search list (their login keychain, and the System keychain, where an
        // administrator's and an MDM's roots go), and Apple's own roots.
        let mut pem = listed("/usr/bin/security", &["find-certificate", "-a", "-p"]);
        pem.extend(listed("/usr/bin/security", &["find-certificate", "-a", "-p", "/System/Library/Keychains/SystemRootCertificates.keychain"]));
        return from_pem(&pem);
    }
    if cfg!(windows) {
        let script = "Get-ChildItem Cert:\\LocalMachine\\Root, Cert:\\CurrentUser\\Root | ForEach-Object { [Convert]::ToBase64String($_.RawData) }";
        let out = listed("powershell", &["-NoProfile", "-NonInteractive", "-Command", script]);
        return from_base64_lines(&String::from_utf8_lossy(&out));
    }
    if have_file {
        return Vec::new();
    }
    const BUNDLES: [&str; 6] = [
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/ssl/ca-bundle.pem",
        "/etc/pki/tls/cacert.pem",
        "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
        "/etc/ssl/cert.pem",
    ];
    BUNDLES.iter().find_map(|b| std::fs::read(b).ok()).map(|pem| from_pem(&pem)).unwrap_or_default()
}

/// One base64 DER certificate a line (Windows' export).
fn from_base64_lines(text: &str) -> Vec<CertificateDer<'static>> {
    use base64::Engine as _;
    text.lines()
        .filter_map(|l| base64::engine::general_purpose::STANDARD.decode(l.trim()).ok())
        .filter(|der| !der.is_empty())
        .map(CertificateDer::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn certificates_are_read_from_bundles_and_exports() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let one = crate::pki::Pki::open(dir.path())?.ca_pem.clone();
        let bundle = format!("# a company's bundle\n{one}\nnot a certificate\n{one}");
        assert_eq!(from_pem(bundle.as_bytes()).len(), 2);
        assert!(from_pem(b"nothing here").is_empty());
        let der = from_pem(one.as_bytes()).into_iter().next().ok_or_else(|| anyhow::anyhow!("no certificate"))?;
        use base64::Engine as _;
        let line = base64::engine::general_purpose::STANDARD.encode(der.as_ref());
        assert_eq!(from_base64_lines(&format!("{line}\r\n\r\nnot base64!\n")), vec![der]);
        Ok(())
    }
}

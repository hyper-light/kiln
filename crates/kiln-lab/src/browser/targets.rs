//! The targets a round measures, each a record (plan 01a1a832e9cc step 4): which server starts,
//! on which port, how the probe page reaches each operation on it (its carrier for streams), the
//! protocol the page must have got, and the browsers it runs in. Goals 3, 5 and 6 add theirs here
//! (kiln's protocol over a WebSocket, over WebTransport, with error correction).

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Server {
    /// kiln, the build under test (`--kiln`).
    Kiln,
    /// h2o (`--h2o`), HTTP/2 and HTTP/3 from one C server (f32).
    H2o,
    /// Caddy (`--caddy`), HTTP/3 by quic-go (f33).
    Caddy,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Target {
    pub name: String,
    pub server: Server,
    pub port: u16,
    /// HTTP/3 (Chromium forced to QUIC for the origin, its key's hash taken: f31).
    pub http3: bool,
    /// How streams and bursts reach the page: `ws` (kiln today's sockets), `sse` or `fetch-stream`.
    pub carrier: String,
    /// The protocol each operation must have come over (`nextHopProtocol`).
    pub protocol: String,
    pub browsers: Vec<String>,
    /// The QUIC congestion controller, where the server lets it be chosen.
    pub quic_cc: Option<String>,
    pub about: String,
}

fn t(name: &str, server: Server, port: u16, http3: bool, carrier: &str, about: &str) -> Target {
    Target {
        name: name.to_owned(),
        server,
        port,
        http3,
        carrier: carrier.to_owned(),
        protocol: if http3 { "h3" } else { "h2" }.to_owned(),
        // WebKit on Linux has no HTTP/3 (libsoup: f30).
        browsers: if http3 { vec!["chromium".to_owned()] } else { vec!["chromium".to_owned(), "webkit".to_owned()] },
        quic_cc: http3.then(|| "cubic".to_owned()),
        about: about.to_owned(),
    }
}

/// Every target the lab knows. The ports differ so no round's server meets another's leftovers.
pub fn targets() -> Vec<Target> {
    vec![
        t("kiln-today", Server::Kiln, 8443, false, "ws", "kiln's API and sockets as the UI uses them, TLS on kiln's own listener (HTTP/2 for requests; each socket an HTTP/1.1 connection of its own)"),
        t("kiln-today-b", Server::Kiln, 8453, false, "ws", "kiln today again, for the A/A: the lab's noise"),
        t("h2o-h2", Server::H2o, 9443, false, "sse", "h2o over HTTP/2: static JSON compressed, streams as Server-Sent Events"),
        t("h2o-h2-fetch", Server::H2o, 9443, false, "fetch-stream", "h2o over HTTP/2: streams as a streaming fetch"),
        t("caddy-h2", Server::Caddy, 10443, false, "sse", "Caddy over HTTP/2: static JSON compressed, streams as Server-Sent Events"),
        t("caddy-h2-fetch", Server::Caddy, 10443, false, "fetch-stream", "Caddy over HTTP/2: streams as a streaming fetch"),
        t("h2o-h3", Server::H2o, 9443, true, "sse", "h2o over HTTP/3 (quicly)"),
        t("caddy-h3", Server::Caddy, 10443, true, "sse", "Caddy over HTTP/3 (quic-go)"),
    ]
}

pub fn named(name: &str) -> Option<Target> {
    targets().into_iter().find(|t| t.name == name)
}

impl Target {
    /// Chromium's flags for this target: QUIC forced for its origin, the lab leaf's key taken.
    pub fn chromium_args(&self, spki: &str) -> Vec<String> {
        // Local Network Access off: the probe page reaches the lab's control echo on another
        // private address than its own origin's (the calibration's unshaped path), which Chromium
        // otherwise holds for a permission prompt. The target's own operations are same-origin.
        let mut a = vec![format!("--ignore-certificate-errors-spki-list={spki}"), "--disable-features=LocalNetworkAccessChecks,LocalNetworkAccessChecksWebSockets".to_owned()];
        if self.http3 {
            a.push("--enable-quic".to_owned());
            a.push(format!("--origin-to-force-quic-on=10.77.0.1:{}", self.port));
        } else {
            a.push("--disable-quic".to_owned());
        }
        a
    }

    pub fn origin(&self) -> String {
        format!("https://10.77.0.1:{}", self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g1_s5_http3_targets_run_in_chromium_only_and_force_quic() {
        for t in targets() {
            if t.http3 {
                assert_eq!(t.browsers, vec!["chromium".to_owned()], "{}", t.name);
                assert!(t.chromium_args("K").iter().any(|a| a == &format!("--origin-to-force-quic-on=10.77.0.1:{}", t.port)));
            } else {
                assert!(t.browsers.contains(&"webkit".to_owned()), "{}", t.name);
                assert!(t.chromium_args("K").iter().any(|a| a == "--disable-quic"));
            }
        }
        assert_eq!(named("kiln-today").map(|t| t.carrier), Some("ws".to_owned()));
    }
}

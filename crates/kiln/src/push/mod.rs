//! Notifications to people's devices (docs/notifications.md): the browser's own push (Web Push,
//! RFC 8030), encrypted for each device (RFC 8291) and signed by this server (RFC 8292), when an
//! agent needs you, finishes, stops or pauses, and when a machine with agents runs low on battery.
//!
//! - **Keys** ([`vapid`]): the server makes its key pair once and keeps it with its data.
//! - **Encryption** ([`ece`]): one record per message, for one device's keys.

pub mod ece;
pub mod vapid;

use serde::{Deserialize, Serialize};

/// `[push]` (docs/notifications.md).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Push {
    /// Push to people's devices at all. Off: Settings says so, and no device is sent anything.
    pub enabled: bool,
    /// Who push services may contact about this server (VAPID's `sub`): `mailto:you@example.com` or
    /// an `https://` URL. Empty: the server's public URL when it is `https://`, else
    /// `mailto:kiln@localhost`.
    pub contact: String,
    /// Push endpoints allowed on this machine or a private network, besides the public internet's
    /// push services (a push service of your own, a test's): hosts, addresses or networks, as
    /// `[ci] allow_private_targets` writes them. Such an endpoint may be plain `http://`.
    pub allow_private: Vec<String>,
}

impl Default for Push {
    fn default() -> Self {
        Self { enabled: true, contact: String::new(), allow_private: Vec::new() }
    }
}

impl Push {
    /// VAPID's `sub`: `contact`, else the public URL when it is https, else a placeholder.
    pub fn contact_for(&self, public_url: &str) -> String {
        let c = self.contact.trim();
        if !c.is_empty() {
            return c.to_owned();
        }
        let url = public_url.trim().trim_end_matches('/');
        if url.starts_with("https://") { url.to_owned() } else { "mailto:kiln@localhost".to_owned() }
    }

    /// Where pushes may go: the public internet, and what `allow_private` lets through.
    pub fn reach(&self) -> crate::http::io::Reach {
        crate::http::io::Reach { allow: self.allow_private.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_contact_is_said_or_the_public_url_or_a_placeholder() {
        let mut p = Push::default();
        assert_eq!(p.contact_for("https://kiln.example.com/"), "https://kiln.example.com");
        assert_eq!(p.contact_for("http://adacachy:7070"), "mailto:kiln@localhost", "only an https URL says who runs it");
        p.contact = " mailto:ops@example.com ".into();
        assert_eq!(p.contact_for("https://kiln.example.com"), "mailto:ops@example.com");
    }
}

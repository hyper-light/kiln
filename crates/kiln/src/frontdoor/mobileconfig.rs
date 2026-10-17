//! `GET /ca.mobileconfig`: an Apple configuration profile (iOS, iPadOS, macOS) carrying kiln's
//! CA certificate as a root (`com.apple.security.root`), unsigned. Installing it is half the
//! job on iOS: the root must then be trusted in Settings → General → About → Certificate Trust
//! Settings ("Enable full trust for root certificates").
//!
//! Its identifiers come from the CA's fingerprint, so installing it again replaces the profile
//! instead of adding a second one, and another kiln's CA is a profile of its own.

use base64::Engine as _;

/// The profile for the CA certificate `ca_der`.
pub fn profile(ca_der: &[u8]) -> String {
    let sha = crate::util::sha256_hex(ca_der);
    let short: String = sha.chars().take(16).collect();
    let data = base64::engine::general_purpose::STANDARD.encode(ca_der);
    // Lines of 52 characters, as Apple's own tools write <data>.
    let wrapped: Vec<String> = data.as_bytes().chunks(52).map(|c| format!("\t\t\t{}", String::from_utf8_lossy(c))).collect();
    let fingerprint = sha.to_ascii_uppercase().as_bytes().chunks(2).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(":");
    let description = format!("Lets this device trust kiln's own HTTPS (the front door of your kiln runners). Certificate SHA-256: {fingerprint}");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>PayloadContent</key>
	<array>
		<dict>
			<key>PayloadCertificateFileName</key>
			<string>kiln-ca.cer</string>
			<key>PayloadContent</key>
			<data>
{data}
			</data>
			<key>PayloadDescription</key>
			<string>{description}</string>
			<key>PayloadDisplayName</key>
			<string>kiln CA</string>
			<key>PayloadIdentifier</key>
			<string>dev.kiln.ca.{short}.root</string>
			<key>PayloadType</key>
			<string>com.apple.security.root</string>
			<key>PayloadUUID</key>
			<string>{cert_uuid}</string>
			<key>PayloadVersion</key>
			<integer>1</integer>
		</dict>
	</array>
	<key>PayloadDescription</key>
	<string>{description}</string>
	<key>PayloadDisplayName</key>
	<string>kiln CA ({short_upper})</string>
	<key>PayloadIdentifier</key>
	<string>dev.kiln.ca.{short}</string>
	<key>PayloadOrganization</key>
	<string>kiln</string>
	<key>PayloadRemovalDisallowed</key>
	<false/>
	<key>PayloadType</key>
	<string>Configuration</string>
	<key>PayloadUUID</key>
	<string>{profile_uuid}</string>
	<key>PayloadVersion</key>
	<integer>1</integer>
</dict>
</plist>
"#,
        data = wrapped.join("\n"),
        description = xml_escape(&description),
        short = short,
        short_upper = short.get(..8).unwrap_or(&short).to_ascii_uppercase(),
        cert_uuid = uuid_from(&format!("{sha}:root")),
        profile_uuid = uuid_from(&format!("{sha}:profile")),
    )
}

/// A UUID-shaped identifier derived from `seed` (name-based, version 5 layout).
fn uuid_from(seed: &str) -> String {
    let h: Vec<char> = crate::util::sha256_hex(seed.as_bytes()).to_ascii_uppercase().chars().take(32).collect();
    let mut h = h;
    if let Some(c) = h.get_mut(12) {
        *c = '5';
    }
    if let Some(c) = h.get_mut(16) {
        // The RFC 4122 variant: 10xx.
        let v = c.to_digit(16).unwrap_or(0);
        *c = char::from_digit(8 | (v & 3), 16).unwrap_or('8').to_ascii_uppercase();
    }
    let s: String = h.into_iter().collect();
    format!("{}-{}-{}-{}-{}", s.get(..8).unwrap_or(""), s.get(8..12).unwrap_or(""), s.get(12..16).unwrap_or(""), s.get(16..20).unwrap_or(""), s.get(20..32).unwrap_or(""))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_profile_carries_the_ca_as_a_root() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pki = crate::pki::Pki::open(dir.path())?;
        let p = profile(&pki.ca_der);
        assert!(p.starts_with("<?xml") && p.trim_end().ends_with("</plist>"));
        assert!(p.contains("<string>com.apple.security.root</string>"));
        assert!(p.contains("<string>Configuration</string>"));
        // The certificate, whole, base64 between <data> and </data>.
        let start = p.find("<data>").ok_or_else(|| anyhow::anyhow!("no data"))? + "<data>".len();
        let end = p.find("</data>").ok_or_else(|| anyhow::anyhow!("no end"))?;
        let b64: String = p.get(start..end).unwrap_or("").chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(b64)?, pki.ca_der);
        // Stable identifiers: the same CA gives the same profile.
        assert_eq!(profile(&pki.ca_der), p);
        let other = crate::pki::Pki::open(tempfile::tempdir()?.path())?;
        assert_ne!(profile(&other.ca_der), p);
        let u = uuid_from("x");
        assert_eq!(u.len(), 36);
        assert_eq!(u.chars().nth(14), Some('5'));
        assert!(matches!(u.chars().nth(19), Some('8' | '9' | 'A' | 'B')));
        assert_eq!(xml_escape("a<b>&\"c\""), "a&lt;b&gt;&amp;&quot;c&quot;");
        Ok(())
    }
}

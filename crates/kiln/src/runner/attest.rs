//! What `kiln/attest@v1` reads on the runner (docs/ci.md W18-R01): subjects' digests, from files,
//! `sha256sum` lists and image references. The statements are the server's to make and sign.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::Path;

use kiln_ci::attest::Subject;
use sha2::Digest as _;

/// A file's SHA-256, as hex, read in pieces.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(buf.get(..n).unwrap_or_default());
    }
    Ok(hex::encode(h.finalize()))
}

/// A subject named `name` with `digest` (`sha256:<hex>`).
pub fn subject(name: &str, digest: &str) -> Result<Subject, String> {
    let (alg, hex) = digest.split_once(':').ok_or_else(|| format!("`{digest}` isn't a digest (`sha256:<hex>`)"))?;
    let alg = alg.to_ascii_lowercase();
    let len = match alg.as_str() {
        "sha256" => 64,
        "sha512" => 128,
        _ => return Err(format!("`{digest}`: a digest is sha256 or sha512")),
    };
    if hex.len() != len || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("`{digest}` isn't a {alg} digest"));
    }
    Ok(Subject { name: name.to_owned(), digest: BTreeMap::from([(alg, hex.to_ascii_lowercase())]) })
}

/// The subjects of a `sha256sum` list: `<hex>  <name>` (or `<hex> *<name>`) a line.
pub fn checksums(text: &str) -> Result<Vec<Subject>, String> {
    let mut out = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let (hex, name) = line.split_once(char::is_whitespace).ok_or_else(|| format!("`{line}` isn't `<sha256>  <name>`"))?;
        let name = name.trim_start();
        let name = name.strip_prefix('*').unwrap_or(name);
        out.push(subject(name, &format!("sha256:{hex}"))?);
    }
    Ok(out)
}

/// Whether a kiln `subjects` entry is plainly an image: `docker://`, `oci://` or a digest.
pub fn is_image(s: &str) -> bool {
    s.starts_with("docker://") || s.starts_with("oci://") || s.contains("@sha256:")
}

/// An image reference's repository (its tag and digest dropped) and its digest, when it has one.
pub fn image_name(r: &str) -> (String, Option<String>) {
    let r = r.strip_prefix("docker://").or_else(|| r.strip_prefix("oci://")).unwrap_or(r);
    let (rest, digest) = match r.split_once('@') {
        Some((n, d)) => (n, Some(d.to_owned())),
        None => (r, None),
    };
    // A tag is after the last colon that comes after the last slash (not a registry's port).
    let slash = rest.rfind('/').map_or(0, |i| i + 1);
    let name = match rest.get(slash..).and_then(|last| last.rfind(':')) {
        Some(colon) => rest.get(..slash + colon).unwrap_or(rest),
        None => rest,
    };
    (name.to_owned(), digest)
}

/// An image's digest from its engine's `RepoDigests` (JSON) and `Id`: the registry's manifest
/// digest of `name`'s repository when it was pushed or pulled (else of any), its ID when it never
/// was (`false`).
pub fn pick_digest(name: &str, repo_digests: &str, id: &str) -> Result<(String, bool), String> {
    let list: Vec<String> = serde_json::from_str(repo_digests.trim()).unwrap_or_default();
    let short = |n: &str| n.trim_start_matches("docker.io/").trim_start_matches("library/").to_owned();
    let mine = list.iter().find(|d| d.split_once('@').is_some_and(|(n, _)| short(n) == short(name))).or_else(|| list.first());
    if let Some(d) = mine.and_then(|d| d.split_once('@')).map(|(_, d)| d.to_owned()) {
        return Ok((d, true));
    }
    let id = id.trim();
    if id.starts_with("sha256:") { Ok((id.to_owned(), false)) } else { Err(format!("`{name}` has no digest")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "8b6e1f7c4a1d2e3f4a5b6c7d8e9f00112233445566778899aabbccddeeff0011";

    #[test]
    fn w18_r01_subjects() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let f = dir.path().join("app.tar.gz");
        std::fs::write(&f, b"hello world")?;
        assert_eq!(sha256_file(&f)?, "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9");
        assert!(subject("x", "sha256:abc").is_err() && subject("x", "md5:abc").is_err());
        assert_eq!(subject("x", &format!("SHA256:{}", HEX.to_ascii_uppercase())).map(|s| s.digest.get("sha256").cloned()), Ok(Some(HEX.to_owned())));
        let c = checksums(&format!("{HEX}  dist/a.tar.gz\n{HEX} *b.zip\n\n# made by sha256sum\n")).map_err(anyhow::Error::msg)?;
        assert_eq!(c.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["dist/a.tar.gz", "b.zip"]);
        assert!(checksums("nothex  a").is_err());
        Ok(())
    }

    #[test]
    fn w18_r01_images() {
        assert_eq!(image_name("ghcr.io/acme/app:1.2"), ("ghcr.io/acme/app".into(), None));
        assert_eq!(image_name("localhost:5000/app"), ("localhost:5000/app".into(), None));
        assert_eq!(image_name(&format!("docker://localhost:5000/app:v1@sha256:{HEX}")), ("localhost:5000/app".into(), Some(format!("sha256:{HEX}"))));
        assert!(is_image("oci://x") && is_image(&format!("app@sha256:{HEX}")) && !is_image("dist/*.tar.gz"));
        let digests = format!(r#"["other/x@sha256:{}", "acme/app@sha256:{HEX}"]"#, "0".repeat(64));
        assert_eq!(pick_digest("docker.io/acme/app", &digests, "sha256:ffff"), Ok((format!("sha256:{HEX}"), true)));
        assert_eq!(pick_digest("app", "[]", "sha256:ffff"), Ok(("sha256:ffff".into(), false)), "never pushed: its ID");
        assert!(pick_digest("app", "null", "").is_err());
    }
}

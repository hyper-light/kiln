//! The door's settings, certificates and tickets (goal 4 step 6, plan 01a1a832eb597110a6dcfda227981604).

use super::*;
use kiln_http::h3::Draft;

fn connect(path: &str, origin: Option<&str>) -> Connect {
    Connect { stream: 0, authority: b"kiln.test:7443".to_vec(), path: path.as_bytes().to_vec(), origin: origin.map(|o| o.as_bytes().to_vec()), draft: Draft::Draft02 }
}

#[test]
fn step6_the_door_is_off_by_default() {
    let cfg = crate::config::Config::default();
    assert_eq!(cfg.server.h3_addr, None);
    // No door: no socket was bound, and the endpoint says it's off.
    assert_eq!(info("alice"), json!({ "on": false }));
}

#[test]
fn step6_hash_pinned_certificates_last_under_14_days_and_rotate_with_an_overlap() -> anyhow::Result<()> {
    let r = Rotation::DOOR;
    let names = vec!["kiln.test".to_owned()];
    let t0: i64 = 1_793_800_000;
    let mut issued: Vec<DoorCert> = Vec::new();
    let mut issue_k = |k: i64| -> anyhow::Result<DoorCert> {
        let c = issue(&r, k, &names)?;
        issued.push(c.clone());
        Ok(c)
    };
    let mut certs = Certs::new(r, t0, &mut issue_k)?;
    // The lists pages fetched, one a second, for a ticket's life back.
    let life = (TICKET_LIFE_MS / 1000) as usize;
    let mut fetched: std::collections::VecDeque<[[u8; 32]; 2]> = std::collections::VecDeque::new();
    let mut misses = 0u64;
    let mut moments = 0u64;
    for t in t0..t0 + 60 * 86_400 {
        certs.at(t, &mut issue_k)?;
        fetched.push_back(certs.hashes());
        if fetched.len() > life + 1 {
            fetched.pop_front();
        }
        let served = &certs.current.1;
        // Valid now, and in every list a page could hold (fetched within the ticket's life).
        if !(served.not_before <= t && t <= served.not_after) || fetched.iter().any(|l| !l.contains(&served.sha256)) {
            misses += 1;
        }
        moments += 1;
    }
    println!("rotation: {moments} seconds checked over 60 days, {} certificates, {misses} moments the served one wasn't pinned", issued.len());
    assert_eq!(misses, 0);
    assert!(issued.len() >= 9);
    for c in &issued {
        let (_, x) = x509_parser::parse_x509_certificate(&c.der).map_err(|e| anyhow::anyhow!("{e}"))?;
        let v = x.validity();
        let days = (v.not_after.timestamp() - v.not_before.timestamp()) as f64 / 86_400.0;
        assert!(days < 14.0, "{days} days");
        assert_eq!(v.not_before.timestamp(), c.not_before);
        // ECDSA P-256 (serverCertificateHashes requires it be supported, and RSA is excluded).
        assert_eq!(x.public_key().algorithm.algorithm.to_id_string(), "1.2.840.10045.2.1");
    }
    Ok(())
}

#[test]
fn step6_a_ticket_is_good_once_for_its_principal_from_our_origin() {
    let origins = origins("https://kiln.test:7070", &["kiln.lan".to_owned()]);
    assert!(origins.contains(&"https://kiln.test:7070".to_owned()) && origins.contains(&"https://kiln.lan".to_owned()));
    let key = TicketKey::new([42; 32]);
    let p = Published { url: "https://kiln.test:7443/wt".into(), hashes: vec![[1; 32]], origins: origins.clone(), key: key.clone() };
    let v = info_of(&p, "alice", [7; 32], 1_000);
    let url = v.get("url").and_then(Value::as_str).unwrap_or_default().to_owned();
    assert!(url.starts_with("https://kiln.test:7443/wt?ticket=") && url.ends_with("&u=alice"), "{url}");
    assert_eq!(v.get("hashes").and_then(Value::as_array).map(Vec::len), Some(1));
    // The ticket in the URL is the key's: it admits once.
    let mut used = Used::default();
    let path_of = url.trim_start_matches("https://kiln.test:7443").to_owned();
    let ok = "https://kiln.test:7070";
    assert_eq!(admit(&connect(&path_of, Some(ok)), &origins, &key, &mut used, 1_010), Admission::Accept);
    let ticket = b64(&key.mint([9; 32], "alice", 0));
    let path = |tk: &str, u: &str| format!("/wt?ticket={tk}&u={u}");
    // Admitted once.
    assert_eq!(admit(&connect(&path(&ticket, "alice"), Some(ok)), &origins, &key, &mut used, 10), Admission::Accept);
    // Replayed.
    assert_eq!(admit(&connect(&path(&ticket, "alice"), Some(ok)), &origins, &key, &mut used, 20), Admission::Forbidden);
    // Expired.
    let old = b64(&key.mint([10; 32], "alice", 0));
    assert_eq!(admit(&connect(&path(&old, "alice"), Some(ok)), &origins, &key, &mut used, TICKET_LIFE_MS + 1), Admission::Forbidden);
    // For another user, and one another key made (another server's, or forged).
    let bobs = b64(&key.mint([11; 32], "bob", 100_000));
    assert_eq!(admit(&connect(&path(&bobs, "alice"), Some(ok)), &origins, &key, &mut used, 100_010), Admission::Forbidden);
    let forged = b64(&TicketKey::new([43; 32]).mint([13; 32], "alice", 100_000));
    assert_eq!(admit(&connect(&path(&forged, "alice"), Some(ok)), &origins, &key, &mut used, 100_010), Admission::Forbidden);
    // No ticket, a foreign origin, no origin, a path not served.
    let fresh = b64(&key.mint([12; 32], "alice", 100_000));
    assert_eq!(admit(&connect("/wt?u=alice", Some(ok)), &origins, &key, &mut used, 100_010), Admission::Forbidden);
    assert_eq!(admit(&connect(&path(&fresh, "alice"), Some("https://evil.example")), &origins, &key, &mut used, 100_010), Admission::Forbidden);
    assert_eq!(admit(&connect(&path(&fresh, "alice"), None), &origins, &key, &mut used, 100_010), Admission::Forbidden);
    assert_eq!(admit(&connect(&format!("/elsewhere?ticket={fresh}&u=alice"), Some(ok)), &origins, &key, &mut used, 100_010), Admission::NotFound);
    // The fresh one was never used up by a refusal above: it still opens a session.
    assert_eq!(admit(&connect(&path(&fresh, "alice").replacen("/wt", "/wt/echo", 1), Some(ok)), &origins, &key, &mut used, 100_020), Admission::Accept);
    // A principal with characters a URL escapes round-trips.
    let odd = b64(&key.mint([14; 32], "user:a b", 100_000));
    assert_eq!(admit(&connect(&path(&odd, &url_part("user:a b")), Some(ok)), &origins, &key, &mut used, 100_030), Admission::Accept);
    // And nothing is held of any of them once their lives are past.
    assert_eq!(used.take([0; 32], 400_000, 400_000), Ok(()));
    assert_eq!(used.held(), 1);
}

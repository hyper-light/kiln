use super::*;

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn archive() -> Archive {
    Archive { version: "v20.19.5".into(), url: "https://nodejs.org/dist/v20.19.5/node-v20.19.5-linux-x64.tar.gz".into(), file: "node-v20.19.5-linux-x64.tar.gz".into(), sha256: A.into() }
}

#[test]
fn w29_r24_image_pins_keep_ports_and_refuse_contradictions_or_expressions() -> Result<(), String> {
    for (reference, name) in [("alpine:3", "alpine"), ("registry.test:5000/acme/tool:v1", "registry.test:5000/acme/tool"), ("registry.test:5000/acme/tool", "registry.test:5000/acme/tool"), ("docker://alpine:3", "alpine")] {
        assert_eq!(pin_image(reference, &format!("sha256:{A}"))?, format!("{name}@sha256:{A}"));
    }
    assert!(pin_image(&format!("alpine@sha256:{A}"), &format!("sha256:{B}")).is_err());
    assert_eq!(pin_image(&format!("alpine@sha256:{}", A.to_ascii_uppercase()), &format!("sha256:{A}"))?, format!("alpine@sha256:{A}"));
    assert!(pin_image("alpine:${{env.TAG}}", &format!("sha256:{A}")).is_err());
    assert!(pin_image("https://user:secret@registry.test/base", &format!("sha256:{A}")).is_err());
    Ok(())
}

#[test]
fn w29_r24_execution_snapshot_roundtrips_and_refuses_missing_or_wrong_archives() -> Result<(), String> {
    let locks = Locks { images: [("alpine:3".into(), format!("alpine@sha256:{A}"))].into(), runtimes: [("node20".into(), [("linux/amd64".into(), archive())].into())].into(), ..Default::default() };
    locks.validate()?;
    assert_eq!(locks.runtime("node16", "linux/amd64")?, &archive());
    assert!(locks.runtime("node20", "linux/arm64").is_err());
    assert!(locks.image("alpine:latest").is_err());
    assert_eq!(locks.image("docker://alpine:3")?, format!("alpine@sha256:{A}"));
    let requirement = crate::sched::routing::Constraint::parse(&locks.runtime_requirements().into_iter().next().ok_or("no runtime requirement")?)?;
    assert!(requirement.eval(&crate::facts::RunnerFacts { os: "linux".into(), arch: "x86_64".into(), ..Default::default() }, &[]));
    assert!(!requirement.eval(&crate::facts::RunnerFacts { os: "macos".into(), arch: "aarch64".into(), ..Default::default() }, &[]));
    let text = serde_json::to_string(&locks).map_err(|e| e.to_string())?;
    assert_eq!(serde_json::from_str::<Locks>(&text).map_err(|e| e.to_string())?, locks);
    let mut wrong = locks.clone();
    if let Some(a) = wrong.runtimes.get_mut("node20").and_then(|p| p.get_mut("linux/amd64")) { a.url = a.url.replace("v20.19.5/", "latest-v20.x/"); }
    assert!(wrong.validate().is_err());
    Ok(())
}

#[test]
fn w29_r24_conflicting_execution_inputs_are_atomic_and_bounded() -> Result<(), String> {
    let first = Locks { images: [("alpine:3".into(), format!("alpine@sha256:{A}"))].into(), ..Default::default() };
    let second = Locks { images: [("alpine:3".into(), format!("alpine@sha256:{B}"))].into(), ..Default::default() };
    let mut merged = first.clone();
    assert!(merged.merge(&second).err().is_some_and(|e| e.contains("KCI4054") && e.contains(A) && e.contains(B)));
    assert_eq!(merged, first);
    let invalid = Locks { dockerfiles: [("acme/kit@v1".into(), Dockerfile { path: "../Dockerfile".into(), source_sha256: A.into(), locked: "FROM scratch\n".into() })].into(), ..Default::default() };
    assert!(invalid.validate().is_err());
    let mut many = Locks::default();
    for n in 0..=MAX_ITEMS { many.images.insert(format!("alpine:v{n}"), format!("alpine@sha256:{A}")); }
    assert!(many.validate().is_err());
    Ok(())
}

#[test]
fn w29_r24_assignments_pin_every_image_atomically_and_never_guess_pool_images() -> Result<(), String> {
    use crate::wf::ir::{Container, Tpl};
    use crate::wire::{AssignmentV2, ExecutorSpec};
    let pin = format!("alpine@sha256:{A}");
    let locks = Locks { images: [("alpine:3".into(), pin.clone())].into(), ..Default::default() };
    let container = || Container { image: Tpl { src: "alpine:3".into(), ..Default::default() }, ..Default::default() };
    let mut assignment = AssignmentV2 { job_container: Some(container()), job_services: vec![("db".into(), container())], executor: ExecutorSpec { kind: "firecracker".into(), image: Some("alpine:3".into()), ..Default::default() }, ..Default::default() };
    locks.assignment(&mut assignment)?;
    assert_eq!(assignment.container.as_ref(), Some(&pin));
    assert_eq!(assignment.services.get("db"), Some(&pin));
    assert_eq!(assignment.executor.image.as_ref(), Some(&pin));
    let once = assignment.clone(); locks.assignment(&mut assignment)?;
    assert_eq!(once, assignment);
    let mut bad = once.clone(); bad.executor.image = Some("unlocked:latest".into());
    let before = bad.clone();
    assert!(locks.assignment(&mut bad).err().is_some_and(|e| e.contains("KCI4050")));
    assert_eq!(bad, before);
    bad.executor.image = None;
    assert!(locks.assignment(&mut bad).is_err());
    bad.executor.kind = "docker".into(); bad.executor.image = Some("unused-pool-default:latest".into());
    locks.assignment(&mut bad)?;
    assert!(bad.executor.image.is_none());
    Ok(())
}

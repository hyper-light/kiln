//! W27-R08 (docs/ci.md; the Transfers plan's step 14, kiln plan
//! `01a17856b1b477eda14a574ae312a123`): custody to production, on a test cluster. A leg on fedora-2
//! builds a multi-platform image (an index of pause 3.10's linux/amd64 and linux/arm64 manifests)
//! from acme/app's commit and pushes it to kiln's registry; the same bytes go to a registry on the
//! cluster's node; production releases web, whose Deployment names the image by its index's
//! digest, through KilnCD. Each pod's record walks back, unbroken, through the node's resolution of
//! the index to its platform's manifest, the cluster's receipt of what CD handed it, CD's receipt of
//! the image from kiln's registry, kiln's receipt of what the leg made, the leg, and kiln's hand-on
//! of the commit. Then production requires a verified chain, and one pod's image is swapped (a test
//! hook: `kubectl set image` on the pod, to pause 3.10.1): the break is recorded at that pod's receipt, and
//! production no longer counts the deploy healthy (its registration needs a person, saying why).
//!
//! The scenario up to the release is `common::production`'s, shared with tests/custody.rs's end to
//! end. It needs a test cluster, never production's: `KILN_CUSTODY_KUBECONFIG` (and
//! `KILN_CUSTODY_CONTEXT`, else the kubeconfig's current context), the registry.k8s.io and
//! docker.io registries reachable from here and from its node, and `kubectl` and `curl`:
//! `KILN_CUSTODY_KUBECONFIG=/tmp/kilncd-e2e/kubeconfig KILN_CUSTODY_CONTEXT=kilncd-e2e cargo test -p
//! kiln --test custody_production -- --ignored`.

#![cfg(unix)]

mod common;

use common::production::{degraded, get, hops, released, swapped, text, verified};
use serde_json::{Value, json};

#[test]
#[ignore = "needs a test cluster (KILN_CUSTODY_KUBECONFIG), and registry.k8s.io and docker.io"]
fn w27_r08_custody_to_production() -> anyhow::Result<()> {
    let r = released()?;
    let (s, ns, index, commit, dir) = (&r.s, r.cluster.namespace.as_str(), r.index.as_str(), r.commit.as_str(), r.dir.path());
    // Each pod's container runs what the deploy handed on, and its chain walks back to the commit.
    let custody = verified(s)?;
    let pods: Vec<Value> = custody.get("pods").and_then(Value::as_array).cloned().unwrap_or_default();
    anyhow::ensure!(pods.len() == 2, "{custody:#}");
    let amd64 = r.img.manifests.first().map(|m| m.0.clone()).unwrap_or_default();
    for p in &pods {
        assert_eq!((text(p, "platform"), text(p, "wants"), text(p, "runs"), p.get("follows")), ("linux/amd64", amd64.as_str(), amd64.as_str(), Some(&json!(true))), "{p:#}");
        assert!(text(p, "image_id").contains(index) || text(p, "image_id").contains(&amd64), "the runtime reports the index, or its platform's manifest: {p:#}");
        let chain = get(s, &format!("/api/custody?digest={}", text(p, "record")))?;
        assert_eq!(chain.get("unbroken"), Some(&json!(true)), "{chain:#}");
        let h = hops(&chain);
        let (pod, node) = (format!("kubernetes://e2e/{ns}/pod/{}", text(p, "pod")), format!("kubernetes://e2e/node/{}", text(p, "node")));
        for (kind, custodian, taken_for) in [
            ("received", "kiln://hyper-light/server", pod.as_str()),
            ("taken", "kiln://hyper-light/server", node.as_str()),
            ("received", "kiln://hyper-light/server", node.as_str()),
            ("taken", "kiln://hyper-light/server", "kubernetes://e2e"),
            ("received", "kiln://hyper-light/server", "kubernetes://e2e"),
            ("handed-on", "kiln://hyper-light/server/cd", ""),
            ("received", "kiln://hyper-light/server/cd", ""),
            ("handed-on", "kiln://hyper-light/server", ""),
            ("received", "kiln://hyper-light/server", ""),
        ] {
            assert!(h.contains(&(kind.to_owned(), custodian.to_owned(), taken_for.to_owned())), "{kind} by {custodian} for {taken_for:?} in {h:#?}");
        }
        assert!(h.iter().any(|(_, c, _)| c.ends_with("/leg/build")), "the leg that built it: {h:#?}");
        // Through the index's resolution: the node's hand-on of its platform's manifest, made from its
        // receipt of the index.
        let resolved = chain.get("hops").and_then(Value::as_array).into_iter().flatten().find(|x| text(x, "kind") == "taken" && text(x, "taken_for") == node).cloned().unwrap_or_default();
        assert_eq!(resolved.pointer("/subject/0/digest/sha256").and_then(Value::as_str), Some(amd64.as_str()), "{resolved:#}");
        assert_eq!(resolved.get("made_from").and_then(Value::as_array).map(Vec::len), Some(1), "{resolved:#}");
        // The node's receipt says what it pulls: the index's amd64 manifest.
        let pulled = chain.get("hops").and_then(Value::as_array).into_iter().flatten().find(|x| text(x, "kind") == "received" && text(x, "taken_for") == node).cloned().unwrap_or_default();
        assert_eq!(pulled.pointer("/subject/0/digest/sha256").and_then(Value::as_str), Some(index), "the image the deploy named: {pulled:#}");
        assert_eq!(pulled.pointer("/subject/0/annotations/pulls").and_then(Value::as_str), Some(format!("sha256:{amd64}").as_str()), "{pulled:#}");
        // Back to kiln's hand-on of the commit to fedora-2's runner, where the image's chain began.
        let began: Vec<&Value> = chain.get("hops").and_then(Value::as_array).into_iter().flatten().filter(|x| x.get("began") == Some(&json!(true))).collect();
        assert!(began.iter().any(|x| x.get("subject").and_then(Value::as_array).into_iter().flatten().any(|t| t.pointer("/digest/gitCommit") == Some(&json!(commit)))), "back to the commit: {chain:#}");
    }

    // Walked as `kiln custody walk` walks it (step 15): from a pod back to the commit, each hop in
    // words; and with one hop's record removed, then changed where kiln keeps it, the break named
    // at that hop.
    let first = pods.first().map(|p| text(p, "record").to_owned()).unwrap_or_default();
    let walked = common::custody::walked_and_broken(s.port, &s.admin, &dir.join("data/kiln.db"), &first)?;
    let words: Vec<&str> = walked.get("hops").and_then(Value::as_array).into_iter().flatten().map(|h| text(h, "words")).collect();
    assert!(words.first().is_some_and(|w| w.starts_with(&format!("kiln for pod {ns}/")) && w.contains(" from node ")), "{words:#?}");
    assert!(words.iter().any(|w| w.starts_with("kiln's CD handed ")), "{words:#?}");
    assert!(words.iter().any(|w| w.contains("'s leg build on fedora-2")), "{words:#?}");

    // Production requires a verified chain now; one pod's image is swapped under it.
    let pod = text(pods.first().unwrap_or(&Value::Null), "pod").to_owned();
    let record = swapped(&r, &pod)?;
    let breaks = get(s, "/api/custody/breaks")?;
    let at = breaks.get("breaks").and_then(Value::as_array).into_iter().flatten().find(|b| text(b, "record") == record).cloned().unwrap_or_default();
    assert_eq!(text(&at, "kind"), "changed", "recorded at the pod's receipt: {breaks:#}");
    assert!(text(&at, "detail").contains(&amd64), "naming what it should run: {at:#}");
    let chain = get(s, &format!("/api/custody?digest={record}"))?;
    assert_eq!(chain.get("unbroken"), Some(&json!(false)), "{chain:#}");
    // Production doesn't count the deploy healthy: its registration needs a person, saying why.
    degraded(s, &pod)?;
    Ok(())
}

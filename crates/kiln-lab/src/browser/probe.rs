//! The lab machine's capabilities, checked before anything is measured (plan 01a1a832e9cc step 1;
//! research f39): each one in turn, stopping at the first one missing and naming it. Outside its
//! namespaces it checks what it needs to make them; inside, that it made them (as the uid it was
//! started by, with no sudo), that netem, ifb and the redirect work on a real link, that it can
//! make cgroup scopes, and that Chromium and WebKit start in a client's namespace.

#[cfg(target_os = "linux")]
use std::path::Path;
#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
use super::link::{self, Condition, Ns};
#[cfg(target_os = "linux")]
use super::topo::{self, Lab, Scopes};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    /// Whether the lab can't run without it (Firefox is for goals 4, 5 and 7's checks; ethtool has a
    /// fallback).
    pub required: bool,
}

#[cfg(target_os = "linux")]
fn check(name: &str, required: bool, r: anyhow::Result<String>) -> Check {
    match r {
        Ok(detail) => Check { name: name.to_owned(), ok: true, detail, required },
        Err(e) => Check { name: name.to_owned(), ok: false, detail: format!("{e:#}"), required },
    }
}

#[cfg(target_os = "linux")]
fn output(prog: &str, args: &[&str]) -> anyhow::Result<String> {
    let o = Command::new(prog).args(args).stdin(Stdio::null()).output().map_err(|e| anyhow::anyhow!("{prog} isn't there ({e})"))?;
    anyhow::ensure!(o.status.success(), "`{prog} {}` failed: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim());
    Ok(String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or_default().trim().to_owned())
}

/// What the lab checks before it makes its namespaces: elsewhere than Linux, that it isn't Linux.
#[cfg(not(target_os = "linux"))]
pub fn outside() -> Vec<Check> {
    vec![Check { name: "linux".into(), ok: false, detail: format!("this is {}", std::env::consts::OS), required: true }]
}

/// What the lab checks before it makes its namespaces.
#[cfg(target_os = "linux")]
pub fn outside() -> Vec<Check> {
    let mut out = vec![check("linux", true, Ok(std::env::consts::OS.to_owned()))];
    out.push(check("unprivileged", true, topo::uid().map_or_else(|| Err(anyhow::anyhow!("no uid in /proc/self/status")), |u| if u == 0 { Err(anyhow::anyhow!("started as root: the lab runs as a person")) } else { Ok(format!("uid {u}")) })));
    out.push(check("unshare", true, output("unshare", &["--version"])));
    out.push(check("nsenter", true, output("nsenter", &["--version"])));
    out.push(check("ip", true, output("ip", &["-V"])));
    out.push(check("tc", true, output(&topo::tc(), &["-V"]).map_err(|e| anyhow::anyhow!("{e:#}: install iproute2's tc, or point KILN_LAB_TC at one"))));
    out.push(check("user namespaces", true, output("unshare", &["--user", "--map-root-user", "--net", "true"]).map(|_| "unshare --user --map-root-user --net".to_owned())));
    out
}

/// What the lab checks inside its namespaces, given the web driver's folder.
#[cfg(target_os = "linux")]
pub fn inside(web: &Path) -> Vec<Check> {
    let mut out = Vec::new();
    let outer = std::env::var(topo::OUTER_UID).unwrap_or_default();
    out.push(check("started without root", true, if outer.is_empty() || outer == "0" { Err(anyhow::anyhow!("started by uid {outer:?}")) } else { Ok(format!("uid {outer} outside, root only inside its own user namespace")) }));
    let lab = Lab::up(1);
    let mut lab = match lab {
        Ok(l) => {
            out.push(check("namespaces and veth", true, Ok(format!("a server and a client namespace joined through the link ({} commands)", l.log.len()))));
            l
        }
        Err(e) => {
            out.push(check("namespaces and veth", true, Err(e)));
            return out;
        }
    };
    out.push(check("ethtool", false, if lab.ethtool { Ok("offloads off by ethtool".into()) } else { Err(anyhow::anyhow!("no ethtool: GSO bounded by ip instead")) }));
    let cond = Condition { profile: link::profile_named("50k").unwrap_or_else(|| link::profiles().swap_remove(0)), loss: link::Loss::Random { pct: 1.0 }, seed: 1 };
    out.push(check("netem, ifb and the redirect", true, lab.shape(0, &cond).map(|()| lab.qdisc_stats(0).lines().filter(|l| l.starts_with("qdisc")).collect::<Vec<_>>().join("; "))));
    out.push(check("cgroup scopes", true, Scopes::mine().and_then(|s| {
        let d = s.scope("kiln-lab-probe")?;
        let w = Scopes::wrap(&d, &["sleep".into(), "0.2".into()]);
        let (prog, args) = w.split_first().ok_or_else(|| anyhow::anyhow!("an empty command"))?;
        let st = Command::new(prog).args(args).status()?;
        anyhow::ensure!(st.success(), "a process couldn't move into {}", d.display());
        let cpu = Scopes::cpu_us(&d)?;
        let _ = std::fs::remove_dir(&d);
        Ok(format!("{} (cpu.stat read: {cpu} µs)", s.root.display()))
    })));
    out.push(check("node", true, output("node", &["--version"])));
    let pw = web.join("node_modules/playwright/package.json");
    out.push(check("playwright", true, std::fs::read_to_string(&pw).map_err(|e| anyhow::anyhow!("{} ({e}): run `npm ci` in {}", pw.display(), web.display())).and_then(|t| {
        let v: serde_json::Value = serde_json::from_str(&t)?;
        Ok(v.get("version").and_then(serde_json::Value::as_str).unwrap_or("?").to_owned())
    })));
    if out.iter().any(|c| c.required && !c.ok) {
        return out;
    }
    // The browsers, started in the client's namespace as the lab starts them.
    let argv = vec!["node".to_owned(), web.join("driver.mjs").to_string_lossy().into_owned(), "probe".to_owned()];
    let browsers = lab.command(Ns::Client(0), &argv).and_then(|mut c| Ok(c.stdin(Stdio::null()).output()?));
    match browsers {
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout);
            for line in text.lines() {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                let name = v.get("browser").and_then(serde_json::Value::as_str).unwrap_or("?").to_owned();
                let ok = v.get("ok").and_then(serde_json::Value::as_bool) == Some(true);
                let detail = if ok { v.get("version").and_then(serde_json::Value::as_str).unwrap_or("").to_owned() } else { v.get("error").and_then(serde_json::Value::as_str).unwrap_or("").chars().take(300).collect() };
                out.push(Check { name: format!("{name} starts"), ok, detail, required: name != "firefox" });
            }
            if !o.status.success() && out.iter().all(|c| !c.name.ends_with("starts")) {
                out.push(Check { name: "browsers start".into(), ok: false, detail: String::from_utf8_lossy(&o.stderr).chars().take(400).collect(), required: true });
            }
        }
        Err(e) => out.push(check("browsers start", true, Err(e))),
    }
    out
}

/// The checks as a table, and the first required one missing.
pub fn say(checks: &[Check]) -> (String, Option<&Check>) {
    use std::fmt::Write as _;
    let mut o = String::from("| capability | | what it found |\n| --- | --- | --- |\n");
    for c in checks {
        let mark = if c.ok { "ok" } else if c.required { "**missing**" } else { "missing (optional)" };
        let _ = writeln!(o, "| {} | {mark} | {} |", c.name, c.detail.replace('|', "/").replace('\n', " "));
    }
    (o, checks.iter().find(|c| c.required && !c.ok))
}

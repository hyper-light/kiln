//! iPhones and iPads on a Mac (docs/previews.md, "Simulators"): on a Mac with Xcode, a live page in
//! WebKit laid out as an iPhone or iPad runs in an iOS Simulator, in the real Mobile Safari, driven
//! through `safaridriver` as Safari is (`super::driven`). kiln keeps a simulator of its own for each
//! model ("kiln iPhone 15"), boots it with no Simulator window, and lets its Safari be automated.
//!
//! What a Mac needs once: Xcode, an iOS simulator runtime (`xcodebuild -downloadPlatform iOS`), and
//! Safari's remote automation (`safaridriver --enable`, which asks for an administrator's password).

use std::time::Duration;

use serde_json::Value;

use super::devices::Device;

/// A simulator kiln drives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sim {
    pub udid: String,
    pub name: String,
    pub runtime: String,
}

/// Whether a device is one a simulator is (an iPhone, an iPad).
pub fn is_ios(device: &Device) -> bool {
    device.mobile && (device.agent.contains("iPhone") || device.agent.contains("iPad"))
}

/// Its family, as Xcode's device types say it.
fn family(device: &Device) -> &'static str {
    if device.agent.contains("iPad") { "iPad" } else { "iPhone" }
}

/// The newest iOS runtime that's there (`simctl list -j runtimes`): its identifier and name.
fn runtime(list: &Value) -> Option<(String, String)> {
    let all = list.get("runtimes")?.as_array()?;
    all.iter()
        .filter(|r| r.get("isAvailable").and_then(Value::as_bool) != Some(false))
        .filter(|r| r.get("platform").and_then(Value::as_str).map_or_else(|| r.get("identifier").and_then(Value::as_str).is_some_and(|i| i.contains(".iOS-")), |p| p == "iOS"))
        .filter_map(|r| Some((r.get("identifier")?.as_str()?.to_owned(), r.get("name")?.as_str()?.to_owned(), r.get("version").and_then(Value::as_str).unwrap_or("").to_owned())))
        .max_by(|a, b| version(&a.2).cmp(&version(&b.2)))
        .map(|(id, name, _)| (id, name))
}

/// Whether `simctl list -j runtimes` lists an iOS runtime that's there.
pub fn has_runtime(list: &Value) -> bool {
    runtime(list).is_some()
}

/// A version's parts, to order them ("17.5" before "18.0").
fn version(v: &str) -> Vec<u32> {
    v.split('.').map(|n| n.parse().unwrap_or(0)).collect()
}

/// The device type for `device` (`simctl list -j devicetypes`): its own model, else the newest of
/// its name (`iPhone SE` is `iPhone SE (3rd generation)`), else the newest of its family.
fn device_type(list: &Value, device: &Device) -> Option<(String, String)> {
    let types: Vec<(String, String, String)> = list
        .get("devicetypes")?
        .as_array()?
        .iter()
        .filter_map(|t| Some((t.get("identifier")?.as_str()?.to_owned(), t.get("name")?.as_str()?.to_owned(), t.get("productFamily").and_then(Value::as_str).unwrap_or("").to_owned())))
        .collect();
    let pick = |f: &dyn Fn(&(String, String, String)) -> bool| types.iter().filter(|t| f(t)).last().map(|(id, name, _)| (id.clone(), name.clone()));
    pick(&|t| t.1 == device.name).or_else(|| pick(&|t| t.1.starts_with(device.name))).or_else(|| pick(&|t| t.2 == family(device) || t.1.starts_with(family(device))))
}

/// kiln's own simulator of a device type and runtime (`simctl list -j devices`), when it has one.
fn kept(list: &Value, name: &str, runtime: &str) -> Option<(String, bool)> {
    let devices = list.get("devices")?.get(runtime)?.as_array()?;
    devices.iter().find(|d| d.get("name").and_then(Value::as_str) == Some(name)).and_then(|d| Some((d.get("udid")?.as_str()?.to_owned(), d.get("state").and_then(Value::as_str) == Some("Booted"))))
}

/// `xcrun simctl <args>`: what it printed.
async fn simctl(args: &[&str]) -> anyhow::Result<String> {
    let mut cmd = std::process::Command::new("xcrun");
    cmd.arg("simctl").args(args);
    let out = crate::os::proc::output(cmd, Duration::from_secs(300)).await.map_err(|e| anyhow::anyhow!("simctl {}: {e:#}", args.first().unwrap_or(&"")))?;
    if !out.status.success() {
        anyhow::bail!("simctl {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// What `simctl list -j <what>` says.
async fn list(what: &str) -> anyhow::Result<Value> {
    Ok(serde_json::from_str(&simctl(&["list", "-j", what]).await?)?)
}

/// kiln's simulator for `device`, made if it isn't there, booted (with no window), its Safari
/// let be automated.
pub async fn ensure(device: &Device) -> anyhow::Result<Sim> {
    anyhow::ensure!(cfg!(target_os = "macos"), "iOS simulators run on a Mac");
    let runtimes = list("runtimes").await.map_err(|e| anyhow::anyhow!("this Mac has no Xcode simulators ({e:#}): install Xcode to run iPhones and iPads"))?;
    let (rt, rt_name) = runtime(&runtimes).ok_or_else(|| anyhow::anyhow!("this Mac has no iOS simulator runtime: install one with `xcodebuild -downloadPlatform iOS` (several GB), then open it again"))?;
    let (ty, ty_name) = device_type(&list("devicetypes").await?, device).ok_or_else(|| anyhow::anyhow!("Xcode on this Mac knows no {} simulator", family(device)))?;
    let name = format!("kiln {ty_name}");
    let (udid, booted) = match kept(&list("devices").await?, &name, &rt) {
        Some(found) => found,
        None => (simctl(&["create", &name, &ty, &rt]).await?.trim().to_owned(), false),
    };
    if !booted {
        // Booted this way it has no window: nothing shows on the Mac's screen.
        simctl(&["boot", &udid]).await?;
        simctl(&["bootstatus", &udid, "-b"]).await?;
    }
    // Its Safari, open to remote automation (Settings › Safari › Advanced, set as Safari reads it).
    for domain in ["com.apple.WebInspector", "com.apple.webinspectord"] {
        for key in ["RemoteAutomationEnabled", "WebInspectorEnabled"] {
            let _ = simctl(&["spawn", &udid, "defaults", "write", domain, key, "-bool", "YES"]).await;
        }
    }
    Ok(Sim { udid, name, runtime: rt_name })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn device(name: &str) -> Device {
        super::super::devices::device(name).unwrap_or(Device { name: "none", width: 1, height: 1, scale: 1.0, mobile: false, agent: "" })
    }

    #[test]
    fn a_device_finds_its_simulator_model_and_runtime() {
        assert!(is_ios(&device("iPhone 15")) && is_ios(&device("iPad Air")));
        assert!(!is_ios(&device("Pixel 8")) && !is_ios(&device("Laptop")));
        let types = json!({"devicetypes": [
            {"identifier": "t.iPhone-SE-3rd", "name": "iPhone SE (3rd generation)", "productFamily": "iPhone"},
            {"identifier": "t.iPhone-15", "name": "iPhone 15", "productFamily": "iPhone"},
            {"identifier": "t.iPhone-17", "name": "iPhone 17", "productFamily": "iPhone"},
            {"identifier": "t.iPad-Air-11", "name": "iPad Air 11-inch (M3)", "productFamily": "iPad"},
        ]});
        assert_eq!(device_type(&types, &device("iPhone 15")).map(|t| t.0), Some("t.iPhone-15".into()));
        assert_eq!(device_type(&types, &device("iPhone SE")).map(|t| t.0), Some("t.iPhone-SE-3rd".into()));
        assert_eq!(device_type(&types, &device("iPad Air")).map(|t| t.0), Some("t.iPad-Air-11".into()));
        // A model Xcode no longer has: the newest of its family.
        let newer = json!({"devicetypes": [{"identifier": "t.iPhone-17", "name": "iPhone 17", "productFamily": "iPhone"}]});
        assert_eq!(device_type(&newer, &device("iPhone 15")).map(|t| t.0), Some("t.iPhone-17".into()));
        let runtimes = json!({"runtimes": [
            {"identifier": "r.iOS-17-5", "name": "iOS 17.5", "version": "17.5", "platform": "iOS", "isAvailable": true},
            {"identifier": "r.iOS-18-2", "name": "iOS 18.2", "version": "18.2", "platform": "iOS", "isAvailable": true},
            {"identifier": "r.iOS-19-0", "name": "iOS 19.0", "version": "19.0", "platform": "iOS", "isAvailable": false},
            {"identifier": "r.watchOS-11", "name": "watchOS 11", "version": "11.0", "platform": "watchOS", "isAvailable": true},
        ]});
        assert_eq!(runtime(&runtimes), Some(("r.iOS-18-2".into(), "iOS 18.2".into())));
        assert!(runtime(&json!({"runtimes": []})).is_none(), "none there: Xcode's own list is empty until one's downloaded");
        let devices = json!({"devices": {"r.iOS-18-2": [{"name": "kiln iPhone 15", "udid": "U1", "state": "Booted"}, {"name": "iPhone 15", "udid": "U2", "state": "Shutdown"}]}});
        assert_eq!(kept(&devices, "kiln iPhone 15", "r.iOS-18-2"), Some(("U1".into(), true)));
        assert!(kept(&devices, "kiln iPad Air 11-inch (M3)", "r.iOS-18-2").is_none());
    }

    /// The real thing, on a Mac with Xcode and an iOS runtime: kiln's simulator of an iPhone 15
    /// made (or found), booted with no window, its Safari open to automation; asked again, the
    /// same one at once.
    #[test]
    #[ignore = "needs a Mac with Xcode and an iOS simulator runtime (it makes and boots a simulator)"]
    fn a_macs_simulator_is_made_booted_and_open_to_automation() -> anyhow::Result<()> {
        if !cfg!(target_os = "macos") {
            return Ok(());
        }
        crate::rt::block_on(async {
            let sim = ensure(&device("iPhone 15")).await?;
            assert!(sim.name.starts_with("kiln iPhone") && sim.runtime.starts_with("iOS"), "{sim:?}");
            let devices = list("devices").await?;
            let booted = devices.get("devices").and_then(Value::as_object).into_iter().flat_map(|m| m.values()).filter_map(Value::as_array).flatten().any(|d| d.get("udid").and_then(Value::as_str) == Some(sim.udid.as_str()) && d.get("state").and_then(Value::as_str) == Some("Booted"));
            assert!(booted, "{sim:?} isn't booted");
            let on = simctl(&["spawn", &sim.udid, "defaults", "read", "com.apple.WebInspector", "RemoteAutomationEnabled"]).await?;
            assert_eq!(on.trim(), "1", "its Safari's remote automation");
            let t0 = std::time::Instant::now();
            let again = ensure(&device("iPhone 15")).await?;
            assert_eq!(again.udid, sim.udid);
            assert!(t0.elapsed() < Duration::from_secs(20), "found again, not made again: {:?}", t0.elapsed());
            Ok(())
        })
    }
}

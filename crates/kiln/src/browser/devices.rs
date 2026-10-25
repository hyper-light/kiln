//! Devices a live page can be laid out as (docs/previews.md, "Devices"): a window's size in CSS
//! pixels, its density, whether it's touched, and what it says it is (its user agent). By default a
//! page is laid out as its viewer is (the frame's size, the screen's density); a device profile
//! holds it to the device's instead, and the viewer shows it scaled to fit.

use serde::Serialize;

/// One device.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Device {
    pub name: &'static str,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    /// Touched, and laid out as a phone or tablet lays a page out.
    pub mobile: bool,
    /// What its browser says it is (none: the engine's own).
    #[serde(skip_serializing_if = "str::is_empty")]
    pub agent: &'static str,
}

const IOS: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";
const IPAD: &str = "Mozilla/5.0 (iPad; CPU OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1";
const ANDROID: &str = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36";

/// The devices kiln knows, phones first.
pub const DEVICES: &[Device] = &[
    Device { name: "iPhone 15", width: 393, height: 852, scale: 3.0, mobile: true, agent: IOS },
    Device { name: "iPhone SE", width: 375, height: 667, scale: 2.0, mobile: true, agent: IOS },
    Device { name: "Pixel 8", width: 412, height: 915, scale: 2.625, mobile: true, agent: ANDROID },
    Device { name: "iPad Air", width: 820, height: 1180, scale: 2.0, mobile: true, agent: IPAD },
    Device { name: "Laptop", width: 1440, height: 900, scale: 2.0, mobile: false, agent: "" },
    Device { name: "Desktop", width: 1920, height: 1080, scale: 1.0, mobile: false, agent: "" },
];

/// The device named `name` (as `DEVICES` names them, in any case).
pub fn device(name: &str) -> Option<Device> {
    DEVICES.iter().copied().find(|d| d.name.eq_ignore_ascii_case(name.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_are_found_by_name() {
        assert_eq!(device("iphone 15").map(|d| (d.width, d.height)), Some((393, 852)));
        assert!(device("Pixel 8").is_some_and(|d| d.mobile && d.agent.contains("Android")));
        assert!(device("Desktop").is_some_and(|d| !d.mobile && d.agent.is_empty()));
        assert!(device("a toaster").is_none());
        // Each its own name, a real size and density.
        for (i, d) in DEVICES.iter().enumerate() {
            assert!(d.width >= 320 && d.height >= 480 && (1.0..=3.0).contains(&d.scale), "{d:?}");
            assert!(DEVICES.iter().skip(i + 1).all(|e| e.name != d.name));
        }
    }
}

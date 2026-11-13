//! The exact present PNP devnode's PCI identity, not ADL's segment-less BDF.
//!
//! Microsoft documents the complete single/multi-segment LocationInfo forms. The single form's
//! segment zero follows that contract together with UEFI PI 1.9 volume 5 §1.2; an absent token in
//! any other text is never a zero default. See WINDOWS-PCI-SEGMENT-SOURCES.md.

#[cfg(target_os = "windows")]
mod sys;

use crate::{PciAddress, Unread};

/// One actual ADL alias whose exact present devnode agreed with the full PCI identity.
/// Kept beside that GPU's ADL counter receipt; this is neither another inventory nor a grant.
#[derive(Debug)]
pub struct WindowsAlias {
    pub(crate) instance: String,
    pub(crate) udid: String,
}

impl WindowsAlias {
    pub fn instance(&self) -> &str {
        &self.instance
    }

    pub fn udid(&self) -> &str {
        &self.udid
    }
}

/// Joins every verified alias to the same actual current-boot adapter. A LUID is separate from the
/// stable PCI identity; zero is not treated as a sentinel. The existing KMT owner supplies resolve.
pub fn same_luid(
    aliases: &[WindowsAlias],
    mut resolve: impl FnMut(&str) -> Result<[u8; 8], Unread>,
) -> Result<[u8; 8], Unread> {
    let mut found = None;
    for alias in aliases {
        let luid = resolve(alias.instance())?;
        if found.is_some_and(|previous| previous != luid) {
            return Err(Unread::Ambiguous);
        }
        found = Some(luid);
    }
    found.ok_or(Unread::Gone)
}

/// Reads only the exact configured devnode. No phantom lookup, suffix stripping, ordinal,
/// device enumeration, cached address, driver change or default segment is used.
#[cfg(target_os = "windows")]
pub fn pci_for_instance(instance: &str) -> Result<PciAddress, Unread> {
    sys::pci_for_instance(instance)
}

// Longest accepted grammar: u32 domain (10 decimal digits), u8 bus (3), device<32 (2),
// function<8 (1), with the exact separators and one terminating UTF16 NUL.
pub(crate) const LOCATION_UNITS: usize =
    "PCI segment  bus , device , function ".len() + 10 + 3 + 2 + 1 + 1;

pub(crate) fn location(units: &[u16], bytes: usize) -> Result<PciAddress, Unread> {
    let count = bytes
        .checked_div(std::mem::size_of::<u16>())
        .filter(|_| bytes.is_multiple_of(std::mem::size_of::<u16>()))
        .filter(|n| *n > 0 && *n <= LOCATION_UNITS)
        .ok_or(Unread::Malformed)?;
    let text = units.get(..count).ok_or(Unread::Malformed)?;
    let (last, prefix) = text.split_last().ok_or(Unread::Malformed)?;
    if *last != 0 {
        return Err(Unread::Malformed);
    }
    let mut ascii = [0u8; LOCATION_UNITS];
    for (unit, byte) in prefix.iter().zip(&mut ascii) {
        *byte = u8::try_from(*unit)
            .ok()
            .filter(|b| b.is_ascii() && *b != 0)
            .ok_or(Unread::Malformed)?;
    }
    let text = std::str::from_utf8(ascii.get(..prefix.len()).ok_or(Unread::Malformed)?)
        .map_err(|_| Unread::Malformed)?;
    parse_location(text).ok_or(Unread::Malformed)
}

fn parse_location(text: &str) -> Option<PciAddress> {
    let (domain, rest) = if let Some(rest) = text.strip_prefix("PCI segment ") {
        let (domain, rest) = rest.split_once(" bus ")?;
        (decimal(domain)?, rest)
    } else {
        (0, text.strip_prefix("PCI bus ")?)
    };
    let (bus, rest) = rest.split_once(", device ")?;
    let (device, function) = rest.split_once(", function ")?;
    Some(PciAddress {
        domain,
        bus: u8::try_from(decimal(bus)?).ok()?,
        device: u8::try_from(decimal(device)?).ok().filter(|n| *n < 32)?,
        function: u8::try_from(decimal(function)?).ok().filter(|n| *n < 8)?,
    })
}

fn decimal(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

pub(crate) fn corroborate(pci: PciAddress, bus: u32, address: u32) -> Result<PciAddress, Unread> {
    // DEVICE_REGISTRY_PROPERTY::DevicePropertyAddress: PCI device in the high word,
    // function in the low word; neither is an encoded PCI segment.
    let expected = (u32::from(pci.device) << 16) | u32::from(pci.function);
    if bus != u32::from(pci.bus) || address != expected {
        return Err(Unread::Ambiguous);
    }
    Ok(pci)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str) -> Result<PciAddress, Unread> {
        let units: Vec<_> = text.encode_utf16().chain([0]).collect();
        location(&units, units.len() * 2)
    }

    /// AC-T7.3.1: only the documented complete native forms establish a full PCI identity.
    #[test]
    fn complete_single_and_nonzero_segment_properties_are_checked() {
        let single = read("PCI bus 196, device 0, function 0").unwrap();
        assert_eq!(single, PciAddress::parse("0000:c4:00.0").unwrap());
        let multi = read("PCI segment 42 bus 196, device 31, function 7").unwrap();
        assert_eq!(multi, PciAddress::parse("002a:c4:1f.7").unwrap());
        assert_eq!(corroborate(multi, 196, (31 << 16) | 7), Ok(multi));
        assert_eq!(
            corroborate(multi, 195, (31 << 16) | 7),
            Err(Unread::Ambiguous)
        );
        assert_eq!(corroborate(multi, 196, 31), Err(Unread::Ambiguous));
        for text in [
            "",
            "bus 196, device 0, function 0",
            "PCI segment bus 196, device 0, function 0",
            "PCI bus -1, device 0, function 0",
            "PCI bus +1, device 0, function 0",
            "PCI bus 256, device 0, function 0",
            "PCI bus 1, device 32, function 0",
            "PCI bus 1, device 0, function 8",
            "PCI segment 4294967296 bus 1, device 0, function 0",
            "PCI bus 1, device 0, function 0 trailing",
            "PCI bus 1, device 0, function 0\0tail",
            "PCI-Bus 1, Gerät 0, Funktion 0",
        ] {
            assert_eq!(read(text), Err(Unread::Malformed), "{text:?}");
        }
    }

    /// AC-T7.3.1: native byte size and UTF16 termination are checked before grammar parsing.
    #[test]
    fn truncated_or_unterminated_properties_never_supply_a_default() {
        let mut units: Vec<_> = "PCI bus 1, device 0, function 0".encode_utf16().collect();
        assert_eq!(location(&units, units.len() * 2), Err(Unread::Malformed));
        units.push(0);
        assert_eq!(
            location(&units, units.len() * 2 - 1),
            Err(Unread::Malformed)
        );
        assert_eq!(
            location(&units, units.len() * 2 + 2),
            Err(Unread::Malformed)
        );
        units[0] = 0xd800;
        assert_eq!(location(&units, units.len() * 2), Err(Unread::Malformed));
    }

    /// AC-T7.3.1: a complete consistent current native join, not integer equality, identifies aliases.
    #[test]
    fn every_actual_alias_must_have_the_same_current_luid() {
        let aliases = [
            WindowsAlias {
                instance: "actual-a".into(),
                udid: "recorded-a".into(),
            },
            WindowsAlias {
                instance: "actual-b".into(),
                udid: "recorded-b".into(),
            },
        ];
        let mut observed = Vec::new();
        assert_eq!(
            same_luid(&aliases, |instance| {
                observed.push(instance.to_owned());
                Ok([0; 8])
            }),
            Ok([0; 8])
        );
        assert_eq!(observed, ["actual-a", "actual-b"]);
        assert_eq!(
            same_luid(&aliases, |instance| Ok(if instance == "actual-a" {
                [1; 8]
            } else {
                [2; 8]
            })),
            Err(Unread::Ambiguous)
        );
        assert_eq!(
            same_luid(&aliases, |_| Err(Unread::Gone)),
            Err(Unread::Gone)
        );
        assert_eq!(
            same_luid(&[], |_| panic!("empty aliases must not query anything")),
            Err(Unread::Gone)
        );
    }
}

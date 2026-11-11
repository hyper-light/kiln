//! The crate's own parser and messages, given what drivers print and what they never should
//! (CLAUDE.md §6: hostile input to every parser).

use super::*;

/// xorshift64: the hostile inputs' deterministic source.
pub(crate) fn next(x: &mut u64) -> u64 {
    *x ^= *x << 13;
    *x ^= *x >> 7;
    *x ^= *x << 17;
    *x
}

/// A string of up to `max` bytes drawn from `alphabet`.
pub(crate) fn hostile(x: &mut u64, alphabet: &[u8], max: u64) -> Vec<u8> {
    let len = next(x) % (max + 1);
    (0..len).map(|_| pick(x, alphabet)).collect()
}

/// One byte of `alphabet`.
fn pick(x: &mut u64, alphabet: &[u8]) -> u8 {
    let size = u64::try_from(alphabet.len()).unwrap();
    alphabet[usize::try_from(next(x) % size).unwrap()]
}

/// `seed` with up to three bytes replaced from `alphabet`, deleted or inserted: input next to valid.
fn mutated(x: &mut u64, seed: &[u8], alphabet: &[u8]) -> Vec<u8> {
    let mut bytes = seed.to_vec();
    for _ in 0..next(x) % 4 {
        let at = usize::try_from(next(x) % (u64::try_from(bytes.len()).unwrap() + 1)).unwrap();
        match next(x) % 3 {
            0 if at < bytes.len() => bytes[at] = pick(x, alphabet),
            1 if at < bytes.len() => {
                bytes.remove(at);
            }
            _ => bytes.insert(at, pick(x, alphabet)),
        }
    }
    bytes
}

/// AC-T7.3.1: do parse the address NVML prints for a GPU and the one the kernel prints for it; expect
/// one address, printed as the kernel prints it.
#[test]
fn nvml_and_the_kernel_name_one_pci_address() {
    let nvml = PciAddress::parse("00000000:01:00.0").unwrap();
    let kernel = PciAddress::parse("0000:01:00.0").unwrap();
    assert_eq!(nvml, kernel);
    assert_eq!(kernel.to_string(), "0000:01:00.0");
    assert_eq!(
        PciAddress::parse("10000:E1:1f.7").unwrap(),
        PciAddress {
            domain: 0x10000,
            bus: 0xe1,
            device: 0x1f,
            function: 7
        }
    );
    assert_eq!(
        PciAddress::parse("10000:E1:1f.7").unwrap().to_string(),
        "10000:e1:1f.7"
    );
}

/// AC-T7.3.1: do parse addresses no bus has (a device past 0x1f, a function past 7, a sign, a
/// missing or extra digit); expect each refused.
#[test]
fn addresses_no_bus_has_are_refused() {
    for bad in [
        "",
        "0000:01:00",
        "0000:01:00.8",
        "0000:01:20.0",
        "0000:1:00.0",
        "0000:001:00.0",
        "+000:01:00.0",
        "000000000:01:00.0",
        ":01:00.0",
        "0000:01:00.0 ",
        "0000:0g:00.0",
        "0000:01:00.00",
        "0000:01:00.-0",
    ] {
        assert_eq!(PciAddress::parse(bad), None, "{bad:?}");
    }
}

/// AC-T7.3.1, hostile input: do parse 100,000 strings, half of address characters and noise, half a
/// valid address with bytes replaced, deleted or inserted; expect no panic, and each that parses to
/// print as an address that parses to itself.
#[test]
fn hostile_addresses_never_panic() {
    let mut x = 0x9e37_79b9_7f4a_7c15;
    let mut parsed = 0;
    for round in 0..100_000 {
        let alphabet = b"0123456789abcdefABCDEF:.+- x\n\0";
        let bytes = if round % 2 == 0 {
            hostile(&mut x, alphabet, 18)
        } else {
            mutated(&mut x, b"0000:01:00.0", alphabet)
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        if let Some(address) = PciAddress::parse(text) {
            parsed += 1;
            assert_eq!(PciAddress::parse(&address.to_string()), Some(address));
        }
    }
    // Non-vacuity: some of the mutated half are still addresses.
    assert!(parsed > 0);
}

/// AC-T7.3.1: do show a refusal; expect it to say each vendor's reason, NVML's code in words.
#[test]
fn a_refusal_says_each_vendors_reason() {
    let none = NoGpus {
        nvidia: Absent::Nvml { code: 9 },
        amd: Absent::Unsupported,
        apple: Absent::Unsupported,
    };
    assert_eq!(
        none.to_string(),
        "no GPU to read: NVIDIA: NVML refused: nvmlReturn_t 9 (NVIDIA's driver isn't loaded); AMD: \
         this platform has no interface Hex reads for them; Apple: this platform has no interface Hex reads for them"
    );
    assert_eq!(
        Unread::Nvml { code: 777 }.to_string(),
        "NVML refused: nvmlReturn_t 777"
    );
}

/// AC-T7.3.1: do name `Gpus` where a thread may own or share it (the node-facts probe's); expect it to
/// be `Send` and `Sync`, its NVML handles included.
#[test]
fn gpus_go_to_the_probes_thread() {
    fn owned_or_shared_across_threads<T: Send + Sync>() {}
    owned_or_shared_across_threads::<Gpus>();
}

/// AC-T7.3.1: do carry native Metal IDs with the high bits set; expect every bit retained and no
/// invented PCI address. PCI's existing domain32/bus8/device5/function3 representation is unchanged.
#[test]
fn identities_keep_native_bits_and_their_address_kind() {
    let address = PciAddress::parse("ffffffff:ff:1f.7").unwrap();
    let pci = Identity::Pci(address);
    let metal = Identity::Metal(0xfedc_ba98_7654_3210);
    assert_eq!(pci.key(), 0xffff_ffff_ffff);
    assert_eq!(pci.pci(), Some(address));
    assert_eq!(metal.key(), 0xfedc_ba98_7654_3210);
    assert_eq!(metal.pci(), None);
    assert_eq!(metal.to_string(), "metal:18364758544493064720");
    let gpu = Gpu {
        vendor: Vendor::Apple,
        model: "native fixture".into(),
        memory_total_bytes: 1,
        identity: metal,
        driver_version: "fixture release".into(),
    };
    assert_eq!(gpu.key(), metal.key());
    assert_eq!(gpu.pci(), None);
    // Keys alone do not erase the address kind even when their numeric parts coincide.
    assert_ne!(pci, Identity::Metal(pci.key()));
}

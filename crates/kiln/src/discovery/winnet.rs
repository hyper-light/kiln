//! Windows network adapters through `GetAdaptersAddresses` (iphlpapi), declared by hand: each
//! adapter's names, interface indexes, type and state, its IPv4 addresses with their on-link
//! prefix lengths, its IPv6 addresses, name servers and DNS suffixes. What
//! [`super::interfaces`] picks multicast interfaces from and unicast DNS-SD takes its name
//! servers and search domains from.
//!
//! Self-contained (std only), so it type-checks for Windows from any host:
//! `rustc --edition 2024 --crate-type lib --target x86_64-pc-windows-gnu`. The structures are
//! the Vista+ (`_LH`) layouts; their sizes and offsets on 64-bit Windows are asserted at compile
//! time against the values of the Windows SDK headers.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// One network adapter.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Adapter {
    /// The friendly name, e.g. `Ethernet` or `Wi-Fi`.
    pub name: String,
    /// The adapter's GUID name, e.g. `{4D36E972-E325-11CE-BFC1-08002BE10318}`.
    pub id: String,
    pub description: String,
    /// The IPv4 interface index (0 when IPv4 is off on the adapter).
    pub index: u32,
    /// The IPv6 interface index (0 when IPv6 is off on the adapter).
    pub ipv6_index: u32,
    /// The IANA interface type (`IF_TYPE_*`).
    pub if_type: u32,
    /// Operationally up.
    pub up: bool,
    /// Multicast-capable (no `IP_ADAPTER_NO_MULTICAST`).
    pub multicast: bool,
    /// IPv4 addresses with their on-link prefix lengths.
    pub v4: Vec<(Ipv4Addr, u8)>,
    pub v6: Vec<Ipv6Addr>,
    pub dns_servers: Vec<IpAddr>,
    /// The adapter's DNS suffix and its additional search suffixes.
    pub dns_suffixes: Vec<String>,
}

pub const IF_TYPE_PPP: u32 = 23;
pub const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
pub const IF_TYPE_PROP_VIRTUAL: u32 = 53;
pub const IF_TYPE_TUNNEL: u32 = 131;

const AF_UNSPEC: u32 = 0;
const AF_INET: u16 = 2;
const AF_INET6: u16 = 23;
const GAA_FLAG_SKIP_ANYCAST: u32 = 0x2;
const GAA_FLAG_SKIP_MULTICAST: u32 = 0x4;
const IP_ADAPTER_NO_MULTICAST: u32 = 0x10;
const IF_OPER_STATUS_UP: i32 = 1;
const ERROR_SUCCESS: u32 = 0;
const ERROR_BUFFER_OVERFLOW: u32 = 111;
const ERROR_NO_DATA: u32 = 232;
/// Most adapters and addresses read, whatever the system reports.
const MAX_ITEMS: usize = 256;

#[repr(C)]
struct SocketAddress {
    sockaddr: *const u8,
    length: i32,
}

#[repr(C)]
struct UnicastAddress {
    length: u32,
    flags: u32,
    next: *const UnicastAddress,
    address: SocketAddress,
    prefix_origin: i32,
    suffix_origin: i32,
    dad_state: i32,
    valid_lifetime: u32,
    preferred_lifetime: u32,
    lease_lifetime: u32,
    on_link_prefix_length: u8,
}

#[repr(C)]
struct DnsServerAddress {
    length: u32,
    reserved: u32,
    next: *const DnsServerAddress,
    address: SocketAddress,
}

#[repr(C)]
struct DnsSuffix {
    next: *const DnsSuffix,
    string: [u16; 256],
}

#[repr(C)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

/// `IP_ADAPTER_ADDRESSES_LH`.
#[repr(C)]
struct AdapterAddresses {
    length: u32,
    if_index: u32,
    next: *const AdapterAddresses,
    adapter_name: *const u8,
    first_unicast: *const UnicastAddress,
    first_anycast: *const c_void,
    first_multicast: *const c_void,
    first_dns_server: *const DnsServerAddress,
    dns_suffix: *const u16,
    description: *const u16,
    friendly_name: *const u16,
    physical_address: [u8; 8],
    physical_address_length: u32,
    flags: u32,
    mtu: u32,
    if_type: u32,
    oper_status: i32,
    ipv6_if_index: u32,
    zone_indices: [u32; 16],
    first_prefix: *const c_void,
    transmit_link_speed: u64,
    receive_link_speed: u64,
    first_wins_server: *const c_void,
    first_gateway: *const c_void,
    ipv4_metric: u32,
    ipv6_metric: u32,
    luid: u64,
    dhcpv4_server: SocketAddress,
    compartment_id: u32,
    network_guid: Guid,
    connection_type: i32,
    tunnel_type: i32,
    dhcpv6_server: SocketAddress,
    dhcpv6_client_duid: [u8; 130],
    dhcpv6_client_duid_length: u32,
    dhcpv6_iaid: u32,
    first_dns_suffix: *const DnsSuffix,
}

// The layouts of the Windows SDK (and mingw-w64) headers on 64-bit Windows.
#[cfg(target_pointer_width = "64")]
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<AdapterAddresses>() == 448);
    assert!(offset_of!(AdapterAddresses, first_dns_server) == 48);
    assert!(offset_of!(AdapterAddresses, friendly_name) == 72);
    assert!(offset_of!(AdapterAddresses, flags) == 92);
    assert!(offset_of!(AdapterAddresses, if_type) == 100);
    assert!(offset_of!(AdapterAddresses, oper_status) == 104);
    assert!(offset_of!(AdapterAddresses, ipv6_if_index) == 108);
    assert!(offset_of!(AdapterAddresses, dhcpv6_server) == 280);
    assert!(offset_of!(AdapterAddresses, first_dns_suffix) == 440);
    assert!(size_of::<UnicastAddress>() == 64);
    assert!(offset_of!(UnicastAddress, address) == 16);
    assert!(offset_of!(UnicastAddress, on_link_prefix_length) == 56);
    assert!(size_of::<DnsServerAddress>() == 32);
    assert!(size_of::<DnsSuffix>() == 520);
};

#[link(name = "iphlpapi")]
unsafe extern "system" {
    fn GetAdaptersAddresses(family: u32, flags: u32, reserved: *mut c_void, addresses: *mut AdapterAddresses, size: *mut u32) -> u32;
}

/// A NUL-terminated UTF-16 string the system owns, at most `max` units.
///
/// # Safety
/// `p` is null or points at a NUL-terminated UTF-16 string that stays valid for the call.
unsafe fn wide(p: *const u16, max: usize) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut units = Vec::new();
    for i in 0..max {
        // SAFETY: the string is NUL-terminated and `i` has not passed the terminator yet.
        let u = unsafe { *p.add(i) };
        if u == 0 {
            break;
        }
        units.push(u);
    }
    String::from_utf16_lossy(&units)
}

/// A NUL-terminated ANSI string the system owns, at most `max` bytes.
///
/// # Safety
/// As [`wide`].
unsafe fn narrow(p: *const u8, max: usize) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut bytes = Vec::new();
    for i in 0..max {
        // SAFETY: as in `wide`.
        let b = unsafe { *p.add(i) };
        if b == 0 {
            break;
        }
        bytes.push(b);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The IP address of a `SOCKET_ADDRESS` (IPv4 or IPv6), if it holds one.
///
/// # Safety
/// `a.sockaddr` is null or points at `a.length` readable bytes.
unsafe fn ip_of(a: &SocketAddress) -> Option<IpAddr> {
    let len = usize::try_from(a.length).ok()?;
    if a.sockaddr.is_null() || len < 8 {
        return None;
    }
    // SAFETY: the system says `length` bytes are there.
    let bytes = unsafe { std::slice::from_raw_parts(a.sockaddr, len) };
    let family = u16::from_le_bytes([*bytes.first()?, *bytes.get(1)?]);
    match family {
        // sockaddr_in: family, port, then the address.
        AF_INET => {
            let o: [u8; 4] = bytes.get(4..8)?.try_into().ok()?;
            Some(IpAddr::V4(Ipv4Addr::from(o)))
        }
        // sockaddr_in6: family, port, flow info, then the address.
        AF_INET6 => {
            let o: [u8; 16] = bytes.get(8..24)?.try_into().ok()?;
            Some(IpAddr::V6(Ipv6Addr::from(o)))
        }
        _ => None,
    }
}

/// Every adapter, as the system lists them.
pub fn adapters() -> std::io::Result<Vec<Adapter>> {
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST;
    let mut size: u32 = 16 * 1024;
    // The list can grow between the sizing call and the next one: a few tries.
    for _ in 0..4 {
        // u64 elements: the structures need 8-byte alignment.
        let mut buf: Vec<u64> = vec![0; usize::try_from(size).unwrap_or(16 * 1024).div_ceil(8)];
        let head = buf.as_mut_ptr().cast::<AdapterAddresses>();
        // SAFETY: `buf` holds `size` writable, 8-aligned bytes and outlives the call.
        let rc = unsafe { GetAdaptersAddresses(AF_UNSPEC, flags, std::ptr::null_mut(), head, &mut size) };
        match rc {
            ERROR_SUCCESS => {
                // SAFETY: on success the buffer holds a list of adapters that point into it.
                return Ok(unsafe { read_list(head) });
            }
            ERROR_NO_DATA => return Ok(Vec::new()),
            ERROR_BUFFER_OVERFLOW => continue,
            code => return Err(std::io::Error::from_raw_os_error(i32::try_from(code).unwrap_or(i32::MAX))),
        }
    }
    Err(std::io::Error::other("the adapter list kept growing"))
}

/// # Safety
/// `head` is the start of a list `GetAdaptersAddresses` filled in, still alive.
unsafe fn read_list(head: *const AdapterAddresses) -> Vec<Adapter> {
    let mut out = Vec::new();
    let mut p = head;
    while !p.is_null() && out.len() < MAX_ITEMS {
        // SAFETY: a list entry the system wrote.
        let a = unsafe { &*p };
        let mut adapter = Adapter {
            // SAFETY: strings of the entry, NUL-terminated.
            name: unsafe { wide(a.friendly_name, 256) },
            id: unsafe { narrow(a.adapter_name, 256) },
            description: unsafe { wide(a.description, 256) },
            index: a.if_index,
            ipv6_index: a.ipv6_if_index,
            if_type: a.if_type,
            up: a.oper_status == IF_OPER_STATUS_UP,
            multicast: a.flags & IP_ADAPTER_NO_MULTICAST == 0,
            ..Adapter::default()
        };
        let mut u = a.first_unicast;
        while !u.is_null() && adapter.v4.len() + adapter.v6.len() < MAX_ITEMS {
            // SAFETY: an entry of the adapter's unicast list.
            let ua = unsafe { &*u };
            // SAFETY: its socket address, as the system filled it in.
            match unsafe { ip_of(&ua.address) } {
                Some(IpAddr::V4(ip)) => adapter.v4.push((ip, ua.on_link_prefix_length.min(32))),
                Some(IpAddr::V6(ip)) => adapter.v6.push(ip),
                None => {}
            }
            u = ua.next;
        }
        let mut d = a.first_dns_server;
        while !d.is_null() && adapter.dns_servers.len() < 16 {
            // SAFETY: an entry of the adapter's name server list.
            let da = unsafe { &*d };
            // SAFETY: as above.
            if let Some(ip) = unsafe { ip_of(&da.address) } {
                adapter.dns_servers.push(ip);
            }
            d = da.next;
        }
        // SAFETY: the adapter's DNS suffix, NUL-terminated.
        let suffix = unsafe { wide(a.dns_suffix, 256) };
        if !suffix.is_empty() {
            adapter.dns_suffixes.push(suffix);
        }
        let mut s = a.first_dns_suffix;
        while !s.is_null() && adapter.dns_suffixes.len() < 16 {
            // SAFETY: an entry of the adapter's suffix list; its string is an inline array.
            let sa = unsafe { &*s };
            let len = sa.string.iter().position(|c| *c == 0).unwrap_or(sa.string.len());
            let text = String::from_utf16_lossy(sa.string.get(..len).unwrap_or_default());
            if !text.is_empty() && !adapter.dns_suffixes.contains(&text) {
                adapter.dns_suffixes.push(text);
            }
            s = sa.next;
        }
        out.push(adapter);
        p = a.next;
    }
    out
}

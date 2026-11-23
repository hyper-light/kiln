//! AD-1. Winsock packet timestamps. The control message carries a counter without its clock
//! identity. IP_PKTINFO identifies the receiving interface, whose active configuration names
//! that clock. An unstamped packet can still carry QUIC data.
//! Microsoft Learn: “Winsock timestamping” and “Packet timestamping”.
#![allow(
    unsafe_code,
    reason = "Winsock extension, IP Helper and ancillary-data ABI"
)]

use core::ffi::c_void;
use core::mem::{size_of, size_of_val};
use core::net::{IpAddr, SocketAddr};
use core::ptr;
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use windows_sys::Win32::NetworkManagement::IpHelper::{
    ConvertInterfaceIndexToLuid, FreeMibTable, GetUnicastIpAddressTable, HIFTIMESTAMPCHANGE,
    INTERFACE_HARDWARE_CROSSTIMESTAMP, INTERFACE_TIMESTAMP_CAPABILITIES,
    MIB_UNICASTIPADDRESS_TABLE, PINTERFACE_TIMESTAMP_CONFIG_CHANGE_CALLBACK,
};
use windows_sys::Win32::NetworkManagement::Ndis::NET_LUID_LH;
use windows_sys::Win32::Networking::WinSock::*;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use crate::error::RtError;
use crate::netsys::{Io, winsock};

type Active =
    unsafe extern "system" fn(*const NET_LUID_LH, *mut INTERFACE_TIMESTAMP_CAPABILITIES) -> u32;
type Cross =
    unsafe extern "system" fn(*const NET_LUID_LH, *mut INTERFACE_HARDWARE_CROSSTIMESTAMP) -> u32;
type Register = unsafe extern "system" fn(
    PINTERFACE_TIMESTAMP_CONFIG_CHANGE_CALLBACK,
    *const c_void,
    *mut HIFTIMESTAMPCHANGE,
) -> u32;
type Export = unsafe extern "system" fn() -> isize;

struct Api {
    active: Active,
    cross: Option<Cross>,
    hz: u64,
}
static CHANGES: AtomicU64 = AtomicU64::new(0);

unsafe extern "system" fn config_changed(_: *const c_void) {
    CHANGES.fetch_add(1, Ordering::SeqCst);
}

impl Api {
    fn get() -> Option<&'static Self> {
        static API: OnceLock<Option<Api>> = OnceLock::new();
        API.get_or_init(|| {
            // The Vista-era ConvertInterfaceIndexToLuid import keeps iphlpapi loaded. Resolve
            // newer timestamp APIs dynamically so a machine predating build 20348 can start.
            // SAFETY: a static terminated DLL name and ASCII export names, each with the named SDK ABI.
            let (active, cross, register) = unsafe {
                let module = GetModuleHandleW(windows_sys::w!("iphlpapi.dll"));
                if module.is_null() {
                    return None;
                }
                let active = GetProcAddress(
                    module,
                    c"GetInterfaceActiveTimestampCapabilities".as_ptr().cast(),
                )?;
                let cross = GetProcAddress(
                    module,
                    c"CaptureInterfaceHardwareCrossTimestamp".as_ptr().cast(),
                );
                let register = GetProcAddress(
                    module,
                    c"RegisterInterfaceTimestampConfigChange".as_ptr().cast(),
                )?;
                (
                    core::mem::transmute::<Export, Active>(active),
                    cross.map(|f| core::mem::transmute::<Export, Cross>(f)),
                    core::mem::transmute::<Export, Register>(register),
                )
            };
            let mut hz = 0;
            // SAFETY: a live writable counter frequency; on success Windows supplies QPC ticks/sec.
            if unsafe { QueryPerformanceFrequency(&raw mut hz) } == 0 || hz <= 0 {
                return None;
            }
            let mut handle = ptr::null_mut();
            // SAFETY: one subscription per process, its callback in this image and its sole state
            // the static atomic above. No socket or caller allocation is retained by IP Helper.
            if unsafe { register(Some(config_changed), ptr::null(), &raw mut handle) } != 0 {
                return None;
            }
            Some(Self {
                active,
                cross,
                hz: u64::try_from(hz).ok()?,
            })
        })
        .as_ref()
    }

    fn clock(&self, luid: NET_LUID_LH) -> Option<Source> {
        let mut caps = INTERFACE_TIMESTAMP_CAPABILITIES::default();
        // SAFETY: luid is the interface IP Helper returned and caps is a live SDK output.
        if unsafe { (self.active)(&raw const luid, &raw mut caps) } != 0 {
            return None;
        }
        self.source(luid, caps)
    }

    fn source(&self, luid: NET_LUID_LH, caps: INTERFACE_TIMESTAMP_CAPABILITIES) -> Option<Source> {
        let hw = caps.HardwareCapabilities;
        let hw_rx = hw.AllReceive
            || hw.PtpV2OverUdpIPv4AllMessageReceive
            || hw.PtpV2OverUdpIPv6AllMessageReceive
            || hw.PtpV2OverUdpIPv4EventMessageReceive
            || hw.PtpV2OverUdpIPv6EventMessageReceive;
        if caps.SoftwareCapabilities.AllReceive && !hw_rx {
            return Some(Source::Software);
        }
        if !caps.SoftwareCapabilities.AllReceive && hw.AllReceive && caps.SupportsCrossTimestamp {
            let capture = self.cross?;
            let first = crossing(luid, capture)?;
            return Some(Source::Hardware {
                luid,
                capture,
                previous: first,
                latest: first,
            });
        }
        None
    }

    fn interfaces(&self, bound: SocketAddr) -> BTreeMap<u32, Option<Source>> {
        let mut interfaces = BTreeMap::new();
        let family = if bound.is_ipv4() { AF_INET } else { AF_INET6 };
        let mut table: *mut MIB_UNICASTIPADDRESS_TABLE = ptr::null_mut();
        // SAFETY: the requested family determines the arm for every row in this SDK allocation.
        if unsafe { GetUnicastIpAddressTable(family, &raw mut table) } != 0 || table.is_null() {
            return interfaces;
        }
        // SAFETY: the SDK's trailing Table field contains exactly NumEntries initialized records.
        let rows = unsafe {
            std::slice::from_raw_parts(
                ptr::addr_of!((*table).Table).cast::<windows_sys::Win32::NetworkManagement::IpHelper::MIB_UNICASTIPADDRESS_ROW>(),
                usize::try_from((*table).NumEntries).unwrap_or(0),
            )
        };
        for row in rows {
            if row.DadState != IpDadStatePreferred && row.DadState != IpDadStateDeprecated {
                continue;
            }
            // SAFETY: IP Helper returned only the specifically requested address family.
            let ip = unsafe {
                match bound {
                    SocketAddr::V4(_) => {
                        IpAddr::from(row.Address.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes())
                    }
                    SocketAddr::V6(v6) => {
                        if v6.scope_id() != 0 && v6.scope_id() != row.InterfaceIndex {
                            continue;
                        }
                        IpAddr::from(row.Address.Ipv6.sin6_addr.u.Byte)
                    }
                }
            };
            if bound.ip().is_unspecified() || bound.ip() == ip {
                interfaces
                    .entry(row.InterfaceIndex)
                    .or_insert_with(|| self.clock(row.InterfaceLuid));
            }
        }
        // SAFETY: this is the same successful allocation; no row escapes its lifetime.
        unsafe {
            FreeMibTable(table.cast());
        }
        interfaces
    }
}

#[derive(Clone, Copy)]
enum Source {
    Software,
    Hardware {
        luid: NET_LUID_LH,
        capture: Cross,
        previous: INTERFACE_HARDWARE_CROSSTIMESTAMP,
        latest: INTERFACE_HARDWARE_CROSSTIMESTAMP,
    },
}

fn scaled(ticks: u64, rate: u64, units: u64) -> Option<u64> {
    u64::try_from(
        u128::from(ticks)
            .checked_mul(u128::from(units))?
            .checked_div(u128::from(rate))?,
    )
    .ok()
}

fn crossing(luid: NET_LUID_LH, capture: Cross) -> Option<INTERFACE_HARDWARE_CROSSTIMESTAMP> {
    let mut pair = INTERFACE_HARDWARE_CROSSTIMESTAMP::default();
    // SAFETY: IP Helper supplied the interface LUID and this is a correctly sized SDK output.
    if unsafe { capture(&raw const luid, &raw mut pair) } != 0
        || pair.SystemTimestamp1 > pair.SystemTimestamp2
    {
        return None;
    }
    Some(pair)
}

// The nominal HardwareClockFrequencyHz can be zero or differ from the actual NIC clock.
// Interpolate between measured crossings; never extrapolate across an unknown clock change.
fn qpc_at(
    ticks: u64,
    first: &INTERFACE_HARDWARE_CROSSTIMESTAMP,
    last: &INTERFACE_HARDWARE_CROSSTIMESTAMP,
) -> Option<u64> {
    if first.SystemTimestamp1 >= last.SystemTimestamp1
        || first.SystemTimestamp2 > last.SystemTimestamp1
        || ticks > last.HardwareClockTimestamp
    {
        return None;
    }
    first.SystemTimestamp1.checked_add(scaled(
        ticks.checked_sub(first.HardwareClockTimestamp)?,
        last.HardwareClockTimestamp
            .checked_sub(first.HardwareClockTimestamp)?,
        last.SystemTimestamp1.checked_sub(first.SystemTimestamp1)?,
    )?)
}

impl Source {
    fn age(&mut self, ticks: u64, qpc_hz: u64) -> Option<u64> {
        let (at, after) = match self {
            Self::Software => (ticks, ticks),
            Self::Hardware {
                luid,
                capture,
                previous,
                latest,
            } => {
                if ticks >= latest.HardwareClockTimestamp {
                    let next = crossing(*luid, *capture)?;
                    if next.HardwareClockTimestamp <= latest.HardwareClockTimestamp
                        || next.SystemTimestamp1 < latest.SystemTimestamp2
                    {
                        *previous = next;
                        *latest = next;
                        return None;
                    }
                    *previous = *latest;
                    *latest = next;
                }
                (qpc_at(ticks, previous, latest)?, latest.SystemTimestamp2)
            }
        };
        let mut now = 0;
        // SAFETY: writable QPC output. A clock jump, timestamp from the future or overflow is unusable.
        if unsafe { QueryPerformanceCounter(&raw mut now) } == 0 {
            return None;
        }
        let now = u64::try_from(now).ok()?;
        if now < after {
            return None;
        }
        scaled(now.checked_sub(at)?, qpc_hz, 1_000_000_000)
    }
}

type Receive = unsafe extern "system" fn(
    SOCKET,
    *mut WSAMSG,
    *mut u32,
    *mut windows_sys::Win32::System::IO::OVERLAPPED,
    LPWSAOVERLAPPED_COMPLETION_ROUTINE,
) -> i32;

pub(super) struct Receiver {
    recv: Receive,
    api: Option<&'static Api>,
    available: bool,
    generation: u64,
    pending: bool,
    interfaces: BTreeMap<u32, Option<Source>>,
}

pub(super) struct Received {
    pub from: SocketAddr,
    pub to: Option<IpAddr>,
    pub bytes: usize,
    pub age_ns: Option<u64>,
}

fn ioctl<I, O>(s: SOCKET, op: u32, input: &I, output: Option<&mut O>) -> bool {
    let mut returned = 0;
    let out = output.map_or(ptr::null_mut(), core::ptr::from_mut);
    let Some((ilen, olen)) = u32::try_from(size_of::<I>())
        .ok()
        .zip(u32::try_from(size_of::<O>()).ok())
    else {
        return false;
    };
    // SAFETY: input/output are live for this synchronous call, sized by their actual SDK types.
    unsafe {
        WSAIoctl(
            s,
            op,
            ptr::from_ref(input).cast(),
            ilen,
            out.cast(),
            if out.is_null() { 0 } else { olen },
            &raw mut returned,
            ptr::null_mut(),
            None,
        ) == 0
    }
}

impl Receiver {
    pub(super) fn open(s: SOCKET, bound: SocketAddr, stamps: bool) -> Option<Self> {
        let mut recv: LPFN_WSARECVMSG = None;
        if !ioctl(
            s,
            SIO_GET_EXTENSION_FUNCTION_POINTER,
            &WSAID_WSARECVMSG,
            Some(&mut recv),
        ) {
            return None;
        }
        let recv = recv?;
        let (level, opt) = if bound.is_ipv6() {
            (IPPROTO_IPV6, IPV6_PKTINFO)
        } else {
            (IPPROTO_IP, IP_PKTINFO)
        };
        let on = 1u32;
        // SAFETY: this provider's live socket and a correctly sized DWORD option.
        if unsafe {
            setsockopt(
                s,
                level,
                opt,
                ptr::from_ref(&on).cast(),
                i32::try_from(size_of_val(&on)).ok()?,
            )
        } != 0
        {
            return None;
        }
        let generation = CHANGES.load(Ordering::SeqCst);
        let api = stamps.then(Api::get).flatten().filter(|_| {
            ioctl::<_, ()>(
                s,
                SIO_TIMESTAMPING,
                &TIMESTAMPING_CONFIG {
                    Flags: TIMESTAMPING_FLAG_RX,
                    TxTimestampsBuffered: 0,
                },
                None,
            )
        });
        let interfaces = api.map(|api| api.interfaces(bound)).unwrap_or_default();
        let available = interfaces.values().any(Option::is_some);
        Some(Self {
            recv,
            api,
            available,
            generation,
            pending: false,
            interfaces,
        })
    }
    pub(super) fn stamps(&self) -> bool {
        self.available
    }

    fn timestamp(&mut self, control: &Controls) -> Option<u64> {
        let api = self.api?;
        let generation = CHANGES.load(Ordering::SeqCst);
        if self.generation != generation {
            self.generation = generation;
            self.pending = true;
            self.interfaces.clear();
        }
        // A timestamp has no type tag. Datagrams already queued at reconfiguration may
        // still use the previous clock. Resume only after Winsock reports an empty queue.
        if self.pending {
            return None;
        }
        let (ticks, (_, index)) = (control.stamp?, control.interface?);
        let source = self.interfaces.entry(index).or_insert_with(|| {
            let mut luid = NET_LUID_LH::default();
            // SAFETY: initialized IP_PKTINFO index, live LUID output.
            (unsafe { ConvertInterfaceIndexToLuid(index, &raw mut luid) } == 0)
                .then(|| api.clock(luid))
                .flatten()
        });
        let age = source.as_mut()?.age(ticks, api.hz)?;
        (generation == CHANGES.load(Ordering::SeqCst)).then_some(age)
    }

    pub(super) fn receive(
        &mut self,
        s: SOCKET,
        buf: &mut [u8],
    ) -> Result<Io<Option<Received>>, RtError> {
        let mut address = SOCKADDR_STORAGE::default();
        let mut data = WSABUF {
            len: u32::try_from(buf.len()).map_err(|_| RtError::BadConfig {
                what: "Winsock datagram capacity",
            })?,
            buf: buf.as_mut_ptr(),
        };
        let mut control = Control {
            _align: [],
            bytes: [0; CONTROL],
        };
        let mut msg = WSAMSG {
            name: ptr::from_mut(&mut address).cast(),
            namelen: i32::try_from(size_of::<SOCKADDR_STORAGE>()).unwrap_or(i32::MAX),
            lpBuffers: &raw mut data,
            dwBufferCount: 1,
            Control: WSABUF {
                len: u32::try_from(CONTROL).unwrap_or(0),
                buf: control.bytes.as_mut_ptr(),
            },
            dwFlags: 0,
        };
        let mut bytes = 0;
        let before = CHANGES.load(Ordering::SeqCst);
        // SAFETY: extension was retrieved from this socket's provider, all buffers are live
        // and correctly sized, and the nonblocking call uses no overlapped completion.
        if unsafe { (self.recv)(s, &raw mut msg, &raw mut bytes, ptr::null_mut(), None) }
            == SOCKET_ERROR
        {
            // SAFETY: thread-local Winsock error immediately following this call.
            let code = unsafe { WSAGetLastError() };
            return match code {
                WSAEWOULDBLOCK => {
                    let after = CHANGES.load(Ordering::SeqCst);
                    if self.generation != after {
                        self.interfaces.clear();
                    }
                    self.generation = after;
                    self.pending = before != after;
                    Ok(Io::WouldBlock)
                }
                WSAEINTR => Ok(Io::Interrupted),
                WSAEMSGSIZE => Ok(Io::Ready(None)),
                WSAECONNRESET | WSAENETRESET | WSAECONNREFUSED | WSAEHOSTUNREACH
                | WSAENETUNREACH => Ok(Io::Astray),
                _ => Err(RtError::DriverRefused {
                    call: "WSARecvMsg",
                    code: Some(code),
                }),
            };
        }
        if msg.dwFlags & (MSG_TRUNC | MSG_PARTIAL) != 0 {
            return Ok(Io::Ready(None));
        }
        let Some(from) = winsock::from_sockaddr(&address) else {
            return Ok(Io::Ready(None));
        };
        let fields = if msg.dwFlags & MSG_CTRUNC == 0 {
            usize::try_from(msg.Control.len)
                .ok()
                .and_then(|n| control.bytes.get(..n))
                .and_then(controls)
                .unwrap_or_default()
        } else {
            Controls::default()
        };
        Ok(Io::Ready(Some(Received {
            from,
            to: fields.interface.map(|(ip, _)| ip),
            bytes: usize::try_from(bytes).unwrap_or(0),
            age_ns: self.timestamp(&fields),
        })))
    }
}

const fn aligned(n: usize) -> usize {
    n.next_multiple_of(size_of::<usize>())
}
const fn space(n: usize) -> usize {
    aligned(size_of::<CMSGHDR>()).saturating_add(aligned(n))
}
const CONTROL: usize = space(size_of::<IN6_PKTINFO>()).saturating_add(space(size_of::<u64>()));
#[repr(C)]
struct Control {
    _align: [usize; 0],
    bytes: [u8; CONTROL],
}
#[derive(Default)]
struct Controls {
    stamp: Option<u64>,
    interface: Option<(IpAddr, u32)>,
}

fn controls(mut buf: &[u8]) -> Option<Controls> {
    let mut fields = Controls::default();
    let header = aligned(size_of::<CMSGHDR>());
    while !buf.is_empty() {
        if buf.len() < header {
            return None;
        }
        // SAFETY: buf has the full SDK header; this copies initialized bytes without assuming alignment.
        let c = unsafe { ptr::read_unaligned(buf.as_ptr().cast::<CMSGHDR>()) };
        let body = buf.get(header..c.cmsg_len)?;
        match (c.cmsg_level, c.cmsg_type) {
            (SOL_SOCKET, k) if u32::try_from(k).ok() == Some(SO_TIMESTAMP) => {
                if fields
                    .stamp
                    .replace(u64::from_ne_bytes(body.try_into().ok()?))
                    .is_some()
                {
                    return None;
                }
            }
            (IPPROTO_IP, IP_PKTINFO) => {
                if body.len() != size_of::<IN_PKTINFO>() {
                    return None;
                }
                // SAFETY: size matches IN_PKTINFO; address union's integer is fully initialized.
                let p = unsafe { ptr::read_unaligned(body.as_ptr().cast::<IN_PKTINFO>()) };
                // SAFETY: this is the IPv4 arm Winsock filled, all bits form a valid address.
                let ip = IpAddr::from(unsafe { p.ipi_addr.S_un.S_addr }.to_ne_bytes());
                if fields.interface.replace((ip, p.ipi_ifindex)).is_some() {
                    return None;
                }
            }
            (IPPROTO_IPV6, IPV6_PKTINFO) => {
                if body.len() != size_of::<IN6_PKTINFO>() {
                    return None;
                }
                // SAFETY: size matches IN6_PKTINFO, and all bytes of the IPv6 address are initialized.
                let p = unsafe { ptr::read_unaligned(body.as_ptr().cast::<IN6_PKTINFO>()) };
                // SAFETY: this arm is the exact sixteen network-order bytes Winsock supplied.
                let ip = IpAddr::from(unsafe { p.ipi6_addr.u.Byte });
                if fields.interface.replace((ip, p.ipi6_ifindex)).is_some() {
                    return None;
                }
            }
            _ => {}
        }
        buf = buf.get(aligned(c.cmsg_len)..).unwrap_or_default();
    }
    Some(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ad_1_hardware_clock_uses_measured_crossings_and_rejects_unknown_time() {
        let first = INTERFACE_HARDWARE_CROSSTIMESTAMP {
            HardwareClockTimestamp: 200,
            SystemTimestamp1: 10,
            SystemTimestamp2: 11,
        };
        let last = INTERFACE_HARDWARE_CROSSTIMESTAMP {
            HardwareClockTimestamp: 300,
            SystemTimestamp1: 50,
            SystemTimestamp2: 51,
        };
        assert_eq!(qpc_at(250, &first, &last), Some(30));
        assert_eq!(qpc_at(199, &first, &last), None);
        assert_eq!(qpc_at(301, &first, &last), None);
        assert_eq!(qpc_at(200, &first, &first), None);
        assert_eq!(qpc_at(250, &last, &first), None);
        assert_eq!(scaled(u64::MAX, 1, u64::MAX), None);
    }

    #[test]
    fn ad_1_missing_duplicate_or_truncated_winsock_metadata_cannot_prove_arrival() {
        let mut bytes = [0; space(size_of::<u64>()).saturating_mul(2)];
        let header_size = aligned(size_of::<CMSGHDR>());
        let message_size = space(size_of::<u64>());
        for frame in bytes.chunks_exact_mut(message_size) {
            let head = CMSGHDR {
                cmsg_len: header_size.saturating_add(size_of::<u64>()),
                cmsg_level: SOL_SOCKET,
                cmsg_type: i32::try_from(SO_TIMESTAMP).unwrap_or(0),
            };
            // SAFETY: each frame has space for the complete SDK header and timestamp body.
            unsafe {
                ptr::write_unaligned(frame.as_mut_ptr().cast::<CMSGHDR>(), head);
            }
            if let Some(body) = frame.get_mut(header_size..header_size.saturating_add(8)) {
                body.copy_from_slice(&123_u64.to_ne_bytes());
            }
        }
        let single = bytes.get(..message_size).and_then(controls);
        assert!(single.is_some_and(|c| c.stamp == Some(123) && c.interface.is_none()));
        assert!(controls(&bytes).is_none());
        assert!(
            bytes
                .get(..message_size.saturating_sub(1))
                .and_then(controls)
                .is_none()
        );
        assert!(controls(&[]).is_some_and(|c| c.stamp.is_none()));
    }
}

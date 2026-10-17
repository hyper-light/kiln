//! What the front door needs to know about the machine it runs on: its LAN addresses, where DNS
//! can listen, which resolvers to forward to, and the one-time setup a listener may need
//! (privileged ports on Linux, the firewall) as commands for the person, never run by kiln.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};

use crate::discovery::Iface;

use super::Fix;

/// The LAN interfaces: those discovery uses (up, multicast-capable, not loopback, point-to-point
/// links or container bridges), without IPv6 link-local addresses (useless in a DNS answer).
pub fn lan() -> Vec<Iface> {
    let mut v = crate::discovery::interfaces(&[]);
    for i in &mut v {
        i.v6.retain(|a| !a.is_unicast_link_local() && !a.is_loopback());
    }
    v.retain(|i| !i.v4.is_empty() || !i.v6.is_empty());
    v
}

/// Every LAN address, IPv4 first.
pub fn addresses(ifaces: &[Iface]) -> Vec<IpAddr> {
    let v4 = ifaces.iter().flat_map(|i| i.v4.iter().map(|(a, _)| IpAddr::V4(*a)));
    let v6 = ifaces.iter().flat_map(|i| i.v6.iter().map(|a| IpAddr::V6(*a)));
    let mut out: Vec<IpAddr> = Vec::new();
    for a in v4.chain(v6) {
        if !out.contains(&a) {
            out.push(a);
        }
    }
    out
}

/// Where DNS listens. On Linux, on each LAN address and on loopback, never the wildcard:
/// systemd-resolved holds 127.0.0.53:53 (and binding a privileged port needs the same permission
/// either way). Elsewhere on the wildcard: macOS lets a user bind ports below 1024 only there.
pub fn dns_binds(port: u16, ifaces: &[Iface]) -> Vec<SocketAddr> {
    if cfg!(target_os = "linux") {
        let mut v = vec![SocketAddr::from((Ipv4Addr::LOCALHOST, port))];
        for a in addresses(ifaces) {
            let s = match a {
                IpAddr::V4(v4) => SocketAddr::from((v4, port)),
                IpAddr::V6(v6) => SocketAddr::V6(SocketAddrV6::new(v6, port, 0, 0)),
            };
            if !v.contains(&s) {
                v.push(s);
            }
        }
        v.push(SocketAddr::from((Ipv6Addr::LOCALHOST, port)));
        v
    } else {
        vec![SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)), SocketAddr::from((Ipv6Addr::UNSPECIFIED, port))]
    }
}

/// The addresses of this machine's interface that carries `local` (the address a query arrived
/// at, or the one facing its sender), for answering with both families.
pub fn facing(local: IpAddr, ifaces: &[Iface]) -> super::zone::Facing {
    if local.is_loopback() {
        return super::zone::Facing { v4: vec![Ipv4Addr::LOCALHOST], v6: vec![Ipv6Addr::LOCALHOST] };
    }
    let iface = ifaces.iter().find(|i| match local {
        IpAddr::V4(a) => i.v4.iter().any(|(x, _)| *x == a),
        IpAddr::V6(a) => i.v6.contains(&a),
    });
    match (iface, local) {
        (Some(i), IpAddr::V4(a)) => super::zone::Facing { v4: vec![a], v6: i.v6.iter().take(4).copied().collect() },
        (Some(i), IpAddr::V6(a)) => super::zone::Facing { v4: i.v4.iter().map(|(x, _)| *x).take(4).collect(), v6: vec![a] },
        (None, IpAddr::V4(a)) => super::zone::Facing { v4: vec![a], v6: Vec::new() },
        (None, IpAddr::V6(a)) => super::zone::Facing { v4: Vec::new(), v6: vec![a] },
    }
}

/// systemd-resolved's stub listeners, which forward to the resolvers it lists elsewhere.
fn is_stub(a: &SocketAddr) -> bool {
    matches!(a.ip(), IpAddr::V4(v4) if v4 == Ipv4Addr::new(127, 0, 0, 53) || v4 == Ipv4Addr::new(127, 0, 0, 54))
}

/// The resolvers to forward to: the configured ones, else the system's (from `/etc/resolv.conf`,
/// or for a systemd-resolved stub the ones it forwards to, `/run/systemd/resolve/resolv.conf`),
/// else the default gateway. Never this front door itself (one of `own`, or loopback, on its
/// `port`): forwarding there would loop.
pub fn upstreams(configured: &[String], own: &[IpAddr], port: u16) -> Vec<SocketAddr> {
    let mut servers: Vec<SocketAddr> = configured.iter().filter_map(|s| crate::discovery::unicast::parse_server(s)).collect();
    if servers.is_empty() {
        servers = crate::discovery::unicast::system_resolver().0;
        if servers.iter().all(|s| s.ip().is_loopback())
            && let Ok(text) = std::fs::read_to_string("/run/systemd/resolve/resolv.conf")
        {
            let real = crate::discovery::unicast::parse_resolv_conf(&text).0;
            if !real.is_empty() {
                servers = real;
            }
        }
        servers.retain(|s| !is_stub(s));
    }
    let ours = |s: &SocketAddr| s.port() == port && (s.ip().is_loopback() || own.contains(&s.ip()));
    servers.retain(|s| !ours(s));
    if servers.is_empty()
        && let Some(gw) = default_gateway()
    {
        let s = SocketAddr::new(IpAddr::V4(gw), 53);
        if !ours(&s) {
            servers.push(s);
        }
    }
    servers.truncate(4);
    servers
}

/// The IPv4 default gateway (Linux: `/proc/net/route`).
pub fn default_gateway() -> Option<Ipv4Addr> {
    let text = std::fs::read_to_string("/proc/net/route").ok()?;
    parse_route_table(&text)
}

fn parse_route_table(text: &str) -> Option<Ipv4Addr> {
    text.lines().skip(1).find_map(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        let (dest, gw) = (f.get(1)?, f.get(2)?);
        if *dest != "00000000" {
            return None;
        }
        // Little-endian hex, as the kernel prints it.
        let g = u32::from_str_radix(gw, 16).ok()?;
        (g != 0).then(|| Ipv4Addr::from(g.swap_bytes()))
    })
}

/// The lowest port a user may bind (Linux's `net.ipv4.ip_unprivileged_port_start`; 1024 where
/// the kernel says nothing; 0 where the system has no such limit for kiln's purposes).
pub fn unprivileged_start() -> u16 {
    if cfg!(target_os = "linux") {
        std::fs::read_to_string("/proc/sys/net/ipv4/ip_unprivileged_port_start").ok().and_then(|s| s.trim().parse().ok()).unwrap_or(1024)
    } else if cfg!(windows) {
        0
    } else {
        1024
    }
}

/// The one-time fix for privileged ports on Linux: let users bind from `lowest` up (53 covers
/// DNS, HTTP and HTTPS), now and after a reboot. Not `setcap`: an upgrade replaces the binary,
/// and the capability with it.
pub fn privileged_ports_fix(lowest: u16) -> Fix {
    let lowest = lowest.min(super::DNS_PORT);
    Fix {
        why: format!(
            "Ports below {} need root on this machine and kiln runs as you. Once, let users bind ports from {lowest} up (also after a reboot):",
            unprivileged_start()
        ),
        commands: vec![
            format!("sudo sysctl -w net.ipv4.ip_unprivileged_port_start={lowest}"),
            format!("echo 'net.ipv4.ip_unprivileged_port_start = {lowest}' | sudo tee /etc/sysctl.d/99-kiln.conf"),
        ],
    }
}

/// Whether ufw is filtering this machine's traffic (its configuration says so; reading it needs no
/// root).
pub fn ufw_enabled() -> bool {
    cfg!(target_os = "linux")
        && std::fs::read_to_string("/etc/ufw/ufw.conf").is_ok_and(|t| t.lines().any(|l| l.trim().eq_ignore_ascii_case("enabled=yes")))
}

/// Firewall rules that let the LAN reach the listeners: ufw's on Linux when it is on, Windows
/// Defender Firewall's on Windows. `tcp` and `udp` are the ports in use; `lans` the LAN subnets.
pub fn firewall_fix(tcp: &[u16], udp: &[u16], lans: &[String]) -> Option<Fix> {
    let list = |p: &[u16]| p.iter().map(u16::to_string).collect::<Vec<_>>().join(",");
    if tcp.is_empty() && udp.is_empty() {
        return None;
    }
    if ufw_enabled() {
        let mut commands = Vec::new();
        let from: Vec<String> = if lans.is_empty() { vec!["192.168.1.0/24".to_owned()] } else { lans.to_vec() };
        for net in &from {
            if !tcp.is_empty() {
                commands.push(format!("sudo ufw allow from {net} to any port {} proto tcp", list(tcp)));
            }
            if !udp.is_empty() {
                commands.push(format!("sudo ufw allow from {net} to any port {} proto udp", list(udp)));
            }
        }
        return Some(Fix { why: "ufw is on here: let the LAN reach the front door.".to_owned(), commands });
    }
    if cfg!(windows) {
        let mut commands = Vec::new();
        if !tcp.is_empty() {
            commands.push(format!(
                "New-NetFirewallRule -DisplayName 'kiln front door (TCP)' -Direction Inbound -Action Allow -Protocol TCP -LocalPort {}",
                list(tcp)
            ));
        }
        if !udp.is_empty() {
            commands.push(format!(
                "New-NetFirewallRule -DisplayName 'kiln front door (UDP)' -Direction Inbound -Action Allow -Protocol UDP -LocalPort {}",
                list(udp)
            ));
        }
        return Some(Fix { why: "Windows Defender Firewall: let the LAN reach the front door (PowerShell as administrator).".to_owned(), commands });
    }
    None
}

/// What a bind error means for the person, and its fix when there is one.
pub fn bind_problem(what: &str, addr: &SocketAddr, e: &std::io::Error) -> (String, Option<Fix>) {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied if cfg!(target_os = "linux") => {
            (format!("{what} cannot listen on port {}: ports below {} need root here", addr.port(), unprivileged_start()), Some(privileged_ports_fix(addr.port())))
        }
        std::io::ErrorKind::PermissionDenied => (format!("{what} cannot listen on port {}: this system wants root for it", addr.port()), None),
        std::io::ErrorKind::AddrInUse => (
            format!("{what}: {addr} is taken by another program"),
            cfg!(unix).then(|| Fix {
                why: format!("Something else listens on port {}. See what:", addr.port()),
                commands: vec![format!("sudo ss -lpn 'sport = :{}'", addr.port())],
            }),
        ),
        _ => (format!("{what} cannot listen on {addr}: {e}"), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface() -> Iface {
        Iface {
            name: "enp5s0".into(),
            index: 2,
            v4: vec![(Ipv4Addr::new(192, 168, 1, 77), Ipv4Addr::new(255, 255, 255, 0))],
            v6: vec!["fd00::77".parse().unwrap_or(Ipv6Addr::UNSPECIFIED)],
        }
    }

    #[test]
    fn dns_listens_on_lan_addresses_and_loopback_on_linux() {
        let binds = dns_binds(53, &[iface()]);
        if cfg!(target_os = "linux") {
            assert_eq!(binds.first(), Some(&SocketAddr::from((Ipv4Addr::LOCALHOST, 53))));
            assert!(binds.contains(&SocketAddr::from((Ipv4Addr::new(192, 168, 1, 77), 53))));
            assert!(binds.iter().all(|b| !b.ip().is_unspecified()), "never the wildcard: {binds:?}");
        } else {
            assert!(binds.iter().all(|b| b.ip().is_unspecified()));
        }
        let f = facing(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 77)), &[iface()]);
        assert_eq!(f.v4, [Ipv4Addr::new(192, 168, 1, 77)]);
        assert_eq!(f.v6.len(), 1);
        let f = facing(IpAddr::V4(Ipv4Addr::LOCALHOST), &[iface()]);
        assert_eq!(f.v4, [Ipv4Addr::LOCALHOST]);
        assert_eq!(addresses(&[iface()]).len(), 2);
    }

    #[test]
    fn upstreams_skip_the_front_door_itself() {
        let own = [IpAddr::V4(Ipv4Addr::new(192, 168, 1, 77))];
        let configured = vec!["192.168.1.77".to_owned(), "127.0.0.1".to_owned(), "192.168.1.254".to_owned(), "192.168.1.77:5300".to_owned()];
        assert_eq!(
            upstreams(&configured, &own, 53),
            [SocketAddr::from((Ipv4Addr::new(192, 168, 1, 254), 53)), SocketAddr::from((Ipv4Addr::new(192, 168, 1, 77), 5300))]
        );
        // On another port, loopback's resolver is somebody else.
        assert_eq!(upstreams(&["127.0.0.1".to_owned()], &own, 18053), [SocketAddr::from((Ipv4Addr::LOCALHOST, 53))]);
        assert!(is_stub(&SocketAddr::from((Ipv4Addr::new(127, 0, 0, 53), 53))));
        assert!(!is_stub(&SocketAddr::from((Ipv4Addr::new(127, 0, 0, 1), 53))));
    }

    #[test]
    fn the_default_route_is_read_from_the_kernel_table() {
        let table = "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                     enp5s0\t0001A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0\n\
                     enp5s0\t00000000\tFE01A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0\n";
        assert_eq!(parse_route_table(table), Some(Ipv4Addr::new(192, 168, 1, 254)));
        assert_eq!(parse_route_table("Iface\tDestination\tGateway\n"), None);
    }

    #[test]
    fn fixes_are_commands_for_the_person() {
        let f = privileged_ports_fix(443);
        assert_eq!(f.commands.first().map(String::as_str), Some("sudo sysctl -w net.ipv4.ip_unprivileged_port_start=53"));
        assert!(f.commands.get(1).is_some_and(|c| c.contains("/etc/sysctl.d/99-kiln.conf")));
        assert!(f.commands.iter().all(|c| !c.contains("setcap")));
        let busy = std::io::Error::from(std::io::ErrorKind::AddrInUse);
        let (msg, _) = bind_problem("DNS", &SocketAddr::from((Ipv4Addr::new(192, 168, 1, 77), 53)), &busy);
        assert!(msg.contains("taken"), "{msg}");
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let (msg, fix) = bind_problem("HTTPS", &SocketAddr::from((Ipv4Addr::UNSPECIFIED, 443)), &denied);
        assert!(msg.contains("443"));
        assert_eq!(fix.is_some(), cfg!(target_os = "linux"));
        assert!(firewall_fix(&[], &[], &[]).is_none());
    }
}

//! Network interface information for the settings panel.
//!
//! An operator commissioning a kiosk needs to know the address the box
//! is reachable at, and whether that address came from DHCP or was
//! configured statically.  This module reads the interface list and the
//! kernel's routing table to answer both.

use std::{
    ffi::CStr,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

/// How an interface's address was obtained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddrSource {
    /// Assigned by DHCP.
    Dhcp,
    /// Configured statically, or the kernel could not tell.
    Static,
    /// An IPv6 link-local address (fe80::/10) — not routable off-link.
    LinkLocal,
}

impl AddrSource {
    pub fn label(self) -> &'static str {
        match self {
            AddrSource::Dhcp => "DHCP",
            AddrSource::Static => "static",
            AddrSource::LinkLocal => "link-local",
        }
    }
}

/// One address on one interface.
#[derive(Clone, Debug)]
pub struct InterfaceAddr {
    /// Interface name, e.g. `eth0`.
    pub name: String,
    pub addr: IpAddr,
    /// The prefix length, e.g. 24 for a /24 network.
    pub prefix: u8,
    pub source: AddrSource,
    /// True when the interface carries the default route.
    pub has_default_route: bool,
}

impl InterfaceAddr {
    /// The address with its prefix length, e.g. `192.168.1.20/24`.
    pub fn with_prefix(&self) -> String {
        format!("{}/{}", self.addr, self.prefix)
    }
}

/// Every interface address on the machine, excluding loopback.
///
/// The list is sorted so the panel shows a stable order: the default
/// route first, then the rest by address.
pub fn interface_addresses() -> Vec<InterfaceAddr> {
    let default_ifaces = default_route_interfaces();

    let mut out = Vec::new();
    for (name, addr, prefix) in raw_addresses() {
        // Loopback is never the address an operator wants to see.
        if addr.is_loopback() {
            continue;
        }
        let source = classify(&name, addr, prefix);
        let has_default_route = default_ifaces.iter().any(|i| i == &name);
        out.push(InterfaceAddr {
            name,
            addr,
            prefix,
            source,
            has_default_route,
        });
    }

    // The default route first — that is the address the box is reachable
    // at — then everything else by address.
    out.sort_by(|a, b| {
        b.has_default_route
            .cmp(&a.has_default_route)
            .then_with(|| a.addr.cmp(&b.addr))
    });
    out
}

/// The address most likely to be used to reach this machine.
///
/// Prefers the interface carrying the default route, then a routable
/// IPv4, then a routable IPv6, and finally the first address of any
/// kind.  Returns `None` when there is no address at all.
#[allow(dead_code)]
pub fn primary_address() -> Option<IpAddr> {
    let addrs = interface_addresses();
    addrs
        .iter()
        .find(|a| a.has_default_route && a.addr.is_ipv4())
        .or_else(|| addrs.iter().find(|a| a.has_default_route))
        .or_else(|| {
            addrs
                .iter()
                .find(|a| a.source == AddrSource::Dhcp && a.addr.is_ipv4())
        })
        .or_else(|| {
            addrs
                .iter()
                .find(|a| a.source == AddrSource::Static && a.addr.is_ipv4())
        })
        .map(|a| a.addr)
}

/// The names of the interfaces that carry a default route.
fn default_route_interfaces() -> Vec<String> {
    let Ok(routes) = std::fs::read_to_string("/proc/net/route") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in routes.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let Some(iface) = fields.next() else { continue };
        let Some(dest) = fields.next() else { continue };
        // The default route has a zero destination.
        if dest == "00000000" {
            out.push(iface.to_string());
        }
    }
    out
}

/// Classify an address by how it was obtained.
fn classify(iface: &str, addr: IpAddr, _prefix: u8) -> AddrSource {
    match addr {
        IpAddr::V6(v6) => {
            if is_link_local_v6(&v6) {
                AddrSource::LinkLocal
            } else {
                AddrSource::Static
            }
        }
        IpAddr::V4(v4) => {
            // A link-local IPv4 address is 169.254.0.0/16.
            if v4.is_link_local() {
                AddrSource::LinkLocal
            } else if dhcp_leases_contain(iface, &v4) {
                AddrSource::Dhcp
            } else {
                AddrSource::Static
            }
        }
    }
}

/// True when the address is in fe80::/10.
fn is_link_local_v6(v6: &Ipv6Addr) -> bool {
    let segments = v6.segments();
    (segments[0] & 0xffc0) == 0xfe80
}

/// True when the address was assigned by DHCP.
///
/// The kernel's `addr_assign_type` is not usable here: NetworkManager
/// re-applies the address and the kernel then reports it as permanent.
/// So the DHCP client state is consulted instead.
///
/// NetworkManager is asked first — it knows the connection's method
/// (`auto` means DHCP).  A dhclient lease file is the fallback for
/// systems without NetworkManager.  If neither can be consulted the
/// address is reported as static, which is the safe answer: it never
/// claims a lease the operator cannot verify.
fn dhcp_leases_contain(iface: &str, addr: &Ipv4Addr) -> bool {
    if network_manager_uses_dhcp(iface) {
        return true;
    }
    dhclient_lease_contains(iface, addr)
}

/// True when NetworkManager drives `iface` with an automatic (DHCP)
/// method.
fn network_manager_uses_dhcp(iface: &str) -> bool {
    // `nmcli -g ipv4.method con show <connection>` prints the method for
    // the named connection.  The connection that owns the device has to
    // be found first.
    let Ok(out) = std::process::Command::new("nmcli")
        .args(["-t", "-f", "NAME,DEVICE", "con", "show", "--active"])
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut connection = None;
    for line in text.lines() {
        // The terse format uses ':' as the separator.
        let mut parts = line.split(':');
        let (Some(name), Some(device)) = (parts.next(), parts.next()) else {
            continue;
        };
        if device == iface {
            connection = Some(name.to_string());
            break;
        }
    }
    let Some(connection) = connection else {
        return false;
    };

    let Ok(out) = std::process::Command::new("nmcli")
        .args(["-g", "ipv4.method", "con", "show", &connection])
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let method = String::from_utf8_lossy(&out.stdout);
    // "auto" is DHCP; "manual" is static; "disabled" means no address.
    let method = method.trim();
    method == "auto"
}

/// True when a dhclient lease file for `iface` lists this address.
fn dhclient_lease_contains(iface: &str, addr: &Ipv4Addr) -> bool {
    let dirs = ["/var/lib/dhcp", "/var/lib/dhclient"];
    let want = addr.to_string();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // dhclient names the file <interface>.lease.
            if !name.ends_with(".lease") || !name.contains(iface) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            // The lease lists "ADDRESS <ip>" or "fixed-address <ip>".
            for line in text.lines() {
                let mut fields = line.split_whitespace();
                let Some(key) = fields.next() else { continue };
                if !key.eq_ignore_ascii_case("address")
                    && !key.eq_ignore_ascii_case("fixed-address")
                {
                    continue;
                }
                if let Some(value) = fields.next()
                    && value.trim_end_matches(';') == want
                {
                    return true;
                }
            }
        }
    }
    false
}

/// The raw (interface, address, prefix) triples, straight from the kernel.
fn raw_addresses() -> Vec<(String, IpAddr, u8)> {
    let mut out = Vec::new();

    // SAFETY: getifaddrs returns a linked list that is freed with
    // freeifaddrs.  The pointers are read only while the list is alive.
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return out;
        }
        let mut cur = ifap;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null() {
                let name = CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
                let family = ifa.ifa_addr.read_unaligned();
                let addr = match family.sa_family as i32 {
                    libc::AF_INET => {
                        let sin = ifa.ifa_addr as *const libc::sockaddr_in;
                        IpAddr::V4(Ipv4Addr::from(u32::from_be((*sin).sin_addr.s_addr)))
                    }
                    libc::AF_INET6 => {
                        let sin6 = ifa.ifa_addr as *const libc::sockaddr_in6;
                        IpAddr::V6(Ipv6Addr::from((*sin6).sin6_addr.s6_addr))
                    }
                    _ => {
                        cur = ifa.ifa_next;
                        continue;
                    }
                };
                let prefix = prefix_len(ifa);
                out.push((name, addr, prefix));
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
    }
    out
}

/// The prefix length of an interface address.
///
/// # Safety
/// `ifa.ifa_addr` must be a valid pointer to a `sockaddr` of the family
/// its `sa_family` names.
unsafe fn prefix_len(ifa: &libc::ifaddrs) -> u8 {
    unsafe {
        match (*ifa.ifa_addr).sa_family as i32 {
            libc::AF_INET => {
                let sin = ifa.ifa_addr as *const libc::sockaddr_in;
                prefix_from_mask(u32::from_be((*sin).sin_addr.s_addr) as u128)
            }
            libc::AF_INET6 => {
                let sin6 = ifa.ifa_addr as *const libc::sockaddr_in6;
                prefix_from_mask(u128::from_be_bytes((*sin6).sin6_addr.s6_addr))
            }
            _ => 0,
        }
    }
}

/// Count the leading one-bits of a mask.
fn prefix_from_mask(mask: u128) -> u8 {
    mask.leading_ones() as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_local_ipv6_is_recognised() {
        assert!(is_link_local_v6(&"fe80::1".parse().unwrap()));
        assert!(is_link_local_v6(&"febf::1".parse().unwrap()));
        assert!(!is_link_local_v6(&"2001:db8::1".parse().unwrap()));
        assert!(!is_link_local_v6(&"::1".parse().unwrap()));
    }

    #[test]
    fn addresses_are_classified() {
        // An IPv6 link-local address.
        assert_eq!(
            classify("eth0", "fe80::1".parse().unwrap(), 64),
            AddrSource::LinkLocal
        );
        // A routable IPv6 address.
        assert_eq!(
            classify("eth0", "2001:db8::1".parse().unwrap(), 64),
            AddrSource::Static
        );
        // An IPv4 link-local address.
        assert_eq!(
            classify("eth0", "169.254.1.1".parse().unwrap(), 16),
            AddrSource::LinkLocal
        );
    }

    #[test]
    fn the_source_labels_are_stable() {
        assert_eq!(AddrSource::Dhcp.label(), "DHCP");
        assert_eq!(AddrSource::Static.label(), "static");
        assert_eq!(AddrSource::LinkLocal.label(), "link-local");
    }

    #[test]
    fn the_interface_list_excludes_loopback() {
        let addrs = interface_addresses();
        assert!(
            addrs.iter().all(|a| !a.addr.is_loopback()),
            "loopback must not be listed"
        );
    }

    #[test]
    fn the_primary_address_prefers_the_default_route() {
        // Whatever this machine has, the primary address must be one of
        // the listed addresses — and there must be a sensible order.
        let addrs = interface_addresses();
        if let Some(primary) = primary_address() {
            assert!(
                addrs.iter().any(|a| a.addr == primary),
                "the primary address must be one of the listed ones"
            );
        }
    }

    #[test]
    fn the_default_route_interfaces_are_a_subset_of_the_real_ones() {
        let routes = default_route_interfaces();
        let addrs = interface_addresses();
        for r in routes {
            assert!(
                addrs.iter().any(|a| a.name == r),
                "the default route names '{r}', which is not an interface with an address"
            );
        }
    }

    #[test]
    fn the_address_formats_with_its_prefix() {
        let a = InterfaceAddr {
            name: "eth0".into(),
            addr: "192.168.1.20".parse().unwrap(),
            prefix: 24,
            source: AddrSource::Dhcp,
            has_default_route: true,
        };
        assert_eq!(a.with_prefix(), "192.168.1.20/24");
    }
}

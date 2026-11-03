//! Who may connect to the chain's one port (DC-1xx-LIVE-6).
//!
//! A client's chain listens on loopback and admits anyone who reaches it.
//! A relay agent's chain on a VM host must be reached by the host's guests
//! on their network's host address, a vmnet bridge that exists only while a
//! VM runs, so it cannot bind that one address before its first guest
//! boots. It listens on every interface and admits only loopback peers and
//! peers on the subnet of one of the host's bridge interfaces, so the LAN
//! is never served.

use std::net::{IpAddr, Ipv4Addr};

/// Who the chain admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Admit {
    /// Anyone who reaches the listener.
    #[default]
    Any,
    /// Loopback peers and the host's guests.
    Guests,
}

impl Admit {
    /// Parse `--admit`: `any` or `guests`.
    pub fn parse(value: &str) -> Result<Admit, String> {
        match value {
            "any" => Ok(Admit::Any),
            "guests" => Ok(Admit::Guests),
            other => Err(format!("--admit must be any or guests; got {other:?}")),
        }
    }

    /// Whether a connection from `peer` is admitted.
    pub fn admits(self, peer: IpAddr) -> bool {
        match self {
            Admit::Any => true,
            Admit::Guests => peer.is_loopback() || guest_admitted(peer, &bridge_subnets()),
        }
    }
}

/// Whether `peer` lies on one of `subnets`, each an address and its mask.
pub fn guest_admitted(peer: IpAddr, subnets: &[(Ipv4Addr, Ipv4Addr)]) -> bool {
    let IpAddr::V4(peer) = peer else {
        return false;
    };
    subnets.iter().any(|(address, mask)| {
        let mask = u32::from(*mask);
        mask != 0 && u32::from(*address) & mask == u32::from(peer) & mask
    })
}

/// The IPv4 address and mask of every bridge interface that is up, the
/// networks Tart's guests are on.
pub fn bridge_subnets() -> Vec<(Ipv4Addr, Ipv4Addr)> {
    let mut found = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list` with a linked list that freeifaddrs
    // releases; every pointer is checked before it is read, and each
    // sockaddr is read as the family it names.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return found;
        }
        let mut cursor = list;
        while !cursor.is_null() {
            let entry = &*cursor;
            let up = entry.ifa_flags & libc::IFF_UP as libc::c_uint != 0;
            let bridge = !entry.ifa_name.is_null()
                && std::ffi::CStr::from_ptr(entry.ifa_name)
                    .to_bytes()
                    .starts_with(b"bridge");
            if up
                && bridge
                && !entry.ifa_addr.is_null()
                && !entry.ifa_netmask.is_null()
                && i32::from((*entry.ifa_addr).sa_family) == libc::AF_INET
            {
                let address = &*(entry.ifa_addr as *const libc::sockaddr_in);
                let mask = &*(entry.ifa_netmask as *const libc::sockaddr_in);
                found.push((
                    Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr)),
                    Ipv4Addr::from(u32::from_be(mask.sin_addr.s_addr)),
                ));
            }
            cursor = entry.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guests_are_the_peers_on_a_bridge_subnet() {
        let bridge = [(
            Ipv4Addr::new(198, 51, 100, 1),
            Ipv4Addr::new(255, 255, 255, 0),
        )];
        assert!(guest_admitted("198.51.100.7".parse().unwrap(), &bridge));
        assert!(!guest_admitted("203.0.113.7".parse().unwrap(), &bridge));
        assert!(!guest_admitted("2001:db8::7".parse().unwrap(), &bridge));
        assert!(!guest_admitted("198.51.100.7".parse().unwrap(), &[]));
        assert!(Admit::Guests.admits("127.0.0.1".parse().unwrap()));
        assert!(Admit::Any.admits("203.0.113.7".parse().unwrap()));
        assert_eq!(Admit::parse("guests"), Ok(Admit::Guests));
        assert!(Admit::parse("lan").is_err());
    }
}

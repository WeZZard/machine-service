//! The host's own names and addresses, read at run time (no address is ever
//! written into the code).

use std::ffi::CStr;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// An IPv4 address of one of this host's interfaces, with its netmask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interface4 {
    /// The address.
    pub address: Ipv4Addr,
    /// The netmask.
    pub netmask: Ipv4Addr,
}

impl Interface4 {
    /// Whether `other` is on this interface's subnet.
    pub fn contains(&self, other: Ipv4Addr) -> bool {
        let mask = u32::from(self.netmask);
        mask != 0 && u32::from(self.address) & mask == u32::from(other) & mask
    }
}

fn interfaces() -> (Vec<Interface4>, Vec<Ipv6Addr>) {
    let mut v4 = Vec::new();
    let mut v6 = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `getifaddrs` fills `list` with a linked list that stays valid
    // until `freeifaddrs`; every pointer is checked before it is read, and
    // each `sockaddr` is read as the type its family names.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return (v4, v6);
        }
        let mut cursor = list;
        while !cursor.is_null() {
            let entry = &*cursor;
            let up = entry.ifa_flags & libc::IFF_UP as libc::c_uint != 0;
            if up && !entry.ifa_addr.is_null() {
                match i32::from((*entry.ifa_addr).sa_family) {
                    libc::AF_INET => {
                        let address = &*(entry.ifa_addr as *const libc::sockaddr_in);
                        let netmask = if entry.ifa_netmask.is_null() {
                            Ipv4Addr::UNSPECIFIED
                        } else {
                            let mask = &*(entry.ifa_netmask as *const libc::sockaddr_in);
                            Ipv4Addr::from(u32::from_be(mask.sin_addr.s_addr))
                        };
                        v4.push(Interface4 {
                            address: Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr)),
                            netmask,
                        });
                    }
                    libc::AF_INET6 => {
                        let address = &*(entry.ifa_addr as *const libc::sockaddr_in6);
                        v6.push(Ipv6Addr::from(address.sin6_addr.s6_addr));
                    }
                    _ => {}
                }
            }
            cursor = entry.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    (v4, v6)
}

/// This host's IPv4 interfaces that are up.
pub fn interfaces4() -> Vec<Interface4> {
    interfaces().0
}

/// The addresses of this host's interfaces that are up, loopback included,
/// without IPv6 link-local addresses, which need a scope to be dialled.
pub fn local_addresses() -> Vec<IpAddr> {
    let (v4, v6) = interfaces();
    v4.into_iter()
        .map(|i| IpAddr::V4(i.address))
        .chain(
            v6.into_iter()
                .filter(|a| a.segments()[0] & 0xffc0 != 0xfe80)
                .map(IpAddr::V6),
        )
        .collect()
}

/// This host's address on the subnet of `guest`: the address a virtual
/// machine on a host-local network reaches its host by (BK-5).
pub fn address_toward(guest: Ipv4Addr) -> Option<Ipv4Addr> {
    interfaces4()
        .into_iter()
        .filter(|i| !i.address.is_loopback() && i.address != guest)
        .find(|i| i.contains(guest))
        .map(|i| i.address)
}

/// This host's name, as the system reports it.
pub fn host_name() -> Option<String> {
    let mut buffer = [0 as libc::c_char; 256];
    // SAFETY: the buffer is writable for its length, and `gethostname`
    // terminates the name within it on success; the last byte stays zero.
    let ok = unsafe { libc::gethostname(buffer.as_mut_ptr(), buffer.len() - 1) } == 0;
    if !ok {
        return None;
    }
    // SAFETY: the buffer is zero-terminated, as above.
    let name = unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_string_lossy()
        .trim()
        .to_string();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_is_listed_and_found_by_subnet() {
        let addresses = local_addresses();
        assert!(
            addresses.contains(&IpAddr::V4(Ipv4Addr::LOCALHOST)),
            "{addresses:?}"
        );
        let loopback = interfaces4()
            .into_iter()
            .find(|i| i.address.is_loopback())
            .expect("a loopback interface");
        assert!(loopback.contains(Ipv4Addr::new(127, 0, 0, 2)));
        assert!(!loopback.contains(Ipv4Addr::new(198, 51, 100, 1)));
        // Loopback itself is never the answer for a guest.
        assert_eq!(address_toward(Ipv4Addr::new(127, 0, 0, 2)), None);
        assert!(host_name().is_some());
    }

    #[test]
    fn a_zero_netmask_contains_nothing() {
        let i = Interface4 {
            address: Ipv4Addr::new(10, 0, 0, 1),
            netmask: Ipv4Addr::UNSPECIFIED,
        };
        assert!(!i.contains(Ipv4Addr::new(10, 0, 0, 2)));
        let i = Interface4 {
            address: Ipv4Addr::new(10, 1, 2, 1),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
        };
        assert!(i.contains(Ipv4Addr::new(10, 1, 2, 77)));
        assert!(!i.contains(Ipv4Addr::new(10, 1, 3, 77)));
    }
}

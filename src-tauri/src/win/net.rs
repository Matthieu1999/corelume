//! Byte counters of the physical network adapters that are up. Virtual adapters (VMware,
//! WSL/Hyper-V, WAN miniports) and filter layers are skipped: they repeat physical traffic.

use serde::Serialize;
use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, GetAdaptersAddresses,
    GetIfTable2, IP_ADAPTER_ADDRESSES_LH, MIB_IF_TABLE2,
};
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6, AF_UNSPEC};

/// Interface LUID → (name, bytes received, bytes sent) since boot.
pub fn counters() -> HashMap<u64, (String, u64, u64)> {
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    if unsafe { GetIfTable2(&mut table) } != 0 || table.is_null() {
        return HashMap::new();
    }
    let rows = unsafe { std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize) };
    let out = rows
        .iter()
        .filter(|r| {
            let flags = r.InterfaceAndOperStatusFlags._bitfield;
            let hardware = flags & 1 != 0;
            let filter = flags & 2 != 0;
            hardware && !filter && r.OperStatus == IfOperStatusUp
        })
        .map(|r| (unsafe { r.InterfaceLuid.Value }, (wide(&r.Alias), r.InOctets, r.OutOctets)))
        .collect();
    unsafe { FreeMibTable(table.cast()) };
    out
}

/// Adapter details for the Network page, keyed by the same name as `counters`.
#[derive(Serialize, Clone)]
pub struct Details {
    pub name: String,
    pub description: String,
    pub mac: String,
    pub link_bps: u64,
    pub ips: Vec<String>,
}

/// Details of the given adapters (LUIDs from `counters`).
pub fn details(luids: &[u64]) -> Vec<Details> {
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
    let mut len = 16 * 1024u32;
    let mut buf: Vec<u64> = Vec::new(); // u64 for the struct's alignment
    for _ in 0..3 {
        buf.resize((len as usize).div_ceil(8), 0);
        let rc = unsafe { GetAdaptersAddresses(AF_UNSPEC as u32, flags, std::ptr::null(), buf.as_mut_ptr().cast(), &mut len) };
        if rc == 0 {
            break;
        }
        if rc != 111 {
            // anything but ERROR_BUFFER_OVERFLOW
            return Vec::new();
        }
    }
    let mut out = Vec::new();
    let mut node = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    while !node.is_null() {
        let a = unsafe { &*node };
        if luids.contains(&unsafe { a.Luid.Value }) {
            let mut ips = Vec::new();
            let mut ip = a.FirstUnicastAddress;
            while !ip.is_null() {
                let sa = unsafe { (*ip).Address.lpSockaddr };
                if let Some(s) = sockaddr(sa.cast()) {
                    ips.push(s);
                }
                ip = unsafe { (*ip).Next };
            }
            out.push(Details {
                name: unsafe { pwstr(a.FriendlyName) },
                description: unsafe { pwstr(a.Description) },
                mac: mac(&a.PhysicalAddress[..(a.PhysicalAddressLength as usize).min(8)]),
                link_bps: a.ReceiveLinkSpeed.max(a.TransmitLinkSpeed),
                ips,
            });
        }
        node = a.Next;
    }
    out
}

/// IPv4 or global IPv6 text; link-local IPv6 (fe80::) is noise for a dashboard.
fn sockaddr(sa: *const u8) -> Option<String> {
    if sa.is_null() {
        return None;
    }
    let family = unsafe { (sa as *const u16).read_unaligned() };
    let bytes = |at: usize, n: usize| unsafe { std::slice::from_raw_parts(sa.add(at), n) }.to_vec();
    match family {
        f if f == AF_INET => Some(Ipv4Addr::from(<[u8; 4]>::try_from(bytes(4, 4)).ok()?).to_string()),
        f if f == AF_INET6 => {
            let ip = Ipv6Addr::from(<[u8; 16]>::try_from(bytes(8, 16)).ok()?);
            (!ip.is_unicast_link_local()).then(|| ip.to_string())
        }
        _ => None,
    }
}

fn mac(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join("-")
}

fn wide(units: &[u16]) -> String {
    String::from_utf16_lossy(&units[..units.iter().position(|&c| c == 0).unwrap_or(units.len())])
}

unsafe fn pwstr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let len = (0..).take_while(|&i| unsafe { *p.add(i) } != 0).count();
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_addresses() {
        assert_eq!(mac(&[0x00, 0x1A, 0x2B, 0x3C, 0x4D, 0x5E]), "00-1A-2B-3C-4D-5E");
        assert_eq!(wide(&[b'W' as u16, b'i' as u16, 0, b'x' as u16]), "Wi");
        // sockaddr_in: family 2, port, then 192.168.1.20.
        let v4 = [2u8, 0, 0, 0, 192, 168, 1, 20, 0, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(sockaddr(v4.as_ptr()).as_deref(), Some("192.168.1.20"));
        // sockaddr_in6 with fe80::1 is skipped.
        let mut v6 = [0u8; 28];
        v6[0] = 23;
        v6[8] = 0xfe;
        v6[9] = 0x80;
        v6[23] = 1;
        assert_eq!(sockaddr(v6.as_ptr()), None);
    }
}

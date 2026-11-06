//! Cómo se llama este equipo y dónde se le encuentra en la red local, para
//! que la tablet lo muestre y el usuario no tenga que buscar su IP.

use std::collections::hash_map::DefaultHasher;
use std::ffi::CStr;
use std::hash::{Hash, Hasher};
use std::net::Ipv4Addr;
use std::sync::OnceLock;

/// Nombres de interfaces que no llevan a la red de la tablet: contenedores,
/// máquinas virtuales y VPN.
const NOT_LAN: [&str; 9] = ["lo", "docker", "br-", "veth", "virbr", "vmnet", "tun", "wg", "CloudflareWARP"];

fn is_lan(interface: &str, ip: Ipv4Addr) -> bool {
    ip.is_private() && !NOT_LAN.iter().any(|p| interface.starts_with(p))
}

/// Direcciones IPv4 de la red local de este equipo.
pub fn addresses() -> Vec<Ipv4Addr> {
    let mut out = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs rellena `list` si devuelve 0, y se libera una vez al
    // terminar; los punteros de cada nodo viven hasta entonces.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut node = list;
        while let Some(ifa) = node.as_ref() {
            node = ifa.ifa_next;
            let up = ifa.ifa_flags & libc::IFF_UP as u32 != 0;
            if !up || ifa.ifa_addr.is_null() || (*ifa.ifa_addr).sa_family != libc::AF_INET as u16 {
                continue;
            }
            let addr = &*(ifa.ifa_addr as *const libc::sockaddr_in);
            let ip = Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
            if is_lan(&CStr::from_ptr(ifa.ifa_name).to_string_lossy(), ip) {
                out.push(ip);
            }
        }
        libc::freeifaddrs(list);
    }
    out
}

/// Nombre del equipo, el que ve el usuario en la tablet.
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname escribe como mucho `buf.len()` bytes en `buf`.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let name = if ok { CStr::from_bytes_until_nul(&buf).ok() } else { None };
    name.map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "PC".into())
}

/// Identifica a este equipo aunque cambie su IP, y lo reconoce cuando se le
/// encuentra por dos redes (cable y WiFi). Es un resumen: el machine-id no
/// debe salir del equipo.
pub fn server_id() -> String {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let machine = std::fs::read_to_string("/etc/machine-id").unwrap_or_else(|_| hostname());
        let mut hash = DefaultHasher::new();
        machine.trim().hash(&mut hash);
        format!("{:016x}", hash.finish())
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_the_local_network() {
        let home = Ipv4Addr::new(192, 168, 0, 178);
        assert!(is_lan("eno1", home));
        assert!(is_lan("wlp4s0", home));
        assert!(!is_lan("docker0", Ipv4Addr::new(172, 17, 0, 1)));
        assert!(!is_lan("CloudflareWARP", Ipv4Addr::new(172, 16, 0, 2)));
        assert!(!is_lan("eno1", Ipv4Addr::new(8, 8, 8, 8)));
    }
}

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

/// Las mismas, con delante la que lleva a la puerta de enlace: es la red de
/// casa, la que casi seguro comparte la tablet.
pub fn addresses_best_first() -> Vec<Ipv4Addr> {
    let mut all = addresses();
    if let Some(main) = route_ip() {
        if let Some(i) = all.iter().position(|ip| *ip == main) {
            let ip = all.remove(i);
            all.insert(0, ip);
        }
    }
    all
}

/// La IP propia por la que sale el tráfico hacia la puerta de enlace. Se
/// pregunta por la puerta de enlace y no por internet porque con una VPN
/// (Cloudflare WARP) internet sale por el túnel.
fn route_ip() -> Option<Ipv4Addr> {
    let routes = std::fs::read_to_string("/proc/net/route").ok()?;
    let gateway = default_gateway(&routes)?;
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect((gateway, 9)).ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) => Some(ip),
        _ => None,
    }
}

/// La puerta de enlace de la ruta por defecto con menor métrica.
fn default_gateway(routes: &str) -> Option<Ipv4Addr> {
    routes
        .lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            let (iface, destination, gateway, metric) = (*f.first()?, *f.get(1)?, *f.get(2)?, *f.get(6)?);
            if destination != "00000000" || gateway == "00000000" || NOT_LAN.iter().any(|p| iface.starts_with(p)) {
                return None;
            }
            let ip = u32::from_str_radix(gateway, 16).ok()?;
            Some((metric.parse::<u32>().ok()?, Ipv4Addr::from(ip.to_le_bytes())))
        })
        .min_by_key(|(metric, _)| *metric)
        .map(|(_, ip)| ip)
}

/// Lo que lleva el QR de la ventana. Es una dirección web normal, así que:
/// * la app de Encaje la reconoce por `#encaje` y se conecta con su visor,
///   probando también las otras IP del PC (cable y WiFi);
/// * la cámara de cualquier celular la abre en el navegador, en el visor web.
pub fn qr_link(port: u16) -> Option<String> {
    let ips = addresses_best_first();
    let first = ips.first()?;
    let mut link = format!("http://{first}:{port}/#encaje&id={}&n={}", server_id(), encode(&hostname()));
    if ips.len() > 1 {
        let rest: Vec<String> = ips[1..].iter().map(|ip| ip.to_string()).collect();
        link.push_str(&format!("&h={}", rest.join(",")));
    }
    Some(link)
}

fn encode(v: &str) -> String {
    v.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
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

    #[test]
    fn gateway_skips_the_vpn() {
        let routes = "Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\n\
            CloudflareWARP\t00000000\t020010AC\t0003\t0\t0\t0\t00000000\n\
            wlp4s0\t00000000\t0100A8C0\t0003\t0\t0\t600\t00000000\n";
        assert_eq!(default_gateway(routes), Some(Ipv4Addr::new(192, 168, 0, 1)));
    }

    #[test]
    fn names_go_escaped_in_the_link() {
        assert_eq!(encode("PC de Ana"), "PC%20de%20Ana");
    }
}

//! Wake-on-LAN: remembers each computer's network card address while it's
//! online, and broadcasts the "magic packet" that wakes it from sleep. Works on
//! the local network over a cable; Wi-Fi cards usually don't listen while asleep.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};

pub type Mac = [u8; 6];

/// The computer this session window wakes while it can't reach it.
static TARGET: std::sync::OnceLock<(Mac, Option<Ipv4Addr>)> = std::sync::OnceLock::new();

pub fn set_target(mac: Option<&str>, addr: &str) {
    if let Some(mac) = mac.and_then(parse_mac) {
        let _ = TARGET.set((mac, ipv4_of(addr)));
    }
}

pub fn has_target() -> bool {
    TARGET.get().is_some()
}

/// Called before each connection attempt. A Mac woken by the network may only
/// stay up for about 45 seconds, so the packet is repeated until a connection
/// succeeds; the agent then keeps the computer awake.
pub fn nudge() {
    if let Some((mac, ip)) = TARGET.get() {
        let _ = wake(mac, *ip);
    }
}

pub fn format_mac(mac: &Mac) -> String {
    mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// Accepts `a4:83:e7:01:02:03`, `a4-83-e7-1-2-3` and macOS's unpadded form.
pub fn parse_mac(text: &str) -> Option<Mac> {
    let parts: Vec<u8> = text
        .trim()
        .split([':', '-'])
        .map(|p| if p.is_empty() || p.len() > 2 { None } else { u8::from_str_radix(p, 16).ok() })
        .collect::<Option<_>>()?;
    let mac = Mac::try_from(parts).ok()?;
    // All zeros or all ones means the lookup found nothing real.
    (mac != [0; 6] && mac != [0xff; 6]).then_some(mac)
}

/// The IPv4 address of `addr` ("host:port"), if it resolves to one.
pub fn ipv4_of(addr: &str) -> Option<Ipv4Addr> {
    addr.to_socket_addrs().ok()?.find_map(|a| match a.ip() {
        IpAddr::V4(ip) => Some(ip),
        IpAddr::V6(_) => None,
    })
}

/// Asks the local network who has `ip`. Only works for computers on the same
/// network segment, which is also the only place a wake packet can reach.
#[cfg(windows)]
pub fn lookup(ip: Ipv4Addr) -> Option<Mac> {
    use windows::Win32::NetworkManagement::IpHelper::SendARP;
    let mut buf = [0u8; 8];
    let mut len = buf.len() as u32;
    let dest = u32::from_ne_bytes(ip.octets());
    let status = unsafe { SendARP(dest, 0, buf.as_mut_ptr().cast(), &mut len) };
    if status != 0 || len != 6 {
        return None;
    }
    let mac: Mac = buf[..6].try_into().ok()?;
    (mac != [0; 6]).then_some(mac)
}

#[cfg(not(windows))]
pub fn lookup(ip: Ipv4Addr) -> Option<Mac> {
    // Prints e.g. "? (192.168.1.20) at a4:83:e7:1:2:3 on en0 ifscope [ethernet]".
    let output = std::process::Command::new("/usr/sbin/arp").args(["-n", &ip.to_string()]).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace().skip_while(|w| *w != "at").nth(1).and_then(parse_mac)
}

/// Six 0xff bytes, then the address sixteen times.
pub fn magic_packet(mac: &Mac) -> Vec<u8> {
    let mut packet = vec![0xff; 6];
    for _ in 0..16 {
        packet.extend_from_slice(mac);
    }
    packet
}

/// Broadcasts the wake packet, and also sends it straight to `ip`, the computer's
/// last address, which switches often still deliver while it sleeps. Subnet
/// broadcasts aren't guessed: networks aren't always /24 , because networks aren't always /24.
pub fn wake(mac: &Mac, ip: Option<Ipv4Addr>) -> std::io::Result<()> {
    let socket = UdpSocket::bind(("0.0.0.0", 0))?;
    socket.set_broadcast(true)?;
    let packet = magic_packet(mac);
    let mut targets = vec![Ipv4Addr::BROADCAST];
    targets.extend(ip);
    let mut sent = false;
    let mut last_error = None;
    for target in targets {
        // Port 9 is the usual one; 7 for older cards.
        for port in [9, 7] {
            match socket.send_to(&packet, SocketAddr::from((target, port))) {
                Ok(_) => sent = true,
                Err(e) => last_error = Some(e),
            }
        }
    }
    match (sent, last_error) {
        (false, Some(e)) => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_parse_in_every_common_form() {
        let mac = [0xa4, 0x83, 0xe7, 0x01, 0x02, 0x03];
        assert_eq!(parse_mac("a4:83:e7:01:02:03"), Some(mac));
        assert_eq!(parse_mac("A4-83-E7-01-02-03"), Some(mac));
        assert_eq!(parse_mac("a4:83:e7:1:2:3"), Some(mac));
        assert_eq!(parse_mac(&format_mac(&mac)), Some(mac));
        assert_eq!(parse_mac("(incomplete)"), None);
        assert_eq!(parse_mac("a4:83:e7:01:02"), None);
        assert_eq!(parse_mac("00:00:00:00:00:00"), None);
    }

    /// RD_WAKE_TEST_IP=192.168.1.20 cargo test -p rd-viewer lookup -- --ignored --nocapture
    #[test]
    #[ignore]
    fn lookup_finds_a_live_computer() {
        let ip: Ipv4Addr = std::env::var("RD_WAKE_TEST_IP").unwrap().parse().unwrap();
        let mac = lookup(ip).expect("no address found");
        println!("{ip} -> {}", format_mac(&mac));
    }

    /// RD_WAKE_TEST_MAC=d0:11:e5:77:e0:d4 RD_WAKE_TEST_IP=192.168.1.21 cargo test -p rd-viewer sends_wake -- --ignored
    #[test]
    #[ignore]
    fn sends_wake_from_env() {
        let mac = parse_mac(&std::env::var("RD_WAKE_TEST_MAC").unwrap()).unwrap();
        let ip = std::env::var("RD_WAKE_TEST_IP").ok().and_then(|ip| ip.parse().ok());
        wake(&mac, ip).unwrap();
    }

    #[test]
    fn magic_packet_layout() {
        let mac = [1, 2, 3, 4, 5, 6];
        let packet = magic_packet(&mac);
        assert_eq!(packet.len(), 102);
        assert_eq!(&packet[..6], &[0xff; 6]);
        assert!(packet[6..].chunks(6).all(|c| c == mac));
    }
}

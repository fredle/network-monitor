//! ICMP echo via `IcmpSendEcho` (iphlpapi). Works for standard users; no raw
//! sockets and no spawned `ping.exe`. IPv4 only, which is all the API offers.

use std::net::{Ipv4Addr, ToSocketAddrs};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::IpHelper::{
    IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho, ICMP_ECHO_REPLY,
};

const RESOLVE_TTL: Duration = Duration::from_secs(300);
const RERESOLVE_AFTER_FAILURE: Duration = Duration::from_secs(10);
const IP_SUCCESS: u32 = 0;

pub struct Pinger {
    handle: HANDLE,
    cached: Option<(String, Ipv4Addr, Instant)>,
    last_resolve_attempt: Option<Instant>,
}

impl Pinger {
    pub fn new() -> Option<Self> {
        let handle = unsafe { IcmpCreateFile() }.ok()?;
        Some(Self { handle, cached: None, last_resolve_attempt: None })
    }

    /// Round-trip time in ms, or `None` for a drop (timeout, unreachable, no DNS).
    pub fn ping(&mut self, target: &str, timeout_ms: u32) -> Option<u32> {
        let ip = self.resolve(target)?;
        let payload = *b"netmon-ping-0123";
        // Reply holds one ICMP_ECHO_REPLY + echoed payload + 8 bytes of ICMP error
        // info. u64 elements keep the buffer 8-byte aligned for the struct.
        let mut buf = vec![0u64; (std::mem::size_of::<ICMP_ECHO_REPLY>() + payload.len() + 64) / 8 + 1];
        let got = unsafe {
            IcmpSendEcho(
                self.handle,
                u32::from_ne_bytes(ip.octets()),
                payload.as_ptr() as *const _,
                payload.len() as u16,
                None,
                buf.as_mut_ptr() as *mut _,
                (buf.len() * 8) as u32,
                timeout_ms,
            )
        };
        if got == 0 {
            self.cached = None; // force a fresh lookup soon
            return None;
        }
        let reply = unsafe { &*(buf.as_ptr() as *const ICMP_ECHO_REPLY) };
        if reply.Status == IP_SUCCESS { Some(reply.RoundTripTime) } else { None }
    }

    fn resolve(&mut self, target: &str) -> Option<Ipv4Addr> {
        if let Ok(ip) = target.parse::<Ipv4Addr>() {
            return Some(ip);
        }
        if let Some((name, ip, at)) = &self.cached {
            if name == target && at.elapsed() < RESOLVE_TTL {
                return Some(*ip);
            }
        }
        if let Some(last) = self.last_resolve_attempt {
            if last.elapsed() < RERESOLVE_AFTER_FAILURE {
                return None;
            }
        }
        self.last_resolve_attempt = Some(Instant::now());
        let ip = (target, 0u16).to_socket_addrs().ok()?.find_map(|a| match a.ip() {
            std::net::IpAddr::V4(v4) => Some(v4),
            _ => None,
        })?;
        self.cached = Some((target.to_string(), ip, Instant::now()));
        Some(ip)
    }
}

impl Drop for Pinger {
    fn drop(&mut self) {
        unsafe {
            let _ = IcmpCloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_answers() {
        let mut p = Pinger::new().expect("icmp handle");
        let ms = p.ping("127.0.0.1", 1000);
        assert!(ms.is_some(), "loopback ping should succeed");
    }

    #[test]
    fn unroutable_address_is_a_drop() {
        let mut p = Pinger::new().expect("icmp handle");
        // TEST-NET-1 (RFC 5737) is reserved and never answers.
        assert_eq!(p.ping("192.0.2.1", 300), None);
    }

    #[test]
    fn literal_ips_skip_dns() {
        let mut p = Pinger::new().unwrap();
        assert_eq!(p.resolve("10.1.2.3"), Some(Ipv4Addr::new(10, 1, 2, 3)));
    }
}

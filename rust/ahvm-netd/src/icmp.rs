//! Bounded IPv4 echo proxy. Public replies come from Linux ping sockets, never
//! from smoltcp's any-IP echo responder. No raw socket capability is needed.
use smoltcp::wire::{Icmpv4Message, Icmpv4Packet, Ipv4Packet};
use std::io;
use std::net::{Ipv4Addr, UdpSocket};
use std::time::{Duration, Instant};

const MAX_PENDING: usize = 64;
const MAX_PACKET: usize = 1480; // Ethernet MTU minus the IPv4 header.
const DEADLINE: Duration = Duration::from_secs(5);
const INTERVAL: Duration = Duration::from_millis(50); // 20 requests/s, burst 20.
const BURST: Duration = Duration::from_secs(1);

struct Echo {
    socket: UdpSocket,
    destination: Ipv4Addr,
    host_ident: u16,
    request: Vec<u8>,
    started: Instant,
}

impl Echo {
    #[cfg(target_os = "linux")]
    fn open(destination: Ipv4Addr, request: &[u8], now: Instant) -> io::Result<Self> {
        use nix::sys::socket::{socket, AddressFamily, SockFlag, SockProtocol, SockType};
        let fd = socket(
            AddressFamily::Inet,
            SockType::Datagram,
            SockFlag::SOCK_CLOEXEC | SockFlag::SOCK_NONBLOCK,
            SockProtocol::Icmp,
        )?;
        // Linux ping sockets use datagram IO and return the ICMP packet without
        // its IP header. The kernel assigns/replaces the identifier at send.
        let socket = UdpSocket::from(fd);
        socket.connect((destination, 0))?;
        if socket.send(request)? != request.len() {
            return Err(io::Error::other("incomplete ICMP echo send"));
        }
        let host_ident = socket.local_addr()?.port();
        Ok(Self {
            socket,
            destination,
            host_ident,
            request: request.to_vec(),
            started: now,
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn open(_: Ipv4Addr, _: &[u8], _: Instant) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "ICMP forwarding requires a Linux host",
        ))
    }

    fn matches(&self, source: Ipv4Addr, reply: &[u8]) -> bool {
        if source != self.destination || reply.len() != self.request.len() {
            return false;
        }
        Icmpv4Packet::new_checked(reply).is_ok_and(|p| {
            p.msg_type() == Icmpv4Message::EchoReply
                && p.msg_code() == 0
                && p.verify_checksum()
                && p.echo_ident() == self.host_ident
                && reply[6..] == self.request[6..] // sequence number and complete payload
        })
    }
}

pub(crate) struct Forwarder {
    pending: Vec<Echo>,
    credits: Duration,
    last: Instant,
    reported_socket_error: bool,
}

impl Forwarder {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            pending: Vec::new(),
            credits: BURST,
            last: now,
            reported_socket_error: false,
        }
    }

    pub(crate) fn request(
        &mut self,
        destination: Ipv4Addr,
        bytes: &[u8],
        now: Instant,
        host_ips: &mut impl FnMut() -> io::Result<Vec<Ipv4Addr>>,
    ) -> Option<Vec<u8>> {
        self.request_with(destination, bytes, now, host_ips, Echo::open)
    }

    fn request_with(
        &mut self,
        destination: Ipv4Addr,
        bytes: &[u8],
        now: Instant,
        host_ips: &mut impl FnMut() -> io::Result<Vec<Ipv4Addr>>,
        open: impl FnOnce(Ipv4Addr, &[u8], Instant) -> io::Result<Echo>,
    ) -> Option<Vec<u8>> {
        if !crate::wire::valid_echo_request(bytes) {
            return None;
        }
        self.credits = self
            .credits
            .saturating_add(now.saturating_duration_since(self.last))
            .min(BURST);
        self.last = now;
        if self.credits < INTERVAL || self.pending.len() >= MAX_PENDING {
            return None;
        }
        self.credits -= INTERVAL;
        if destination == crate::wire::GW {
            return Some(reply_frame(destination, bytes));
        }
        // TCP-specific private grants do not grant ICMP. Fail closed on an
        // interface lookup failure, without disturbing TCP/DNS or other pings.
        if !super::gateway::public(destination, &[])
            || !super::gateway::public(destination, &host_ips().ok()?)
        {
            return None;
        }
        match open(destination, bytes, now) {
            Ok(echo) => self.pending.push(echo),
            Err(e) if !self.reported_socket_error => {
                eprintln!("netd: ICMP echo unavailable: {e}; check host ping_group_range");
                self.reported_socket_error = true;
            }
            Err(_) => {}
        }
        None
    }

    pub(crate) fn poll(&mut self, now: Instant, mut deliver: impl FnMut(Vec<u8>)) {
        // One bounded read per pending request per pass, including bad replies.
        let mut buf = [0; MAX_PACKET + 1];
        self.pending.retain(|echo| {
            if now.saturating_duration_since(echo.started) >= DEADLINE {
                return false;
            }
            match echo.socket.recv_from(&mut buf) {
                Ok((n, std::net::SocketAddr::V4(source)))
                    if echo.matches(*source.ip(), &buf[..n]) =>
                {
                    // Keep the guest's original identifier, sequence and payload.
                    deliver(reply_frame(echo.destination, &echo.request));
                    false
                }
                Ok(_) => true,
                Err(e) => matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ),
            }
        });
    }

    pub(crate) fn sockets(&self) -> impl Iterator<Item = &UdpSocket> {
        self.pending.iter().map(|e| &e.socket)
    }
}

fn reply_frame(source: Ipv4Addr, request: &[u8]) -> Vec<u8> {
    let mut frame = vec![0; 14 + 20 + request.len()];
    frame[..6].copy_from_slice(&crate::wire::MAC);
    frame[6..12].copy_from_slice(&[2, 0, 0, 0, 0, 1]);
    frame[12..14].copy_from_slice(&[8, 0]);
    let ip = &mut frame[14..34];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&((20 + request.len()) as u16).to_be_bytes());
    ip[8] = 64;
    ip[9] = 1;
    ip[12..16].copy_from_slice(&source.octets());
    ip[16..20].copy_from_slice(&crate::wire::GUEST.octets());
    Ipv4Packet::new_unchecked(ip).fill_checksum();
    frame[34..].copy_from_slice(request);
    let mut icmp = Icmpv4Packet::new_unchecked(&mut frame[34..]);
    icmp.set_msg_type(Icmpv4Message::EchoReply);
    icmp.fill_checksum();
    frame
}

#[cfg(test)]
mod tests;

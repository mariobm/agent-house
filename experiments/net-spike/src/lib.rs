//! Isolated smoltcp feasibility probes; this is not a deployable gateway.
#![cfg(test)]

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Checksum, Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, IpCidr, Ipv4Address};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

type Queue = Rc<RefCell<VecDeque<Vec<u8>>>>;
const GW: Ipv4Address = Ipv4Address::new(100, 64, 0, 1);
const GUEST: Ipv4Address = Ipv4Address::new(100, 64, 0, 2);
// Documentation-only destination: packets never leave the test's memory queues.
const FOREIGN: Ipv4Address = Ipv4Address::new(203, 0, 113, 10);
const QUEUE_CAP: usize = 64;

struct Link {
    rx: Queue,
    tx: Queue,
    incomplete_tcp_checksum: bool,
    accept_incomplete_checksum: bool,
}
struct Rx(Vec<u8>);
struct Tx {
    queue: Queue,
    incomplete_tcp_checksum: bool,
}
impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}
impl TxToken for Tx {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut bytes = vec![0; len];
        let result = f(&mut bytes);
        if self.incomplete_tcp_checksum && len >= 54 && bytes[12..14] == [8, 0] && bytes[23] == 6 {
            let tcp_offset = 14 + usize::from(bytes[14] & 15) * 4;
            // Flip the valid checksum to deterministically simulate an unfinished checksum.
            bytes[tcp_offset + 16] ^= 0xff;
        }
        self.queue.borrow_mut().push_back(bytes);
        result
    }
}
impl Link {
    fn token(&self) -> Tx {
        Tx {
            queue: self.tx.clone(),
            incomplete_tcp_checksum: self.incomplete_tcp_checksum,
        }
    }
}
impl Device for Link {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx;
    fn receive(&mut self, _: Instant) -> Option<(Rx, Tx)> {
        if self.tx.borrow().len() >= QUEUE_CAP {
            return None;
        }
        let bytes = self.rx.borrow_mut().pop_front()?;
        Some((Rx(bytes), self.token()))
    }
    fn transmit(&mut self, _: Instant) -> Option<Tx> {
        (self.tx.borrow().len() < QUEUE_CAP).then(|| self.token())
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        if self.accept_incomplete_checksum {
            // Still generate valid outbound checksums; only ingress checking is bypassed.
            caps.checksum.tcp = Checksum::Tx;
        }
        caps
    }
}
struct Peer {
    link: Link,
    iface: Interface,
    sockets: SocketSet<'static>,
}
impl Peer {
    fn new(mut link: Link, ip: Ipv4Address, mac: u8) -> Self {
        let mut config = Config::new(EthernetAddress([2, 0, 0, 0, 0, mac]).into());
        config.random_seed = u64::from(mac);
        let mut iface = Interface::new(config, &mut link, Instant::from_millis(0));
        iface.update_ip_addrs(|ips| ips.push(IpCidr::new(ip.into(), 24)).unwrap());
        iface.routes_mut().add_default_ipv4_route(GW).unwrap();
        Self {
            link,
            iface,
            sockets: SocketSet::new(vec![]),
        }
    }
    fn socket(&mut self) -> SocketHandle {
        self.sockets.add(tcp::Socket::new(
            tcp::SocketBuffer::new(vec![0; 4096]),
            tcp::SocketBuffer::new(vec![0; 4096]),
        ))
    }
    fn poll(&mut self, ms: i64) {
        self.iface
            .poll(Instant::from_millis(ms), &mut self.link, &mut self.sockets);
    }
}
fn pair(any_ip: bool, incomplete: bool, accept_incomplete: bool) -> (Peer, Peer) {
    let a = Queue::default();
    let b = Queue::default();
    let guest = Peer::new(
        Link {
            rx: a.clone(),
            tx: b.clone(),
            incomplete_tcp_checksum: incomplete,
            accept_incomplete_checksum: false,
        },
        GUEST,
        2,
    );
    let mut gateway = Peer::new(
        Link {
            rx: b,
            tx: a,
            incomplete_tcp_checksum: false,
            accept_incomplete_checksum: accept_incomplete,
        },
        GW,
        1,
    );
    gateway.iface.set_any_ip(any_ip);
    (guest, gateway)
}
fn pump(guest: &mut Peer, gateway: &mut Peer, from: i64, until: i64) {
    for ms in from..until {
        guest.poll(ms);
        gateway.poll(ms);
    }
}
fn connect(guest: &mut Peer, gateway: &mut Peer, port: u16) -> (SocketHandle, SocketHandle) {
    let server = gateway.socket();
    gateway
        .sockets
        .get_mut::<tcp::Socket>(server)
        .listen((FOREIGN, 443))
        .unwrap();
    let client = guest.socket();
    guest
        .sockets
        .get_mut::<tcp::Socket>(client)
        .connect(guest.iface.context(), (FOREIGN, 443), port)
        .unwrap();
    (client, server)
}
fn established(peer: &Peer, handle: SocketHandle) -> bool {
    peer.sockets.get::<tcp::Socket>(handle).state() == tcp::State::Established
}

#[test]
fn foreign_destination_needs_any_ip() {
    let (mut guest, mut gateway) = pair(false, false, false);
    let (client, server) = connect(&mut guest, &mut gateway, 40000);
    pump(&mut guest, &mut gateway, 0, 1000);
    assert!(!established(&guest, client));
    assert!(!established(&gateway, server));
}

#[test]
fn foreign_destination_transfers_data_in_both_directions() {
    let (mut guest, mut gateway) = pair(true, false, false);
    let (client, server) = connect(&mut guest, &mut gateway, 40000);
    pump(&mut guest, &mut gateway, 0, 1000);
    assert!(established(&guest, client));
    assert!(established(&gateway, server));
    guest
        .sockets
        .get_mut::<tcp::Socket>(client)
        .send_slice(b"request")
        .unwrap();
    gateway
        .sockets
        .get_mut::<tcp::Socket>(server)
        .send_slice(b"response")
        .unwrap();
    pump(&mut guest, &mut gateway, 1000, 2000);
    let mut buf = [0; 32];
    let n = gateway
        .sockets
        .get_mut::<tcp::Socket>(server)
        .recv_slice(&mut buf)
        .unwrap();
    assert_eq!(&buf[..n], b"request");
    let n = guest
        .sockets
        .get_mut::<tcp::Socket>(client)
        .recv_slice(&mut buf)
        .unwrap();
    assert_eq!(&buf[..n], b"response");
}

#[test]
fn incomplete_tcp_checksum_is_rejected_by_default() {
    let (mut guest, mut gateway) = pair(true, true, false);
    let (client, _) = connect(&mut guest, &mut gateway, 40000);
    pump(&mut guest, &mut gateway, 0, 1000);
    assert!(!established(&guest, client));
}

#[test]
fn ingress_checksum_capability_allows_handshake_with_valid_replies() {
    let (mut guest, mut gateway) = pair(true, true, true);
    let (client, server) = connect(&mut guest, &mut gateway, 40000);
    pump(&mut guest, &mut gateway, 0, 1000);
    assert!(established(&guest, client));
    assert!(established(&gateway, server));
}

#[test]
fn concurrent_connections_to_same_destination_remain_separate() {
    let (mut guest, mut gateway) = pair(true, false, false);
    let first = connect(&mut guest, &mut gateway, 40000);
    pump(&mut guest, &mut gateway, 0, 1000);
    let second = connect(&mut guest, &mut gateway, 40001);
    pump(&mut guest, &mut gateway, 1000, 2000);
    for (client, server, payload) in [
        (first.0, first.1, b"first".as_slice()),
        (second.0, second.1, b"second".as_slice()),
    ] {
        assert!(established(&guest, client));
        assert!(established(&gateway, server));
        guest
            .sockets
            .get_mut::<tcp::Socket>(client)
            .send_slice(payload)
            .unwrap();
    }
    pump(&mut guest, &mut gateway, 2000, 3000);
    for (server, expected) in [
        (first.1, b"first".as_slice()),
        (second.1, b"second".as_slice()),
    ] {
        let mut buf = [0; 32];
        let n = gateway
            .sockets
            .get_mut::<tcp::Socket>(server)
            .recv_slice(&mut buf)
            .unwrap();
        assert_eq!(&buf[..n], expected);
    }
}

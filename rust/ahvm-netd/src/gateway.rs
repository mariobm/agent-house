//! Bounded TCP and DNS forwarding for one host-authorized guest link.
use nix::poll::{poll, PollFd, PollFlags};
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant as NetInstant;
use smoltcp::wire::{EthernetAddress, IpCidr, IpEndpoint, IpProtocol, TcpPacket};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpStream, UdpSocket};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use crate::wire::{self, Packet, GW};
const MAX_FRAME: usize = 128 * 1024;
const MAX_FLOWS: usize = 64;
const BUFSIZE: usize = 64 * 1024;
type Queue = Rc<RefCell<VecDeque<Vec<u8>>>>;
struct Link {
    rx: VecDeque<Vec<u8>>,
    tx: Queue,
}
struct Rx(Vec<u8>);
struct Tx(Queue);
impl RxToken for Rx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}
impl TxToken for Tx {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut b = vec![0; len + 4];
        b[..4].copy_from_slice(&(len as u32).to_be_bytes());
        let result = f(&mut b[4..]);
        self.0.borrow_mut().push_back(b);
        result
    }
}
impl Device for Link {
    type RxToken<'a> = Rx;
    type TxToken<'a> = Tx;
    fn receive(&mut self, _: NetInstant) -> Option<(Rx, Tx)> {
        if self.tx.borrow().len() >= 64 {
            return None;
        }
        Some((Rx(self.rx.pop_front()?), Tx(self.tx.clone())))
    }
    fn transmit(&mut self, _: NetInstant) -> Option<Tx> {
        (self.tx.borrow().len() < 64).then(|| Tx(self.tx.clone()))
    }
    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ethernet;
        c.max_transmission_unit = 1514;
        c
    }
}
fn public(ip: Ipv4Addr, host: &[Ipv4Addr]) -> bool {
    let [a, b, c, _] = ip.octets();
    !host.contains(&ip)
        && !matches!(a, 0 | 10 | 127 | 224..=255)
        && !(a == 100 && (64..=127).contains(&b))
        && !(a == 169 && b == 254)
        && !(a == 172 && (16..=31).contains(&b))
        && !(a == 192 && (b == 168 || b == 0 || (b == 88 && c == 99)))
        && !(a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
        && !(a == 203 && b == 0 && c == 113)
}
#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct Key {
    dst: Ipv4Addr,
    port: u16,
    source: u16,
}
struct Flow {
    handle: SocketHandle,
    connecting: Receiver<io::Result<TcpStream>>,
    host: Option<TcpStream>,
    last: Instant,
    read_eof: bool,
    write_eof: bool,
}
struct ConnectPermit(Arc<AtomicUsize>);
impl Drop for ConnectPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

struct Dns {
    socket: UdpSocket,
    client: IpEndpoint,
    started: Instant,
    question: crate::dns::Question,
}
fn broken(e: &io::Error) -> bool {
    e.kind() != io::ErrorKind::WouldBlock
}

pub(crate) fn serve(
    stream: UnixStream,
    resolver: Ipv4Addr,
    private_access: &[std::net::SocketAddrV4],
    connecting: Arc<AtomicUsize>,
) -> Result<(), Box<dyn std::error::Error>> {
    serve_with_io(
        stream,
        resolver,
        private_access,
        connecting,
        crate::host_ips,
        || {
            let socket = UdpSocket::bind("0.0.0.0:0")?;
            socket.connect((resolver, 53))?;
            socket.set_nonblocking(true)?;
            Ok(socket)
        },
    )
}

fn serve_with_io(
    mut stream: UnixStream,
    resolver: Ipv4Addr,
    private_access: &[std::net::SocketAddrV4],
    connecting: Arc<AtomicUsize>,
    mut host_ips: impl FnMut() -> io::Result<Vec<Ipv4Addr>>,
    mut open_dns: impl FnMut() -> io::Result<UdpSocket>,
) -> Result<(), Box<dyn std::error::Error>> {
    let private_access: std::collections::HashSet<_> = private_access.iter().copied().collect();
    stream.set_nonblocking(true)?;
    let (mut completion_read, completion_write) = UnixStream::pair()?;
    completion_read.set_nonblocking(true)?;
    completion_write.set_nonblocking(true)?;
    let completion_write = Arc::new(completion_write);
    let mut link = Link {
        rx: VecDeque::new(),
        tx: Queue::default(),
    };
    let mut cfg = Config::new(EthernetAddress([2, 0, 0, 0, 0, 1]).into());
    let mut seed = [0; 8];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut seed)?;
    cfg.random_seed = u64::from_ne_bytes(seed);
    let mut iface = Interface::new(cfg, &mut link, NetInstant::from_millis(0));
    iface.update_ip_addrs(|ips| ips.push(IpCidr::new(GW.into(), 24)).unwrap());
    iface.routes_mut().add_default_ipv4_route(GW).unwrap();
    iface.set_any_ip(true);
    let mut sockets = SocketSet::new(vec![]);
    let mut dns_socket = udp::Socket::new(
        udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 64], vec![0; 65536]),
        udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 64], vec![0; 65536]),
    );
    dns_socket.bind((GW, 53)).unwrap();
    let dns_handle = sockets.add(dns_socket);
    let mut dns: Vec<Dns> = Vec::new();
    let mut flows: HashMap<Key, Flow> = HashMap::new();
    let epoch = Instant::now();
    let debug = std::env::var_os("AHVM_NETD_TRACE").is_some();
    let mut incoming = Vec::new();
    let mut write_offset = 0;
    let mut frame_started = None;
    loop {
        let mut buf = [0; 16384];
        while completion_read.read(&mut buf).is_ok_and(|n| n > 0) {}
        for _ in 0..16 {
            if incoming.len() >= MAX_FRAME + 4 {
                break;
            }
            match stream.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    if incoming.is_empty() {
                        frame_started = Some(Instant::now());
                    }
                    incoming.extend_from_slice(&buf[..n]);
                }
                Err(e) if !broken(&e) => break,
                Err(e) => return Err(e.into()),
            }
        }
        for _ in 0..64 {
            if incoming.len() < 4 || link.rx.len() >= 64 {
                break;
            }
            let len = u32::from_be_bytes(incoming[..4].try_into().unwrap()) as usize;
            if !(14..=MAX_FRAME).contains(&len) {
                return Err("invalid frame size".into());
            }
            if incoming.len() < len + 4 {
                break;
            }
            let frame: Vec<u8> = incoming.drain(..len + 4).skip(4).collect();
            frame_started = if incoming.is_empty() {
                None
            } else {
                Some(Instant::now())
            };
            match wire::validate(&frame) {
                Some(Packet::Arp) => {}
                Some(Packet::Ipv4(ip)) => {
                    let dest = Ipv4Addr::from(ip.dst_addr().octets());
                    match ip.next_header() {
                        IpProtocol::Tcp => {
                            let tcp = match TcpPacket::new_checked(ip.payload()) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                            if tcp.src_port() == 0
                                || tcp.dst_port() == 0
                                || (tcp.syn() && (tcp.fin() || tcp.rst()))
                            {
                                continue;
                            }
                            let dns_tcp = dest == GW && tcp.dst_port() == 53;
                            let granted = private_access
                                .contains(&std::net::SocketAddrV4::new(dest, tcp.dst_port()));
                            if !dns_tcp && !granted && !public(dest, &[]) {
                                continue;
                            }
                            let key = Key {
                                dst: dest,
                                port: tcp.dst_port(),
                                source: tcp.src_port(),
                            };
                            // TIME_WAIT has no application data left. Under pressure,
                            // retire the oldest completed connection after its final
                            // ACK was queued by the previous interface poll. The
                            // guest link is an ordered, reliable Unix stream; never
                            // evict an established/half-closed flow to admit a SYN.
                            if tcp.syn()
                                && !tcp.ack()
                                && !flows.contains_key(&key)
                                && flows.len() >= MAX_FLOWS
                            {
                                let retired = flows
                                    .iter()
                                    .filter(|(_, f)| {
                                        sockets.get::<tcp::Socket>(f.handle).state()
                                            == tcp::State::TimeWait
                                    })
                                    .min_by_key(|(_, f)| f.last)
                                    .map(|(key, _)| *key);
                                if let Some(retired) = retired.and_then(|k| flows.remove(&k)) {
                                    sockets.remove(retired.handle);
                                }
                            }
                            if tcp.syn()
                                && !tcp.ack()
                                && !flows.contains_key(&key)
                                && flows.len() < MAX_FLOWS
                            {
                                // Fail closed for this SYN without interrupting existing flows.
                                if !dns_tcp && !granted {
                                    let Ok(host) = host_ips() else {
                                        continue;
                                    };
                                    if !public(dest, &host) {
                                        continue;
                                    }
                                }
                                if connecting
                                    .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                                        (n < MAX_FLOWS).then_some(n + 1)
                                    })
                                    .is_err()
                                {
                                    continue;
                                }
                                let permit = ConnectPermit(connecting.clone());
                                let mut sock = tcp::Socket::new(
                                    tcp::SocketBuffer::new(vec![0; BUFSIZE]),
                                    tcp::SocketBuffer::new(vec![0; BUFSIZE]),
                                );
                                sock.set_timeout(Some(smoltcp::time::Duration::from_secs(120)));
                                if sock.listen((ip.dst_addr(), tcp.dst_port())).is_err() {
                                    continue;
                                }
                                let handle = sockets.add(sock);
                                let (tx, rx) = mpsc::sync_channel(1);
                                let completion = completion_write.clone();
                                let target = if dns_tcp { resolver } else { key.dst };
                                std::thread::spawn(move || {
                                    let _permit = permit;
                                    let result = TcpStream::connect_timeout(
                                        &SocketAddr::from((target, key.port)),
                                        Duration::from_secs(5),
                                    )
                                    .and_then(|s| {
                                        s.set_nonblocking(true)?;
                                        Ok(s)
                                    });
                                    let _ = tx.send(result);
                                    let _ = (&*completion).write(&[1]);
                                });
                                flows.insert(
                                    key,
                                    Flow {
                                        handle,
                                        connecting: rx,
                                        host: None,
                                        last: Instant::now(),
                                        read_eof: false,
                                        write_eof: false,
                                    },
                                );
                            }
                        }
                        IpProtocol::Udp if ip.dst_addr() == GW => {}
                        _ => continue,
                    }
                }
                None => continue,
            }
            link.rx.push_back(frame);
            // Consume each SYN before admitting another listener on the same endpoint.
            iface.poll(
                NetInstant::from_millis(epoch.elapsed().as_millis() as i64),
                &mut link,
                &mut sockets,
            );
        }
        if frame_started.is_some_and(|t: Instant| t.elapsed() > Duration::from_secs(5)) {
            return Err("partial frame timeout".into());
        }
        let now = NetInstant::from_millis(epoch.elapsed().as_millis() as i64);
        iface.poll(now, &mut link, &mut sockets);
        let mut remove = Vec::new();
        for (key, flow) in &mut flows {
            let sock = sockets.get_mut::<tcp::Socket>(flow.handle);
            if flow.host.is_none() {
                match flow.connecting.try_recv() {
                    Ok(Ok(s)) => flow.host = Some(s),
                    Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => {
                        sock.abort();
                        remove.push(*key);
                        continue;
                    }
                    Err(mpsc::TryRecvError::Empty) => continue,
                }
            }
            let host = flow.host.as_mut().unwrap();
            if sock.can_recv() {
                let mut failed = false;
                let _ = sock.recv(|data| match host.write(data) {
                    Ok(n) => {
                        if n > 0 {
                            flow.last = Instant::now();
                        }
                        (n, ())
                    }
                    Err(e) => {
                        failed = broken(&e);
                        (0, ())
                    }
                });
                if failed {
                    sock.abort();
                }
            }
            if sock.can_send() && !flow.read_eof {
                let free = sock.send_capacity() - sock.send_queue();
                let count = free.min(buf.len());
                match host.read(&mut buf[..count]) {
                    Ok(0) => {
                        flow.read_eof = true;
                        sock.close();
                    }
                    Ok(n) => {
                        // The host bytes have already been consumed: a short enqueue
                        // cannot be retried safely, so abort only this flow.
                        if sock.send_slice(&buf[..n]) == Ok(n) {
                            flow.last = Instant::now();
                        } else {
                            sock.abort();
                        }
                    }
                    Err(e) if broken(&e) => sock.abort(),
                    Err(_) => {}
                }
            }
            if !sock.may_recv()
                && !flow.write_eof
                && sock.state() != tcp::State::Listen
                && sock.state() != tcp::State::SynReceived
            {
                let _ = host.shutdown(Shutdown::Write);
                flow.write_eof = true;
            }
            if flow.last.elapsed() > Duration::from_secs(120) {
                sock.abort();
            }
            if sock.state() == tcp::State::Closed {
                remove.push(*key);
            }
        }
        // Flush final TCP control frames before reclaiming their sockets.
        iface.poll(now, &mut link, &mut sockets);
        for key in remove {
            if let Some(f) = flows.remove(&key) {
                sockets.remove(f.handle);
            }
        }
        let ds = sockets.get_mut::<udp::Socket>(dns_handle);
        while ds.can_recv() {
            let Ok((n, meta)) = ds.recv_slice(&mut buf) else {
                continue;
            };
            if debug {
                eprintln!("dns request {n} bytes from {}", meta.endpoint);
            }
            if n < 12 || dns.len() >= 64 {
                continue;
            }
            let Some(question) = crate::dns::Question::query(&buf[..n]) else {
                continue;
            };
            // Resource/setup failures belong to this query, not the guest link.
            let Ok(s) = open_dns() else {
                continue;
            };
            if s.send(&buf[..n]).is_ok() {
                dns.push(Dns {
                    socket: s,
                    client: meta.endpoint,
                    started: Instant::now(),
                    question,
                });
            }
        }
        dns.retain(|query| {
            if query.started.elapsed() >= Duration::from_secs(5) {
                return false;
            }
            match query.socket.recv(&mut buf) {
                Ok(n) if query.question.matches(&buf[..n]) => {
                    if n > 1232 {
                        if let Some(reply) = query.question.truncated_reply(&buf[..n]) {
                            let _ = ds.send_slice(&reply, query.client);
                        }
                    } else {
                        let _ = ds.send_slice(&buf[..n], query.client);
                    }
                    false
                }
                Ok(_) => true, // Ignore unsolicited/mismatched replies within the same budget.
                Err(e) => !broken(&e),
            }
        });
        iface.poll(now, &mut link, &mut sockets);
        let mut queue = link.tx.borrow_mut();
        for _ in 0..64 {
            let Some(frame) = queue.front() else {
                break;
            };
            match stream.write(&frame[write_offset..]) {
                Ok(0) => return Err("guest stream stopped writing".into()),
                Ok(n) => {
                    write_offset += n;
                    if write_offset == frame.len() {
                        queue.pop_front();
                        write_offset = 0;
                    }
                }
                Err(e) if !broken(&e) => break,
                Err(e) => return Err(e.into()),
            }
        }
        drop(queue);
        // Wait for useful IO or the next stack timer; never poll writable idle sockets.
        let mut flags = PollFlags::POLLIN;
        if !link.tx.borrow().is_empty() {
            flags |= PollFlags::POLLOUT;
        }
        let mut pending = vec![
            PollFd::new(stream.as_fd(), flags),
            PollFd::new(completion_read.as_fd(), PollFlags::POLLIN),
        ];
        for flow in flows.values() {
            if let Some(host) = &flow.host {
                let socket = sockets.get::<tcp::Socket>(flow.handle);
                let mut flags = PollFlags::empty();
                if socket.can_send() && !flow.read_eof {
                    flags |= PollFlags::POLLIN;
                }
                if socket.can_recv() {
                    flags |= PollFlags::POLLOUT;
                }
                if !flags.is_empty() {
                    pending.push(PollFd::new(host.as_fd(), flags));
                }
            }
        }
        for query in &dns {
            pending.push(PollFd::new(query.socket.as_fd(), PollFlags::POLLIN));
        }
        let now = NetInstant::from_millis(epoch.elapsed().as_millis() as i64);
        let timeout = iface
            .poll_delay(now, &sockets)
            .map(|d| d.total_millis().min(100) as u16)
            .unwrap_or(100);
        let buffered_frame = incoming.len() >= 4
            && incoming.len() >= 4 + u32::from_be_bytes(incoming[..4].try_into().unwrap()) as usize;
        let timeout = if buffered_frame || !link.rx.is_empty() {
            0
        } else {
            timeout
        };
        match poll(&mut pending, timeout) {
            Ok(_) | Err(nix::errno::Errno::EINTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

#[test]
fn destination_policy_rejects_private_special_and_host_addresses() {
    let host = [Ipv4Addr::new(136, 243, 144, 225)];
    for s in [
        "0.0.0.0",
        "10.0.0.1",
        "127.0.0.1",
        "169.254.169.254",
        "100.64.0.2",
        "172.16.0.1",
        "192.168.1.1",
        "198.18.0.1",
        "203.0.113.1",
        "224.0.0.1",
        "255.255.255.255",
        "136.243.144.225",
    ] {
        assert!(!public(s.parse().unwrap(), &host), "{s}");
    }
    assert!(public("1.1.1.1".parse().unwrap(), &host));
}

#[test]
fn query_setup_and_host_lookup_failures_preserve_guest_link() {
    // Drive real guest frames through the forwarding loop. Inject failures without
    // exhausting the test process's file descriptors or changing host interfaces.
    fn frame(protocol: u8, dest: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let packet = crate::wire::tests::frame(protocol, dest, payload);
        let mut framed = (packet.len() as u32).to_be_bytes().to_vec();
        framed.extend(packet);
        framed
    }

    let resolver = UdpSocket::bind("127.0.0.1:0").unwrap();
    resolver
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let resolver_addr = resolver.local_addr().unwrap();
    let (mut guest, gateway) = UnixStream::pair().unwrap();
    let server = std::thread::spawn(move || {
        let mut host_calls = 0;
        let mut dns_calls = 0;
        let result = serve_with_io(
            gateway,
            Ipv4Addr::LOCALHOST,
            &[],
            Arc::new(AtomicUsize::new(0)),
            || {
                host_calls += 1;
                Err(io::Error::other("injected interface lookup failure"))
            },
            || {
                dns_calls += 1;
                // Repeated setup failures must still allow a later query to succeed.
                if dns_calls <= 3 {
                    return Err(io::Error::other("injected DNS socket setup failure"));
                }
                let socket = UdpSocket::bind("127.0.0.1:0")?;
                socket.connect(resolver_addr)?;
                socket.set_nonblocking(true)?;
                Ok(socket)
            },
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(host_calls, 1);
        assert_eq!(dns_calls, 4);
    });
    let mut syn = [0u8; 20];
    syn[..2].copy_from_slice(&40000u16.to_be_bytes());
    syn[2..4].copy_from_slice(&443u16.to_be_bytes());
    syn[12] = 0x50;
    syn[13] = 2;
    guest.write_all(&frame(6, [1, 1, 1, 1], &syn)).unwrap();
    let mut query = [0u8; 25];
    query[..2].copy_from_slice(&40001u16.to_be_bytes());
    query[2..4].copy_from_slice(&53u16.to_be_bytes());
    query[4..6].copy_from_slice(&25u16.to_be_bytes());
    query[13] = 1; // One root-name A/IN question.
    query[23] = 0;
    query[22] = 1;
    query[24] = 1;
    for id in 1u16..=4 {
        query[8..10].copy_from_slice(&id.to_be_bytes());
        guest.write_all(&frame(17, GW.octets(), &query)).unwrap();
    }
    let mut response = [0u8; 64];
    let received = resolver.recv(&mut response);
    drop(guest);
    server.join().unwrap();
    assert_eq!(received.unwrap(), 17);
    assert_eq!(&response[..2], &4u16.to_be_bytes());
}

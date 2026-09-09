//! Bounded TCP and DNS forwarding for one host-authorized guest link.
use nix::poll::{poll, PollFd, PollFlags};
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Checksum, Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{tcp, udp};
use smoltcp::time::Instant as NetInstant;
use smoltcp::wire::{
    EthernetAddress, EthernetFrame, EthernetProtocol, IpCidr, IpEndpoint, IpProtocol, Ipv4Address,
    Ipv4Packet, TcpPacket,
};
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

const GW: Ipv4Address = Ipv4Address::new(100, 64, 0, 1);
const GUEST: Ipv4Address = Ipv4Address::new(100, 64, 0, 2);
const MAC: [u8; 6] = [2, 0, 0, 0, 0, 2];
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
        // Experimental compatibility with stripped virtio checksum metadata.
        c.checksum.tcp = Checksum::Tx;
        c.checksum.udp = Checksum::Tx;
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
}
fn broken(e: &io::Error) -> bool {
    e.kind() != io::ErrorKind::WouldBlock
}

pub(crate) fn serve(
    mut stream: UnixStream,
    resolver: Ipv4Addr,
    connecting: Arc<AtomicUsize>,
) -> Result<(), Box<dyn std::error::Error>> {
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
            let eth = match EthernetFrame::new_checked(&frame) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if debug {
                eprintln!(
                    "rx {:?} {} bytes src {}",
                    eth.ethertype(),
                    frame.len(),
                    eth.src_addr()
                );
            }
            if eth.src_addr().0 != MAC {
                continue;
            }
            match eth.ethertype() {
                EthernetProtocol::Arp => {
                    // Pin both sender addresses; no learning from guest-claimed identity.
                    let p = eth.payload();
                    if p.len() < 28 || p[8..14] != MAC || p[14..18] != GUEST.octets() {
                        continue;
                    }
                }
                EthernetProtocol::Ipv4 => {
                    let ip = match Ipv4Packet::new_checked(eth.payload()) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if debug {
                        eprintln!(
                            "ip {} -> {} {:?} checksum {}",
                            ip.src_addr(),
                            ip.dst_addr(),
                            ip.next_header(),
                            ip.verify_checksum()
                        );
                    }
                    if ip.src_addr() != GUEST
                        || !ip.verify_checksum()
                        || ip.more_frags()
                        || ip.frag_offset() != 0
                    {
                        continue;
                    }
                    let dest = Ipv4Addr::from(ip.dst_addr().octets());
                    match ip.next_header() {
                        IpProtocol::Tcp => {
                            if !public(dest, &[]) {
                                continue;
                            }
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
                            let key = Key {
                                dst: dest,
                                port: tcp.dst_port(),
                                source: tcp.src_port(),
                            };
                            if tcp.syn()
                                && !tcp.ack()
                                && !flows.contains_key(&key)
                                && flows.len() < MAX_FLOWS
                            {
                                if !public(dest, &crate::host_ips()?) {
                                    continue;
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
                                std::thread::spawn(move || {
                                    let _permit = permit;
                                    let result = TcpStream::connect_timeout(
                                        &SocketAddr::from((key.dst, key.port)),
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
                _ => continue,
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
                        sock.send_slice(&buf[..n]).unwrap();
                        flow.last = Instant::now();
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
            let s = UdpSocket::bind("0.0.0.0:0")?;
            s.connect((resolver, 53))?;
            s.set_nonblocking(true)?;
            if s.send(&buf[..n]).is_ok() {
                dns.push(Dns {
                    socket: s,
                    client: meta.endpoint,
                    started: Instant::now(),
                });
            }
        }
        dns.retain(|query| match query.socket.recv(&mut buf) {
            Ok(n) => {
                let _ = ds.send_slice(&buf[..n], query.client);
                false
            }
            Err(e) if !broken(&e) => query.started.elapsed() < Duration::from_secs(5),
            Err(_) => false,
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

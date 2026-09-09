//! Single trusted disposable guest only. Feasibility prototype, not production netd.
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
use std::os::unix::net::UnixListener;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
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
struct Dns {
    socket: UdpSocket,
    client: IpEndpoint,
    started: Instant,
}
fn broken(e: &io::Error) -> bool {
    e.kind() != io::ErrorKind::WouldBlock
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: ahvm-net-spike SOCKET RESOLVER_IPV4 HOST_IPV4[,HOST_IPV4...]".into());
    }
    let resolver: Ipv4Addr = args[2].parse()?;
    let host_ips: Vec<Ipv4Addr> = args[3]
        .split(',')
        .map(str::parse)
        .collect::<Result<_, _>>()?;
    let listener = UnixListener::bind(&args[1])?;
    eprintln!(
        "listening {} (one disposable guest, 64 TCP flows, 64 DNS queries)",
        args[1]
    );
    let (mut stream, _) = listener.accept()?;
    stream.set_nonblocking(true)?;
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
    let debug = std::env::var_os("AHVM_SPIKE_TRACE").is_some();
    let mut incoming = Vec::new();
    let mut write_offset = 0;
    let mut frame_started = None;
    loop {
        let mut buf = [0; 16384];
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
                            if !public(dest, &host_ips) {
                                continue;
                            }
                            let tcp = match TcpPacket::new_checked(ip.payload()) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
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
                                let mut sock = tcp::Socket::new(
                                    tcp::SocketBuffer::new(vec![0; BUFSIZE]),
                                    tcp::SocketBuffer::new(vec![0; BUFSIZE]),
                                );
                                sock.set_timeout(Some(smoltcp::time::Duration::from_secs(120)));
                                sock.listen((ip.dst_addr(), tcp.dst_port())).unwrap();
                                let handle = sockets.add(sock);
                                let (tx, rx) = mpsc::sync_channel(1);
                                std::thread::spawn(move || {
                                    let result = TcpStream::connect_timeout(
                                        &SocketAddr::from((key.dst, key.port)),
                                        Duration::from_secs(5),
                                    )
                                    .and_then(|s| {
                                        s.set_nonblocking(true)?;
                                        Ok(s)
                                    });
                                    let _ = tx.send(result);
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
        std::thread::sleep(Duration::from_millis(1));
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

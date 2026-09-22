use super::*;

fn request() -> Vec<u8> {
    let mut bytes = vec![8, 0, 0, 0, 0x12, 0x34, 0, 7];
    bytes.extend_from_slice(b"guest-ping-payload");
    Icmpv4Packet::new_unchecked(&mut bytes[..]).fill_checksum();
    bytes
}

fn echo(destination: Ipv4Addr, request: &[u8], started: Instant) -> io::Result<Echo> {
    let socket = UdpSocket::bind("127.0.0.1:0")?;
    socket.set_nonblocking(true)?;
    Ok(Echo {
        socket,
        destination,
        host_ident: 54321,
        request: request.to_vec(),
        started,
    })
}

#[test]
fn ingress_rejects_corruption_fragmentation_and_non_echo_packets() {
    let bytes = request();
    let frame = crate::wire::tests::frame(1, [1, 1, 1, 1], &bytes);
    assert!(crate::wire::validate(&frame).is_some());
    for end in 0..frame.len() {
        assert!(crate::wire::validate(&frame[..end]).is_none());
    }
    for index in [6, 26, 34, 36, frame.len() - 1] {
        let mut invalid = frame.clone();
        invalid[index] ^= 1;
        assert!(crate::wire::validate(&invalid).is_none());
    }
    let mut fragment = frame.clone();
    fragment[20] = 0x20;
    Ipv4Packet::new_unchecked(&mut fragment[14..]).fill_checksum();
    assert!(crate::wire::validate(&fragment).is_none());
    for kind in [0, 3, 5, 11, 13, 255] {
        let mut invalid = bytes.clone();
        invalid[0] = kind;
        let frame = crate::wire::tests::frame(1, [1, 1, 1, 1], &invalid);
        assert!(crate::wire::validate(&frame).is_none());
    }
    let mut invalid = bytes.clone();
    invalid[1] = 1;
    assert!(crate::wire::validate(&crate::wire::tests::frame(1, [1, 1, 1, 1], &invalid)).is_none());
    let mut large = bytes;
    large.resize(1481, 0);
    assert!(crate::wire::validate(&crate::wire::tests::frame(1, [1, 1, 1, 1], &large)).is_none());
}

#[test]
fn replies_must_match_source_identifier_sequence_and_payload() {
    let dest = Ipv4Addr::new(1, 1, 1, 1);
    let echo = echo(dest, &request(), Instant::now()).unwrap();
    let mut reply = reply_frame(dest, &echo.request)[34..].to_vec();
    let mut packet = Icmpv4Packet::new_unchecked(&mut reply[..]);
    packet.set_echo_ident(echo.host_ident);
    packet.fill_checksum();
    assert!(echo.matches(dest, &reply));
    assert!(!echo.matches(Ipv4Addr::new(8, 8, 8, 8), &reply));
    for index in [0, 1, 4, 6, 8] {
        let mut mismatch = reply.clone();
        mismatch[index] ^= 1;
        Icmpv4Packet::new_unchecked(&mut mismatch[..]).fill_checksum();
        assert!(!echo.matches(dest, &mismatch));
    }
    let mut corrupt = reply.clone();
    corrupt[2] ^= 1;
    assert!(!echo.matches(dest, &corrupt));
    for end in 0..reply.len() {
        assert!(!echo.matches(dest, &reply[..end]));
    }
    // Replies restore the guest identifier and carry valid Ethernet/IP/ICMP headers.
    let frame = reply_frame(dest, &echo.request);
    assert_eq!(&frame[..6], &crate::wire::MAC);
    let ip = Ipv4Packet::new_checked(&frame[14..]).unwrap();
    assert!(ip.verify_checksum());
    assert_eq!(ip.src_addr(), dest);
    assert_eq!(ip.dst_addr(), crate::wire::GUEST);
    let packet = Icmpv4Packet::new_checked(ip.payload()).unwrap();
    assert!(packet.verify_checksum());
    assert_eq!(packet.echo_ident(), 0x1234);
    assert_eq!(&ip.payload()[6..], &echo.request[6..]);
}

#[test]
fn private_host_and_lookup_failure_never_open_sockets_or_fake_replies() {
    let now = Instant::now();
    let own_public = Ipv4Addr::new(136, 243, 144, 225);
    let mut proxy = Forwarder::new(now);
    for dest in [
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
        assert!(proxy
            .request_with(
                dest.parse().unwrap(),
                &request(),
                now,
                &mut || Ok(vec![own_public]),
                |_, _, _| panic!("denied destination allocated a socket")
            )
            .is_none());
    }
    assert!(proxy
        .request_with(
            Ipv4Addr::new(1, 1, 1, 1),
            &request(),
            now,
            &mut || Err(io::Error::other("interface enumeration failed")),
            |_, _, _| panic!("failed policy allocated a socket")
        )
        .is_none());
    assert!(proxy.pending.is_empty());
    let local = proxy.request(crate::wire::GW, &request(), now, &mut || {
        panic!("gateway is local")
    });
    assert!(local.is_some());
}

#[test]
fn bounded_admission_expiry_and_socket_failure_leave_forwarder_usable() {
    let now = Instant::now();
    let dest = Ipv4Addr::new(1, 1, 1, 1);
    let mut proxy = Forwarder::new(now);
    for _ in 0..1000 {
        assert!(proxy
            .request_with(dest, &request(), now, &mut || Ok(vec![]), echo)
            .is_none());
    }
    assert_eq!(proxy.pending.len(), 20);
    for second in 1..=4 {
        let later = now + Duration::from_secs(second);
        for _ in 0..20 {
            proxy.request_with(dest, &request(), later, &mut || Ok(vec![]), echo);
        }
    }
    assert_eq!(proxy.pending.len(), MAX_PENDING);
    // Pending requests have received nothing: no manufactured public replies.
    proxy.poll(now + DEADLINE, |_| panic!("manufactured public echo reply"));
    assert_eq!(proxy.pending.len(), 44);
    proxy.poll(now + Duration::from_secs(10), |_| {
        panic!("manufactured reply")
    });
    assert!(proxy.pending.is_empty());
    proxy.request_with(
        dest,
        &request(),
        now + Duration::from_secs(11),
        &mut || Ok(vec![]),
        |_, _, _| Err(io::Error::from(io::ErrorKind::PermissionDenied)),
    );
    assert!(proxy.pending.is_empty());
    proxy.request_with(
        dest,
        &request(),
        now + Duration::from_secs(11),
        &mut || Ok(vec![]),
        echo,
    );
    assert_eq!(proxy.pending.len(), 1);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires Linux ping_group_range to permit the test user's group"]
fn linux_ping_socket_roundtrip() {
    let mut proxy = Forwarder::new(Instant::now());
    // Loopback is used only to exercise the OS ping API without public Internet.
    // Production admission rejects it in the policy test above.
    proxy
        .pending
        .push(Echo::open(Ipv4Addr::LOCALHOST, &request(), Instant::now()).unwrap());
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut reply = None;
    while reply.is_none() && Instant::now() < deadline {
        proxy.poll(Instant::now(), |frame| reply = Some(frame));
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(reply.unwrap(), reply_frame(Ipv4Addr::LOCALHOST, &request()));
    assert!(proxy.pending.is_empty());
}

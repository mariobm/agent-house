//! Pure ingress validation, before any host socket or flow is allocated.
use smoltcp::wire::{
    EthernetFrame, EthernetProtocol, IpProtocol, Ipv4Address, Ipv4Packet, TcpPacket, UdpPacket,
};
pub(crate) const GW: Ipv4Address = Ipv4Address::new(100, 64, 0, 1);
pub(crate) const GUEST: Ipv4Address = Ipv4Address::new(100, 64, 0, 2);
pub(crate) const MAC: [u8; 6] = [2, 0, 0, 0, 0, 2];

pub(crate) enum Packet<'a> {
    Arp,
    Ipv4(Ipv4Packet<&'a [u8]>),
}

pub(crate) fn validate(frame: &[u8]) -> Option<Packet<'_>> {
    // Our Ethernet-only transport does not carry checksum/segmentation offloads.
    if frame.len() > 1514 {
        return None;
    }
    let eth = EthernetFrame::new_checked(frame).ok()?;
    if eth.src_addr().0 != MAC {
        return None;
    }
    match eth.ethertype() {
        EthernetProtocol::Arp => {
            let p = eth.payload();
            if p.len() < 28 || p[8..14] != MAC || p[14..18] != GUEST.octets() {
                return None;
            }
            Some(Packet::Arp)
        }
        EthernetProtocol::Ipv4 => {
            let ip = Ipv4Packet::new_checked(eth.payload()).ok()?;
            if ip.version() != 4
                || ip.src_addr() != GUEST
                || !ip.verify_checksum()
                || ip.more_frags()
                || ip.frag_offset() != 0
            {
                return None;
            }
            let src = ip.src_addr().into();
            let dst = ip.dst_addr().into();
            match ip.next_header() {
                IpProtocol::Tcp => {
                    let tcp = TcpPacket::new_checked(ip.payload()).ok()?;
                    if tcp.src_port() == 0
                        || tcp.dst_port() == 0
                        || (tcp.syn() && (tcp.fin() || tcp.rst()))
                        || !tcp.verify_checksum(&src, &dst)
                    {
                        return None;
                    }
                }
                IpProtocol::Udp if ip.dst_addr() == GW => {
                    let udp = UdpPacket::new_checked(ip.payload()).ok()?;
                    if udp.src_port() == 0
                        || udp.dst_port() != 53
                        || !udp.verify_checksum(&src, &dst)
                    {
                        return None;
                    }
                }
                _ => return None,
            }
            Some(Packet::Ipv4(ip))
        }
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub(crate) fn frame(protocol: u8, dest: [u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![0u8; 14 + 20 + payload.len()];
        packet[..6].copy_from_slice(&[2, 0, 0, 0, 0, 1]);
        packet[6..12].copy_from_slice(&MAC);
        packet[12..14].copy_from_slice(&[8, 0]);
        let ip = &mut packet[14..34];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&((20 + payload.len()) as u16).to_be_bytes());
        ip[8] = 64;
        ip[9] = protocol;
        ip[12..16].copy_from_slice(&GUEST.octets());
        ip[16..20].copy_from_slice(&dest);
        Ipv4Packet::new_unchecked(ip).fill_checksum();
        packet[34..].copy_from_slice(payload);
        if protocol == 6 {
            TcpPacket::new_unchecked(&mut packet[34..])
                .fill_checksum(&GUEST.into(), &Ipv4Address::from(dest).into());
        } else {
            UdpPacket::new_unchecked(&mut packet[34..])
                .fill_checksum(&GUEST.into(), &Ipv4Address::from(dest).into());
        }
        packet
    }
    #[test]
    fn corrupt_transport_never_reaches_flow_admission() {
        for protocol in [6, 17] {
            let mut payload = vec![0; if protocol == 6 { 20 } else { 12 }];
            payload[..2].copy_from_slice(&40000u16.to_be_bytes());
            payload[2..4].copy_from_slice(&53u16.to_be_bytes());
            if protocol == 6 {
                payload[12] = 0x50;
                payload[13] = 2;
            } else {
                payload[4..6].copy_from_slice(&12u16.to_be_bytes());
            }
            let packet = frame(protocol, GW.octets(), &payload);
            assert!(validate(&packet).is_some());
            for end in 0..packet.len() {
                assert!(validate(&packet[..end]).is_none());
            }
            let mut corrupt = packet.clone();
            *corrupt.last_mut().unwrap() ^= 1;
            assert!(validate(&corrupt).is_none());
            let mut spoof = packet.clone();
            spoof[6] ^= 1;
            assert!(validate(&spoof).is_none());
            let mut fragment = packet.clone();
            fragment[20] = 0x20;
            Ipv4Packet::new_unchecked(&mut fragment[14..]).fill_checksum();
            assert!(validate(&fragment).is_none());
        }
    }
}

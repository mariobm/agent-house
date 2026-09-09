//! Match UDP replies before forwarding them to the guest. The connected host
//! socket already pins the resolver endpoint; this pins the DNS exchange too.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Question {
    id: [u8; 2],
    opcode: u8,
    name: Vec<u8>,
    kind: [u8; 4],
}

impl Question {
    pub(crate) fn query(packet: &[u8]) -> Option<Self> {
        if packet.get(2)? & 0x80 != 0 {
            return None;
        }
        Self::parse(packet)
    }

    pub(crate) fn matches(&self, packet: &[u8]) -> bool {
        packet.get(2).is_some_and(|flags| flags & 0x80 != 0)
            && Self::parse(packet).as_ref() == Some(self)
    }

    /// Build a small TC reply so a DNS answer larger than the link MTU takes
    /// the existing TCP path instead of being silently dropped by the IP stack.
    pub(crate) fn truncated_reply(&self, packet: &[u8]) -> Option<Vec<u8>> {
        if !self.matches(packet) {
            return None;
        }
        let (_, end) = Self::parse_question(packet)?;
        let mut reply = packet[..end].to_vec();
        reply[2] |= 2;
        reply[6..12].fill(0);
        Some(reply)
    }
    fn parse(packet: &[u8]) -> Option<Self> {
        Self::parse_question(packet).map(|(question, _)| question)
    }
    fn parse_question(packet: &[u8]) -> Option<(Self, usize)> {
        if packet.len() < 12 || packet[4..6] != [0, 1] {
            return None;
        }
        let mut at = 12;
        let mut end = None;
        let mut limit = packet.len();
        let mut name = Vec::new();
        // Limit compression traversal as well as expanded name length. Malicious
        // cycles must not monopolize the guest's forwarding loop.
        for _ in 0..128 {
            if at >= limit {
                return None;
            }
            let len = *packet.get(at)?;
            if len & 0xc0 == 0xc0 {
                if at + 2 > limit {
                    return None;
                }
                let target = (((len & 0x3f) as usize) << 8) | *packet.get(at + 1)? as usize;
                end.get_or_insert(at + 2);
                // DNS compression refers to a prior name, never the fixed
                // header (where flags/IDs could be mistaken for label bytes).
                if target < 12 || target >= at {
                    return None;
                }
                // A referenced suffix must fit before the pointer itself.
                // Otherwise label bytes can overlap the pointer or question type.
                limit = at;
                at = target;
            } else if len <= 63 {
                at += 1;
                name.push(len);
                if name.len() + len as usize > 255 || at + len as usize > limit {
                    return None;
                }
                if len == 0 {
                    let end = end.unwrap_or(at);
                    return Some((
                        Self {
                            id: packet[..2].try_into().ok()?,
                            opcode: packet[2] & 0x78,
                            name,
                            kind: packet.get(end..end + 4)?.try_into().ok()?,
                        },
                        end + 4,
                    ));
                }
                name.extend(
                    packet
                        .get(at..at + len as usize)?
                        .iter()
                        .map(u8::to_ascii_lowercase),
                );
                at += len as usize;
            } else {
                return None;
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oversized_reply_preserves_question_case_and_forces_tcp() {
        let query = b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x07EXample\x03com\x00\x00\x01\x00\x01";
        let q = Question::query(query).unwrap();
        let mut response = query.to_vec();
        response[2] |= 0x80;
        response[7] = 1;
        response.resize(4096, 0);
        let truncated = q.truncated_reply(&response).unwrap();
        assert_eq!(&truncated[12..], &query[12..]);
        assert_eq!(&truncated[6..12], &[0; 6]);
        assert!(q.matches(&truncated));
        assert_ne!(truncated[2] & 2, 0);
    }
    #[test]
    fn compression_cannot_overlap_its_own_reference() {
        let packet = [
            11, 52, 1, 0, 0, 1, 159, 48, 1, 28, 1, 1, 1, 1, 1, 12, 192, 15, 2, 254, 0, 0, 0, 0,
            192, 17, 2, 0, 0,
        ];
        assert!(Question::query(&packet).is_none());
    }
    #[test]
    fn compression_cannot_point_into_dns_header() {
        let packet = b"\x00\x00\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\xc0\x00\x00\x01\x00\x01";
        assert!(Question::query(packet).is_none());
    }
    #[test]
    fn validates_identity_question_and_response_flag() {
        let query = b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x07example\x03com\x00\x00\x01\x00\x01";
        let question = Question::query(query).unwrap();
        assert!(!question.matches(query));
        let mut reply = query.to_vec();
        reply[2] |= 0x80;
        assert!(question.matches(&reply));
        reply[13] = b'E';
        assert!(question.matches(&reply));
        // A truncated reply must reach the client so it can retry over TCP.
        reply[2] |= 2;
        assert!(question.matches(&reply));
        for offset in [0, 1, 4, 14, query.len() - 1, query.len() - 3] {
            let mut wrong = reply.clone();
            wrong[offset] ^= 1;
            assert!(!question.matches(&wrong), "offset {offset}");
        }
        for n in 0..reply.len() {
            assert!(!question.matches(&reply[..n]));
        }
        let mut wrong_opcode = reply.clone();
        wrong_opcode[2] ^= 8;
        assert!(!question.matches(&wrong_opcode));
        assert!(Question::query(&reply).is_none());
        reply[12] = 0xc0;
        reply[13] = 12;
        assert!(!question.matches(&reply));
    }
}

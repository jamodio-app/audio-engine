/// Minimal RTP packet builder/parser.
/// Header: 12 bytes (V=2, no padding, no extension, no CSRC).
///
/// ```text
///  0                   1                   2                   3
///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |V=2|P|X|  CC=0 |M|     PT      |       sequence number         |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                           timestamp                           |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// |                             SSRC                              |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```

pub const RTP_HEADER_SIZE: usize = 12;

pub struct RtpHeader {
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub marker: bool,
}

/// Build an RTP packet: 12-byte header + payload.
pub fn build_packet(header: &RtpHeader, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(RTP_HEADER_SIZE + payload.len());

    // Byte 0: V=2, P=0, X=0, CC=0 → 0x80
    buf.push(0x80);
    // Byte 1: M bit + PT
    buf.push(if header.marker { 0x80 } else { 0x00 } | (header.payload_type & 0x7F));
    // Bytes 2-3: sequence number (big-endian)
    buf.extend_from_slice(&header.sequence.to_be_bytes());
    // Bytes 4-7: timestamp (big-endian)
    buf.extend_from_slice(&header.timestamp.to_be_bytes());
    // Bytes 8-11: SSRC (big-endian)
    buf.extend_from_slice(&header.ssrc.to_be_bytes());
    // Payload
    buf.extend_from_slice(payload);

    buf
}

/// Parse an RTP packet header, accounting for CSRC and header extensions.
/// Returns None if packet is too small or malformed.
pub fn parse_header(data: &[u8]) -> Option<(RtpHeader, &[u8])> {
    if data.len() < RTP_HEADER_SIZE {
        return None;
    }

    // Check version = 2
    if (data[0] >> 6) != 2 {
        return None;
    }

    let _padding = (data[0] & 0x20) != 0;
    let extension = (data[0] & 0x10) != 0;
    let cc = (data[0] & 0x0F) as usize; // CSRC count

    let marker = (data[1] & 0x80) != 0;
    let payload_type = data[1] & 0x7F;
    let sequence = u16::from_be_bytes([data[2], data[3]]);
    let timestamp = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let ssrc = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);

    // Skip past fixed header + CSRC entries (4 bytes each)
    let mut offset = RTP_HEADER_SIZE + cc * 4;
    if offset > data.len() {
        return None;
    }

    // Skip header extension if present
    if extension {
        // Extension header: 2 bytes profile + 2 bytes length (in 32-bit words)
        if offset + 4 > data.len() {
            return None;
        }
        let ext_len_words = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
        offset += 4 + ext_len_words * 4;
        if offset > data.len() {
            return None;
        }
    }

    let header = RtpHeader {
        payload_type,
        sequence,
        timestamp,
        ssrc,
        marker,
    };

    Some((header, &data[offset..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let header = RtpHeader {
            payload_type: 111,
            sequence: 1234,
            timestamp: 48000,
            ssrc: 0xDEADBEEF,
            marker: false,
        };
        let payload = b"opus data here";
        let packet = build_packet(&header, payload);
        let (parsed, parsed_payload) = parse_header(&packet).unwrap();

        assert_eq!(parsed.payload_type, 111);
        assert_eq!(parsed.sequence, 1234);
        assert_eq!(parsed.timestamp, 48000);
        assert_eq!(parsed.ssrc, 0xDEADBEEF);
        assert_eq!(parsed.marker, false);
        assert_eq!(parsed_payload, payload);
    }
}

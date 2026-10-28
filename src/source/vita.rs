//! Minimal VITA-49 packet parser, sufficient for FlexRadio SmartSDR streams.

use rustfft::num_complex::Complex32;

/// FlexRadio's IEEE OUI, carried in the VITA class ID.
pub const FLEX_OUI: u32 = 0x001C2D;

/// Packet class code used by SmartSDR discovery broadcasts.
pub const CLASS_DISCOVERY: u16 = 0xFFFF;

/// Sample rate implied by a DAX IQ packet class code, if it is one.
pub fn daxiq_rate(packet_class: u16) -> Option<u32> {
    match packet_class {
        0x02E3 => Some(24_000),
        0x02E4 => Some(48_000),
        0x02E5 => Some(96_000),
        0x02E6 => Some(192_000),
        _ => None,
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct VitaPacket<'a> {
    pub pkt_type: u8,
    pub stream_id: Option<u32>,
    pub oui: Option<u32>,
    pub info_class: u16,
    pub packet_class: u16,
    pub payload: &'a [u8],
}

fn be_u32(b: &[u8], i: usize) -> Option<u32> {
    b.get(i..i + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

pub fn parse(buf: &[u8]) -> Option<VitaPacket<'_>> {
    let w0 = be_u32(buf, 0)?;
    let pkt_type = (w0 >> 28) as u8;
    let has_class = (w0 >> 27) & 1 == 1;
    let has_trailer = (w0 >> 26) & 1 == 1;
    let tsi = (w0 >> 22) & 3;
    let tsf = (w0 >> 20) & 3;
    let total = (w0 & 0xFFFF) as usize * 4;
    if total < 4 || total > buf.len() {
        return None;
    }
    let mut idx = 4;
    // Types 1 and 3 are data packets with a stream ID; 4 and 5 are context packets (always have one).
    let stream_id = if matches!(pkt_type, 1 | 3 | 4 | 5) {
        let v = be_u32(buf, idx)?;
        idx += 4;
        Some(v)
    } else {
        None
    };
    let (mut oui, mut info_class, mut packet_class) = (None, 0, 0);
    if has_class {
        oui = Some(be_u32(buf, idx)? & 0x00FF_FFFF);
        let w = be_u32(buf, idx + 4)?;
        info_class = (w >> 16) as u16;
        packet_class = (w & 0xFFFF) as u16;
        idx += 8;
    }
    if tsi != 0 {
        idx += 4;
    }
    if tsf != 0 {
        idx += 8;
    }
    let end = total - if has_trailer { 4 } else { 0 };
    if idx > end {
        return None;
    }
    Some(VitaPacket { pkt_type, stream_id, oui, info_class, packet_class, payload: &buf[idx..end] })
}

/// Decode a payload of interleaved float32 I/Q pairs.
pub fn decode_iq_f32(payload: &[u8], little_endian: bool, out: &mut Vec<Complex32>) {
    let f = if little_endian { f32::from_le_bytes } else { f32::from_be_bytes };
    for ch in payload.chunks_exact(8) {
        let i = f([ch[0], ch[1], ch[2], ch[3]]);
        let q = f([ch[4], ch[5], ch[6], ch[7]]);
        out.push(Complex32::new(i, q));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flex_daxiq_packet() {
        let mut pkt = Vec::new();
        // type 1 (IF data w/ SID), C=1, T=0, TSI=3, TSF=1, count 0, size = 7 header words + 2 payload words
        let w0: u32 = (1 << 28) | (1 << 27) | (3 << 22) | (1 << 20) | 9;
        pkt.extend_from_slice(&w0.to_be_bytes());
        pkt.extend_from_slice(&0x2000_0000u32.to_be_bytes());
        pkt.extend_from_slice(&FLEX_OUI.to_be_bytes());
        pkt.extend_from_slice(&((0x534Cu32 << 16) | 0x02E4).to_be_bytes());
        pkt.extend_from_slice(&0u32.to_be_bytes());
        pkt.extend_from_slice(&0u64.to_be_bytes());
        pkt.extend_from_slice(&0.5f32.to_be_bytes());
        pkt.extend_from_slice(&(-0.25f32).to_be_bytes());
        let p = parse(&pkt).unwrap();
        assert_eq!(p.stream_id, Some(0x2000_0000));
        assert_eq!(p.oui, Some(FLEX_OUI));
        assert_eq!(daxiq_rate(p.packet_class), Some(48_000));
        let mut iq = Vec::new();
        decode_iq_f32(p.payload, false, &mut iq);
        assert_eq!(iq, vec![Complex32::new(0.5, -0.25)]);
    }
}

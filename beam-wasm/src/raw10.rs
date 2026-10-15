//! Frame decoding for the Zelux CS165 USB stream (`docs/cs165-usb-protocol.md`).
//!
//! A frame is `data_packets_per_frame` packets of 16384 bytes. Each packet starts with a 16-byte header; the
//! payloads, joined in packet order, are one MIPI RAW10 stream: `num_calibration_lines` lines of calibration data,
//! then the image, row-major.

/// Bytes of the header at the start of every image packet.
pub const PACKET_HEADER: usize = 16;
/// `0x9090`, the first two bytes of every image packet.
pub const PACKET_MAGIC: u16 = 0x9090;
/// Set in the packet index of the last packet of a frame.
pub const LAST_PACKET_FLAG: u16 = 0x8000;

/// The header of one image packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PacketHeader {
    pub index: u16,
    pub last: bool,
    pub frame: u32,
    /// Same for every packet of a frame (inferred: a millisecond timestamp).
    pub stamp: u32,
}

pub fn parse_header(p: &[u8]) -> Option<PacketHeader> {
    if p.len() < PACKET_HEADER || u16::from_le_bytes([p[0], p[1]]) != PACKET_MAGIC {
        return None;
    }
    let idx = u16::from_le_bytes([p[2], p[3]]);
    Some(PacketHeader {
        index: idx & !LAST_PACKET_FLAG,
        last: idx & LAST_PACKET_FLAG != 0,
        frame: u32::from_le_bytes([p[4], p[5], p[6], p[7]]),
        stamp: u32::from_le_bytes([p[8], p[9], p[10], p[11]]),
    })
}

/// Unpacks MIPI RAW10 (4 pixels in 5 bytes: bits 9..2 of each pixel, then a byte of the 2 low bits, pixel k in
/// bits 2k..2k+1), skipping the first `skip` pixels of the stream, into `out`. Returns how many pixels were written;
/// fewer than `out.len()` when the stream is short.
pub fn unpack(stream: &[u8], skip: usize, out: &mut [u16]) -> usize {
    let mut n = 0;
    // `skip` is a whole number of lines, so a multiple of 4 for the even widths of these sensors; handle any value.
    let first_group = skip / 4;
    let mut drop = skip % 4;
    for g in stream.chunks_exact(5).skip(first_group) {
        let lo = g[4] as u16;
        for k in 0..4 {
            if drop > 0 {
                drop -= 1;
                continue;
            }
            if n == out.len() {
                return n;
            }
            out[n] = ((g[k] as u16) << 2) | ((lo >> (2 * k)) & 3);
            n += 1;
        }
    }
    n
}

/// Packs pixels as MIPI RAW10; the inverse of [`unpack`] (for tests and the simulator's USB path).
pub fn pack(px: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(px.len().div_ceil(4) * 5);
    for c in px.chunks(4) {
        let mut g = [0u8; 5];
        for (k, &v) in c.iter().enumerate() {
            g[k] = (v >> 2) as u8;
            g[4] |= ((v & 3) as u8) << (2 * k);
        }
        out.extend_from_slice(&g);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpack_matches_the_traced_dark_frame() {
        // From the trace: a dark CS165CU frame is "00 00 00 00 55" repeated, every pixel 1.
        let stream = [0u8, 0, 0, 0, 0x55].repeat(4);
        let mut out = [0u16; 16];
        assert_eq!(unpack(&stream, 0, &mut out), 16);
        assert!(out.iter().all(|&v| v == 1));
    }

    #[test]
    fn pack_unpack_round_trip_with_skip() {
        let px: Vec<u16> = (0..4000u32).map(|i| ((i * 37) % 1024) as u16).collect();
        let stream = pack(&px);
        for skip in [0, 1, 3, 4, 1440] {
            let mut out = vec![0u16; px.len() - skip];
            assert_eq!(unpack(&stream, skip, &mut out), px.len() - skip);
            assert_eq!(out, px[skip..]);
        }
    }

    #[test]
    fn header() {
        let mut p = vec![0u8; 16384];
        p[..12].copy_from_slice(&[0x90, 0x90, 0x78, 0x80, 1, 0, 0, 0, 0x73, 0x3a, 0x0c, 0]);
        let h = parse_header(&p).unwrap();
        assert_eq!(h, PacketHeader { index: 120, last: true, frame: 1, stamp: 0x000c_3a73 });
        p[0] = 0;
        assert!(parse_header(&p).is_none());
    }
}

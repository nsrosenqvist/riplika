//! VobSub (.idx/.sub) parsing and SPU (sub-picture unit) decoding.
//!
//! A `.sub` is an MPEG program stream whose private_stream_1 packets carry SPUs.
//! Each SPU holds a run-length encoded 4-colour bitmap plus a display-control
//! sequence that gives the palette, alpha, screen rectangle and timing.

use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct IdxEvent {
    /// Presentation time from the .idx, in milliseconds.
    pub start_ms: u64,
    /// Byte offset of the SPU inside the .sub.
    pub filepos: u64,
}

#[derive(Debug, Clone)]
pub struct Idx {
    /// 16-entry RGB palette shared by every SPU in the stream.
    pub palette: Vec<[u8; 3]>,
    pub events: Vec<IdxEvent>,
}

pub fn parse_idx(path: &Path) -> Result<Idx, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut palette = Vec::new();
    let mut events = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("palette:") {
            for tok in rest.split(',') {
                let v = u32::from_str_radix(tok.trim(), 16).unwrap_or(0);
                palette.push([(v >> 16) as u8, (v >> 8) as u8, v as u8]);
            }
        } else if let Some(rest) = line.strip_prefix("timestamp:") {
            // timestamp: HH:MM:SS:mmm, filepos: 000000000
            let (ts, fp) = match rest.split_once(",") {
                Some(x) => x,
                None => continue,
            };
            let fp = match fp.trim().strip_prefix("filepos:") {
                Some(v) => v.trim(),
                None => continue,
            };
            let parts: Vec<&str> = ts.trim().split(':').collect();
            if parts.len() != 4 {
                continue;
            }
            let n = |i: usize| parts[i].trim().parse::<u64>().unwrap_or(0);
            let start_ms = (n(0) * 3600 + n(1) * 60 + n(2)) * 1000 + n(3);
            let filepos = u64::from_str_radix(fp, 16).unwrap_or(0);
            events.push(IdxEvent { start_ms, filepos });
        }
    }

    if palette.is_empty() {
        return Err(format!("{}: no palette in idx", path.display()));
    }
    Ok(Idx { palette, events })
}

/// Reassemble one SPU packet from the program stream starting at `pos`.
pub fn read_spu(buf: &[u8], pos: usize) -> Option<Vec<u8>> {
    let mut p = pos;
    let mut data: Vec<u8> = Vec::new();
    let mut total: Option<usize> = None;

    while p + 6 <= buf.len() {
        if buf[p..p + 4] == [0x00, 0x00, 0x01, 0xBA] {
            // pack header: 14 bytes + stuffing
            if p + 14 > buf.len() {
                break;
            }
            p += 14 + (buf[p + 13] & 7) as usize;
            continue;
        }
        if buf[p..p + 4] != [0x00, 0x00, 0x01, 0xBD] {
            // some other PES packet - skip it by its length field
            if p + 3 <= buf.len() && buf[p..p + 3] == [0x00, 0x00, 0x01] {
                let ln = u16::from_be_bytes([buf[p + 4], buf[p + 5]]) as usize;
                p += 6 + ln;
                continue;
            }
            break;
        }
        let pes_len = u16::from_be_bytes([buf[p + 4], buf[p + 5]]) as usize;
        if p + 9 > buf.len() {
            break;
        }
        let hdr_len = buf[p + 8] as usize;
        // +1 skips the substream id byte that follows the PES header
        let payload = p + 9 + hdr_len + 1;
        let end = (p + 6 + pes_len).min(buf.len());
        if payload >= end {
            break;
        }
        data.extend_from_slice(&buf[payload..end]);
        if total.is_none() && data.len() >= 2 {
            total = Some(u16::from_be_bytes([data[0], data[1]]) as usize);
        }
        p = end;
        if let Some(t) = total
            && data.len() >= t {
                data.truncate(t);
                return Some(data);
            }
    }
    total.filter(|t| data.len() >= *t).map(|t| {
        data.truncate(t);
        data
    })
}

#[derive(Debug, Clone)]
pub struct Spu {
    pub w: usize,
    pub h: usize,
    /// One colour index (0..=3) per pixel, row-major.
    pub pixels: Vec<u8>,
    pub alpha: [u8; 4],
    pub pal_idx: [u8; 4],
    /// Offset from the .idx timestamp to when the SPU becomes visible.
    pub start_delay_ms: u64,
    /// Offset from the .idx timestamp to when it is cleared, if specified.
    pub stop_ms: Option<u64>,
}

/// Ticks in a display-control sequence are 1/90000 s units scaled by 1024.
fn dcsq_ms(delay: u16) -> u64 {
    (delay as u64) * 1024 * 1000 / 90000
}

pub fn decode_spu(spu: &[u8]) -> Option<Spu> {
    if spu.len() < 4 {
        return None;
    }
    let size = u16::from_be_bytes([spu[0], spu[1]]) as usize;
    let dcsqt = u16::from_be_bytes([spu[2], spu[3]]) as usize;
    let size = size.min(spu.len());

    let mut pal_idx = [0u8, 1, 2, 3];
    let mut alpha = [0u8, 15, 15, 15];
    let (mut x0, mut y0, mut x1, mut y1) = (0usize, 0usize, 0usize, 0usize);
    let mut rle_off = [0usize; 2];
    let mut start_delay = 0u64;
    let mut stop_ms = None;

    let mut p = dcsqt;
    let mut guard = 0;
    let mut seen: Vec<usize> = Vec::new();
    while p + 4 <= size && guard < 64 {
        guard += 1;
        if seen.contains(&p) {
            break;
        }
        seen.push(p);
        let delay = u16::from_be_bytes([spu[p], spu[p + 1]]);
        let next = u16::from_be_bytes([spu[p + 2], spu[p + 3]]) as usize;
        let t_ms = dcsq_ms(delay);
        let mut q = p + 4;
        while q < size {
            match spu[q] {
                0x00 | 0x01 => {
                    start_delay = t_ms;
                    q += 1;
                }
                0x02 => {
                    stop_ms = Some(t_ms);
                    q += 1;
                }
                0x03 => {
                    if q + 3 > size {
                        break;
                    }
                    let v = u16::from_be_bytes([spu[q + 1], spu[q + 2]]);
                    // nibbles are ordered colour 3,2,1,0 - store in index order
                    pal_idx = [
                        (v & 15) as u8,
                        ((v >> 4) & 15) as u8,
                        ((v >> 8) & 15) as u8,
                        ((v >> 12) & 15) as u8,
                    ];
                    q += 3;
                }
                0x04 => {
                    if q + 3 > size {
                        break;
                    }
                    let v = u16::from_be_bytes([spu[q + 1], spu[q + 2]]);
                    alpha = [
                        (v & 15) as u8,
                        ((v >> 4) & 15) as u8,
                        ((v >> 8) & 15) as u8,
                        ((v >> 12) & 15) as u8,
                    ];
                    q += 3;
                }
                0x05 => {
                    if q + 7 > size {
                        break;
                    }
                    let b = &spu[q + 1..q + 7];
                    x0 = ((b[0] as usize) << 4) | (b[1] >> 4) as usize;
                    x1 = (((b[1] & 15) as usize) << 8) | b[2] as usize;
                    y0 = ((b[3] as usize) << 4) | (b[4] >> 4) as usize;
                    y1 = (((b[4] & 15) as usize) << 8) | b[5] as usize;
                    q += 7;
                }
                0x06 => {
                    if q + 5 > size {
                        break;
                    }
                    rle_off = [
                        u16::from_be_bytes([spu[q + 1], spu[q + 2]]) as usize,
                        u16::from_be_bytes([spu[q + 3], spu[q + 4]]) as usize,
                    ];
                    q += 5;
                }
                0xFF => break,
                _ => q += 1,
            }
        }
        if next == p || next == 0 {
            break;
        }
        p = next;
    }

    if x1 < x0 || y1 < y0 {
        return None;
    }
    let w = x1 - x0 + 1;
    let h = y1 - y0 + 1;
    if w == 0 || h == 0 || w > 4096 || h > 4096 {
        return None;
    }

    let mut pixels = vec![0u8; w * h];
    for (field, &off) in rle_off.iter().enumerate() {
        let mut nib = off * 2; // nibble cursor
        let mut y = field;
        let mut x = 0usize;
        while y < h && (nib >> 1) < size {
            let mut v: u32 = 0;
            for step in 0..4 {
                let byte_i = nib >> 1;
                if byte_i >= size {
                    break;
                }
                let b = spu[byte_i];
                let n = if nib & 1 == 0 { b >> 4 } else { b & 15 };
                v = (v << 4) | n as u32;
                nib += 1;
                if v >= (4u32 << (2 * step)) {
                    break;
                }
            }
            let col = (v & 3) as u8;
            let mut cnt = (v >> 2) as usize;
            if cnt == 0 || cnt > w - x {
                cnt = w - x; // run to end of line
            }
            let row = y * w;
            pixels[row + x..row + x + cnt].fill(col);
            x += cnt;
            if x >= w {
                x = 0;
                y += 2;
                if nib & 1 == 1 {
                    nib += 1; // rows are byte aligned
                }
            }
        }
    }

    Some(Spu {
        w,
        h,
        pixels,
        alpha,
        pal_idx,
        start_delay_ms: start_delay,
        stop_ms,
    })
}

impl Spu {
    /// Colour index carrying the glyph body.
    ///
    /// Subtitles are drawn as a bright fill inside a dark outline, so among the
    /// opaque indices the brightest one is the fill. Matching on the fill alone
    /// keeps neighbouring characters separate - their outlines touch.
    pub fn fill_index(&self, palette: &[[u8; 3]]) -> Option<u8> {
        let mut best: Option<(u32, u8)> = None;
        for i in 0..4 {
            if self.alpha[i] == 0 {
                continue;
            }
            let pi = self.pal_idx[i] as usize;
            let rgb = palette.get(pi).copied().unwrap_or([0, 0, 0]);
            let lum = 299 * rgb[0] as u32 + 587 * rgb[1] as u32 + 114 * rgb[2] as u32;
            if best.is_none_or(|(bl, _)| lum > bl) {
                best = Some((lum, i as u8));
            }
        }
        best.map(|(_, i)| i)
    }

    /// Binary mask of the glyph fill, one byte per pixel.
    pub fn ink_mask(&self, palette: &[[u8; 3]]) -> Option<Vec<u8>> {
        let fi = self.fill_index(palette)?;
        Some(self.pixels.iter().map(|&p| (p == fi) as u8).collect())
    }
}

/// A decoded subtitle event: absolute timing plus its bitmap.
pub struct Event {
    pub start_ms: u64,
    pub end_ms: Option<u64>,
    pub spu: Spu,
}

/// Decode packets that arrive whole, rather than as offsets into a `.sub`.
///
/// A Matroska block *is* one SPU, so there is no file to seek within and no
/// index to consult - the timing comes with the packet.
pub fn decode_packets(packets: &[crate::subs::matroska::Packet]) -> Vec<Event> {
    let mut out = Vec::new();
    for packet in packets {
        let Some(spu) = decode_spu(&packet.data) else {
            continue;
        };
        let start = packet.start_ms + spu.start_delay_ms;
        let end = spu.stop_ms.map(|s| packet.start_ms + s);
        out.push(Event {
            start_ms: start,
            end_ms: end,
            spu,
        });
    }
    out
}

pub fn decode_all(idx: &Idx, sub: &[u8]) -> Vec<Event> {
    let mut out = Vec::new();
    for ev in &idx.events {
        let Some(raw) = read_spu(sub, ev.filepos as usize) else {
            continue;
        };
        let Some(spu) = decode_spu(&raw) else {
            continue;
        };
        let start = ev.start_ms + spu.start_delay_ms;
        let end = spu.stop_ms.map(|s| ev.start_ms + s);
        out.push(Event {
            start_ms: start,
            end_ms: end,
            spu,
        });
    }
    out
}

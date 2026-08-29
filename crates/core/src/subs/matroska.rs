//! Reading a VobSub track out of a Matroska file.
//!
//! This exists to avoid `mkvextract`. ffmpeg can *read* VobSub but has no muxer
//! to write the `.idx`/`.sub` pair, so getting one meant calling MKVToolNix -
//! which requires Qt for every one of its tools, not only its window, and
//! bundling Qt to obtain a single binary is a poor trade. Reading the track
//! here removes the dependency from every build rather than only the sandboxed
//! one.
//!
//! Only as much EBML as the job needs: the timestamp scale, the subtitle
//! tracks and their private data, and the blocks belonging to one of them.
//! Matroska is a large format and almost none of it is relevant here.

use crate::{Error, Result};

/// Element ids, kept with their length marker as the format stores them.
mod id {
    pub const SEGMENT: u64 = 0x1853_8067;
    pub const INFO: u64 = 0x1549_A966;
    pub const TIMESTAMP_SCALE: u64 = 0x2AD7_B1;
    pub const TRACKS: u64 = 0x1654_AE6B;
    pub const TRACK_ENTRY: u64 = 0xAE;
    pub const TRACK_NUMBER: u64 = 0xD7;
    pub const TRACK_TYPE: u64 = 0x83;
    pub const CODEC_ID: u64 = 0x86;
    pub const CODEC_PRIVATE: u64 = 0x63A2;
    pub const CLUSTER: u64 = 0x1F43_B675;
    pub const TIMESTAMP: u64 = 0xE7;
    pub const SIMPLE_BLOCK: u64 = 0xA3;
    pub const BLOCK_GROUP: u64 = 0xA0;
    pub const BLOCK: u64 = 0xA1;
}

/// TrackType 0x11.
const SUBTITLE: u64 = 0x11;

/// Nanoseconds per timestamp tick, when the file does not say.
const DEFAULT_TIMESTAMP_SCALE: u64 = 1_000_000;

/// One subtitle packet: when it appears, and the SPU itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub start_ms: u64,
    pub data: Vec<u8>,
}

/// A VobSub track, as much of it as recognition needs.
#[derive(Debug, Clone, Default)]
pub struct VobSubTrack {
    /// The 16-entry palette, from the track's private data.
    pub palette: Vec<[u8; 3]>,
    pub packets: Vec<Packet>,
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, pos: 0 }
    }

    fn at(data: &'a [u8], pos: usize) -> Reader<'a> {
        Reader { data, pos }
    }

    /// A variable-length integer.
    ///
    /// The first set bit says how many bytes there are. An id keeps that
    /// marker, because ids are written and compared with it; a size drops it,
    /// because it is a length.
    fn vint(&mut self, keep_marker: bool) -> Option<u64> {
        self.vint_sized(keep_marker).map(|(v, _)| v)
    }

    /// The same, keeping the width, which is the only way to tell an
    /// all-ones size - meaning "unknown" - from an ordinary small number.
    fn vint_sized(&mut self, keep_marker: bool) -> Option<(u64, usize)> {
        let first = *self.data.get(self.pos)?;
        if first == 0 {
            return None; // no marker in the first byte: not something we parse
        }
        let width = first.leading_zeros() as usize + 1;
        if self.pos + width > self.data.len() {
            return None;
        }
        let mut value = if keep_marker {
            first as u64
        } else {
            (first as u64) & !(1 << (8 - width))
        };
        for i in 1..width {
            value = (value << 8) | self.data[self.pos + i] as u64;
        }
        self.pos += width;
        Some((value, width))
    }

    /// The next element: its id, and where its body is.
    fn element(&mut self) -> Option<(u64, usize, usize)> {
        let id = self.vint(true)?;
        let (size, width) = self.vint_sized(false)?;
        let start = self.pos;
        // An unknown size means "to the end of the parent", which live-muxed
        // files use for the segment. It is every *value* bit set, so how many
        // bits there are has to be known: a one-byte size of 1 is the number
        // one, while a one-byte size of 127 is unknown.
        let value_bits = 7 * width;
        let unknown = value_bits < 64 && size == (1u64 << value_bits) - 1;
        let len = if unknown || start + size as usize > self.data.len() {
            self.data.len() - start
        } else {
            size as usize
        };
        self.pos = start + len;
        Some((id, start, len))
    }
}

fn unsigned(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0u64, |acc, b| (acc << 8) | *b as u64)
}

/// The palette line out of a VobSub track's private data.
///
/// The private data is the text of a `.idx` file, so the palette is the same
/// line `mkvextract` would have written into one.
pub fn parse_palette(private: &[u8]) -> Vec<[u8; 3]> {
    let text = String::from_utf8_lossy(private);
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("palette:") {
            return rest
                .split(',')
                .filter_map(|tok| u32::from_str_radix(tok.trim(), 16).ok())
                .map(|v| [(v >> 16) as u8, (v >> 8) as u8, v as u8])
                .collect();
        }
    }
    Vec::new()
}

/// What a track says about itself.
#[derive(Debug, Clone, Default)]
struct TrackInfo {
    number: u64,
    codec: String,
    private: Vec<u8>,
    is_subtitle: bool,
}

fn read_tracks(data: &[u8], start: usize, len: usize) -> Vec<TrackInfo> {
    let mut out = Vec::new();
    let mut r = Reader::at(&data[..start + len], start);
    while r.pos < start + len {
        let Some((eid, s, l)) = r.element() else { break };
        if eid != id::TRACK_ENTRY {
            continue;
        }
        let mut track = TrackInfo::default();
        let mut e = Reader::at(&data[..s + l], s);
        while e.pos < s + l {
            let Some((fid, fs, fl)) = e.element() else { break };
            let body = &data[fs..fs + fl];
            match fid {
                id::TRACK_NUMBER => track.number = unsigned(body),
                id::TRACK_TYPE => track.is_subtitle = unsigned(body) == SUBTITLE,
                id::CODEC_ID => track.codec = String::from_utf8_lossy(body).into_owned(),
                id::CODEC_PRIVATE => track.private = body.to_vec(),
                _ => {}
            }
        }
        out.push(track);
    }
    out
}

/// The blocks belonging to one track, with their times.
fn read_clusters(data: &[u8], start: usize, len: usize, track: u64, scale: u64) -> Vec<Packet> {
    let mut out = Vec::new();
    let mut r = Reader::at(&data[..start + len], start);

    while r.pos < start + len {
        let Some((eid, s, l)) = r.element() else { break };
        if eid != id::CLUSTER {
            continue;
        }
        let mut cluster_time = 0u64;
        let mut c = Reader::at(&data[..s + l], s);
        while c.pos < s + l {
            let Some((cid, cs, cl)) = c.element() else { break };
            match cid {
                id::TIMESTAMP => cluster_time = unsigned(&data[cs..cs + cl]),
                id::SIMPLE_BLOCK => {
                    if let Some(p) = read_block(&data[cs..cs + cl], track, cluster_time, scale) {
                        out.push(p);
                    }
                }
                // A block group wraps a block that has a duration or is
                // referenced; the block inside is the same shape.
                id::BLOCK_GROUP => {
                    let mut g = Reader::at(&data[..cs + cl], cs);
                    while g.pos < cs + cl {
                        let Some((gid, gs, gl)) = g.element() else { break };
                        if gid == id::BLOCK
                            && let Some(p) =
                                read_block(&data[gs..gs + gl], track, cluster_time, scale)
                        {
                            out.push(p);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// One block: track number, a signed offset from the cluster, flags, payload.
fn read_block(block: &[u8], wanted: u64, cluster_time: u64, scale: u64) -> Option<Packet> {
    let mut r = Reader::new(block);
    let track = r.vint(false)?;
    if track != wanted {
        return None;
    }
    if r.pos + 3 > block.len() {
        return None;
    }
    let offset = i16::from_be_bytes([block[r.pos], block[r.pos + 1]]) as i64;
    // r.pos + 2 is the flags byte, which says nothing a subtitle needs
    let payload = &block[r.pos + 3..];
    if payload.is_empty() {
        return None;
    }
    let ticks = cluster_time as i64 + offset;
    // Nanoseconds to milliseconds, once, rather than per use.
    let start_ms = (ticks.max(0) as u64).saturating_mul(scale) / 1_000_000;
    Some(Packet {
        start_ms,
        data: payload.to_vec(),
    })
}

/// Read the *n*th VobSub track, counting subtitle tracks from zero.
///
/// The numbering matches ffmpeg's `0:s:n`, so a caller that chose a stream by
/// probing can ask for the same one here.
pub fn read_vobsub(data: &[u8], subtitle_index: usize) -> Result<VobSubTrack> {
    let mut r = Reader::new(data);
    let mut segment = None;
    while r.pos < data.len() {
        let Some((eid, s, l)) = r.element() else { break };
        if eid == id::SEGMENT {
            segment = Some((s, l));
            break;
        }
    }
    let Some((seg_start, seg_len)) = segment else {
        return Err(Error("not a Matroska file".into()));
    };

    // One pass for the parts that describe the file, a second for the data,
    // because the tracks can be declared after the clusters begin.
    let mut scale = DEFAULT_TIMESTAMP_SCALE;
    let mut tracks: Vec<TrackInfo> = Vec::new();
    let mut r = Reader::at(&data[..seg_start + seg_len], seg_start);
    while r.pos < seg_start + seg_len {
        let Some((eid, s, l)) = r.element() else { break };
        match eid {
            id::INFO => {
                let mut i = Reader::at(&data[..s + l], s);
                while i.pos < s + l {
                    let Some((iid, is, il)) = i.element() else { break };
                    if iid == id::TIMESTAMP_SCALE {
                        scale = unsigned(&data[is..is + il]).max(1);
                    }
                }
            }
            id::TRACKS => tracks = read_tracks(data, s, l),
            _ => {}
        }
    }

    let subtitles: Vec<&TrackInfo> = tracks.iter().filter(|t| t.is_subtitle).collect();
    let track = subtitles.get(subtitle_index).ok_or_else(|| {
        Error(format!(
            "no subtitle track {subtitle_index} ({} in the file)",
            subtitles.len()
        ))
    })?;
    if !track.codec.starts_with("S_VOBSUB") {
        return Err(Error(format!(
            "subtitle track {subtitle_index} is {}, not VobSub",
            track.codec
        )));
    }

    Ok(VobSubTrack {
        palette: parse_palette(&track.private),
        packets: read_clusters(data, seg_start, seg_len, track.number, scale),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an element: id, then its size as a vint, then the body.
    fn element(id: u64, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let id_bytes = id.to_be_bytes();
        let first = id_bytes.iter().position(|b| *b != 0).unwrap_or(7);
        out.extend_from_slice(&id_bytes[first..]);
        // a four-byte size, which is always long enough here and always valid
        let size = body.len() as u32;
        out.push(0x10 | ((size >> 24) & 0x0F) as u8);
        out.push((size >> 16) as u8);
        out.push((size >> 8) as u8);
        out.push(size as u8);
        out.extend_from_slice(body);
        out
    }

    fn simple_block(track: u8, offset: i16, payload: &[u8]) -> Vec<u8> {
        let mut b = vec![0x80 | track];
        b.extend_from_slice(&offset.to_be_bytes());
        b.push(0x00);
        b.extend_from_slice(payload);
        element(id::SIMPLE_BLOCK, &b)
    }

    /// A file shaped like one ffmpeg writes for a single VobSub track.
    fn file() -> Vec<u8> {
        let private = b"size: 720x480\npalette: 000000, ffffff, 808080, 1a1a1a\n";
        let track = element(
            id::TRACK_ENTRY,
            &[
                element(id::TRACK_NUMBER, &[1]),
                element(id::TRACK_TYPE, &[0x11]),
                element(id::CODEC_ID, b"S_VOBSUB"),
                element(id::CODEC_PRIVATE, private),
            ]
            .concat(),
        );
        let info = element(id::INFO, &element(id::TIMESTAMP_SCALE, &[0x0F, 0x42, 0x40]));
        let cluster = element(
            id::CLUSTER,
            &[
                element(id::TIMESTAMP, &[0x03, 0xE8]), // 1000 ticks = 1 s
                simple_block(1, 100, b"first"),
                simple_block(1, 500, b"second"),
                simple_block(2, 0, b"another track"),
            ]
            .concat(),
        );
        let segment = element(id::SEGMENT, &[info, element(id::TRACKS, &track), cluster].concat());
        [element(0x1A45_DFA3, b"ebml"), segment].concat()
    }

    #[test]
    fn the_palette_comes_out_of_the_track_private_data() {
        // which is the text of a .idx - the same line mkvextract would write
        let t = read_vobsub(&file(), 0).unwrap();
        assert_eq!(t.palette.len(), 4);
        assert_eq!(t.palette[0], [0, 0, 0]);
        assert_eq!(t.palette[1], [0xFF, 0xFF, 0xFF]);
        assert_eq!(t.palette[2], [0x80, 0x80, 0x80]);
    }

    #[test]
    fn packets_come_out_with_their_times() {
        let t = read_vobsub(&file(), 0).unwrap();
        assert_eq!(t.packets.len(), 2, "the other track's block must not appear");
        // cluster at 1000 ticks, block at +100, one tick is a millisecond
        assert_eq!(t.packets[0].start_ms, 1100);
        assert_eq!(t.packets[0].data, b"first");
        assert_eq!(t.packets[1].start_ms, 1500);
    }

    #[test]
    fn the_timestamp_scale_is_honoured() {
        // a file that counts in ten-millisecond ticks rather than milliseconds
        let mut data = file();
        let scale = element(id::TIMESTAMP_SCALE, &[0x00, 0x98, 0x96, 0x80]); // 10 ms
        let info = element(id::INFO, &scale);
        let old = element(id::INFO, &element(id::TIMESTAMP_SCALE, &[0x0F, 0x42, 0x40]));
        let at = data
            .windows(old.len())
            .position(|w| w == old.as_slice())
            .expect("the info element is in there");
        data.splice(at..at + old.len(), info);
        let t = read_vobsub(&data, 0).unwrap();
        assert_eq!(t.packets[0].start_ms, 11_000, "ten times the ticks");
    }

    #[test]
    fn a_block_group_holds_a_block_of_the_same_shape() {
        let private = b"palette: 000000\n";
        let track = element(
            id::TRACK_ENTRY,
            &[
                element(id::TRACK_NUMBER, &[1]),
                element(id::TRACK_TYPE, &[0x11]),
                element(id::CODEC_ID, b"S_VOBSUB"),
                element(id::CODEC_PRIVATE, private),
            ]
            .concat(),
        );
        let mut block = vec![0x81u8];
        block.extend_from_slice(&50i16.to_be_bytes());
        block.push(0);
        block.extend_from_slice(b"grouped");
        let cluster = element(
            id::CLUSTER,
            &[
                element(id::TIMESTAMP, &[0x00]),
                element(id::BLOCK_GROUP, &element(id::BLOCK, &block)),
            ]
            .concat(),
        );
        let segment = element(id::SEGMENT, &[element(id::TRACKS, &track), cluster].concat());
        let t = read_vobsub(&segment, 0).unwrap();
        assert_eq!(t.packets.len(), 1);
        assert_eq!(t.packets[0].data, b"grouped");
        assert_eq!(t.packets[0].start_ms, 50);
    }

    #[test]
    fn subtitle_tracks_are_numbered_as_ffmpeg_numbers_them() {
        // so a stream chosen by probing can be asked for here by the same index
        let entry = |n: u8, kind: u8, codec: &[u8]| {
            element(
                id::TRACK_ENTRY,
                &[
                    element(id::TRACK_NUMBER, &[n]),
                    element(id::TRACK_TYPE, &[kind]),
                    element(id::CODEC_ID, codec),
                    element(id::CODEC_PRIVATE, b"palette: 00ff00\n"),
                ]
                .concat(),
            )
        };
        let tracks = element(
            id::TRACKS,
            &[
                entry(1, 0x01, b"V_MPEG2"),   // video, not counted
                entry(2, 0x11, b"S_VOBSUB"),  // subtitle 0
                entry(3, 0x11, b"S_VOBSUB"),  // subtitle 1
            ]
            .concat(),
        );
        let cluster = element(
            id::CLUSTER,
            &[element(id::TIMESTAMP, &[0]), simple_block(3, 0, b"from the second")].concat(),
        );
        let segment = element(id::SEGMENT, &[tracks, cluster].concat());
        let t = read_vobsub(&segment, 1).unwrap();
        assert_eq!(t.packets[0].data, b"from the second");
    }

    #[test]
    fn a_text_track_is_refused_rather_than_decoded_as_pictures() {
        let track = element(
            id::TRACK_ENTRY,
            &[
                element(id::TRACK_NUMBER, &[1]),
                element(id::TRACK_TYPE, &[0x11]),
                element(id::CODEC_ID, b"S_TEXT/UTF8"),
            ]
            .concat(),
        );
        let segment = element(id::SEGMENT, &element(id::TRACKS, &track));
        let e = read_vobsub(&segment, 0).unwrap_err();
        assert!(e.0.contains("not VobSub"), "{}", e.0);
    }

    #[test]
    fn asking_for_a_track_that_is_not_there_says_how_many_are() {
        let e = read_vobsub(&file(), 5).unwrap_err();
        assert!(e.0.contains("no subtitle track 5"), "{}", e.0);
        assert!(e.0.contains("1 in the file"), "{}", e.0);
    }

    #[test]
    fn something_that_is_not_matroska_is_refused() {
        assert!(read_vobsub(b"not a matroska file at all", 0).is_err());
        assert!(read_vobsub(&[], 0).is_err());
    }
}

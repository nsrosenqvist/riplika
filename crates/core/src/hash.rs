//! Checksums, computed as the bytes go past.
//!
//! A game image is gigabytes, so nothing here ever holds the whole of one:
//! [`Hasher`] is fed a chunk at a time and keeps only what it needs between
//! them. That is also why the hashes are written out rather than pulled in as
//! dependencies - the streaming shape is the requirement, and both of these
//! are short.
//!
//! CRC32 and SHA-1 because those are what a preservation database quotes. CRC32
//! is the traditional index and cheap; SHA-1 is what settles it, since a CRC32
//! is short enough to collide on purpose and games have been known to.

use crate::Result;
use crate::host::Fs;
use std::path::Path;

/// How much is read at a time. Large enough that the read dominates the
/// syscall, small enough not to matter to a machine.
pub const CHUNK: usize = 4 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Digests {
    pub crc32: u32,
    pub sha1: [u8; 20],
    pub bytes: u64,
}

impl Digests {
    /// As a preservation database writes them: lower case, no separators.
    pub fn crc32_hex(&self) -> String {
        format!("{:08x}", self.crc32)
    }

    pub fn sha1_hex(&self) -> String {
        self.sha1.iter().map(|b| format!("{b:02x}")).collect()
    }
}

#[derive(Default)]
pub struct Hasher {
    crc: Crc32,
    sha: Sha1,
    bytes: u64,
}

impl Hasher {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, data: &[u8]) {
        self.crc.update(data);
        self.sha.update(data);
        self.bytes += data.len() as u64;
    }

    pub fn finish(self) -> Digests {
        Digests { crc32: self.crc.finish(), sha1: self.sha.finish(), bytes: self.bytes }
    }
}

/// Hash a whole file, reporting how far along it is.
pub fn of_file(fs: &dyn Fs, path: &Path, progress: &mut dyn FnMut(u64, u64)) -> Result<Digests> {
    let total = fs.size(path)?;
    let mut hasher = Hasher::new();
    let mut at = 0u64;
    while at < total {
        let want = CHUNK.min((total - at) as usize);
        let chunk = fs.read_range(path, at, want)?;
        if chunk.is_empty() {
            break;
        }
        hasher.update(&chunk);
        at += chunk.len() as u64;
        progress(at, total);
    }
    Ok(hasher.finish())
}

const CRC_TABLE: [u32; 256] = crc_table();

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

/// CRC-32 as everything from zip to PNG to the preservation databases uses it.
pub struct Crc32(u32);

impl Default for Crc32 {
    fn default() -> Self {
        Crc32(0xFFFF_FFFF)
    }
}

impl Crc32 {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, data: &[u8]) {
        let mut c = self.0;
        for byte in data {
            c = CRC_TABLE[((c ^ u32::from(*byte)) & 0xFF) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }

    pub fn finish(self) -> u32 {
        !self.0
    }
}

/// SHA-1, fed a chunk at a time.
pub struct Sha1 {
    state: [u32; 5],
    /// Bytes seen, which is also what the length padding needs at the end.
    length: u64,
    /// Whatever did not fill a block last time.
    buffer: [u8; 64],
    buffered: usize,
}

impl Default for Sha1 {
    fn default() -> Self {
        Sha1 {
            state: [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0],
            length: 0,
            buffer: [0; 64],
            buffered: 0,
        }
    }
}

impl Sha1 {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.length = self.length.wrapping_add(data.len() as u64);
        if self.buffered > 0 {
            let take = (64 - self.buffered).min(data.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&data[..take]);
            self.buffered += take;
            data = &data[take..];
            if self.buffered < 64 {
                // Still a part block, and the tail handling below would
                // overwrite the count with zero and lose what is held.
                return;
            }
            let block = self.buffer;
            self.compress(&block);
            self.buffered = 0;
        }
        let mut blocks = data.chunks_exact(64);
        for block in &mut blocks {
            let mut fixed = [0u8; 64];
            fixed.copy_from_slice(block);
            self.compress(&fixed);
        }
        let rest = blocks.remainder();
        self.buffer[..rest.len()].copy_from_slice(rest);
        self.buffered = rest.len();
    }

    pub fn finish(mut self) -> [u8; 20] {
        let bits = self.length.wrapping_mul(8);
        self.update(&[0x80]);
        // The length has to land in the last eight bytes of a block, so pad to
        // fifty-six and no further.
        while self.buffered != 56 {
            self.update(&[0]);
        }
        // `update` has been counting these into the length; the value written
        // is the one taken before any of it.
        let mut tail = [0u8; 64];
        tail[..self.buffered].copy_from_slice(&self.buffer[..self.buffered]);
        tail[56..].copy_from_slice(&bits.to_be_bytes());
        self.compress(&tail);

        let mut out = [0u8; 20];
        for (chunk, word) in out.chunks_exact_mut(4).zip(self.state) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 80];
        for (i, c) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = self.state;
        for (i, word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (slot, add) in self.state.iter_mut().zip([a, b, c, d, e]) {
            *slot = slot.wrapping_add(add);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeFs;

    fn sha1_of(data: &[u8]) -> String {
        let mut s = Sha1::new();
        s.update(data);
        s.finish().iter().map(|b| format!("{b:02x}")).collect()
    }

    fn crc_of(data: &[u8]) -> String {
        let mut c = Crc32::new();
        c.update(data);
        format!("{:08x}", c.finish())
    }

    #[test]
    fn sha1_agrees_with_the_published_vectors() {
        assert_eq!(sha1_of(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_of(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1_of(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    #[test]
    fn crc32_agrees_with_what_zlib_says() {
        assert_eq!(crc_of(b""), "00000000");
        assert_eq!(crc_of(b"abc"), "352441c2");
        assert_eq!(crc_of(b"123456789"), "cbf43926");
    }

    /// The whole point of the streaming shape: a gigabyte arrives in pieces,
    /// and where the pieces fall must not change the answer.
    #[test]
    fn how_the_bytes_are_split_makes_no_difference() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i.wrapping_mul(31) >> 3) as u8).collect();
        let whole = {
            let mut h = Hasher::new();
            h.update(&data);
            h.finish()
        };
        // Sizes chosen to land on both sides of the sixty-four byte block: one
        // that never fills a block, one that fills exactly, one that straddles.
        for size in [1, 7, 63, 64, 65, 100, 128, 999, 4096] {
            let mut h = Hasher::new();
            for piece in data.chunks(size) {
                h.update(piece);
            }
            assert_eq!(h.finish(), whole, "split into {size}-byte pieces");
        }
    }

    #[test]
    fn a_length_that_lands_exactly_on_a_block_boundary_still_pads() {
        // The padding has to spill into a block of its own here, which is the
        // case an implementation that pads to fifty-six gets wrong.
        for len in [55, 56, 57, 63, 64, 119, 120, 128] {
            let data = vec![b'a'; len];
            let mut streamed = Sha1::new();
            for b in &data {
                streamed.update(&[*b]);
            }
            let at_once = sha1_of(&data);
            let bit_by_bit: String = streamed.finish().iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(bit_by_bit, at_once, "length {len}");
        }
    }

    #[test]
    fn the_byte_count_comes_back_with_the_hashes() {
        let mut h = Hasher::new();
        h.update(&[0u8; 100]);
        h.update(&[0u8; 23]);
        assert_eq!(h.finish().bytes, 123);
    }

    #[test]
    fn a_file_is_read_in_pieces_and_hashed_whole() {
        let text = "the quick brown fox".repeat(1000);
        let fs = FakeFs::new().with_file("/x.iso", &text);
        let mut seen = Vec::new();
        let digests =
            of_file(&fs, Path::new("/x.iso"), &mut |at, total| seen.push((at, total))).unwrap();
        assert_eq!(digests.bytes, text.len() as u64);
        assert_eq!(digests.sha1_hex(), sha1_of(text.as_bytes()));
        assert_eq!(digests.crc32_hex(), crc_of(text.as_bytes()));
        assert_eq!(seen.last(), Some(&(text.len() as u64, text.len() as u64)));
    }

    #[test]
    fn an_empty_file_hashes_rather_than_hanging() {
        let fs = FakeFs::new().with_file("/empty.iso", "");
        let d = of_file(&fs, Path::new("/empty.iso"), &mut |_, _| {}).unwrap();
        assert_eq!(d.bytes, 0);
        assert_eq!(d.sha1_hex(), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn the_hex_is_written_the_way_a_database_quotes_it() {
        let d = Digests { crc32: 0x0a1b, sha1: [0xff; 20], bytes: 0 };
        assert_eq!(d.crc32_hex(), "00000a1b", "padded to eight, lower case");
        assert_eq!(d.sha1_hex().len(), 40);
        assert!(d.sha1_hex().chars().all(|c| c.is_ascii_hexdigit()));
    }
}

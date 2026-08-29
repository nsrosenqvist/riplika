//! Repairing one thing ffmpeg leaves behind in an MP4.
//!
//! When chapters are carried from Matroska into MP4, ffmpeg writes them into a
//! Nero `chpl` atom - which every player in this pipeline reads - and *also*
//! writes a `tref`/`chap` reference on each track pointing at a QuickTime
//! chapter track that it never writes. The result is a file that says it has a
//! chapter track and does not, which ffprobe reports on every read:
//!
//! ```text
//! [mov,mp4,...] Referenced QT chapter track not found
//! ```
//!
//! Every ffmpeg tried does it, with or without explicit stream mapping, and no
//! muxer flag prevents it; `disable_chpl` removes the chapters and leaves the
//! dangling reference behind, which is the worse half of the trade.
//!
//! So the reference is removed afterwards, by renaming the box to `free`. An
//! MP4 reader skips a `free` box wherever it appears, and renaming rather than
//! deleting keeps every byte where it was - which matters, because the sample
//! tables elsewhere in the file record absolute offsets and shifting anything
//! would invalidate all of them.

use crate::Result;
use crate::host::Fs;
use std::path::Path;

/// A box header: its size, its type, and where its body starts.
struct Header {
    size: u64,
    kind: [u8; 4],
    body: usize,
}

fn header(d: &[u8], at: usize) -> Option<Header> {
    let end = at.checked_add(8)?;
    if end > d.len() {
        return None;
    }
    let mut size = u32::from_be_bytes(d[at..at + 4].try_into().ok()?) as u64;
    let kind: [u8; 4] = d[at + 4..at + 8].try_into().ok()?;
    let mut body = at + 8;
    if size == 1 {
        // A 64-bit size, for boxes larger than 4 GB.
        if body + 8 > d.len() {
            return None;
        }
        size = u64::from_be_bytes(d[body..body + 8].try_into().ok()?);
        body += 8;
    }
    if size < (body - at) as u64 {
        return None;
    }
    Some(Header { size, kind, body })
}

fn children(d: &[u8], from: usize, to: usize) -> Vec<(usize, Header)> {
    let mut out = Vec::new();
    let mut at = from;
    while at + 8 <= to {
        let Some(h) = header(d, at) else { break };
        let next = match at.checked_add(h.size as usize) {
            Some(n) if n <= to => n,
            _ => break,
        };
        out.push((at, h));
        at = next;
    }
    out
}

/// The track ids a `moov` actually contains, from each track header.
fn track_ids(moov: &[u8]) -> Vec<u32> {
    let mut ids = Vec::new();
    for (at, trak) in children(moov, 0, moov.len()) {
        if &trak.kind != b"trak" {
            continue;
        }
        for (tat, tkhd) in children(moov, trak.body, at + trak.size as usize) {
            if &tkhd.kind != b"tkhd" {
                continue;
            }
            // version byte, three flag bytes, then times, then the id: at
            // version 1 the times are 64-bit rather than 32.
            let version = moov.get(tkhd.body).copied().unwrap_or(0);
            let id_at = tkhd.body + 4 + if version == 1 { 16 } else { 8 };
            if id_at + 4 <= tat + tkhd.size as usize && id_at + 4 <= moov.len() {
                ids.push(u32::from_be_bytes(moov[id_at..id_at + 4].try_into().unwrap()));
            }
        }
    }
    ids
}

/// Offsets, within `moov`, of chapter references pointing at no track.
///
/// Only references that name a track this file does not contain: a real one is
/// left alone, because a file that genuinely has a chapter track needs it.
pub fn dangling_chapter_refs(moov: &[u8]) -> Vec<usize> {
    let present = track_ids(moov);
    let mut out = Vec::new();
    for (at, trak) in children(moov, 0, moov.len()) {
        if &trak.kind != b"trak" {
            continue;
        }
        let trak_end = at + trak.size as usize;
        for (rat, tref) in children(moov, trak.body, trak_end) {
            if &tref.kind != b"tref" {
                continue;
            }
            let tref_end = rat + tref.size as usize;
            let mut refs = 0;
            let mut dangling = 0;
            for (cat, chap) in children(moov, tref.body, tref_end) {
                if &chap.kind != b"chap" {
                    continue;
                }
                let mut idx = chap.body;
                while idx + 4 <= cat + chap.size as usize && idx + 4 <= moov.len() {
                    let id = u32::from_be_bytes(moov[idx..idx + 4].try_into().unwrap());
                    refs += 1;
                    if !present.contains(&id) {
                        dangling += 1;
                    }
                    idx += 4;
                }
            }
            // Only when every reference in it is dangling: a tref carrying
            // something real as well must keep it.
            if refs > 0 && refs == dangling {
                out.push(rat);
            }
        }
    }
    out
}

/// Find `moov`, and its offset in the file, without reading the whole file.
fn find_moov(fs: &dyn Fs, path: &Path) -> Result<Option<(u64, Vec<u8>)>> {
    let mut at: u64 = 0;
    loop {
        let head = fs.read_range(path, at, 16)?;
        let Some(h) = header(&head, 0) else { return Ok(None) };
        if &h.kind == b"moov" {
            let body = at + h.body as u64;
            let len = h.size - h.body as u64;
            // A moov is a table of contents, not the video, so it is megabytes
            // at worst. A larger one means the file is not what it claims, and
            // reading it into memory on trust is how a bad file becomes a
            // crash.
            const SANE: u64 = 256 * 1024 * 1024;
            if len > SANE {
                return Ok(None);
            }
            let data = fs.read_range(path, body, len as usize)?;
            return Ok(Some((body, data)));
        }
        // A zero or overflowing size cannot advance, and a loop that cannot
        // advance does not end.
        match at.checked_add(h.size) {
            Some(next) if next > at => at = next,
            _ => return Ok(None),
        }
    }
}

/// Remove chapter references pointing at a track that was never written.
///
/// Returns how many were removed. Missing or unreadable structure is not an
/// error: the file is playable either way, and this is tidying rather than
/// producing.
pub fn drop_dangling_chapter_refs(fs: &dyn Fs, path: &Path) -> Result<usize> {
    let Some((offset, moov)) = find_moov(fs, path)? else { return Ok(0) };
    let found = dangling_chapter_refs(&moov);
    for at in &found {
        // The type field, four bytes in. Same length, so nothing moves.
        fs.write_at(path, offset + *at as u64 + 4, b"free")?;
    }
    Ok(found.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeFs;

    /// A box: four-byte type, four-byte length, contents.
    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn tkhd(id: u32) -> Vec<u8> {
        let mut body = vec![0u8; 4]; // version 0, then flags
        body.extend_from_slice(&[0; 8]); // creation and modification times
        body.extend_from_slice(&id.to_be_bytes());
        body.extend_from_slice(&[0; 60]);
        boxed(b"tkhd", &body)
    }

    fn tref_chap(points_at: u32) -> Vec<u8> {
        boxed(b"tref", &boxed(b"chap", &points_at.to_be_bytes()))
    }

    fn moov(traks: &[Vec<u8>]) -> Vec<u8> {
        traks.concat()
    }

    #[test]
    fn a_reference_to_a_track_that_is_not_there_is_found() {
        // ffmpeg's own output: two real tracks, each pointing at a chapter
        // track numbered past the end of the file
        let trak1 = boxed(b"trak", &[tkhd(1), tref_chap(3)].concat());
        let trak2 = boxed(b"trak", &[tkhd(2), tref_chap(3)].concat());
        let m = moov(&[trak1, trak2]);
        assert_eq!(dangling_chapter_refs(&m).len(), 2);
    }

    #[test]
    fn a_reference_to_a_track_that_is_there_is_left_alone() {
        // a file that really does carry a chapter track needs its reference
        let trak1 = boxed(b"trak", &[tkhd(1), tref_chap(2)].concat());
        let trak2 = boxed(b"trak", &tkhd(2));
        assert!(dangling_chapter_refs(&moov(&[trak1, trak2])).is_empty());
    }

    #[test]
    fn a_file_with_no_chapter_reference_is_untouched() {
        let m = moov(&[boxed(b"trak", &tkhd(1))]);
        assert!(dangling_chapter_refs(&m).is_empty());
    }

    #[test]
    fn rubbish_is_not_a_panic() {
        // this reads files produced elsewhere, so it meets malformed ones
        for bad in [
            vec![],
            vec![0u8; 3],
            vec![0xff; 64],
            b"\x00\x00\x00\x08trak".to_vec(),
            [&2u32.to_be_bytes()[..], b"trak"].concat(),
        ] {
            let _ = dangling_chapter_refs(&bad);
        }
    }

    #[test]
    fn the_reference_is_renamed_and_nothing_moves() {
        // Renamed rather than removed: sample tables elsewhere record absolute
        // offsets, so shifting a single byte would invalidate every one.
        let trak = boxed(b"trak", &[tkhd(1), tref_chap(3)].concat());
        let file = [boxed(b"ftyp", b"isom"), boxed(b"moov", &trak)].concat();
        let fs = FakeFs::new();
        let path = Path::new("/out/ep.mp4.part");
        fs.write(path, &file).unwrap();

        assert_eq!(drop_dangling_chapter_refs(&fs, path).unwrap(), 1);

        let after = fs.read(path).unwrap();
        assert_eq!(after.len(), file.len(), "the file changed length");
        assert_eq!(
            after.windows(4).filter(|w| *w == b"free").count(),
            1,
            "the reference was not renamed"
        );
        assert_eq!(after.windows(4).filter(|w| *w == b"tref").count(), 0);
        // and every byte that changed lies inside that one type field
        // ("tref" and "free" share two letters in place, so only two of the
        // four actually differ)
        let was = file.windows(4).position(|w| w == b"tref").unwrap();
        let changed: Vec<usize> = (0..file.len()).filter(|&i| after[i] != file[i]).collect();
        assert!(
            changed.iter().all(|i| (was..was + 4).contains(i)),
            "bytes outside the type field were rewritten: {changed:?}"
        );
    }

    #[test]
    fn a_file_with_no_moov_is_not_an_error() {
        let fs = FakeFs::new();
        let path = Path::new("/out/ep.mp4.part");
        fs.write(path, &boxed(b"ftyp", b"isom")).unwrap();
        assert_eq!(drop_dangling_chapter_refs(&fs, path).unwrap(), 0);
    }
}

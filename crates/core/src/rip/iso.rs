//! Reading a DVD's own table of contents.
//!
//! Probing titles one by one answers "what is title N", but not "how many
//! titles are there", and guessing that is dangerous. This disc has content at
//! titles 2-19 and again at 39-58 with a seventeen-title hole between, so any
//! stop-after-N-empties rule either gives up in the hole - returning a Parks
//! and Recreation disc with no episodes on it, which looks like a disc that
//! simply has none - or never stops early and probes all 99.
//!
//! The disc already knows. `VIDEO_TS.IFO` carries a title table, and reading it
//! costs three sector reads and no disc spin-up beyond what has already
//! happened. So this walks ISO 9660 to the IFO and reads the count, and the
//! probe loop then has an exact bound.
//!
//! Everything here is parsing, and all of it is pure: the caller supplies the
//! sectors.

use crate::{Error, Result};

/// ISO 9660 and DVD-Video both use 2 KB sectors.
pub const SECTOR: usize = 2048;

/// The primary volume descriptor is always sector 16.
pub const PVD_SECTOR: u64 = 16;

/// One entry in an ISO 9660 directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub lba: u32,
    pub size: u32,
    pub is_dir: bool,
}

/// One entry in the DVD's title table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TitleEntry {
    /// Title number as the demuxer's `-title` option counts them, from 1.
    pub number: u32,
    pub chapters: u16,
    /// Which video title set holds it.
    pub vts: u8,
    /// Which title within that set.
    pub title_in_vts: u8,
}

/// Where the root directory lives, from the primary volume descriptor.
pub fn parse_pvd_root(pvd: &[u8]) -> Option<(u32, u32)> {
    if pvd.len() < 190 || pvd[0] != 1 || &pvd[1..6] != b"CD001" {
        return None;
    }
    // the root directory record is embedded at offset 156
    let rec = &pvd[156..190];
    let lba = u32::from_le_bytes([rec[2], rec[3], rec[4], rec[5]]);
    let size = u32::from_le_bytes([rec[10], rec[11], rec[12], rec[13]]);
    if lba == 0 || size == 0 {
        return None;
    }
    Some((lba, size))
}

/// Parse the records in a directory extent.
pub fn dir_entries(data: &[u8], length: usize) -> Vec<DirEntry> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let end = length.min(data.len());
    while i < end {
        let rec_len = data[i] as usize;
        if rec_len == 0 {
            // Records never straddle a sector; a zero length means padding to
            // the end of this one.
            i = (i / SECTOR + 1) * SECTOR;
            continue;
        }
        if i + rec_len > end || rec_len < 33 {
            break;
        }
        let rec = &data[i..i + rec_len];
        let name_len = rec[32] as usize;
        if 33 + name_len <= rec_len {
            let raw = &rec[33..33 + name_len];
            // Names carry a ";1" version suffix that nothing else uses.
            let name: String = raw.iter().map(|b| *b as char).collect();
            let name = name.split(';').next().unwrap_or("").trim_end_matches('.').to_string();
            out.push(DirEntry {
                name,
                lba: u32::from_le_bytes([rec[2], rec[3], rec[4], rec[5]]),
                size: u32::from_le_bytes([rec[10], rec[11], rec[12], rec[13]]),
                is_dir: rec[25] & 0x02 != 0,
            });
        }
        i += rec_len;
    }
    out
}

/// Sector offset of the title table, relative to the start of the IFO.
pub fn tt_srpt_offset(ifo: &[u8]) -> Option<u32> {
    if ifo.len() < 0xC8 || &ifo[..12] != b"DVDVIDEO-VMG" {
        return None;
    }
    // DVD-Video stores its numbers big-endian, unlike ISO 9660 around it
    let v = u32::from_be_bytes([ifo[0xC4], ifo[0xC5], ifo[0xC6], ifo[0xC7]]);
    if v == 0 { None } else { Some(v) }
}

/// Parse the title table.
pub fn parse_tt_srpt(data: &[u8]) -> Vec<TitleEntry> {
    if data.len() < 8 {
        return Vec::new();
    }
    let count = u16::from_be_bytes([data[0], data[1]]) as usize;
    let mut out = Vec::new();
    for i in 0..count {
        let off = 8 + i * 12;
        if off + 12 > data.len() {
            break;
        }
        let e = &data[off..off + 12];
        out.push(TitleEntry {
            number: i as u32 + 1,
            chapters: u16::from_be_bytes([e[2], e[3]]),
            vts: e[6],
            title_in_vts: e[7],
        });
    }
    out
}

/// Read the title table, given something that can read sectors.
///
/// The reader takes a starting sector and a count, and returns that many
/// sectors' worth of bytes.
pub fn title_table(read: &mut dyn FnMut(u64, usize) -> Result<Vec<u8>>) -> Result<Vec<TitleEntry>> {
    let pvd = read(PVD_SECTOR, 1)?;
    let (root_lba, root_size) =
        parse_pvd_root(&pvd).ok_or_else(|| Error("not an ISO 9660 volume".into()))?;

    let sectors = root_size.div_ceil(SECTOR as u32) as usize;
    let root = read(root_lba as u64, sectors)?;
    let video_ts = dir_entries(&root, root_size as usize)
        .into_iter()
        .find(|e| e.is_dir && e.name.eq_ignore_ascii_case("VIDEO_TS"))
        .ok_or_else(|| Error("no VIDEO_TS directory - not a DVD-Video disc".into()))?;

    let sectors = video_ts.size.div_ceil(SECTOR as u32) as usize;
    let dir = read(video_ts.lba as u64, sectors)?;
    let ifo = dir_entries(&dir, video_ts.size as usize)
        .into_iter()
        .find(|e| !e.is_dir && e.name.eq_ignore_ascii_case("VIDEO_TS.IFO"))
        .ok_or_else(|| Error("no VIDEO_TS.IFO".into()))?;

    let header = read(ifo.lba as u64, 1)?;
    let offset = tt_srpt_offset(&header)
        .ok_or_else(|| Error("VIDEO_TS.IFO has no title table".into()))?;

    // The table can exceed one sector: 12 bytes an entry, up to 99 titles.
    let table = read(ifo.lba as u64 + offset as u64, 2)?;
    let titles = parse_tt_srpt(&table);
    if titles.is_empty() {
        return Err(Error("the disc lists no titles".into()));
    }
    Ok(titles)
}

/// Read sectors from a block device or image file.
pub fn device_reader(path: &std::path::Path) -> Result<impl FnMut(u64, usize) -> Result<Vec<u8>>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file =
        std::fs::File::open(path).map_err(|e| Error(format!("{}: {e}", path.display())))?;
    let display = path.display().to_string();
    Ok(move |lba: u64, count: usize| -> Result<Vec<u8>> {
        file.seek(SeekFrom::Start(lba * SECTOR as u64))
            .map_err(|e| Error(format!("{display}: {e}")))?;
        let mut buf = vec![0u8; count * SECTOR];
        file.read_exact(&mut buf)
            .map_err(|e| Error(format!("{display}: {e}")))?;
        Ok(buf)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn dir_record(name: &str, lba: u32, size: u32, is_dir: bool) -> Vec<u8> {
        let name_bytes = name.as_bytes();
        let len = 33 + name_bytes.len();
        let len = if len % 2 == 1 { len + 1 } else { len };
        let mut r = vec![0u8; len];
        r[0] = len as u8;
        r[2..6].copy_from_slice(&lba.to_le_bytes());
        r[10..14].copy_from_slice(&size.to_le_bytes());
        r[25] = if is_dir { 0x02 } else { 0 };
        r[32] = name_bytes.len() as u8;
        r[33..33 + name_bytes.len()].copy_from_slice(name_bytes);
        r
    }

    fn pvd_with_root(lba: u32, size: u32) -> Vec<u8> {
        let mut v = vec![0u8; SECTOR];
        v[0] = 1;
        v[1..6].copy_from_slice(b"CD001");
        let rec = dir_record("\u{0}", lba, size, true);
        v[156..156 + rec.len()].copy_from_slice(&rec);
        v
    }

    /// A disc shaped like the real one: content, a hole, then more content.
    fn fake_disc() -> impl FnMut(u64, usize) -> Result<Vec<u8>> {
        let mut sectors: HashMap<u64, Vec<u8>> = HashMap::new();
        sectors.insert(PVD_SECTOR, pvd_with_root(259, SECTOR as u32));

        let mut root = vec![0u8; SECTOR];
        let rec = dir_record("VIDEO_TS", 261, SECTOR as u32, true);
        root[..rec.len()].copy_from_slice(&rec);
        sectors.insert(259, root);

        let mut vts = vec![0u8; SECTOR];
        let rec = dir_record("VIDEO_TS.IFO;1", 341, 30720, false);
        vts[..rec.len()].copy_from_slice(&rec);
        sectors.insert(261, vts);

        let mut ifo = vec![0u8; SECTOR];
        ifo[..12].copy_from_slice(b"DVDVIDEO-VMG");
        ifo[0xC4..0xC8].copy_from_slice(&1u32.to_be_bytes());
        sectors.insert(341, ifo);

        // 58 titles, the last ten with the shape of episodes
        let mut tt = vec![0u8; SECTOR * 2];
        tt[0..2].copy_from_slice(&58u16.to_be_bytes());
        for i in 0..58usize {
            let off = 8 + i * 12;
            let chapters: u16 = if (40..=46).contains(&i) { 5 } else { 2 };
            tt[off + 2..off + 4].copy_from_slice(&chapters.to_be_bytes());
            tt[off + 6] = 11;
            tt[off + 7] = (i % 9 + 1) as u8;
        }
        sectors.insert(342, tt);

        move |lba, count| {
            let mut out = Vec::with_capacity(count * SECTOR);
            for n in 0..count {
                out.extend(
                    sectors
                        .get(&(lba + n as u64))
                        .cloned()
                        .unwrap_or_else(|| vec![0u8; SECTOR]),
                );
            }
            Ok(out)
        }
    }

    #[test]
    fn the_title_count_comes_from_the_disc_rather_than_from_probing() {
        // the real disc: 58 titles, with a seventeen-title hole in the middle
        let mut read = fake_disc();
        let titles = title_table(&mut read).unwrap();
        assert_eq!(titles.len(), 58);
        assert_eq!(titles[0].number, 1);
        assert_eq!(titles[57].number, 58);
    }

    #[test]
    fn chapter_counts_come_through_and_identify_the_episodes() {
        let mut read = fake_disc();
        let titles = title_table(&mut read).unwrap();
        // titles 41-47 are the five-chapter episodes
        let five: Vec<u32> = titles.iter().filter(|t| t.chapters == 5).map(|t| t.number).collect();
        assert_eq!(five, vec![41, 42, 43, 44, 45, 46, 47]);
    }

    #[test]
    fn the_root_record_is_read_out_of_the_descriptor() {
        assert_eq!(parse_pvd_root(&pvd_with_root(259, 2048)), Some((259, 2048)));
        assert_eq!(parse_pvd_root(&[0u8; 200]), None);
        // a descriptor claiming a zero-length root is not usable
        assert_eq!(parse_pvd_root(&pvd_with_root(259, 0)), None);
    }

    #[test]
    fn the_version_suffix_is_stripped_from_names() {
        // ISO 9660 writes VIDEO_TS.IFO;1, and nothing else ever wants the ";1"
        let rec = dir_record("VIDEO_TS.IFO;1", 341, 30720, false);
        let e = dir_entries(&rec, rec.len());
        assert_eq!(e[0].name, "VIDEO_TS.IFO");
        assert!(!e[0].is_dir);
        assert_eq!(e[0].lba, 341);
    }

    #[test]
    fn directory_padding_skips_to_the_next_sector_rather_than_stopping() {
        let mut data = vec![0u8; SECTOR * 2];
        let a = dir_record("FIRST", 10, 20, false);
        data[..a.len()].copy_from_slice(&a);
        // rest of sector 0 is zero padding; the next record starts at sector 1
        let b = dir_record("SECOND", 30, 40, false);
        data[SECTOR..SECTOR + b.len()].copy_from_slice(&b);
        let e = dir_entries(&data, SECTOR * 2);
        assert_eq!(e.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), vec!["FIRST", "SECOND"]);
    }

    #[test]
    fn the_ifo_offset_is_big_endian_unlike_the_iso_around_it() {
        let mut ifo = vec![0u8; SECTOR];
        ifo[..12].copy_from_slice(b"DVDVIDEO-VMG");
        ifo[0xC4..0xC8].copy_from_slice(&1u32.to_be_bytes());
        assert_eq!(tt_srpt_offset(&ifo), Some(1));
        // reading it little-endian would give 16777216
        ifo[0xC4..0xC8].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(tt_srpt_offset(&ifo), Some(16_777_216));
    }

    #[test]
    fn a_non_dvd_volume_says_so_rather_than_returning_nothing() {
        let mut read = |lba: u64, _n: usize| -> Result<Vec<u8>> {
            Ok(if lba == PVD_SECTOR {
                pvd_with_root(259, SECTOR as u32)
            } else {
                vec![0u8; SECTOR] // an empty root: no VIDEO_TS
            })
        };
        let e = title_table(&mut read).unwrap_err();
        assert!(e.0.contains("VIDEO_TS"), "{}", e.0);
    }

    #[test]
    fn a_blank_disc_is_an_error() {
        let mut read = |_l: u64, _n: usize| -> Result<Vec<u8>> { Ok(vec![0u8; SECTOR]) };
        assert!(title_table(&mut read).unwrap_err().0.contains("ISO 9660"));
    }

    #[test]
    fn a_truncated_table_stops_cleanly_instead_of_reading_past_it() {
        let mut tt = vec![0u8; 40];
        tt[0..2].copy_from_slice(&99u16.to_be_bytes());
        // claims 99 titles but only holds a couple
        assert!(parse_tt_srpt(&tt).len() < 99);
        assert!(parse_tt_srpt(&[]).is_empty());
    }
}

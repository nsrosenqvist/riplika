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

/// One program chain: a title as the disc lays it out on the surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pgc {
    /// Position in the video title set's chain table, from 1.
    pub number: u32,
    pub seconds: u32,
    /// Cell extents, as sectors *relative to the title set's first VOB*.
    pub cells: Vec<(u64, u64)>,
}

impl Pgc {
    /// How many sectors this chain occupies, counting each once.
    pub fn sectors(&self) -> u64 {
        merge_ranges(&self.cells).iter().map(|(a, b)| b - a).sum()
    }
}

/// Combine overlapping or touching ranges. Play-all chains replay the same
/// cells as the episodes, so without this the same sectors are counted - and
/// would be read - several times over.
pub fn merge_ranges(ranges: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let mut sorted: Vec<(u64, u64)> = ranges.iter().copied().filter(|(a, b)| b > a).collect();
    sorted.sort();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (start, end) in sorted {
        match out.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => out.push((start, end)),
        }
    }
    out
}

/// A DVD time field: hours, minutes, seconds packed as binary-coded decimal.
fn bcd(b: u8) -> u32 {
    ((b >> 4) as u32) * 10 + (b & 0x0F) as u32
}

/// Parse the program chain table out of a video title set's IFO.
///
/// `ifo` must hold the IFO from its first sector; the chain table lives a few
/// sectors in and the offset is read from the header.
pub fn parse_vts_pgcs(ifo: &[u8]) -> Vec<Pgc> {
    if ifo.len() < 0xD0 || &ifo[..12] != b"DVDVIDEO-VTS" {
        return Vec::new();
    }
    let be32 = |b: &[u8], o: usize| -> u64 {
        if o + 4 > b.len() {
            return 0;
        }
        u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as u64
    };
    let be16 = |b: &[u8], o: usize| -> usize {
        if o + 2 > b.len() {
            return 0;
        }
        u16::from_be_bytes([b[o], b[o + 1]]) as usize
    };

    // the chain table's own sector, relative to the start of this IFO
    let table_sector = be32(ifo, 0xCC) as usize;
    let table_at = table_sector * SECTOR;
    if table_at + 8 > ifo.len() {
        return Vec::new();
    }
    let table = &ifo[table_at..];
    let count = be16(table, 0);

    let mut out = Vec::new();
    for i in 0..count {
        let entry = 8 + i * 8;
        if entry + 8 > table.len() {
            break;
        }
        let start = be32(table, entry + 4) as usize;
        if start >= table.len() {
            continue;
        }
        let pgc = &table[start..];
        if pgc.len() < 0xEC {
            continue;
        }
        let cells = pgc[3] as usize;
        let seconds = bcd(pgc[4]) * 3600 + bcd(pgc[5]) * 60 + bcd(pgc[6]);
        let cell_table = be16(pgc, 0xE8);
        let mut extents = Vec::new();
        for c in 0..cells {
            let at = cell_table + c * 24;
            if at + 24 > pgc.len() {
                break;
            }
            let first = be32(pgc, at + 0x08);
            let last = be32(pgc, at + 0x14);
            if last >= first {
                // half-open, so the arithmetic elsewhere is uniform
                extents.push((first, last + 1));
            }
        }
        out.push(Pgc {
            number: i as u32 + 1,
            seconds,
            cells: extents,
        });
    }
    out
}

/// Where a video title set's content actually is on the disc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleSet {
    pub number: u8,
    /// Absolute LBA of the first content VOB, which cell extents are relative to.
    pub vob_lba: u64,
    pub chains: Vec<Pgc>,
}

impl TitleSet {
    /// Absolute sector ranges for one chain.
    pub fn absolute(&self, pgc: &Pgc) -> Vec<(u64, u64)> {
        merge_ranges(
            &pgc.cells
                .iter()
                .map(|(a, b)| (a + self.vob_lba, b + self.vob_lba))
                .collect::<Vec<_>>(),
        )
    }
}

/// Read one video title set's layout.
pub fn title_set(
    read: &mut dyn FnMut(u64, usize) -> Result<Vec<u8>>,
    vts: u8,
) -> Result<TitleSet> {
    let pvd = read(PVD_SECTOR, 1)?;
    let (root_lba, root_size) =
        parse_pvd_root(&pvd).ok_or_else(|| Error("not an ISO 9660 volume".into()))?;
    let root = read(root_lba as u64, root_size.div_ceil(SECTOR as u32) as usize)?;
    let video_ts = dir_entries(&root, root_size as usize)
        .into_iter()
        .find(|e| e.is_dir && e.name.eq_ignore_ascii_case("VIDEO_TS"))
        .ok_or_else(|| Error("no VIDEO_TS directory".into()))?;
    let dir = read(
        video_ts.lba as u64,
        video_ts.size.div_ceil(SECTOR as u32) as usize,
    )?;
    let files = dir_entries(&dir, video_ts.size as usize);

    let find = |name: String| files.iter().find(|e| e.name.eq_ignore_ascii_case(&name)).cloned();
    let ifo = find(format!("VTS_{vts:02}_0.IFO"))
        .ok_or_else(|| Error(format!("no VTS_{vts:02}_0.IFO on this disc")))?;
    let vob = find(format!("VTS_{vts:02}_1.VOB"))
        .ok_or_else(|| Error(format!("no VTS_{vts:02}_1.VOB on this disc")))?;

    // The chain table is a few sectors into the IFO; read enough to hold it.
    let ifo_sectors = (ifo.size.div_ceil(SECTOR as u32) as usize).clamp(1, 64);
    let data = read(ifo.lba as u64, ifo_sectors)?;
    let chains = parse_vts_pgcs(&data);
    if chains.is_empty() {
        return Err(Error(format!("VTS {vts} lists no program chains")));
    }
    Ok(TitleSet {
        number: vts,
        vob_lba: vob.lba as u64,
        chains,
    })
}

#[cfg(test)]
mod cell_tests {
    use super::*;

    /// A title set with three chains: two episodes and a play-all that replays
    /// both, which is the shape a television disc actually has.
    fn vts_ifo() -> Vec<u8> {
        let mut ifo = vec![0u8; SECTOR * 4];
        ifo[..12].copy_from_slice(b"DVDVIDEO-VTS");
        ifo[0xCC..0xD0].copy_from_slice(&2u32.to_be_bytes()); // table two sectors in

        let table_at = 2 * SECTOR;
        let chains: [(u32, &[(u64, u64)]); 3] = [
            (1290, &[(0, 400_000)]),                    // episode one
            (1291, &[(400_000, 800_000)]),              // episode two
            (2581, &[(0, 400_000), (400_000, 800_000)]), // the play-all
        ];
        ifo[table_at..table_at + 2].copy_from_slice(&(chains.len() as u16).to_be_bytes());

        // chain bodies laid out after the entry table
        let mut body_at = 8 + chains.len() * 8;
        for (i, (seconds, cells)) in chains.iter().enumerate() {
            let entry = table_at + 8 + i * 8;
            ifo[entry + 4..entry + 8].copy_from_slice(&(body_at as u32).to_be_bytes());

            let pgc = table_at + body_at;
            ifo[pgc + 3] = cells.len() as u8;
            ifo[pgc + 4] = (((seconds / 3600) / 10) << 4) as u8 | ((seconds / 3600) % 10) as u8;
            let m = (seconds % 3600) / 60;
            ifo[pgc + 5] = ((m / 10) << 4) as u8 | (m % 10) as u8;
            let s = seconds % 60;
            ifo[pgc + 6] = ((s / 10) << 4) as u8 | (s % 10) as u8;
            let cell_table = 0x100usize;
            ifo[pgc + 0xE8..pgc + 0xEA].copy_from_slice(&(cell_table as u16).to_be_bytes());
            for (c, (first, last)) in cells.iter().enumerate() {
                let at = pgc + cell_table + c * 24;
                ifo[at + 0x08..at + 0x0C].copy_from_slice(&(*first as u32).to_be_bytes());
                ifo[at + 0x14..at + 0x18].copy_from_slice(&((*last - 1) as u32).to_be_bytes());
            }
            body_at += cell_table + cells.len() * 24;
        }
        ifo
    }

    #[test]
    fn chains_come_out_with_their_runtimes_and_extents() {
        let chains = parse_vts_pgcs(&vts_ifo());
        assert_eq!(chains.len(), 3);
        assert_eq!(chains[0].seconds, 1290);
        assert_eq!(chains[0].cells, vec![(0, 400_000)]);
        assert_eq!(chains[2].seconds, 2581);
        assert_eq!(chains[2].cells.len(), 2);
    }

    #[test]
    fn a_play_all_shares_its_sectors_with_the_episodes() {
        // this is why rescuing the episodes costs nothing extra for the play-all
        let chains = parse_vts_pgcs(&vts_ifo());
        let episodes = merge_ranges(
            &chains[..2].iter().flat_map(|c| c.cells.clone()).collect::<Vec<_>>(),
        );
        let play_all = merge_ranges(&chains[2].cells);
        assert_eq!(episodes, play_all);
        assert_eq!(episodes, vec![(0, 800_000)]);
    }

    #[test]
    fn overlapping_cells_are_counted_once() {
        assert_eq!(merge_ranges(&[(0, 100), (50, 150)]), vec![(0, 150)]);
        assert_eq!(merge_ranges(&[(0, 100), (100, 200)]), vec![(0, 200)]);
        assert_eq!(merge_ranges(&[(0, 100), (200, 300)]), vec![(0, 100), (200, 300)]);
        assert_eq!(merge_ranges(&[(50, 50)]), vec![]);
    }

    #[test]
    fn a_chains_size_ignores_the_cells_it_repeats() {
        let chains = parse_vts_pgcs(&vts_ifo());
        assert_eq!(chains[2].sectors(), 800_000);
    }

    #[test]
    fn cell_extents_are_relative_to_the_title_sets_vob() {
        let ts = TitleSet {
            number: 11,
            vob_lba: 485_863,
            chains: parse_vts_pgcs(&vts_ifo()),
        };
        let abs = ts.absolute(&ts.chains[1]);
        assert_eq!(abs, vec![(485_863 + 400_000, 485_863 + 800_000)]);
    }

    #[test]
    fn a_non_vts_ifo_yields_nothing_rather_than_nonsense() {
        let mut not_vts = vec![0u8; SECTOR * 4];
        not_vts[..12].copy_from_slice(b"DVDVIDEO-VMG");
        assert!(parse_vts_pgcs(&not_vts).is_empty());
        assert!(parse_vts_pgcs(&[]).is_empty());
    }

    #[test]
    fn a_truncated_ifo_stops_instead_of_reading_past_the_end() {
        let full = vts_ifo();
        for cut in [SECTOR, SECTOR * 2 + 16, SECTOR * 3] {
            let _ = parse_vts_pgcs(&full[..cut.min(full.len())]);
        }
    }
}

/// Sector ranges holding everything that makes a disc navigable.
///
/// A rescue that copies only the episodes produces an image no player can open:
/// the descriptors say where the files are, and the IFOs say where the titles
/// are inside them. They are a few megabytes in total, so they are always
/// included whatever else was asked for.
pub fn metadata_ranges(
    read: &mut dyn FnMut(u64, usize) -> Result<Vec<u8>>,
) -> Result<Vec<(u64, u64)>> {
    let pvd = read(PVD_SECTOR, 1)?;
    let (root_lba, root_size) =
        parse_pvd_root(&pvd).ok_or_else(|| Error("not an ISO 9660 volume".into()))?;
    let root = read(root_lba as u64, root_size.div_ceil(SECTOR as u32) as usize)?;
    let root_entries = dir_entries(&root, root_size as usize);

    let mut ranges: Vec<(u64, u64)> = Vec::new();
    // Seeded from the files themselves, not from the directory: UDF keeps its
    // file entries between the end of the ISO directory and the first file, so
    // stopping at the directory misses exactly the descriptors libdvdread
    // needs, and the image opens as a filesystem but not as a DVD.
    let mut first_file = u64::MAX;
    let mut last_end = root_lba as u64 + root_size.div_ceil(SECTOR as u32) as u64;

    for dir in root_entries.iter().filter(|e| e.is_dir) {
        if !dir.name.eq_ignore_ascii_case("VIDEO_TS") {
            continue;
        }
        let sectors = dir.size.div_ceil(SECTOR as u32) as u64;
        ranges.push((dir.lba as u64, dir.lba as u64 + sectors));
        let listing = read(dir.lba as u64, sectors as usize)?;
        for f in dir_entries(&listing, dir.size as usize) {
            if f.is_dir || f.lba == 0 {
                continue;
            }
            let end = f.lba as u64 + f.size.div_ceil(SECTOR as u32) as u64;
            first_file = first_file.min(f.lba as u64);
            last_end = last_end.max(end);
            // IFOs and their BUP copies; the VOBs are the payload and are
            // rescued only if asked for.
            let upper = f.name.to_ascii_uppercase();
            if upper.ends_with(".IFO") || upper.ends_with(".BUP") {
                ranges.push((f.lba as u64, end));
            }
        }
    }

    // Everything before the first file. A DVD is a hybrid: ISO 9660 for
    // compatibility and UDF for the players, and libdvdread reads the UDF. Its
    // anchor sits at sector 256 and its descriptors and file entries live in
    // this same region, so copying only the ISO structures produces an image
    // that opens as a filesystem and not as a DVD. The whole region is under a
    // megabyte, so there is nothing to be gained by being precise about it.
    ranges.push((0, if first_file == u64::MAX { last_end } else { first_file }));

    // UDF keeps backup anchors at fixed places near the end of the volume: 256
    // sectors before the last, and the last itself. Two sectors each, not a
    // blanket sweep - a wide range at the end would swallow payload on a small
    // volume, and reading past the last recorded sector is a guaranteed error
    // that would then be counted as damage.
    if last_end > 258 {
        ranges.push((last_end - 257, last_end - 255));
        ranges.push((last_end - 1, last_end));
    }

    Ok(merge_ranges(&ranges))
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    /// The real disc's layout: ISO descriptors, then UDF's anchor and file
    /// entries, then the files.
    fn disc() -> impl FnMut(u64, usize) -> Result<Vec<u8>> {
        use std::collections::HashMap;
        let mut sectors: HashMap<u64, Vec<u8>> = HashMap::new();

        let mut pvd = vec![0u8; SECTOR];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        let mut rec = vec![0u8; 34];
        rec[0] = 34;
        rec[2..6].copy_from_slice(&259u32.to_le_bytes());
        rec[10..14].copy_from_slice(&(SECTOR as u32).to_le_bytes());
        rec[25] = 0x02;
        rec[32] = 1;
        pvd[156..190].copy_from_slice(&rec);
        sectors.insert(PVD_SECTOR, pvd);

        let mut root = vec![0u8; SECTOR];
        let mut d = vec![0u8; 42];
        d[0] = 42;
        d[2..6].copy_from_slice(&261u32.to_le_bytes());
        d[10..14].copy_from_slice(&(SECTOR as u32).to_le_bytes());
        d[25] = 0x02;
        d[32] = 8;
        d[33..41].copy_from_slice(b"VIDEO_TS");
        root[..42].copy_from_slice(&d);
        sectors.insert(259, root);

        let mut dir = vec![0u8; SECTOR];
        let mut at = 0usize;
        // VIDEO_TS.IFO at 341, then the first VOB at 356
        for (name, lba, size) in [
            ("VIDEO_TS.IFO;1", 341u32, 30720u32),
            ("VIDEO_TS.VOB;1", 356, 1_200_000),
            ("VTS_01_0.IFO;1", 989, 30720),
        ] {
            let len = 33 + name.len() + (name.len() % 2 == 1) as usize;
            let mut r = vec![0u8; len];
            r[0] = len as u8;
            r[2..6].copy_from_slice(&lba.to_le_bytes());
            r[10..14].copy_from_slice(&size.to_le_bytes());
            r[32] = name.len() as u8;
            r[33..33 + name.len()].copy_from_slice(name.as_bytes());
            dir[at..at + len].copy_from_slice(&r);
            at += len;
        }
        sectors.insert(261, dir);

        move |lba, count| {
            let mut out = Vec::new();
            for n in 0..count {
                out.extend(sectors.get(&(lba + n as u64)).cloned().unwrap_or_else(|| vec![0u8; SECTOR]));
            }
            Ok(out)
        }
    }

    #[test]
    fn everything_before_the_first_file_is_copied() {
        // A DVD is ISO 9660 and UDF at once, and libdvdread reads the UDF. Its
        // file entries sit between the ISO directory and the first file, so a
        // copy that stops at the directory produces an image that opens as a
        // filesystem and not as a DVD - which is exactly what happened.
        let mut read = disc();
        let ranges = metadata_ranges(&mut read).unwrap();
        let covers = |lba: u64| ranges.iter().any(|(a, b)| lba >= *a && lba < *b);
        assert!(covers(16), "ISO primary volume descriptor");
        assert!(covers(32), "UDF main volume descriptor sequence");
        assert!(covers(256), "UDF anchor");
        assert!(covers(263), "UDF file set descriptor");
        assert!(covers(300), "UDF file entries");
        assert!(covers(340), "the sector before the first file");
    }

    #[test]
    fn the_ifos_are_copied_but_not_the_payload() {
        let mut read = disc();
        let ranges = metadata_ranges(&mut read).unwrap();
        let covers = |lba: u64| ranges.iter().any(|(a, b)| lba >= *a && lba < *b);
        assert!(covers(989), "VTS_01_0.IFO");
        // the video itself is only copied when asked for
        assert!(!covers(500), "a VOB sector was copied as metadata");
        assert!(!covers(700), "a VOB sector was copied as metadata");
    }

    #[test]
    fn nothing_is_read_past_the_end_of_the_volume() {
        // a read past the last recorded sector is a guaranteed error, and would
        // be counted as damage
        let mut read = disc();
        let ranges = metadata_ranges(&mut read).unwrap();
        // the furthest any file reaches: VTS_01_0.IFO at 989, 15 sectors
        let last_file_end = 989 + 30720u64.div_ceil(SECTOR as u64);
        assert!(ranges.iter().all(|(_, b)| *b <= last_file_end));
    }

    #[test]
    fn the_metadata_is_a_small_fraction_of_a_disc() {
        let mut read = disc();
        let total: u64 = metadata_ranges(&mut read).unwrap().iter().map(|(a, b)| b - a).sum();
        assert!(total * SECTOR as u64 / 1_000_000 < 5, "{total} sectors is too much");
    }
}

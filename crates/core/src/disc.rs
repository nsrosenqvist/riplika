//! What kind of disc is in the drive.
//!
//! Every workflow below this asks a different question of the disc, so the
//! first thing worth knowing is which question to ask. A DVD is recognised by
//! finding `VIDEO_TS` in its filesystem; an audio CD has no filesystem at all
//! and has to be read from its table of contents instead.
//!
//! That difference is why an audio CD used to show as an empty drive: the only
//! test being made was for an ISO9660 volume label, and a CD-DA hasn't got one.
//! Absence of a label is not absence of a disc.
//!
//! This opens the device itself rather than going through [`Fs`](crate::host::Fs),
//! because reading a table of contents is an ioctl on an open file descriptor
//! and there is nothing for a byte-oriented trait to stand in front of. The
//! seam is the device path instead: everything below works the same on a file,
//! which is how the ISO9660 half is tested without a disc in the drive - and
//! how it stays testable, since the tests here must never touch the real one.

use crate::model::Millis;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;

const SECTOR: usize = 2048;

/// Where the ISO9660 primary volume descriptor sits: sixteen sectors in.
const PVD_OFFSET: u64 = 16 * SECTOR as u64;

/// CD frames per second - the unit every offset on a CD is quoted in.
pub const FRAMES_PER_SECOND: u32 = 75;

/// The gap before the first track. Every disc has it and every disc-id scheme
/// counts it, so offsets are quoted as the drive's LBA plus this.
pub const PREGAP: u32 = 150;

const CDROMREADTOCHDR: libc::c_ulong = 0x5305;
const CDROMREADTOCENTRY: libc::c_ulong = 0x5306;
const CDROM_LBA: u8 = 0x01;
const CDROM_LEADOUT: u8 = 0xAA;

/// Set in a track's control field when the track holds data rather than audio.
const CTRL_DATA: u8 = 0x04;

#[repr(C)]
struct TocHeader {
    first: u8,
    last: u8,
}

/// Mirrors the kernel's `struct cdrom_tocentry`.
///
/// `adr_ctrl` is two four-bit fields in C - `adr` low, `ctrl` high - which
/// Rust has no spelling for, so it is read as one byte and split by hand. The
/// padding is what the C compiler inserts anyway to put the address on a
/// four-byte boundary; naming it keeps the layout obvious rather than implied.
#[repr(C)]
struct TocEntry {
    track: u8,
    adr_ctrl: u8,
    format: u8,
    _pad: u8,
    lba: i32,
    datamode: u8,
    _pad2: [u8; 3],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Track {
    pub number: u8,
    /// Start of the track as the drive reports it, in frames, first track at 0.
    pub start: u32,
    pub is_data: bool,
}

/// A CD's table of contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Toc {
    pub tracks: Vec<Track>,
    /// Where the last track ends.
    pub leadout: u32,
}

impl Toc {
    pub fn audio_tracks(&self) -> impl Iterator<Item = &Track> {
        self.tracks.iter().filter(|t| !t.is_data)
    }

    pub fn audio_count(&self) -> usize {
        self.audio_tracks().count()
    }

    /// A data track alongside the audio: an enhanced CD, or one of the many
    /// 90s games that shipped its soundtrack as playable CD audio.
    pub fn has_data_track(&self) -> bool {
        self.tracks.iter().any(|t| t.is_data)
    }

    /// Is there anything here to rip as music?
    pub fn is_audio(&self) -> bool {
        self.audio_count() > 0
    }

    /// Does the disc begin with data?
    ///
    /// The question that tells a game with a soundtrack from a music CD with
    /// extras on it: Mixed Mode puts the data first, an enhanced CD puts it
    /// last.
    pub fn opens_with_data(&self) -> bool {
        self.tracks.first().is_some_and(|t| t.is_data)
    }

    pub fn duration(&self) -> Millis {
        frames_to_millis(self.leadout)
    }

    /// How long track `number` runs, from where the next one starts.
    pub fn track_duration(&self, number: u8) -> Option<Millis> {
        let i = self.tracks.iter().position(|t| t.number == number)?;
        let start = self.tracks[i].start;
        let end = self.tracks.get(i + 1).map_or(self.leadout, |t| t.start);
        Some(frames_to_millis(end.saturating_sub(start)))
    }

    /// The MusicBrainz disc id: a SHA-1 over the track offsets, which
    /// identifies this exact pressing.
    ///
    /// Note this uses the whole disc's lead-out. On an enhanced CD, where a
    /// data track sits in a second session, libdiscid uses the first session's
    /// lead-out instead and this will disagree with it. Pure audio discs - all
    /// but a handful - are unaffected.
    pub fn musicbrainz_id(&self) -> String {
        let Some((first, last)) = self.ends() else {
            return String::new();
        };
        // One hundred slots: the lead-out, then a track per slot by its own
        // number, and zero wherever there is no track.
        let mut slots = [0u32; 100];
        slots[0] = self.leadout + PREGAP;
        for t in &self.tracks {
            if let Some(slot) = slots.get_mut(t.number as usize) {
                *slot = t.start + PREGAP;
            }
        }
        let mut blob = format!("{first:02X}{last:02X}");
        for s in slots {
            blob.push_str(&format!("{s:08X}"));
        }
        let digest = {
            let mut sha = crate::hash::Sha1::new();
            sha.update(blob.as_bytes());
            sha.finish()
        };
        // MusicBrainz's own base64 alphabet, so the id is safe in a URL.
        base64_of(&digest).replace('+', ".").replace('/', "_").replace('=', "-")
    }

    /// The freedb/CDDB disc id.
    ///
    /// Still worth computing: it is the key both rip-verification databases
    /// want, and CD-Text-less discs are often findable by it when nothing else
    /// works.
    pub fn freedb_id(&self) -> String {
        let Some(first_track) = self.tracks.first() else {
            return String::new();
        };
        let seconds = |frames: u32| (frames + PREGAP) / FRAMES_PER_SECOND;
        let sum: u32 = self.tracks.iter().map(|t| digit_sum(seconds(t.start))).sum();
        let span = seconds(self.leadout).saturating_sub(seconds(first_track.start));
        let id = ((sum % 255) << 24) | (span << 8) | (self.tracks.len() as u32 & 0xFF);
        format!("{id:08x}")
    }

    /// The offsets as the verification databases want them: pregap included,
    /// lead-out last.
    pub fn offsets(&self) -> Vec<u32> {
        self.tracks
            .iter()
            .map(|t| t.start + PREGAP)
            .chain(std::iter::once(self.leadout + PREGAP))
            .collect()
    }

    fn ends(&self) -> Option<(u8, u8)> {
        Some((self.tracks.first()?.number, self.tracks.last()?.number))
    }
}

fn frames_to_millis(frames: u32) -> Millis {
    u64::from(frames) * 1000 / u64::from(FRAMES_PER_SECOND)
}

fn digit_sum(mut n: u32) -> u32 {
    let mut sum = 0;
    while n > 0 {
        sum += n % 10;
        n /= 10;
    }
    sum
}

/// What kind of medium the drive says it is holding.
///
/// From the drive rather than guessed at from the filesystem, which matters:
/// a Blu-ray carries UDF and usually no ISO 9660 at all, so looking for a
/// directory finds nothing and says "data disc". It also decides how a disc
/// has to be read - a CD has raw 2352-byte sectors underneath the 2048 the
/// kernel hands out, and a DVD does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Medium {
    Cd,
    Dvd,
    BluRay,
}

impl Medium {
    /// Bytes in a sector as the disc actually stores it.
    pub fn raw_sector(self) -> usize {
        match self {
            // Sync pattern, header, user data and error correction.
            Medium::Cd => 2352,
            Medium::Dvd | Medium::BluRay => SECTOR,
        }
    }

    pub fn from_profile(profile: u16) -> Option<Medium> {
        match profile {
            0x08..=0x0A => Some(Medium::Cd),
            0x10..=0x1B => Some(Medium::Dvd),
            0x40..=0x43 => Some(Medium::BluRay),
            _ => None,
        }
    }
}

/// Ask the drive what medium it is holding.
pub fn medium(device: &Path) -> Option<Medium> {
    let answer = crate::scsi::ask(device, &crate::scsi::get_configuration(), 8)?;
    let profile = u16::from_be_bytes([*answer.get(6)?, *answer.get(7)?]);
    Medium::from_profile(profile)
}

/// The table of contents, for a disc that has one.
pub fn toc(device: &Path) -> Option<Toc> {
    read_toc(&File::open(device).ok()?)
}

/// What is in the drive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiscKind {
    DvdVideo,
    /// Recognised from its `BDMV` directory when the disc carries an ISO 9660
    /// bridge, and otherwise from what the drive says the medium is - Blu-ray
    /// is UDF, which this does not read.
    BluRay,
    Audio(Toc),
    /// A filesystem that is neither: a PC game, or anything else pressed.
    ///
    /// On a CD it carries the table of contents, because a game can have its
    /// soundtrack beside it as ordinary audio tracks and copying such a disc
    /// means knowing where they begin.
    Data(Option<Toc>),
    /// No disc, or one nothing can be read from.
    Empty,
}

impl DiscKind {
    pub fn has_disc(&self) -> bool {
        *self != DiscKind::Empty
    }

    /// A line for a drive listing.
    pub fn describe(&self) -> String {
        match self {
            DiscKind::DvdVideo => "DVD-Video".into(),
            DiscKind::BluRay => "Blu-ray".into(),
            DiscKind::Data(Some(toc)) if toc.is_audio() => {
                let n = toc.audio_count();
                format!("data disc, {n} audio track(s) beside it")
            }
            DiscKind::Data(_) => "data disc".into(),
            DiscKind::Empty => "empty".into(),
            DiscKind::Audio(toc) => {
                let n = toc.audio_count();
                let mins = toc.duration() / 60_000;
                let extra = if toc.has_data_track() { ", with data track" } else { "" };
                format!("audio CD, {n} tracks, {mins} min{extra}")
            }
        }
    }
}

/// Ask the drive what it is holding.
pub fn identify(device: &Path) -> DiscKind {
    let Ok(mut f) = File::open(device) else {
        return DiscKind::Empty;
    };
    // Which track comes first decides what a mixed disc is, and both kinds
    // exist. A game puts its data first and its soundtrack after - that is
    // Mixed Mode, and a PlayStation disc is the usual example. An enhanced
    // music CD does the opposite: audio first, and a data track added at the
    // end in a session of its own.
    //
    // Reading "has any audio track" as "is an album" therefore files every
    // game with a soundtrack as music, and sends its disc id to MusicBrainz.
    let toc = read_toc(&f);
    if let Some(toc) = &toc
        && toc.is_audio()
        && !toc.opens_with_data()
    {
        return DiscKind::Audio(toc.clone());
    }
    let Some(pvd) = read_at(&mut f, PVD_OFFSET, SECTOR) else {
        return DiscKind::Empty;
    };
    if !is_pvd(&pvd) {
        return DiscKind::Empty;
    }
    let names = root_names(&mut f, &pvd);
    let has = |want: &str| names.iter().any(|n| n == want);
    if has("VIDEO_TS") {
        DiscKind::DvdVideo
    } else if has("BDMV") {
        DiscKind::BluRay
    } else if medium(device) == Some(Medium::BluRay) {
        // A Blu-ray is UDF and often carries no ISO 9660 at all, so the
        // directory says nothing. The drive still knows what it is holding.
        DiscKind::BluRay
    } else {
        DiscKind::Data(toc)
    }
}

fn read_toc(f: &File) -> Option<Toc> {
    let fd = f.as_raw_fd();
    let mut hdr = TocHeader { first: 0, last: 0 };
    if unsafe { libc::ioctl(fd, CDROMREADTOCHDR as _, &raw mut hdr) } != 0 {
        return None;
    }
    if hdr.first == 0 || hdr.last < hdr.first {
        return None;
    }
    let mut tracks = Vec::new();
    for number in hdr.first..=hdr.last {
        let (start, is_data) = read_entry(fd, number)?;
        tracks.push(Track { number, start, is_data });
    }
    let (leadout, _) = read_entry(fd, CDROM_LEADOUT)?;
    Some(Toc { tracks, leadout })
}

fn read_entry(fd: RawFd, track: u8) -> Option<(u32, bool)> {
    let mut e: TocEntry = unsafe { std::mem::zeroed() };
    e.track = track;
    e.format = CDROM_LBA;
    if unsafe { libc::ioctl(fd, CDROMREADTOCENTRY as _, &raw mut e) } != 0 {
        return None;
    }
    let ctrl = e.adr_ctrl >> 4;
    Some((e.lba.max(0) as u32, ctrl & CTRL_DATA != 0))
}

fn read_at(f: &mut File, offset: u64, len: usize) -> Option<Vec<u8>> {
    f.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf).ok()?;
    Some(buf)
}

pub fn is_pvd(pvd: &[u8]) -> bool {
    pvd.len() >= 72 && pvd[0] == 1 && &pvd[1..6] == b"CD001"
}

/// The volume label out of a primary volume descriptor.
pub fn volume_label(pvd: &[u8]) -> Option<String> {
    if !is_pvd(pvd) {
        return None;
    }
    let label: String = pvd[40..72]
        .iter()
        .map(|b| *b as char)
        .collect::<String>()
        .trim_end_matches(['\0', ' '])
        .trim()
        .to_string();
    if label.is_empty() { None } else { Some(label) }
}

/// Where the root directory lives, from the record embedded in the descriptor.
fn root_extent(pvd: &[u8]) -> Option<(u32, u32)> {
    let rec = pvd.get(156..190)?;
    let lba = u32::from_le_bytes(rec.get(2..6)?.try_into().ok()?);
    let len = u32::from_le_bytes(rec.get(10..14)?.try_into().ok()?);
    Some((lba, len))
}

fn root_names(f: &mut File, pvd: &[u8]) -> Vec<String> {
    let Some((lba, len)) = root_extent(pvd) else {
        return Vec::new();
    };
    // A root directory that needs more than this is not one we would recognise
    // anything in anyway, and the cap keeps a corrupt extent from being read as
    // a request for gigabytes.
    let len = (len as usize).min(64 * SECTOR);
    let Some(dir) = read_at(f, u64::from(lba) * SECTOR as u64, len) else {
        return Vec::new();
    };
    directory_names(&dir)
}

/// The names in one ISO9660 directory extent.
pub fn directory_names(dir: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut i = 0usize;
    while i < dir.len() {
        let len = dir[i] as usize;
        if len == 0 {
            // Records never straddle a sector, so a zero length means the rest
            // of this sector is padding and the next one may hold more.
            i = (i / SECTOR + 1) * SECTOR;
            continue;
        }
        if len < 34 || i + len > dir.len() {
            break;
        }
        let name_len = dir[i + 32] as usize;
        if let Some(raw) = dir.get(i + 33..i + 33 + name_len) {
            // "." and ".." are stored as a single zero or one byte.
            if name_len > 1 || raw.first().is_some_and(|b| *b > 1) {
                let name: String = raw.iter().map(|b| *b as char).collect();
                // Files carry a ";1" version suffix; directories do not.
                names.push(name.split(';').next().unwrap_or(&name).to_string());
            }
        }
        i += len;
    }
    names
}

fn base64_of(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shawn McDonald, "Roots" - a real disc, read from a real drive. The two
    /// ids below are what MusicBrainz and freedb actually answered for it, so
    /// these tests fail if the arithmetic drifts away from the services it is
    /// supposed to agree with.
    fn roots() -> Toc {
        let starts =
            [0, 15443, 33892, 50750, 65846, 85908, 107101, 127207, 138996, 153615, 170139, 195211];
        Toc {
            tracks: starts
                .iter()
                .enumerate()
                .map(|(i, start)| Track { number: i as u8 + 1, start: *start, is_data: false })
                .collect(),
            leadout: 225301,
        }
    }

    #[test]
    fn the_disc_id_matches_what_musicbrainz_answered() {
        assert_eq!(roots().musicbrainz_id(), "sgDgBzHLi5stPYlOC7Jc6FPWdM8-");
    }

    #[test]
    fn the_freedb_id_matches_what_the_verification_databases_answered() {
        assert_eq!(roots().freedb_id(), "a20bbc0c");
    }

    #[test]
    fn a_track_runs_until_the_next_one_starts() {
        // Track 8 is 2:37 on the sleeve.
        assert_eq!(roots().track_duration(8), Some(157_186));
    }

    #[test]
    fn the_last_track_runs_to_the_leadout() {
        let toc = roots();
        // "Hallelujah", 6:41 on the sleeve.
        assert_eq!(toc.track_duration(12), Some(401_200));
        assert_eq!(toc.duration() / 60_000, 50);
    }

    #[test]
    fn an_unknown_track_has_no_duration() {
        assert_eq!(roots().track_duration(99), None);
    }

    #[test]
    fn offsets_carry_the_pregap_and_end_with_the_leadout() {
        let offsets = roots().offsets();
        assert_eq!(offsets.first(), Some(&150));
        assert_eq!(offsets.last(), Some(&225_451));
        assert_eq!(offsets.len(), 13);
    }

    #[test]
    fn a_game_with_a_soundtrack_is_not_an_album() {
        // A PlayStation disc is data first and its music after, which is Mixed
        // Mode. Reading "has audio tracks" as "is an album" filed Moto Racer
        // as a music CD and sent its disc id to MusicBrainz.
        let mut toc = roots();
        toc.tracks.insert(0, Track { number: 0, start: 0, is_data: true });
        assert!(toc.is_audio());
        assert!(toc.opens_with_data());
    }

    #[test]
    fn an_enhanced_music_cd_still_is_one() {
        // The other way round: audio first, a data track added at the end in a
        // session of its own. That is a music CD with extras, not a game.
        let mut toc = roots();
        toc.tracks.push(Track { number: 13, start: 250_000, is_data: true });
        assert!(toc.is_audio());
        assert!(!toc.opens_with_data());
    }

    #[test]
    fn a_disc_of_nothing_but_audio_opens_with_audio() {
        assert!(!roots().opens_with_data());
    }

    #[test]
    fn a_data_disc_says_what_is_beside_the_data() {
        let mut toc = roots();
        toc.tracks.insert(0, Track { number: 0, start: 0, is_data: true });
        let described = DiscKind::Data(Some(toc)).describe();
        assert!(described.contains("12 audio"), "{described}");
        assert_eq!(DiscKind::Data(None).describe(), "data disc");
    }

    #[test]
    fn a_data_track_does_not_count_as_music() {
        let mut toc = roots();
        toc.tracks.push(Track { number: 13, start: 200_000, is_data: true });
        assert_eq!(toc.audio_count(), 12);
        assert!(toc.has_data_track());
        assert!(toc.is_audio());
    }

    #[test]
    fn a_disc_of_nothing_but_data_is_not_music() {
        let toc = Toc { tracks: vec![Track { number: 1, start: 0, is_data: true }], leadout: 100 };
        assert!(!toc.is_audio());
    }

    #[test]
    fn an_empty_toc_yields_no_ids_rather_than_nonsense() {
        let toc = Toc { tracks: Vec::new(), leadout: 0 };
        assert_eq!(toc.musicbrainz_id(), "");
        assert_eq!(toc.freedb_id(), "");
    }

    #[test]
    fn a_drive_holding_music_is_described_by_what_is_on_it() {
        assert_eq!(DiscKind::Audio(roots()).describe(), "audio CD, 12 tracks, 50 min");
        assert!(DiscKind::Audio(roots()).has_disc());
        assert!(!DiscKind::Empty.has_disc());
    }

    fn record(name: &[u8]) -> Vec<u8> {
        let mut r = vec![0u8; 33];
        r[32] = name.len() as u8;
        r.extend_from_slice(name);
        if r.len() % 2 == 1 {
            r.push(0);
        }
        r[0] = r.len() as u8;
        r
    }

    #[test]
    fn the_root_directory_gives_up_its_names() {
        let mut dir = Vec::new();
        dir.extend(record(&[0])); // "."
        dir.extend(record(&[1])); // ".."
        dir.extend(record(b"VIDEO_TS"));
        dir.extend(record(b"README.TXT;1"));
        let names = directory_names(&dir);
        assert_eq!(names, vec!["VIDEO_TS", "README.TXT"]);
    }

    #[test]
    fn padding_at_the_end_of_a_sector_is_stepped_over() {
        let mut dir = vec![0u8; SECTOR * 2];
        let first = record(b"BDMV");
        dir[..first.len()].copy_from_slice(&first);
        let second = record(b"CERTIFICATE");
        dir[SECTOR..SECTOR + second.len()].copy_from_slice(&second);
        assert_eq!(directory_names(&dir), vec!["BDMV", "CERTIFICATE"]);
    }

    #[test]
    fn a_truncated_record_stops_the_walk_rather_than_panicking() {
        let mut dir = record(b"VIDEO_TS");
        dir.extend([40, 0, 0]); // claims forty bytes, three are here
        assert_eq!(directory_names(&dir), vec!["VIDEO_TS"]);
    }

    #[test]
    fn a_descriptor_that_is_not_one_is_refused() {
        assert!(!is_pvd(&[0u8; 2048]));
        assert!(volume_label(&[0u8; 2048]).is_none());
    }

    #[test]
    fn the_label_comes_out_trimmed() {
        let mut pvd = vec![0u8; 2048];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[40..72].copy_from_slice(b"PARKS_AND_REC_S6D1              ");
        assert_eq!(volume_label(&pvd).as_deref(), Some("PARKS_AND_REC_S6D1"));
    }

    /// A minimal but real ISO9660 image: a descriptor pointing at a root
    /// directory holding `entries`. Enough for [`identify`] to walk, which is
    /// the point - the parsing helpers are tested above, this tests that the
    /// device is actually read the way they expect.
    fn iso_with(entries: &[&str], label: &str) -> Vec<u8> {
        const ROOT_LBA: u32 = 18;
        let mut dir = Vec::new();
        dir.extend(record(&[0]));
        dir.extend(record(&[1]));
        for e in entries {
            dir.extend(record(e.as_bytes()));
        }

        let mut img = vec![0u8; (ROOT_LBA as usize + 1) * SECTOR];
        let p = 16 * SECTOR;
        img[p] = 1;
        img[p + 1..p + 6].copy_from_slice(b"CD001");
        img[p + 6] = 1;
        let mut name = [b' '; 32];
        name[..label.len()].copy_from_slice(label.as_bytes());
        img[p + 40..p + 72].copy_from_slice(&name);

        // The root directory record lives inside the descriptor.
        let r = p + 156;
        let len = dir.len() as u32;
        img[r] = 34;
        img[r + 2..r + 6].copy_from_slice(&ROOT_LBA.to_le_bytes());
        img[r + 6..r + 10].copy_from_slice(&ROOT_LBA.to_be_bytes());
        img[r + 10..r + 14].copy_from_slice(&len.to_le_bytes());
        img[r + 14..r + 18].copy_from_slice(&len.to_be_bytes());
        img[r + 25] = 0x02; // it is a directory
        img[r + 32] = 1;

        let d = ROOT_LBA as usize * SECTOR;
        img[d..d + dir.len()].copy_from_slice(&dir);
        img
    }

    fn probe(bytes: &[u8], tag: &str) -> DiscKind {
        let path =
            std::env::temp_dir().join(format!("riplika-disc-{}-{tag}.iso", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        let kind = identify(&path);
        let _ = std::fs::remove_file(&path);
        kind
    }

    #[test]
    fn the_drives_own_answer_decides_what_the_medium_is() {
        // The numbers are the MMC profile codes. A CD has raw sectors
        // underneath the cooked ones; the other two do not, and reading a DVD
        // as though it did would ask for bytes that are not there.
        assert_eq!(Medium::from_profile(0x08), Some(Medium::Cd));
        assert_eq!(Medium::from_profile(0x0A), Some(Medium::Cd));
        assert_eq!(Medium::from_profile(0x10), Some(Medium::Dvd));
        assert_eq!(Medium::from_profile(0x1B), Some(Medium::Dvd));
        assert_eq!(Medium::from_profile(0x40), Some(Medium::BluRay));
        assert_eq!(Medium::from_profile(0x43), Some(Medium::BluRay));
        // No disc, or something nobody here has thought about.
        assert_eq!(Medium::from_profile(0x0000), None);
        assert_eq!(Medium::from_profile(0xFFFF), None);
    }

    #[test]
    fn a_cd_sector_is_bigger_than_the_part_the_kernel_hands_out() {
        // 2352 on the disc, 2048 after the drive has checked the error
        // correction and thrown the rest away - and the thrown-away part is
        // what a preservation database hashes.
        assert_eq!(Medium::Cd.raw_sector(), 2352);
        assert_eq!(Medium::Dvd.raw_sector(), SECTOR);
        assert_eq!(Medium::BluRay.raw_sector(), SECTOR);
    }

    #[test]
    fn a_dvd_is_known_by_its_video_ts_directory() {
        assert_eq!(probe(&iso_with(&["VIDEO_TS"], "PARKS_S6D1"), "dvd"), DiscKind::DvdVideo);
    }

    #[test]
    fn a_blu_ray_is_known_by_its_bdmv_directory() {
        let img = iso_with(&["CERTIFICATE", "BDMV"], "SOME_FILM");
        assert_eq!(probe(&img, "bd"), DiscKind::BluRay);
    }

    #[test]
    fn a_filesystem_that_is_neither_is_a_data_disc() {
        let img = iso_with(&["SETUP.EXE;1", "DATA"], "HALF_LIFE");
        // No table of contents to be had from a file, which is not the same
        // as a disc that has none.
        assert_eq!(probe(&img, "data"), DiscKind::Data(None));
    }

    #[test]
    fn something_that_is_not_a_disc_image_reads_as_empty() {
        assert_eq!(probe(&vec![0u8; 40 * SECTOR], "blank"), DiscKind::Empty);
    }

    #[test]
    fn the_label_survives_the_round_trip_through_a_real_image() {
        let img = iso_with(&["VIDEO_TS"], "PARKS_S6D1");
        assert_eq!(volume_label(&img[16 * SECTOR..]).as_deref(), Some("PARKS_S6D1"));
    }
}

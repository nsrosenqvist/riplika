//! Dumping a game disc, and finding out what it was.
//!
//! The order is the other way round from everything else here. A film is
//! identified from its label before it is read, because the label is the only
//! clue and reading takes an hour. A game disc cannot be identified until it
//! has been read, because the bytes *are* the clue - so it is dumped first and
//! named afterwards.
//!
//! The dump goes through the rescue passes rather than a straight copy. Not
//! because game discs are especially damaged, but because some are unreadable
//! on purpose: copy protection writes sectors the drive is meant to fail on,
//! and a plain read either stops at the first one or spends an hour retrying it
//! before reaching the rest.

use crate::disc::Medium;
use crate::hash::{self, Digests};
use crate::host::{Cancel, Fs};
use crate::job::{Event, Stage};
use crate::naming::sanitize;
use crate::redump::{self, Dat};
use crate::rescue::map::Map;
use crate::rescue::{FileSink, PlainDisc, RawCd, ReadError, Rescue, SECTOR, SectorSource};
use crate::{Error, Result};
use std::path::{Path, PathBuf};

/// One track's file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpedTrack {
    pub number: u8,
    pub path: PathBuf,
    pub digests: Digests,
}

/// What came off the disc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dumped {
    /// One entry for an ordinary data disc, one per track for a disc with
    /// audio on it - which is what a preservation database stores and
    /// therefore the only thing it can recognise.
    pub tracks: Vec<DumpedTrack>,
    /// The sheet tying the tracks together, when there is more than one.
    pub cue: Option<PathBuf>,
    /// Where each track began and ended, kept so the sheet can be written
    /// again if the files are renamed - it names them, so renaming without it
    /// leaves a sheet pointing at files that are no longer there.
    pub spans: Vec<crate::cue::TrackSpan>,
    pub mode: crate::cue::DataMode,
    /// Sectors the drive would not give up, which were filled with zeros.
    pub unreadable: u64,
    pub sectors: u64,
    /// Samples of silence put where the read offset pointed past the disc.
    ///
    /// Only ever a handful, and only at the very end - but they are invented,
    /// so the last track cannot match a database however good the read was.
    pub padded: u64,
}

impl Dumped {
    pub fn digests(&self) -> Vec<Digests> {
        self.tracks.iter().map(|t| t.digests).collect()
    }

    /// The file to speak of when there is one worth naming.
    pub fn path(&self) -> Option<&Path> {
        self.cue.as_deref().or_else(|| self.tracks.first().map(|t| t.path.as_path()))
    }

    pub fn bytes(&self) -> u64 {
        self.tracks.iter().map(|t| t.digests.bytes).sum()
    }
}

impl Dumped {
    /// Did the whole disc come off?
    ///
    /// An incomplete image can still be worth keeping - it may well run - but
    /// it will never match a datfile, and saying so beats letting somebody
    /// conclude their disc is simply unknown.
    pub fn is_complete(&self) -> bool {
        self.unreadable == 0
    }
}

/// Read the whole disc into `stem`.
///
/// A data disc becomes one file. A disc with audio beside the data becomes one
/// file per track and a cue sheet, because that is what it is: a flat image of
/// such a disc is a perfectly good copy that no database can recognise.
pub fn dump(
    device: &Path,
    stem: &Path,
    fs: &dyn Fs,
    read_offset: i32,
    cancel: &Cancel,
    events: &mut dyn FnMut(Event),
) -> Result<Dumped> {
    // A CD has to be read raw. The kernel checks each sector's error
    // correction, keeps the 2048 bytes of user data and discards the rest -
    // and the discarded part is what a preservation database hashes, so an
    // image of cooked sectors is a good image that matches nothing.
    let medium = crate::disc::medium(device).unwrap_or(Medium::Dvd);
    let sector = medium.raw_sector();
    let toc = (medium == Medium::Cd).then(|| crate::disc::toc(device)).flatten();
    let sectors = match &toc {
        // The table of contents is authoritative for a CD; the block device
        // reports the cooked length, which is a different number.
        Some(toc) => u64::from(toc.leadout),
        None => PlainDisc::sectors(device)?,
    };
    if sectors == 0 {
        return Err(Error(format!("{} has nothing in it", device.display())));
    }

    let spans = match &toc {
        Some(toc) if toc.tracks.len() > 1 => {
            crate::cue::layout(toc, &crate::disc::pregaps(device, toc))
        }
        _ => vec![crate::cue::TrackSpan {
            number: 1,
            is_data: true,
            start: 0,
            end: sectors as u32,
            pregap: 0,
        }],
    };
    let several = spans.len() > 1;
    if let Some(dir) = stem.parent() {
        fs.create_dir_all(dir)?;
    }

    events(Event::Stage(Stage::Rip));
    let name = stem.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let mut unreadable = 0;
    let mut parts: Vec<(u8, PathBuf)> = Vec::new();
    for span in &spans {
        let dest = if several {
            stem.with_file_name(crate::cue::track_file_name(&name, span.number))
        } else {
            stem.with_extension("iso")
        };
        // Written under a temporary name and moved only when whole, so a
        // killed dump does not leave something that looks finished.
        let part = dest.with_extension(format!(
            "{}.part",
            dest.extension().unwrap_or_default().to_string_lossy()
        ));
        let mut source = Cancellable {
            inner: match medium {
                Medium::Cd => Box::new(RawCd::open(device)?),
                _ => Box::new(PlainDisc::open(device)?),
            },
            cancel,
        };
        let mut sink = FileSink::with_sector(&part, u64::from(span.start), sector)?;
        let mut rescue = Rescue::new(Map::new(u64::from(span.start), u64::from(span.end)))
            .filling_with(vec![0u8; sector]);
        let whole = sectors.max(1) as f32;
        let done = f32::from(span.number.saturating_sub(1));
        rescue.run(&mut source, &mut sink, &mut |p| {
            events(Event::Progress {
                stage: Stage::Rip,
                fraction: if several {
                    (u64::from(span.start) as f32 + p.fraction * span.sectors() as f32) / whole
                } else {
                    p.fraction
                },
                message: Some(if several {
                    format!("track {} - {}", span.number, p.pass)
                } else {
                    p.pass.to_string()
                }),
            });
        })?;
        let _ = done;
        unreadable += rescue.map.count(crate::rescue::map::State::Bad);
        fs.rename(&part, &dest)?;
        parts.push((span.number, dest));
    }

    events(Event::Stage(Stage::Verify));
    // Sizes are known from the layout, so the correction can be applied before
    // anything is hashed rather than hashing everything twice.
    let mut tracks: Vec<DumpedTrack> = parts
        .iter()
        .zip(&spans)
        .map(|((number, path), span)| DumpedTrack {
            number: *number,
            path: path.clone(),
            digests: Digests { crc32: 0, sha1: [0; 20], bytes: span.bytes() },
        })
        .collect();
    let padded = correct_read_offset(fs, &mut tracks, read_offset)?;

    let total: u64 = tracks.len() as u64;
    for (index, track) in tracks.iter_mut().enumerate() {
        // The correction hashes what it writes; only the untouched ones are
        // still to do.
        if track.digests.sha1 != [0; 20] {
            continue;
        }
        track.digests = hash::of_file(fs, &track.path, &mut |at, size| {
            events(Event::Progress {
                stage: Stage::Verify,
                fraction: (index as f32 + at as f32 / size.max(1) as f32) / total.max(1) as f32,
                message: None,
            });
        })?;
    }

    let mode = match &toc {
        Some(toc) => crate::disc::data_mode(device, toc),
        None => crate::cue::DataMode::Mode1,
    };
    // The sheet is what says where one track ends and the next begins, and
    // without it the files are a pile rather than a disc.
    let cue = several
        .then(|| -> Result<PathBuf> {
            let path = stem.with_extension("cue");
            fs.write(&path, crate::cue::cue_sheet(&name, &spans, mode).as_bytes())?;
            Ok(path)
        })
        .transpose()?;

    Ok(Dumped { tracks, cue, spans, mode, unreadable, sectors, padded })
}

/// Bytes in one stereo sample: two channels of sixteen bits.
pub const SAMPLE: u64 = 4;

/// Shift the audio tracks by the drive's read offset.
///
/// A data sector carries its own address, so a drive can sync to it exactly
/// and a data track comes off byte-perfect. Audio has no such marker, and
/// every drive returns it displaced by a fixed number of samples - a property
/// of the model, not a fault. The drive measured against here is +669.
///
/// Uncorrected, the rip plays perfectly and is wrong: shifted by a fifteenth
/// of a second, and matching nothing.
///
/// Returns how many samples had to be silence because they lie outside the
/// audio altogether, which is what a drive that cannot read past the lead-out
/// leaves behind.
pub fn correct_read_offset(fs: &dyn Fs, tracks: &mut [DumpedTrack], samples: i32) -> Result<u64> {
    // Track one of a game disc is data and stays where it is. The audio after
    // it is one continuous run, however many files it has been cut into.
    let first = tracks.iter().position(|t| !t.path.to_string_lossy().is_empty() && t.number > 1);
    let Some(first) = first.filter(|_| samples != 0) else { return Ok(0) };
    let sizes: Vec<u64> = tracks[first..].iter().map(|t| t.digests.bytes).collect();
    let total: u64 = sizes.iter().sum();
    let shift = i64::from(samples) * SAMPLE as i64;

    // Corrected content is written beside each track and moved into place
    // afterwards, so every read still comes from the untouched originals and
    // no more than a chunk is held at a time.
    let mut padded = 0u64;
    let mut at = 0i64;
    let mut staged = Vec::new();
    for (index, size) in sizes.iter().enumerate() {
        let temporary = tracks[first + index].path.with_extension("bin.shifted");
        let mut hasher = crate::hash::Hasher::new();
        let mut written = 0u64;
        while written < *size {
            let want = (crate::hash::CHUNK as u64).min(size - written) as usize;
            let from = at + shift + written as i64;
            let chunk = read_run(fs, &tracks[first..], &sizes, total, from, want, &mut padded)?;
            hasher.update(&chunk);
            if written == 0 {
                fs.write(&temporary, &chunk)?;
            } else {
                fs.append(&temporary, &chunk)?;
            }
            written += chunk.len() as u64;
        }
        staged.push((temporary, hasher.finish()));
        at += *size as i64;
    }

    for (track, (temporary, digests)) in tracks[first..].iter_mut().zip(staged) {
        fs.rename(&temporary, &track.path)?;
        track.digests = digests;
    }
    Ok(padded / SAMPLE)
}

/// Read from the audio run as though its files were one, with silence outside.
fn read_run(
    fs: &dyn Fs,
    audio: &[DumpedTrack],
    sizes: &[u64],
    total: u64,
    at: i64,
    len: usize,
    padded: &mut u64,
) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(len);
    let mut position = at;
    while out.len() < len {
        let want = len - out.len();
        // Before the first audio sector, or past the lead-out. A drive that
        // cannot overread has nothing to offer there, and silence is what
        // every ripper puts in its place.
        if position < 0 {
            let run = (-position).min(want as i64) as usize;
            out.resize(out.len() + run, 0);
            *padded += run as u64;
            position += run as i64;
            continue;
        }
        if position as u64 >= total {
            out.resize(len, 0);
            *padded += want as u64;
            break;
        }
        let mut start = 0u64;
        for (track, size) in audio.iter().zip(sizes) {
            if (position as u64) < start + size {
                let within = position as u64 - start;
                let run = want.min((size - within) as usize);
                out.extend(fs.read_range(&track.path, within, run)?);
                position += run as i64;
                break;
            }
            start += size;
        }
    }
    Ok(out)
}

/// Move a finished dump to where it belongs, under the name it turned out to
/// have.
///
/// Separate from the dump because the name is not known until the bytes are:
/// the disc is read into a holding folder under whatever it called itself, and
/// only a match can say what it really is.
pub fn file_away(
    fs: &dyn Fs,
    dumped: &Dumped,
    root: &Path,
    system: Option<&str>,
    name: &str,
) -> Result<Dumped> {
    let stem = destination(root, system, name);
    if stem.parent() == dumped.tracks.first().and_then(|t| t.path.parent()) {
        // Nothing learned, so nothing to move.
        return Ok(dumped.clone());
    }
    if let Some(dir) = stem.parent() {
        fs.create_dir_all(dir)?;
    }
    let stem_name = stem.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let several = dumped.tracks.len() > 1;

    let mut moved = dumped.clone();
    for track in &mut moved.tracks {
        let to = if several {
            stem.with_file_name(crate::cue::track_file_name(&stem_name, track.number))
        } else {
            stem.with_extension("iso")
        };
        fs.rename(&track.path, &to)?;
        track.path = to;
    }
    if let Some(old) = &moved.cue {
        // Written afresh rather than moved: every FILE line in it names a
        // track by the name it no longer has.
        let _ = fs.remove_file(old);
        let path = stem.with_extension("cue");
        fs.write(&path, crate::cue::cue_sheet(&stem_name, &moved.spans, moved.mode).as_bytes())?;
        moved.cue = Some(path);
    }
    Ok(moved)
}

/// Stops the passes when the user asks.
///
/// The rescue has no notion of cancelling; it has a notion of a read that
/// cannot be retried, which is the same thing from where it stands.
struct Cancellable<'a> {
    inner: Box<dyn SectorSource>,
    cancel: &'a Cancel,
}

impl SectorSource for Cancellable<'_> {
    fn read(&mut self, lba: u64, count: u64) -> std::result::Result<Vec<u8>, ReadError> {
        if self.cancel.is_cancelled() {
            return Err(ReadError::Fatal("cancelled".into()));
        }
        self.inner.read(lba, count)
    }
}

/// What to call the disc, without an extension.
///
/// A stem rather than a filename because one disc can be several files: an
/// image is `<stem>.iso`, but a disc with audio on it is `<stem> (Track NN).bin`
/// once per track and a `<stem>.cue` over the top.
///
/// A match gives the preservation project's own name, which is the point of
/// matching: it is the name every other copy of that disc has. Failing that,
/// what the disc itself offered - which is a guess, and looks like one.
pub fn suggested_stem(found: Option<&redump::Found<'_>>, disc: &crate::game::GameDisc) -> String {
    let name = match found {
        Some(f) => f.rom.name.rsplit_once('.').map_or(f.rom.name.clone(), |(s, _)| s.to_string()),
        None => disc.describe(),
    };
    sanitize(&name)
}

/// What a single-file image is called.
pub fn suggested_name(found: Option<&redump::Found<'_>>, disc: &crate::game::GameDisc) -> String {
    format!("{}.iso", suggested_stem(found, disc))
}

/// Where the disc's files go under `root`, as a stem for them to share.
///
/// Matched discs are filed by system, because that is the one grouping a
/// datfile actually knows and the one an emulator's library expects. Unmatched
/// ones go in a folder that says what they are.
pub fn destination(root: &Path, system: Option<&str>, name: &str) -> PathBuf {
    let folder = match system {
        Some(system) if !system.is_empty() => sanitize(system),
        _ => "Unidentified".to_string(),
    };
    root.join(folder).join(sanitize(name))
}

/// Look an image up in every datfile there is.
pub fn identify<'a>(
    dats: &'a [(PathBuf, Dat)],
    digests: &Digests,
) -> Option<(&'a Dat, redump::Found<'a>)> {
    dats.iter().find_map(|(_, dat)| dat.find(digests).map(|found| (dat, found)))
}

/// Look a whole dump up, however many files it came to.
pub fn identify_all<'a>(
    dats: &'a [(PathBuf, Dat)],
    dumped: &Dumped,
) -> Option<(&'a Dat, &'a redump::Game)> {
    let tracks = dumped.digests();
    dats.iter().find_map(|(_, dat)| dat.find_all(&tracks).map(|game| (dat, game)))
}

/// How much of the disc is worth telling somebody about.
pub fn shortfall(dumped: &Dumped) -> Option<String> {
    if dumped.is_complete() {
        return None;
    }
    let bytes = dumped.unreadable * SECTOR as u64;
    Some(format!(
        "{} of {} sectors could not be read ({} KB); the image is not a faithful copy \
         and will not match any datfile",
        dumped.unreadable,
        dumped.sectors,
        bytes / 1024
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::GameDisc;
    use crate::redump::{Found, Game, Rom};

    fn game() -> Game {
        Game {
            name: "Half-Life (Europe)".into(),
            roms: vec![Rom {
                name: "Half-Life (Europe).iso".into(),
                size: 100,
                crc32: Some(1),
                sha1: None,
            }],
        }
    }

    fn disc() -> GameDisc {
        GameDisc { label: Some("HALFLIFE".into()), serial: None, root: vec!["SETUP.EXE".into()] }
    }

    #[test]
    fn a_matched_disc_takes_the_name_every_other_copy_has() {
        let g = game();
        let found = Found { game: &g, rom: &g.roms[0] };
        assert_eq!(suggested_stem(Some(&found), &disc()), "Half-Life (Europe)");
        assert_eq!(suggested_name(Some(&found), &disc()), "Half-Life (Europe).iso");
    }

    #[test]
    fn an_unmatched_disc_is_named_from_what_it_offered_and_looks_like_a_guess() {
        assert_eq!(suggested_name(None, &disc()), "HALFLIFE.iso");
        assert_eq!(suggested_stem(None, &disc()), "HALFLIFE");
        let ps2 = GameDisc {
            label: Some("SLUS_202.02".into()),
            serial: Some("SLUS-20202".into()),
            root: Vec::new(),
        };
        assert_eq!(suggested_name(None, &ps2), "SLUS_202.02 (SLUS-20202).iso");
        assert_eq!(suggested_stem(None, &ps2), "SLUS_202.02 (SLUS-20202)");
    }

    #[test]
    fn a_matched_disc_is_filed_under_its_system() {
        assert_eq!(
            destination(Path::new("/games"), Some("Sony - PlayStation 2"), "Half-Life (Europe)"),
            Path::new("/games/Sony - PlayStation 2/Half-Life (Europe)")
        );
    }

    #[test]
    fn an_unmatched_disc_is_filed_where_that_is_obvious() {
        assert_eq!(
            destination(Path::new("/games"), None, "HALFLIFE"),
            Path::new("/games/Unidentified/HALFLIFE")
        );
    }

    #[test]
    fn characters_smb_refuses_do_not_reach_the_name() {
        let g = Game {
            name: "Who? (USA)".into(),
            roms: vec![Rom { name: "Who? (USA).iso".into(), size: 1, crc32: None, sha1: None }],
        };
        let found = Found { game: &g, rom: &g.roms[0] };
        assert!(!suggested_name(Some(&found), &disc()).contains('?'));
    }

    fn dumped(unreadable: u64) -> Dumped {
        Dumped {
            tracks: vec![DumpedTrack {
                number: 1,
                path: PathBuf::from("/games/x.iso"),
                digests: Digests { crc32: 0, sha1: [0; 20], bytes: 0 },
            }],
            cue: None,
            padded: 0,
            spans: Vec::new(),
            mode: crate::cue::DataMode::Mode1,
            unreadable,
            sectors: 1_000_000,
        }
    }

    fn many_tracks() -> Dumped {
        Dumped {
            tracks: (1..=3)
                .map(|n| DumpedTrack {
                    number: n,
                    path: PathBuf::from(format!("/games/x (Track {n:02}).bin")),
                    digests: Digests { crc32: 0, sha1: [0; 20], bytes: 100 },
                })
                .collect(),
            cue: Some(PathBuf::from("/games/x.cue")),
            padded: 0,
            spans: (1..=3)
                .map(|n| crate::cue::TrackSpan {
                    number: n,
                    is_data: n == 1,
                    start: u32::from(n) * 100,
                    end: u32::from(n) * 100 + 100,
                    pregap: if n == 1 { 0 } else { 150 },
                })
                .collect(),
            mode: crate::cue::DataMode::Mode2,
            unreadable: 0,
            sectors: 1_000,
        }
    }

    #[test]
    fn a_stem_carries_no_extension_because_one_disc_can_be_many_files() {
        // Appending to a name that already ended in .iso produced
        // "MOTO_RACER.iso (Track 01).bin".
        let g = game();
        let found = Found { game: &g, rom: &g.roms[0] };
        assert!(!suggested_stem(Some(&found), &disc()).contains(".iso"));
        assert!(!suggested_stem(None, &disc()).contains('.'));
    }

    #[test]
    fn a_disc_of_several_tracks_is_spoken_of_by_its_cue_sheet() {
        // The sheet is the disc; the track files on their own are a pile.
        let d = many_tracks();
        assert_eq!(d.path(), Some(Path::new("/games/x.cue")));
        assert_eq!(d.bytes(), 300, "the whole disc, not one track of it");
        assert_eq!(d.digests().len(), 3);
    }

    /// Three audio tracks whose contents are a known run, so a shift can be
    /// checked byte for byte rather than by hash alone.
    fn shiftable() -> (crate::host::FakeFs, Vec<DumpedTrack>) {
        let run: Vec<u8> = (0..120u16).map(|i| i as u8).collect();
        let fs = crate::host::FakeFs::new();
        let mut tracks = vec![DumpedTrack {
            number: 1,
            path: PathBuf::from("/d/t01.bin"),
            digests: Digests { crc32: 0, sha1: [0; 20], bytes: 40 },
        }];
        fs.write(Path::new("/d/t01.bin"), &run[..40]).unwrap();
        for (i, chunk) in run[40..].chunks(40).enumerate() {
            let path = PathBuf::from(format!("/d/t{:02}.bin", i + 2));
            fs.write(&path, chunk).unwrap();
            tracks.push(DumpedTrack {
                number: i as u8 + 2,
                path,
                digests: Digests { crc32: 0, sha1: [0; 20], bytes: chunk.len() as u64 },
            });
        }
        (fs, tracks)
    }

    #[test]
    fn a_positive_offset_pulls_the_audio_forwards() {
        // The whole audio run moves; the data track does not.
        let (fs, mut tracks) = shiftable();
        correct_read_offset(&fs, &mut tracks, 1).unwrap();
        assert_eq!(fs.read(Path::new("/d/t01.bin")).unwrap()[..4], [0, 1, 2, 3]);
        // Track two began at 40; one sample later it begins at 44.
        assert_eq!(fs.read(Path::new("/d/t02.bin")).unwrap()[..4], [44, 45, 46, 47]);
    }

    #[test]
    fn a_shift_reads_across_the_boundary_into_the_next_track() {
        // The correction is over the audio as one run, not per file - the
        // bytes that fill the end of one track come from the start of the
        // next.
        let (fs, mut tracks) = shiftable();
        correct_read_offset(&fs, &mut tracks, 1).unwrap();
        let second = fs.read(Path::new("/d/t02.bin")).unwrap();
        assert_eq!(second.len(), 40);
        assert_eq!(second[36..], [80, 81, 82, 83], "taken from track three");
    }

    #[test]
    fn what_falls_past_the_end_becomes_silence_and_is_counted() {
        // A drive that cannot read past the lead-out has nothing to put there,
        // and how much was invented is worth knowing.
        let (fs, mut tracks) = shiftable();
        let padded = correct_read_offset(&fs, &mut tracks, 2).unwrap();
        assert_eq!(padded, 2, "two samples of silence at the very end");
        let last = fs.read(Path::new("/d/t03.bin")).unwrap();
        assert_eq!(last[32..], [0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn no_offset_changes_nothing_at_all() {
        let (fs, mut tracks) = shiftable();
        let before: Vec<Vec<u8>> = tracks.iter().map(|t| fs.read(&t.path).unwrap()).collect();
        assert_eq!(correct_read_offset(&fs, &mut tracks, 0).unwrap(), 0);
        let after: Vec<Vec<u8>> = tracks.iter().map(|t| fs.read(&t.path).unwrap()).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn a_disc_of_one_data_track_has_no_audio_to_shift() {
        let fs = crate::host::FakeFs::new().with_file("/d/x.iso", "data");
        let mut tracks = vec![DumpedTrack {
            number: 1,
            path: PathBuf::from("/d/x.iso"),
            digests: Digests { crc32: 0, sha1: [0; 20], bytes: 4 },
        }];
        assert_eq!(correct_read_offset(&fs, &mut tracks, 669).unwrap(), 0);
        assert_eq!(fs.read(Path::new("/d/x.iso")).unwrap(), b"data");
    }

    #[test]
    fn filing_a_disc_renames_its_tracks_and_writes_the_sheet_again() {
        // The sheet names every track file, so renaming the files without
        // rewriting it leaves a sheet pointing at names that are gone.
        let fs = crate::host::FakeFs::new()
            .with_file("/games/Unidentified/x (Track 01).bin", "one")
            .with_file("/games/Unidentified/x (Track 02).bin", "two")
            .with_file("/games/Unidentified/x (Track 03).bin", "three")
            .with_file("/games/Unidentified/x.cue", "old sheet");
        let mut dumped = many_tracks();
        for (i, track) in dumped.tracks.iter_mut().enumerate() {
            track.path = PathBuf::from(format!("/games/Unidentified/x (Track {:02}).bin", i + 1));
        }
        dumped.cue = Some(PathBuf::from("/games/Unidentified/x.cue"));

        let filed = file_away(
            &fs,
            &dumped,
            Path::new("/games"),
            Some("Sony - PlayStation"),
            "Moto Racer (Europe)",
        )
        .unwrap();

        assert_eq!(
            filed.tracks[0].path,
            Path::new("/games/Sony - PlayStation/Moto Racer (Europe) (Track 01).bin")
        );
        assert_eq!(
            filed.cue.as_deref(),
            Some(Path::new("/games/Sony - PlayStation/Moto Racer (Europe).cue"))
        );
        let sheet = String::from_utf8(fs.read(filed.cue.as_ref().unwrap()).unwrap()).unwrap();
        assert!(sheet.contains("Moto Racer (Europe) (Track 01).bin"), "{sheet}");
        assert!(!sheet.contains("\"x ("), "the old names are gone: {sheet}");
    }

    #[test]
    fn a_dump_nobody_could_name_stays_where_it_was_put() {
        let fs = crate::host::FakeFs::new().with_file("/games/Unidentified/HALFLIFE.iso", "x");
        let mut dumped = dumped(0);
        dumped.tracks[0].path = PathBuf::from("/games/Unidentified/HALFLIFE.iso");
        let filed = file_away(&fs, &dumped, Path::new("/games"), None, "HALFLIFE").unwrap();
        assert_eq!(filed.tracks[0].path, Path::new("/games/Unidentified/HALFLIFE.iso"));
    }

    #[test]
    fn a_single_file_dump_is_spoken_of_by_that_file() {
        assert_eq!(dumped(0).path(), Some(Path::new("/games/x.iso")));
    }

    #[test]
    fn a_whole_dump_has_nothing_to_report() {
        assert!(dumped(0).is_complete());
        assert_eq!(shortfall(&dumped(0)), None);
    }

    #[test]
    fn a_dump_with_holes_says_so_rather_than_looking_merely_unknown() {
        // Otherwise the missing match reads as "this disc is not in the
        // database" when it is really "this image is not that disc".
        let d = dumped(32);
        assert!(!d.is_complete());
        let why = shortfall(&d).expect("it should complain");
        assert!(why.contains("32 of 1000000"), "{why}");
        assert!(why.contains("datfile"), "{why}");
    }
}

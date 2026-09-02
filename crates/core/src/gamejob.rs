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
    /// Sectors that disagree with the error detection written into them.
    ///
    /// Only a data sector carries such a thing, and it is the one verdict here
    /// that needs neither a second reading nor a database: these bytes are not
    /// the ones that were written.
    pub corrupt: u64,
    /// Sectors the drive read but had to guess at, by its own C2 account.
    ///
    /// Different from unreadable: there are bytes here, and they may even be
    /// the right ones. They are simply not known to be, and on audio - which
    /// carries no error correction a host can check - there is no way to find
    /// out except by reading the disc again and seeing whether it says the
    /// same thing twice.
    pub damaged: u64,
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
        self.unreadable == 0 && self.damaged == 0 && self.corrupt == 0
    }
}

/// What to call the part of the disc being read.
///
/// "Track 1" is the vocabulary of a mixed-mode disc, where a data track sits
/// beside the audio ones and which of them is being copied is worth saying. A
/// PC disc has exactly one, and numbering it explains nothing to somebody
/// watching a single bar move.
fn what_is_being_read(span: &crate::cue::TrackSpan, several: bool, again: u32) -> Option<String> {
    match (several, again) {
        (false, 0) => None,
        (false, n) => Some(format!("reading again ({n})")),
        (true, 0) => Some(format!("track {}", span.number)),
        (true, n) => Some(format!("track {} again ({n})", span.number)),
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
    let mut damaged = 0;
    let mut corrupt = 0;
    let mut parts: Vec<(u8, PathBuf)> = Vec::new();
    // Audio tracks are hashed as they are checked, so they need no second pass.
    let mut checked: Vec<(u8, Digests)> = Vec::new();
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
        events(Event::Progress {
            stage: Stage::Rip,
            fraction: u64::from(span.start) as f32 / sectors.max(1) as f32,
            message: what_is_being_read(span, several, 0),
        });

        match (&toc, span.is_data) {
            // Audio carries no error correction a host can check, so a
            // sector read wrongly comes back looking like a good one. The
            // drive's own C2 account is the only thing that knows, and it is
            // asked for every sector.
            (Some(_), false) => {
                let reading = Reading { fs, sectors: sectors.max(1) as f32, several };
                let mut read = |into: &Path, progress: &mut dyn FnMut(f32)| {
                    read_span(device, into, medium, sector, span, cancel, progress)
                };
                let (digests, bad, guessed) =
                    read_span_twice(&reading, &part, span, &mut read, events)?;
                unreadable += bad;
                damaged += guessed;
                checked.push((span.number, digests));
            }
            // Data sectors carry their own error correction, which the drive
            // checks: a bad one is refused rather than guessed at, so there is
            // nothing for C2 to add here. They go straight through the rescue
            // passes, which know what to do about the ones a disc will not
            // give up.
            _ => {
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
                rescue.run(&mut source, &mut sink, &mut |p| {
                    events(Event::Progress {
                        stage: Stage::Rip,
                        fraction: (u64::from(span.start) as f32
                            + p.fraction * span.sectors() as f32)
                            / whole,
                        message: Some(p.pass.to_string()),
                    });
                })?;
                unreadable += rescue.map.count(crate::rescue::map::State::Bad);
                // A data sector carries a check over itself, so a bad one can
                // be caught here rather than at the datfile - where a bad read
                // and a disc nobody has catalogued look exactly the same, and
                // call for opposite responses.
                if medium == Medium::Cd {
                    let checked =
                        crate::edc::of_file(fs, &part, u64::from(span.start), &mut |_, _| {})?;
                    let wrong = checked.corrupt + checked.misplaced;
                    if wrong > 0 {
                        events(Event::Warning(crate::model::Warning::TrackCorrupt {
                            track: span.number,
                            sectors: wrong,
                        }));
                    }
                    corrupt += wrong;
                }
            }
        }
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
    // What was checked while reading does not need hashing again.
    for track in &mut tracks {
        if let Some((_, digests)) = checked.iter().find(|(n, _)| *n == track.number) {
            track.digests = *digests;
        }
    }
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

    Ok(Dumped { tracks, cue, spans, mode, unreadable, damaged, corrupt, sectors, padded })
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
    let several = dumped.tracks.len() > 1;
    let stem = destination(root, system, name, several);
    if stem.parent() == dumped.tracks.first().and_then(|t| t.path.parent()) {
        // Nothing learned, so nothing to move.
        return Ok(dumped.clone());
    }
    if let Some(dir) = stem.parent() {
        fs.create_dir_all(dir)?;
    }
    let stem_name = stem.file_name().unwrap_or_default().to_string_lossy().into_owned();

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

/// Read one span of sectors into `dest`.
///
/// Answers with the sectors the drive could not read at all, and - on a CD,
/// where the drive is asked for its C2 error pointers too - the sectors it
/// handed back as a guess.
fn read_span(
    device: &Path,
    dest: &Path,
    medium: Medium,
    sector: usize,
    span: &crate::cue::TrackSpan,
    cancel: &Cancel,
    progress: &mut dyn FnMut(f32),
) -> Result<(u64, Vec<u64>)> {
    let mut source = Cancellable {
        inner: match medium {
            Medium::Cd => Box::new(RawCd::open(device)?.watching_c2()),
            _ => Box::new(PlainDisc::open(device)?),
        },
        cancel,
    };
    let mut sink = FileSink::with_sector(dest, u64::from(span.start), sector)?;
    let mut rescue = Rescue::new(Map::new(u64::from(span.start), u64::from(span.end)))
        .filling_with(vec![0u8; sector]);
    rescue.run(&mut source, &mut sink, &mut |p| progress(p.fraction))?;
    Ok((rescue.map.count(crate::rescue::map::State::Bad), source.damaged()))
}

/// How many times a track is re-read before it is given up on.
const ATTEMPTS: u32 = 3;

/// What every span read needs to know, which does not change between them.
struct Reading<'a> {
    fs: &'a dyn Fs,
    /// Sectors on the whole disc, for reporting how far along this one is.
    sectors: f32,
    /// Whether the disc has more than one track, which decides whether saying
    /// which one is worth anything.
    several: bool,
}

/// Reading one span into one file: how far it got, and what it had to guess at.
///
/// A seam, because a drive is the one thing here that cannot be faked and the
/// deciding of when a track is good enough is the part worth testing.
type ReadSpan<'a> = &'a mut dyn FnMut(&Path, &mut dyn FnMut(f32)) -> Result<(u64, Vec<u64>)>;

/// Read an audio span, and again if the drive says it had to guess.
///
/// Audio has no error correction a host can check, so a sector read wrongly
/// comes back looking like any other, and two dumps of one disc disagreed on
/// nineteen tracks of twenty. The reason turned out to be neither the drive
/// nor the reading: asking for C2 error pointers showed the disc is damaged
/// from a certain sector on, and the first sector the drive flagged was the
/// very sector at which two passes first differed. Before it, 7568 sectors
/// were byte-identical between passes; after it, half of them differed.
///
/// So C2 is the tripwire. A track the drive never flagged is read once and
/// believed. A track it flagged is damaged, and no single reading of it can be
/// trusted - so it is read again until two agree, and if none ever do it is
/// reported rather than written out as though it were fine.
///
/// C2 is a tripwire and not a map: differing sectors the drive did not flag
/// all sat within seven sectors of one it did. It says reliably that a disc is
/// damaged, not exactly where.
///
/// Answers with the digests of what was kept, the sectors that would not read
/// at all, and the sectors the drive admitted to guessing at. A damaged track
/// is not an error - the best reading of it is kept, and counted, so that the
/// rest of the disc still comes off and the report can say what is wrong with
/// it. What is not allowed is a damaged track passing for a good one.
fn read_span_twice(
    reading: &Reading,
    dest: &Path,
    span: &crate::cue::TrackSpan,
    read: ReadSpan,
    events: &mut dyn FnMut(Event),
) -> Result<(Digests, u64, u64)> {
    let (fs, whole) = (reading.fs, reading.sectors);
    let against = dest.with_extension("check");
    let mut unreadable = 0;
    let mut flagged = 0;
    let mut previous: Option<Digests> = None;

    for attempt in 0..ATTEMPTS {
        let into = if attempt == 0 { dest } else { &against };
        let mut report = |fraction: f32| {
            events(Event::Progress {
                stage: Stage::Rip,
                fraction: (u64::from(span.start) as f32 + fraction * span.sectors() as f32) / whole,
                message: what_is_being_read(span, reading.several, attempt),
            });
        };
        let damaged;
        (unreadable, damaged) = read(into, &mut report)?;
        let digests = hash::of_file(fs, into, &mut |_, _| {})?;
        // Nothing flagged: the drive read every sector rather than filling any
        // of them in, so there is nothing a second reading could disagree
        // with. This is the ordinary case, and it saves reading the disc twice.
        if damaged.is_empty() {
            // Rename before removing: on a retry `into` *is* the check file,
            // and clearing it first would delete the very reading being kept.
            if attempt > 0 {
                fs.rename(into, dest)?;
            }
            let _ = fs.remove_file(&against);
            return Ok((digests, unreadable, 0));
        }
        flagged = damaged.len() as u64;
        if attempt == 0 {
            events(Event::Warning(crate::model::Warning::TrackDamaged {
                track: span.number,
                sectors: damaged.len(),
            }));
        }
        match previous {
            // Two readings of a damaged track agreeing is the best evidence
            // there is that the drive's guesses were the same guesses, which
            // is not the same as their being right - so it is still counted.
            Some(before) if before == digests => {
                let _ = fs.remove_file(&against);
                return Ok((digests, unreadable, flagged));
            }
            // The second reading is the one kept, so the file on disk is
            // always the reading that was checked.
            Some(_) => {
                fs.rename(&against, dest)?;
                previous = Some(digests);
            }
            None => previous = Some(digests),
        }
    }
    let _ = fs.remove_file(&against);
    // No two readings agreed, so the file on disk is the last of them - the
    // best that can be had. It is kept rather than thrown away, and counted so
    // that nothing downstream mistakes it for a faithful copy.
    let digests = hash::of_file(fs, dest, &mut |_, _| {})?;
    // At least one, always. A track that never agreed with itself reporting no
    // damage would be counted as whole, which is the one outcome this is here
    // to prevent.
    Ok((digests, unreadable, flagged.max(1)))
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

    fn damaged(&self) -> Vec<u64> {
        self.inner.damaged()
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
///
/// A disc that came to one file is that file, sitting in the system folder
/// beside its neighbours. A disc that came to several gets a folder of its
/// own: a PlayStation disc can run to a cue sheet and thirteen tracks, and two
/// of those ripped one after the other leave twenty-eight files in a heap with
/// nothing but the name at the front of each to say which disc they belong to.
pub fn destination(root: &Path, system: Option<&str>, name: &str, several: bool) -> PathBuf {
    let folder = match system {
        Some(system) if !system.is_empty() => sanitize(system),
        _ => "Unidentified".to_string(),
    };
    let name = sanitize(name);
    if several { root.join(folder).join(&name).join(&name) } else { root.join(folder).join(name) }
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

/// The datfile entry that accounts for most of a dump, when none has all of it.
///
/// Answers only when there is something worth saying: an entry of the same
/// length with at least one track matching. Nothing found means nothing found,
/// and is left to say so.
pub fn nearly<'a>(
    dats: &'a [(PathBuf, Dat)],
    dumped: &Dumped,
) -> Option<(&'a Dat, redump::Partial<'a>)> {
    let tracks = dumped.digests();
    dats.iter()
        .filter_map(|(_, dat)| dat.closest(&tracks).map(|p| (dat, p)))
        .max_by_key(|(_, p)| p.matched.len())
}

/// What a near miss amounts to, as a sentence.
///
/// Says the disc's name and which tracks disagree with it, and stops there.
/// Whether a differing track is a bad read or a pressing nobody has catalogued
/// is not something hashes can answer - C2 and the sectors' own error
/// detection answer it, and they are reported alongside. Saying "read these
/// again" here would be wrong about exactly the tracks that matter: on the
/// disc this was written for, track one differs because it is a different
/// pressing, and every sector of it passes its own check.
pub fn near_miss(partial: &redump::Partial<'_>) -> String {
    let list = |ns: &[usize]| ns.iter().map(usize::to_string).collect::<Vec<_>>().join(", ");
    format!(
        "{} of {} tracks match {}.\nTrack{} {} do not, so this is that disc rather than one \
         nobody has catalogued.",
        partial.matched.len(),
        partial.tracks(),
        partial.game.name,
        if partial.differing.len() == 1 { "" } else { "s" },
        list(&partial.differing),
    )
}

/// How much of the disc is worth telling somebody about.
pub fn shortfall(dumped: &Dumped) -> Option<String> {
    if dumped.is_complete() {
        return None;
    }
    if dumped.corrupt > 0 {
        return Some(format!(
            "{} of {} sectors disagree with the error detection written into them; \
             this is a bad read, not an unknown disc",
            dumped.corrupt, dumped.sectors
        ));
    }
    if dumped.unreadable == 0 {
        return Some(format!(
            "{} of {} sectors came back as the drive's best guess rather than as read; \
             the disc is damaged, and the image will not match any datfile",
            dumped.damaged, dumped.sectors
        ));
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

    #[test]
    fn a_disc_of_one_track_is_not_told_which_track_it_is_on() {
        // "track 1" is the vocabulary of a mixed-mode disc, where a data track
        // sits beside the audio ones. A PC disc has exactly one, and numbering
        // it explains nothing to somebody watching a single bar move.
        let only = span();
        assert_eq!(what_is_being_read(&only, false, 0), None);
        assert_eq!(what_is_being_read(&only, true, 0).as_deref(), Some("track 2"));
    }

    #[test]
    fn a_second_reading_is_worth_saying_however_many_tracks_there_are() {
        // This is the disc being read again because the drive admitted to
        // guessing, which is worth knowing whether or not it has a number.
        let only = span();
        assert_eq!(what_is_being_read(&only, false, 2).as_deref(), Some("reading again (2)"));
        assert_eq!(what_is_being_read(&only, true, 2).as_deref(), Some("track 2 again (2)"));
    }
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
            destination(
                Path::new("/games"),
                Some("Sony - PlayStation 2"),
                "Half-Life (Europe)",
                false
            ),
            Path::new("/games/Sony - PlayStation 2/Half-Life (Europe)")
        );
    }

    /// A cue sheet and thirteen tracks used to land loose in the system
    /// folder, so two PlayStation discs in a row were twenty-eight files in a
    /// heap.
    #[test]
    fn a_disc_of_several_tracks_gets_a_folder_of_its_own() {
        assert_eq!(
            destination(
                Path::new("/games"),
                Some("Sony - PlayStation"),
                "Moto Racer (Europe)",
                true
            ),
            Path::new("/games/Sony - PlayStation/Moto Racer (Europe)/Moto Racer (Europe)")
        );
    }

    #[test]
    fn an_unmatched_disc_is_filed_where_that_is_obvious() {
        assert_eq!(
            destination(Path::new("/games"), None, "HALFLIFE", false),
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
            damaged: 0,
            corrupt: 0,
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
            damaged: 0,
            corrupt: 0,
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

    /// Twenty tracks the size Cool Boarders 2's are, filled so that every
    /// byte says where it came from. A disc that shape put track three's
    /// content thirteen million bytes early - inside track two - while every
    /// file still came out the right length, so nothing but the content said
    /// anything was wrong.
    #[test]
    fn a_long_disc_maps_every_track_to_the_right_place() {
        const SIZES: [u64; 19] = [
            42_039_648, 36_164_352, 35_828_016, 39_236_064, 41_326_992, 42_834_624, 38_060_064,
            35_891_520, 39_558_288, 40_047_504, 38_805_648, 12_199_824, 24_877_104, 50_669_136,
            1_909_824, 13_895_616, 11_395_440, 43_714_272, 3_010_560,
        ];
        // Scaled down by a factor that divides every size, so the test is
        // about the arithmetic rather than about moving half a gigabyte.
        const SCALE: u64 = 48;
        let fs = crate::host::FakeFs::new();
        let mut tracks = vec![DumpedTrack {
            number: 1,
            path: PathBuf::from("/d/t01.bin"),
            digests: Digests { crc32: 0, sha1: [0; 20], bytes: 4 },
        }];
        fs.write(Path::new("/d/t01.bin"), b"DATA").unwrap();

        let mut run = 0u64;
        for (i, size) in SIZES.iter().enumerate() {
            let bytes = size / SCALE;
            // Each four-byte sample records its own offset in the run.
            let content: Vec<u8> =
                (0..bytes / 4).flat_map(|s| ((run / 4 + s) as u32).to_le_bytes()).collect();
            let path = PathBuf::from(format!("/d/t{:02}.bin", i + 2));
            fs.write(&path, &content).unwrap();
            tracks.push(DumpedTrack {
                number: i as u8 + 2,
                path,
                digests: Digests { crc32: 0, sha1: [0; 20], bytes: content.len() as u64 },
            });
            run += content.len() as u64;
        }

        let shift = 5i32;
        correct_read_offset(&fs, &mut tracks, shift).unwrap();

        let mut expected = shift as u64;
        for track in tracks.iter().skip(1) {
            let first = fs.read(&track.path).unwrap();
            let says = u32::from_le_bytes(first[..4].try_into().unwrap()) as u64;
            assert_eq!(
                says, expected,
                "track {} starts at sample {says}, should be {expected}",
                track.number
            );
            expected += track.digests.bytes / 4;
        }
    }

    /// The same shape again, but through the real filesystem. The fake one
    /// hands back exactly what was asked for; a real file has an end, and how
    /// a short read near it is counted is the sort of thing only this catches.
    #[test]
    fn a_long_disc_maps_correctly_on_a_real_filesystem() {
        const SIZES: [u64; 6] = [4_200_000, 3_600_000, 3_500_000, 1_200_000, 900_000, 300_000];
        let dir = std::env::temp_dir().join(format!("riplika-offset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fs = crate::host::RealFs;

        let mut tracks = vec![DumpedTrack {
            number: 1,
            path: dir.join("t01.bin"),
            digests: Digests { crc32: 0, sha1: [0; 20], bytes: 4 },
        }];
        std::fs::write(dir.join("t01.bin"), b"DATA").unwrap();
        let mut run = 0u64;
        for (i, size) in SIZES.iter().enumerate() {
            let content: Vec<u8> =
                (0..size / 4).flat_map(|s| ((run / 4 + s) as u32).to_le_bytes()).collect();
            let path = dir.join(format!("t{:02}.bin", i + 2));
            std::fs::write(&path, &content).unwrap();
            tracks.push(DumpedTrack {
                number: i as u8 + 2,
                path,
                digests: Digests { crc32: 0, sha1: [0; 20], bytes: content.len() as u64 },
            });
            run += content.len() as u64;
        }

        correct_read_offset(&fs, &mut tracks, 5).unwrap();
        let mut expected = 5u64;
        let mut wrong = Vec::new();
        for track in tracks.iter().skip(1) {
            let first = std::fs::read(&track.path).unwrap();
            let says = u32::from_le_bytes(first[..4].try_into().unwrap()) as u64;
            if says != expected {
                wrong.push(format!("track {} says {says}, should be {expected}", track.number));
            }
            expected += track.digests.bytes / 4;
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(wrong.is_empty(), "{wrong:#?}");
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
            Path::new(
                "/games/Sony - PlayStation/Moto Racer (Europe)/Moto Racer (Europe) (Track 01).bin"
            )
        );
        assert_eq!(
            filed.cue.as_deref(),
            Some(Path::new(
                "/games/Sony - PlayStation/Moto Racer (Europe)/Moto Racer (Europe).cue"
            ))
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

    /// What one reading of a span produced: sectors missed, sectors guessed at.
    type Reading1 = Result<(u64, Vec<u64>)>;

    /// A reader that hands back a scripted sequence of readings.
    ///
    /// Each entry is what one attempt produces: the bytes written, and the
    /// sectors the drive says it guessed at.
    fn scripted<'a>(
        fs: &'a crate::host::FakeFs,
        readings: Vec<(&'static str, Vec<u64>)>,
        attempts: &'a std::cell::Cell<usize>,
    ) -> impl FnMut(&Path, &mut dyn FnMut(f32)) -> Reading1 + 'a {
        move |into: &Path, _: &mut dyn FnMut(f32)| {
            let (bytes, damaged) = readings[attempts.get().min(readings.len() - 1)].clone();
            attempts.set(attempts.get() + 1);
            fs.write(into, bytes.as_bytes())?;
            Ok((0, damaged))
        }
    }

    fn span() -> crate::cue::TrackSpan {
        crate::cue::TrackSpan { number: 2, is_data: false, start: 100, end: 200, pregap: 150 }
    }

    #[test]
    fn a_track_the_drive_never_flagged_is_read_once_and_believed() {
        // The point of asking for C2 at all: a healthy disc should not be read
        // twice to prove it is healthy.
        let fs = crate::host::FakeFs::new();
        let at = std::cell::Cell::new(0);
        let mut read = scripted(&fs, vec![("good", vec![])], &at);
        let reading = Reading { fs: &fs, sectors: 1000.0, several: true };
        let (_, _, damaged) =
            read_span_twice(&reading, Path::new("/x.bin"), &span(), &mut read, &mut |_| {})
                .expect("a clean reading is not an error");
        assert_eq!(at.get(), 1, "read once");
        assert_eq!(damaged, 0);
        assert_eq!(fs.read(Path::new("/x.bin")).unwrap(), b"good");
    }

    #[test]
    fn a_retry_that_comes_back_clean_is_the_reading_that_is_kept() {
        // The check file *is* the reading on a retry, so clearing it before
        // moving it into place threw away the only good copy.
        let fs = crate::host::FakeFs::new();
        let at = std::cell::Cell::new(0);
        let mut read = scripted(&fs, vec![("guessed", vec![7]), ("clean", vec![])], &at);
        let reading = Reading { fs: &fs, sectors: 1000.0, several: true };
        let (digests, _, damaged) =
            read_span_twice(&reading, Path::new("/x.bin"), &span(), &mut read, &mut |_| {})
                .expect("the retry read it");
        assert_eq!(at.get(), 2);
        assert_eq!(damaged, 0);
        assert_eq!(fs.read(Path::new("/x.bin")).unwrap(), b"clean");
        assert_eq!(digests.bytes, 5, "the digests are of what was kept");
    }

    #[test]
    fn a_flagged_track_that_reads_the_same_twice_is_kept_and_still_counted() {
        let fs = crate::host::FakeFs::new();
        let at = std::cell::Cell::new(0);
        let mut read = scripted(&fs, vec![("same", vec![7, 8])], &at);
        let reading = Reading { fs: &fs, sectors: 1000.0, several: true };
        let mut warnings = Vec::new();
        let (_, _, damaged) =
            read_span_twice(&reading, Path::new("/x.bin"), &span(), &mut read, &mut |e| {
                if let Event::Warning(w) = e {
                    warnings.push(w);
                }
            })
            .expect("two agreeing readings are the best there is");
        assert_eq!(at.get(), 2, "flagged, so it was read again");
        assert_eq!(damaged, 2, "agreeing is not the same as being right");
        assert_eq!(warnings.len(), 1, "said once, not once per attempt");
    }

    #[test]
    fn a_track_that_never_agrees_is_kept_and_reported_rather_than_lost() {
        // Erroring here used to lose the whole disc over one bad track. The
        // rest of it still comes off; what must not happen is the bad track
        // passing for a good one.
        let fs = crate::host::FakeFs::new();
        let at = std::cell::Cell::new(0);
        let mut read =
            scripted(&fs, vec![("one", vec![7]), ("two", vec![7]), ("three", vec![7])], &at);
        let reading = Reading { fs: &fs, sectors: 1000.0, several: true };
        let (_, _, damaged) =
            read_span_twice(&reading, Path::new("/x.bin"), &span(), &mut read, &mut |_| {})
                .expect("a damaged track is a result, not an error");
        assert_eq!(at.get(), ATTEMPTS as usize);
        assert!(damaged > 0, "counted, so nothing downstream calls the dump whole");
        assert_eq!(fs.read(Path::new("/x.bin")).unwrap(), b"three", "the last reading is kept");
        assert!(fs.read(Path::new("/x.check")).is_err(), "no check file left behind");
    }

    #[test]
    fn a_dump_the_drive_had_to_guess_at_is_not_a_whole_dump() {
        // Audio has no error correction a host can check, so a guessed sector
        // reads like any other. The drive's own C2 account is the only thing
        // that knows, and letting it pass silently is how a disc that matches
        // nothing gets mistaken for a disc nobody has catalogued.
        let mut d = dumped(0);
        d.damaged = 47;
        assert!(!d.is_complete());
        let why = shortfall(&d).expect("it should complain");
        assert!(why.contains("47 of 1000000"), "{why}");
        assert!(why.contains("guess"), "{why}");
        assert!(!why.contains("could not be read"), "they were read, just not trustworthily");
    }

    #[test]
    fn a_near_miss_names_the_disc_and_the_tracks_that_let_it_down() {
        let g = Game {
            name: "Cool Boarders 2 (Europe)".into(),
            roms: vec![Rom { name: "x.bin".into(), size: 1, crc32: None, sha1: None }],
        };
        let partial = redump::Partial {
            game: &g,
            matched: vec![1, 3, 4, 5, 6, 7, 8],
            differing: vec![2, 9, 10],
        };
        let said = near_miss(&partial);
        assert!(said.contains("7 of 10 tracks"), "{said}");
        assert!(said.contains("Cool Boarders 2 (Europe)"), "{said}");
        assert!(said.contains("Tracks 2, 9, 10 do not"), "{said}");
        // Deliberately does not say why they differ. A hash cannot tell a bad
        // read from a different pressing; C2 and the sectors' own error
        // detection can, and say so separately.
        assert!(!said.contains("again"), "{said}");
    }

    #[test]
    fn one_bad_track_is_spoken_of_in_the_singular() {
        let g = Game { name: "A Disc".into(), roms: Vec::new() };
        let partial = redump::Partial { game: &g, matched: vec![1], differing: vec![2] };
        let said = near_miss(&partial);
        assert!(said.contains("Track 2 do not"), "{said}");
        assert!(!said.contains("Tracks 2"), "{said}");
    }

    #[test]
    fn a_dump_whose_sectors_fail_their_own_check_is_called_a_bad_read() {
        // The distinction that matters: an uncatalogued disc should be sent
        // in, a bad read should be done again, and until this was checked both
        // came out as "no datfile has this".
        let mut d = dumped(0);
        d.corrupt = 3;
        assert!(!d.is_complete());
        let why = shortfall(&d).expect("it should complain");
        assert!(why.contains("error detection"), "{why}");
        assert!(why.contains("not an unknown disc"), "{why}");
    }

    #[test]
    fn holes_are_spoken_of_ahead_of_guesses_when_a_dump_has_both() {
        let mut d = dumped(32);
        d.damaged = 47;
        let why = shortfall(&d).expect("it should complain");
        assert!(why.contains("could not be read"), "{why}");
    }
}

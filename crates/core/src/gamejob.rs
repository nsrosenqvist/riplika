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
    /// Sectors the drive would not give up, which were filled with zeros.
    pub unreadable: u64,
    pub sectors: u64,
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
    let total: u64 = parts.len() as u64;
    let mut tracks = Vec::new();
    for (index, (number, path)) in parts.into_iter().enumerate() {
        let digests = hash::of_file(fs, &path, &mut |at, size| {
            events(Event::Progress {
                stage: Stage::Verify,
                fraction: (index as f32 + at as f32 / size.max(1) as f32) / total.max(1) as f32,
                message: None,
            });
        })?;
        tracks.push(DumpedTrack { number, path, digests });
    }

    // The sheet is what says where one track ends and the next begins, and
    // without it the files are a pile rather than a disc.
    let cue = several
        .then(|| -> Result<PathBuf> {
            let mode = crate::disc::data_mode(device, toc.as_ref().expect("a CD has a toc"));
            let text = crate::cue::cue_sheet(&name, &spans, mode);
            let path = stem.with_extension("cue");
            fs.write(&path, text.as_bytes())?;
            Ok(path)
        })
        .transpose()?;

    Ok(Dumped { tracks, cue, unreadable, sectors })
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
pub fn destination(
    root: &Path,
    found: Option<&redump::Found<'_>>,
    system: Option<&str>,
    disc: &crate::game::GameDisc,
) -> PathBuf {
    let folder = match (found, system) {
        (Some(_), Some(system)) if !system.is_empty() => sanitize(system),
        _ => "Unidentified".to_string(),
    };
    root.join(folder).join(suggested_stem(found, disc))
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
        let g = game();
        let found = Found { game: &g, rom: &g.roms[0] };
        assert_eq!(
            destination(Path::new("/games"), Some(&found), Some("Sony - PlayStation 2"), &disc()),
            Path::new("/games/Sony - PlayStation 2/Half-Life (Europe)")
        );
    }

    #[test]
    fn an_unmatched_disc_is_filed_where_that_is_obvious() {
        assert_eq!(
            destination(Path::new("/games"), None, None, &disc()),
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

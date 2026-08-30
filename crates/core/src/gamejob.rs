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

use crate::hash::{self, Digests};
use crate::host::{Cancel, Fs};
use crate::job::{Event, Stage};
use crate::naming::sanitize;
use crate::redump::{self, Dat};
use crate::rescue::map::Map;
use crate::rescue::{FileSink, PlainDisc, ReadError, Rescue, SECTOR, SectorSource};
use crate::{Error, Result};
use std::path::{Path, PathBuf};

/// What came off the disc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dumped {
    pub path: PathBuf,
    pub digests: Digests,
    /// Sectors the drive would not give up, which were filled with zeros.
    pub unreadable: u64,
    pub sectors: u64,
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

/// Read the whole disc into `dest`.
pub fn dump(
    device: &Path,
    dest: &Path,
    fs: &dyn Fs,
    cancel: &Cancel,
    events: &mut dyn FnMut(Event),
) -> Result<Dumped> {
    let sectors = PlainDisc::sectors(device)?;
    if sectors == 0 {
        return Err(Error(format!("{} has nothing in it", device.display())));
    }
    events(Event::Stage(Stage::Rip));

    // Written under a temporary name and moved only when whole, so a killed
    // dump does not leave something that looks like a finished image.
    let part = dest.with_extension("iso.part");
    if let Some(dir) = part.parent() {
        fs.create_dir_all(dir)?;
    }
    let unreadable = {
        let mut source = Cancellable { inner: PlainDisc::open(device)?, cancel };
        let mut sink = FileSink::create(&part, 0)?;
        let mut rescue = Rescue::new(Map::new(0, sectors)).for_data();
        rescue.run(&mut source, &mut sink, &mut |p| {
            events(Event::Progress {
                stage: Stage::Rip,
                fraction: p.fraction,
                message: Some(p.pass.to_string()),
            });
        })?;
        rescue.map.count(crate::rescue::map::State::Bad)
    };

    events(Event::Stage(Stage::Verify));
    let digests = hash::of_file(fs, &part, &mut |at, total| {
        events(Event::Progress {
            stage: Stage::Verify,
            fraction: at as f32 / total.max(1) as f32,
            message: None,
        });
    })?;

    fs.rename(&part, dest)?;
    Ok(Dumped { path: dest.to_path_buf(), digests, unreadable, sectors })
}

/// Stops the passes when the user asks.
///
/// The rescue has no notion of cancelling; it has a notion of a read that
/// cannot be retried, which is the same thing from where it stands.
struct Cancellable<'a> {
    inner: PlainDisc,
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

/// What to call the image.
///
/// A match gives the preservation project's own name, which is the point of
/// matching: it is the name every other copy of that disc has. Failing that,
/// what the disc itself offered - which is a guess, and looks like one.
pub fn suggested_name(found: Option<&redump::Found<'_>>, disc: &crate::game::GameDisc) -> String {
    match found {
        Some(f) => sanitize(&f.rom.name),
        None => sanitize(&format!("{}.iso", disc.describe())),
    }
}

/// Where the image goes under `root`.
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
    root.join(folder).join(suggested_name(found, disc))
}

/// Look an image up in every datfile there is.
pub fn identify<'a>(
    dats: &'a [(PathBuf, Dat)],
    digests: &Digests,
) -> Option<(&'a Dat, redump::Found<'a>)> {
    dats.iter().find_map(|(_, dat)| dat.find(digests).map(|found| (dat, found)))
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
        assert_eq!(suggested_name(Some(&found), &disc()), "Half-Life (Europe).iso");
    }

    #[test]
    fn an_unmatched_disc_is_named_from_what_it_offered_and_looks_like_a_guess() {
        assert_eq!(suggested_name(None, &disc()), "HALFLIFE.iso");
        let ps2 = GameDisc {
            label: Some("SLUS_202.02".into()),
            serial: Some("SLUS-20202".into()),
            root: Vec::new(),
        };
        assert_eq!(suggested_name(None, &ps2), "SLUS_202.02 (SLUS-20202).iso");
    }

    #[test]
    fn a_matched_disc_is_filed_under_its_system() {
        let g = game();
        let found = Found { game: &g, rom: &g.roms[0] };
        assert_eq!(
            destination(Path::new("/games"), Some(&found), Some("Sony - PlayStation 2"), &disc()),
            Path::new("/games/Sony - PlayStation 2/Half-Life (Europe).iso")
        );
    }

    #[test]
    fn an_unmatched_disc_is_filed_where_that_is_obvious() {
        assert_eq!(
            destination(Path::new("/games"), None, None, &disc()),
            Path::new("/games/Unidentified/HALFLIFE.iso")
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
            path: PathBuf::from("/games/x.iso"),
            digests: Digests { crc32: 0, sha1: [0; 20], bytes: 0 },
            unreadable,
            sectors: 1_000_000,
        }
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

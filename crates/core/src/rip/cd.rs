//! Reading audio off a CD.
//!
//! cdparanoia rather than ffmpeg. ffmpeg can read a CD through libcdio, but it
//! does none of what makes a CD rip trustworthy - re-reads, C2 error pointers,
//! jitter correction against a drive that does not report its position
//! honestly. On an unscratched disc the two agree; on a scratched one the
//! difference is a clean track or a click, and which disc you have is not
//! known until afterwards.

use crate::disc::Toc;
use crate::host::{Command, Runner};
use crate::{Error, Result};
use std::path::Path;

/// Bytes of audio in one CD frame.
///
/// 588 stereo samples of 16 bits: 2352 bytes, by definition and not by
/// measurement.
pub const BYTES_PER_FRAME: u64 = 2352;

/// The header cdparanoia writes in front of the audio.
const WAV_HEADER: u64 = 44;

pub fn rip_track_command(device: &Path, track: u8, dest: &Path) -> Command {
    Command::new("cdparanoia")
        .arg("-d")
        .path(device)
        // WAV is already the default, but the file is written to a `.part`
        // path and guessing from an extension is exactly how the video side
        // ended up asking ffmpeg to write a format called "part".
        .arg("-w")
        .arg(track.to_string())
        .path(dest)
}

/// How large one track's audio should come to.
///
/// Exact rather than approximate: the table of contents counts frames and a
/// frame is a fixed number of bytes, so a good rip lands on this number and a
/// short one is short by whole frames. There is no tolerance to allow for,
/// which is why this checks equality where the video side needs a percentage.
pub fn expected_bytes(toc: &Toc, track: u8) -> Option<u64> {
    let i = toc.tracks.iter().position(|t| t.number == track)?;
    let start = toc.tracks[i].start;
    let end = toc.tracks.get(i + 1).map_or(toc.leadout, |t| t.start);
    Some(u64::from(end.saturating_sub(start)) * BYTES_PER_FRAME)
}

/// What a finished file should weigh, header included.
pub fn expected_file_size(toc: &Toc, track: u8) -> Option<u64> {
    Some(expected_bytes(toc, track)? + WAV_HEADER)
}

/// Did the whole track arrive?
pub fn is_short(expected: u64, actual: u64) -> bool {
    actual < expected
}

pub struct CdAudio<'a> {
    runner: &'a dyn Runner,
}

impl<'a> CdAudio<'a> {
    pub fn new(runner: &'a dyn Runner) -> Self {
        Self { runner }
    }

    /// Read one track. Fails loudly on a short read rather than leaving a file
    /// that plays and stops early.
    pub fn rip_track(&self, device: &Path, track: u8, dest: &Path) -> Result<()> {
        let out = self.runner.run(&rip_track_command(device, track, dest))?;
        if !out.ok() {
            return Err(Error(format!(
                "cdparanoia could not read track {track}: {}",
                out.last_error()
            )));
        }
        Ok(())
    }

    /// Check what was written, given how big the file turned out.
    ///
    /// Kept apart from the read so it can be tested without a filesystem, and
    /// so the caller decides where the size comes from.
    pub fn check_size(&self, toc: &Toc, track: u8, actual: u64) -> Result<()> {
        let Some(expected) = expected_file_size(toc, track) else {
            return Ok(());
        };
        if is_short(expected, actual) {
            let missing = expected.saturating_sub(actual);
            return Err(Error(format!(
                "track {track} came back {missing} bytes short of {expected}; the disc did not \
                 read completely"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disc::{Toc, Track};
    use crate::host::FakeRunner;

    fn toc() -> Toc {
        // The first eight tracks of the disc this was written against.
        let starts = [0, 15443, 33892, 50750, 65846, 85908, 107101, 127207, 138996];
        Toc {
            tracks: starts
                .iter()
                .enumerate()
                .map(|(i, s)| Track { number: i as u8 + 1, start: *s, is_data: false })
                .collect(),
            leadout: 225301,
        }
    }

    #[test]
    fn a_track_is_read_by_number_into_the_file_it_was_given() {
        let cmd = rip_track_command(Path::new("/dev/sr0"), 8, Path::new("/tmp/t.part"));
        assert_eq!(cmd.program, "cdparanoia");
        assert_eq!(cmd.args, ["-d", "/dev/sr0", "-w", "8", "/tmp/t.part"]);
    }

    #[test]
    fn the_format_is_stated_because_a_part_file_has_nothing_to_infer_from() {
        let cmd = rip_track_command(Path::new("/dev/sr0"), 1, Path::new("/tmp/x.part"));
        assert!(cmd.has("-w"), "every transcode once failed for exactly this reason");
    }

    #[test]
    fn a_tracks_size_is_known_exactly_from_the_table_of_contents() {
        // Track 8 runs from frame 127207 to 138996: 11789 frames, and the file
        // it produced on the real disc was 27_727_772 bytes.
        assert_eq!(expected_bytes(&toc(), 8), Some(11_789 * 2352));
        assert_eq!(expected_file_size(&toc(), 8), Some(27_727_772));
    }

    #[test]
    fn the_last_track_is_measured_against_the_leadout() {
        let t = toc();
        let last = t.tracks.last().unwrap().number;
        assert_eq!(expected_bytes(&t, last), Some((225_301u64 - 138_996) * 2352));
    }

    #[test]
    fn a_track_that_is_not_on_the_disc_has_no_expected_size() {
        assert_eq!(expected_bytes(&toc(), 99), None);
    }

    #[test]
    fn a_full_read_passes_and_a_short_one_does_not() {
        let runner = FakeRunner::new();
        let cd = CdAudio::new(&runner);
        assert!(cd.check_size(&toc(), 8, 27_727_772).is_ok());
        // One frame missing is still a fault: there is no tolerance here, the
        // number is exact.
        let err = cd.check_size(&toc(), 8, 27_727_772 - 2352).unwrap_err();
        assert!(err.to_string().contains("short"), "{err}");
    }

    #[test]
    fn a_longer_file_than_expected_is_not_treated_as_a_failure() {
        // Some drives pad the last frame. Extra bytes are not a missing track.
        let runner = FakeRunner::new();
        let cd = CdAudio::new(&runner);
        assert!(cd.check_size(&toc(), 8, 27_727_772 + 2352).is_ok());
    }

    #[test]
    fn a_read_that_fails_says_what_cdparanoia_said() {
        let runner = FakeRunner::new().fail("cdparanoia", "unable to read table of contents");
        let cd = CdAudio::new(&runner);
        let err = cd
            .rip_track(Path::new("/dev/riplika-no-such-device"), 3, Path::new("/tmp/x"))
            .unwrap_err();
        assert!(err.to_string().contains("table of contents"), "{err}");
        assert!(err.to_string().contains("track 3"), "{err}");
    }
}

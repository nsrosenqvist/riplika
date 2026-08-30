//! Where one track ends and the next begins, and the sheet that says so.
//!
//! A disc with audio on it is not one file. A preservation database stores it
//! as a file per track and a cue sheet tying them together, so a single flat
//! image of such a disc matches nothing however carefully it was read.
//!
//! The subtlety is where to cut. The table of contents gives each track's
//! INDEX 01 - where the music starts - but a track file begins at its INDEX
//! 00, the silent pregap in front of it. Usually that gap is 150 sectors, and
//! assuming so is how a ripper gets most discs right and some wrong: on the
//! disc this was written against, track two's pregap is 225. So the gaps are
//! read from the disc rather than assumed, and everything here takes them as
//! given.

use crate::disc::Toc;

/// Sectors in one second of CD audio.
pub const FRAMES_PER_SECOND: u32 = 75;

/// What track one holds, which decides what the cue sheet calls it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataMode {
    /// An ordinary data CD: a PC game, a software disc.
    Mode1,
    /// A PlayStation disc, and anything else carrying mixed-form sectors.
    Mode2,
    /// Nothing but music.
    Audio,
}

impl DataMode {
    /// Read from the sector header, which says which it is in one byte.
    pub fn of_sector(raw: &[u8]) -> DataMode {
        match raw.get(15) {
            Some(2) => DataMode::Mode2,
            Some(1) => DataMode::Mode1,
            _ => DataMode::Audio,
        }
    }

    fn cue_name(self) -> &'static str {
        match self {
            DataMode::Mode1 => "MODE1/2352",
            DataMode::Mode2 => "MODE2/2352",
            DataMode::Audio => "AUDIO",
        }
    }
}

/// One track, as a file on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackSpan {
    pub number: u8,
    pub is_data: bool,
    /// First sector of the file, which is the pregap when there is one.
    pub start: u32,
    /// One past the last sector.
    pub end: u32,
    /// Sectors of pregap at the front, so the cue can say where the music is.
    pub pregap: u32,
}

impl TrackSpan {
    pub fn sectors(&self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    /// Bytes the file will come to, at 2352 a sector.
    pub fn bytes(&self) -> u64 {
        u64::from(self.sectors()) * 2352
    }
}

/// Work out where each track's file begins and ends.
///
/// `pregaps` is the pregap length for each track that has one, by track
/// number. A track with no entry is taken to start where the table of contents
/// says it does.
pub fn layout(toc: &Toc, pregaps: &[(u8, u32)]) -> Vec<TrackSpan> {
    let gap_of =
        |number: u8| pregaps.iter().find(|(n, _)| *n == number).map(|(_, g)| *g).unwrap_or(0);
    // Where each track's *file* starts: its INDEX 01 less whatever silence
    // runs in front of it.
    let file_start: Vec<u32> =
        toc.tracks.iter().map(|t| t.start.saturating_sub(gap_of(t.number))).collect();

    toc.tracks
        .iter()
        .enumerate()
        .map(|(i, track)| TrackSpan {
            number: track.number,
            is_data: track.is_data,
            start: file_start[i],
            // Each file runs up to where the next one starts; the last runs to
            // the lead-out, so every sector on the disc lands in exactly one
            // file and the total is the disc.
            end: file_start.get(i + 1).copied().unwrap_or(toc.leadout),
            pregap: gap_of(track.number),
        })
        .collect()
}

/// `mm:ss:ff`, which is how a cue sheet writes a position.
pub fn msf(sectors: u32) -> String {
    let seconds = sectors / FRAMES_PER_SECOND;
    format!("{:02}:{:02}:{:02}", seconds / 60, seconds % 60, sectors % FRAMES_PER_SECOND)
}

/// The sheet that ties the track files together.
pub fn cue_sheet(stem: &str, tracks: &[TrackSpan], mode: DataMode) -> String {
    let mut out = String::new();
    for track in tracks {
        out.push_str(&format!("FILE \"{}\" BINARY\n", track_file_name(stem, track.number)));
        let kind = if track.is_data { mode.cue_name() } else { DataMode::Audio.cue_name() };
        out.push_str(&format!("  TRACK {:02} {kind}\n", track.number));
        // The pregap sits at the front of this track's own file, so INDEX 00
        // is where the file begins and INDEX 01 is that far into it.
        if track.pregap > 0 {
            out.push_str("    INDEX 00 00:00:00\n");
            out.push_str(&format!("    INDEX 01 {}\n", msf(track.pregap)));
        } else {
            out.push_str("    INDEX 01 00:00:00\n");
        }
    }
    out
}

/// What one track's file is called, in the preservation projects' style.
pub fn track_file_name(stem: &str, number: u8) -> String {
    format!("{stem} (Track {number:02}).bin")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disc::Track;

    /// Moto Racer (Europe), read from the disc: a data track and twelve of
    /// music. The lengths asserted below are Redump's, so this fails if the
    /// arithmetic drifts from what a correct dump has to weigh.
    fn moto_racer() -> Toc {
        let starts = [
            (1u8, 0u32, true),
            (2, 63873, false),
            (3, 82833, false),
            (4, 93589, false),
            (5, 107639, false),
            (6, 121582, false),
            (7, 141323, false),
            (8, 155411, false),
            (9, 170004, false),
            (10, 185597, false),
            (11, 199901, false),
            (12, 204227, false),
            (13, 216188, false),
        ];
        Toc {
            tracks: starts
                .iter()
                .map(|(n, s, d)| Track { number: *n, start: *s, is_data: *d })
                .collect(),
            leadout: 232014,
        }
    }

    /// What the drive's subchannel says: three seconds before track two,
    /// two before every other. The odd one is the point - a ripper that
    /// assumes 150 everywhere gets this disc wrong by 75 sectors.
    fn pregaps() -> Vec<(u8, u32)> {
        let mut v = vec![(2u8, 225u32)];
        v.extend((3..=13u8).map(|n| (n, 150)));
        v
    }

    #[test]
    fn the_track_lengths_are_the_ones_redump_quotes() {
        let want = [
            149_700_096u64,
            44_770_320,
            25_298_112,
            33_045_600,
            32_793_936,
            46_430_832,
            33_134_976,
            34_322_736,
            36_674_736,
            33_643_008,
            10_174_752,
            28_132_272,
            37_575_552,
        ];
        let spans = layout(&moto_racer(), &pregaps());
        assert_eq!(spans.len(), 13);
        for (span, bytes) in spans.iter().zip(want) {
            assert_eq!(span.bytes(), bytes, "track {}", span.number);
        }
    }

    #[test]
    fn every_sector_on_the_disc_lands_in_exactly_one_file() {
        let spans = layout(&moto_racer(), &pregaps());
        assert_eq!(spans.first().unwrap().start, 0);
        assert_eq!(spans.last().unwrap().end, 232_014);
        for pair in spans.windows(2) {
            assert_eq!(pair[0].end, pair[1].start, "a gap or an overlap between tracks");
        }
        let total: u32 = spans.iter().map(TrackSpan::sectors).sum();
        assert_eq!(total, 232_014, "the parts have to come to the whole disc");
    }

    #[test]
    fn assuming_every_pregap_is_two_seconds_gets_this_disc_wrong() {
        // The reason the gaps are read rather than assumed.
        let assumed: Vec<(u8, u32)> = (2..=13u8).map(|n| (n, 150)).collect();
        let spans = layout(&moto_racer(), &assumed);
        assert_ne!(spans[0].bytes(), 149_700_096);
        assert_eq!(spans[0].bytes(), 149_876_496, "75 sectors too long");
    }

    #[test]
    fn a_disc_of_one_data_track_is_one_file_with_no_pregap() {
        let toc =
            Toc { tracks: vec![Track { number: 1, start: 0, is_data: true }], leadout: 339_463 };
        let spans = layout(&toc, &[]);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].pregap, 0);
        assert_eq!(spans[0].bytes(), 798_416_976, "the PC disc measured earlier");
    }

    #[test]
    fn positions_are_written_the_way_a_cue_sheet_writes_them() {
        assert_eq!(msf(0), "00:00:00");
        assert_eq!(msf(150), "00:02:00");
        assert_eq!(msf(225), "00:03:00");
        assert_eq!(msf(63_873), "14:11:48");
    }

    #[test]
    fn the_sheet_names_every_file_and_says_where_the_music_starts() {
        let spans = layout(&moto_racer(), &pregaps());
        let cue = cue_sheet("Moto Racer (Europe)", &spans, DataMode::Mode2);
        assert!(cue.starts_with("FILE \"Moto Racer (Europe) (Track 01).bin\" BINARY\n"));
        // A PlayStation disc is mixed-form sectors, and calling it MODE1 makes
        // an image no emulator will boot.
        assert!(cue.contains("TRACK 01 MODE2/2352"), "{cue}");
        assert!(cue.contains("TRACK 02 AUDIO"), "{cue}");
        // Track one has no pregap, so no INDEX 00.
        let first = cue.split("FILE").nth(1).unwrap();
        assert!(!first.contains("INDEX 00"), "{first}");
        // Track two's three-second pregap is at the front of its own file.
        let second = cue.split("FILE").nth(2).unwrap();
        assert!(second.contains("INDEX 00 00:00:00"), "{second}");
        assert!(second.contains("INDEX 01 00:03:00"), "{second}");
    }

    #[test]
    fn a_pc_disc_is_mode_one_and_a_playstation_disc_is_mode_two() {
        // Byte fifteen of a raw sector, straight after the sync pattern and
        // the address.
        let mut sector = vec![0u8; 2352];
        sector[15] = 1;
        assert_eq!(DataMode::of_sector(&sector), DataMode::Mode1);
        sector[15] = 2;
        assert_eq!(DataMode::of_sector(&sector), DataMode::Mode2);
        assert_eq!(DataMode::of_sector(&[]), DataMode::Audio);
    }
}

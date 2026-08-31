//! Checking a data sector against the error detection written inside it.
//!
//! The counterpart to C2 for the half of a disc C2 says nothing about. A data
//! sector carries a CRC over its own contents, so a dump of one can be checked
//! without a drive, without a database and without reading the disc twice -
//! which is more than can be said for audio, where a wrong sector looks
//! exactly like a right one and the drive's own account is all there is.
//!
//! This is what settled a disc here whose data track matched no datfile entry:
//! every sector of it passed, so the read was right and the catalogue simply
//! does not have that pressing. Without the check there is no way to tell that
//! from a bad dump, and the two call for opposite responses.

use crate::host::Fs;
use crate::{Error, Result};
use std::path::Path;

/// Bytes a raw data sector takes.
pub const SECTOR: usize = 2352;

/// The twelve bytes every data sector starts with.
const SYNC: [u8; 12] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];

/// The EDC polynomial, reversed for a least-significant-bit-first CRC.
///
/// x^32 + x^31 + x^16 + x^15 + x^4 + x^3 + x + 1, which is `0x8001801B` written
/// the other way round. Quoting it in the usual direction and shifting right
/// anyway is a mistake that fails every sector on the disc, which at least
/// announces itself.
const POLY: u32 = 0xD801_8001;

/// What a sector turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sector {
    /// Checked, and its own error detection agrees.
    Sound,
    /// Checked, and it does not. The bytes are not what was written.
    Corrupt,
    /// Nothing to check: a Form 2 sector that carries no EDC, or a mode this
    /// does not know. Silence, not approval.
    Unchecked,
}

/// Check one raw sector.
pub fn check(sector: &[u8]) -> Sector {
    if sector.len() < SECTOR || sector[..12] != SYNC {
        return Sector::Corrupt;
    }
    let (over, stored) = match sector[15] {
        1 => (0..2064, 2064),
        2 if sector[18] & 0x20 == 0 => (16..2072, 2072),
        // Form 2 carries an EDC only sometimes, and zero means it does not.
        2 => {
            if u32::from_le_bytes([sector[2348], sector[2349], sector[2350], sector[2351]]) == 0 {
                return Sector::Unchecked;
            }
            (16..2348, 2348)
        }
        _ => return Sector::Unchecked,
    };
    let want = u32::from_le_bytes([
        sector[stored],
        sector[stored + 1],
        sector[stored + 2],
        sector[stored + 3],
    ]);
    if crc(&sector[over]) == want { Sector::Sound } else { Sector::Corrupt }
}

/// Does this look like a track of data sectors at all?
///
/// Audio has no sync pattern, no header and no error detection, so running
/// this over an audio track would call every sector of it corrupt. Ask first.
pub fn looks_like_data(sector: &[u8]) -> bool {
    sector.len() >= 12 && sector[..12] == SYNC
}

/// Where a sector says it lives, from its own header.
///
/// Answers nothing for a sector without a readable header.
pub fn address(sector: &[u8]) -> Option<i64> {
    if sector.len() < 16 || sector[..12] != SYNC {
        return None;
    }
    let bcd = |b: u8| i64::from((b >> 4) * 10 + (b & 0x0F));
    Some((bcd(sector[12]) * 60 + bcd(sector[13])) * 75 + bcd(sector[14]) - 150)
}

/// What a whole track came to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Checked {
    pub sectors: u64,
    /// Sectors that had an EDC and agreed with it.
    pub sound: u64,
    /// Sectors whose own error detection says they are not what was written.
    pub corrupt: u64,
    /// Sectors carrying no error detection to check.
    pub unchecked: u64,
    /// Sectors not where the file says they should be.
    pub misplaced: u64,
}

impl Checked {
    /// Is there anything wrong with this track that the track itself knows of?
    pub fn is_sound(&self) -> bool {
        self.corrupt == 0 && self.misplaced == 0
    }
}

/// How much is read at a time. A whole number of sectors, on purpose.
const CHUNK: usize = SECTOR * 1024;

/// Check every sector of a dumped data track.
///
/// `base` is the sector the file starts at, so that each sector can be held to
/// the address written in its own header.
pub fn of_file(
    fs: &dyn Fs,
    path: &Path,
    base: u64,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Checked> {
    let total = fs.size(path)?;
    if total % SECTOR as u64 != 0 {
        return Err(Error(format!(
            "{}: {total} bytes is not a whole number of {SECTOR}-byte sectors",
            path.display()
        )));
    }
    let mut out = Checked { sectors: total / SECTOR as u64, ..Checked::default() };
    let mut at = 0u64;
    while at < total {
        let want = CHUNK.min((total - at) as usize);
        let chunk = fs.read_range(path, at, want)?;
        for (i, sector) in chunk.as_chunks::<SECTOR>().0.iter().enumerate() {
            match check(sector) {
                Sector::Sound => out.sound += 1,
                Sector::Corrupt => out.corrupt += 1,
                Sector::Unchecked => out.unchecked += 1,
            }
            let here = base + at / SECTOR as u64 + i as u64;
            if address(sector).is_some_and(|a| a != here as i64) {
                out.misplaced += 1;
            }
        }
        at += chunk.len() as u64;
        progress(at, total);
        if chunk.len() < want {
            break;
        }
    }
    Ok(out)
}

fn crc(data: &[u8]) -> u32 {
    let mut c: u32 = 0;
    for &b in data {
        let mut r = (c ^ u32::from(b)) & 0xFF;
        for _ in 0..8 {
            r = if r & 1 == 1 { (r >> 1) ^ POLY } else { r >> 1 };
        }
        c = (c >> 8) ^ r;
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The twelve bytes, then a header at 00:02:00 in the given mode.
    fn framed(mode: u8) -> Vec<u8> {
        let mut s = vec![0u8; SECTOR];
        s[..12].copy_from_slice(&SYNC);
        s[12..15].copy_from_slice(&[0x00, 0x02, 0x00]);
        s[15] = mode;
        s
    }

    fn mode1() -> Vec<u8> {
        let mut s = framed(1);
        for i in 0..2048 {
            s[16 + i] = (i * 7 + 3) as u8;
        }
        // Worked out by a separate implementation, so that this is a check of
        // the arithmetic and not of itself.
        s[2064..2068].copy_from_slice(&0x68EE_F435u32.to_le_bytes());
        s
    }

    fn mode2_form1() -> Vec<u8> {
        let mut s = framed(2);
        s[12..15].copy_from_slice(&[0x00, 0x02, 0x10]);
        s[16..24].copy_from_slice(&[0, 0, 0x08, 0, 0, 0, 0x08, 0]);
        for i in 0..2048 {
            s[24 + i] = (i * 5 + 11) as u8;
        }
        s[2072..2076].copy_from_slice(&0x1410_C3FBu32.to_le_bytes());
        s
    }

    #[test]
    fn a_sound_mode_one_sector_agrees_with_its_own_error_detection() {
        assert_eq!(check(&mode1()), Sector::Sound);
    }

    #[test]
    fn a_sound_mode_two_form_one_sector_does_too() {
        assert_eq!(check(&mode2_form1()), Sector::Sound);
    }

    #[test]
    fn one_wrong_byte_anywhere_in_the_user_data_is_caught() {
        for at in [16, 1000, 2063] {
            let mut s = mode1();
            s[at] ^= 0x01;
            assert_eq!(check(&s), Sector::Corrupt, "a flipped bit at {at}");
        }
    }

    #[test]
    fn a_wrong_byte_in_the_header_is_caught_as_well() {
        // The mode 1 EDC covers the sync and header too, which is what makes a
        // sector read from the wrong place detectable and not merely unlikely.
        let mut s = mode1();
        s[14] ^= 0x01;
        assert_eq!(check(&s), Sector::Corrupt);
    }

    #[test]
    fn a_sector_without_a_sync_pattern_is_not_a_sector() {
        let mut s = mode1();
        s[3] = 0;
        assert_eq!(check(&s), Sector::Corrupt);
    }

    #[test]
    fn a_form_two_sector_carrying_no_edc_is_unchecked_rather_than_sound() {
        // Saying nothing. The whole point of the distinction is that a track
        // of these has not been verified, however many sectors came back.
        let mut s = framed(2);
        s[18] = 0x20;
        assert_eq!(check(&s), Sector::Unchecked);
        let checked = Checked { unchecked: 10, ..Checked::default() };
        assert!(checked.is_sound(), "nothing is known to be wrong");
        assert_eq!(checked.sound, 0, "and nothing is known to be right either");
    }

    #[test]
    fn an_audio_sector_has_nothing_to_check() {
        assert_eq!(check(&vec![0x5A; SECTOR]), Sector::Corrupt, "no sync pattern at all");
    }

    #[test]
    fn audio_is_recognised_as_having_nothing_to_check() {
        assert!(looks_like_data(&mode1()));
        assert!(!looks_like_data(&vec![0x5A; SECTOR]), "an audio track is not data");
        assert!(!looks_like_data(&[]));
    }

    #[test]
    fn a_sector_says_where_it_lives() {
        let s = mode1();
        assert_eq!(address(&s), Some(0), "00:02:00 is the first sector of a disc");
        let mut later = mode1();
        later[12..15].copy_from_slice(&[0x01, 0x30, 0x25]);
        assert_eq!(address(&later), Some(90 * 75 + 25 - 150), "01:30:25");
    }

    #[test]
    fn a_track_is_only_sound_when_nothing_in_it_is_known_to_be_wrong() {
        assert!(Checked { sound: 100, ..Checked::default() }.is_sound());
        assert!(!Checked { sound: 99, corrupt: 1, ..Checked::default() }.is_sound());
        assert!(!Checked { sound: 99, misplaced: 1, ..Checked::default() }.is_sound());
    }

    #[test]
    fn a_file_that_is_not_whole_sectors_is_an_error_rather_than_a_verdict() {
        let fs = crate::host::FakeFs::new();
        fs.write(Path::new("/x.bin"), &[0u8; 100]).unwrap();
        assert!(of_file(&fs, Path::new("/x.bin"), 0, &mut |_, _| {}).is_err());
    }

    #[test]
    fn a_whole_track_is_checked_sector_by_sector() {
        let fs = crate::host::FakeFs::new();
        let mut track = mode1();
        track.extend_from_slice(&mode2_form1());
        let mut broken = mode1();
        broken[500] ^= 0xFF;
        track.extend_from_slice(&broken);
        fs.write(Path::new("/t.bin"), &track).unwrap();

        let out = of_file(&fs, Path::new("/t.bin"), 0, &mut |_, _| {}).unwrap();
        assert_eq!(out.sectors, 3);
        assert_eq!(out.sound, 2);
        assert_eq!(out.corrupt, 1);
        assert!(!out.is_sound());
    }

    #[test]
    fn a_sector_in_the_wrong_place_is_noticed_even_when_it_is_sound_in_itself() {
        // A span cut one sector wrong leaves every sector perfectly valid and
        // the whole track shifted, which no checksum of a sector can see.
        let fs = crate::host::FakeFs::new();
        fs.write(Path::new("/t.bin"), &mode1()).unwrap();
        let right = of_file(&fs, Path::new("/t.bin"), 0, &mut |_, _| {}).unwrap();
        assert_eq!(right.misplaced, 0);
        let shifted = of_file(&fs, Path::new("/t.bin"), 1, &mut |_, _| {}).unwrap();
        assert_eq!(shifted.misplaced, 1, "it says it is sector 0 and it is filed as 1");
        assert!(!shifted.is_sound());
    }
}

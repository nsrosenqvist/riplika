//! What a game disc says about itself, before anything is read off it.
//!
//! Not much, and that is the honest position. A PC disc carries a volume label
//! and nothing else - as weak a clue as a DVD's, and for the same reason: it is
//! whatever the publisher typed. What it cannot tell you is which release this
//! is, so identification has to wait until the disc has been dumped and can be
//! matched by [hash](crate::redump).
//!
//! PlayStation discs are the exception worth taking. They carry `SYSTEM.CNF`,
//! naming the executable to boot, and that name is the title's catalogue
//! serial: `SLUS-20202` and the like. Real identification, available before a
//! single sector of the game itself is read.

use crate::rip::iso::{SECTOR, dir_entries, parse_pvd_root};
use crate::{Error, Result};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GameDisc {
    /// The ISO 9660 volume label. Often the publisher's shouting.
    pub label: Option<String>,
    /// PlayStation's serial for the title, when the disc is one.
    pub serial: Option<String>,
    /// What is in the root directory, which is the rest of what can be known.
    pub root: Vec<String>,
}

impl GameDisc {
    /// Something to call the disc before anything has been matched.
    pub fn describe(&self) -> String {
        match (&self.label, &self.serial) {
            (Some(label), Some(serial)) => format!("{label} ({serial})"),
            (Some(label), None) => label.clone(),
            (None, Some(serial)) => serial.clone(),
            (None, None) => "unnamed disc".into(),
        }
    }
}

/// Read what the disc will say. `read` takes an LBA and a sector count.
pub fn inspect(read: &mut dyn FnMut(u64, usize) -> Result<Vec<u8>>) -> Result<GameDisc> {
    let pvd = read(16, 1)?;
    let Some((root_lba, root_size)) = parse_pvd_root(&pvd) else {
        // No ISO 9660. A pressed PC-DVD is often pure UDF, and there is no
        // directory here this can walk - but the disc still says what it is
        // called, and a disc arriving as "unnamed" with its name on the box is
        // the difference between a dump somebody can file and one they cannot.
        return match crate::disc::udf_label_by(read) {
            Some(label) => Ok(GameDisc { label: Some(label), ..Default::default() }),
            None => Err(Error("not an ISO 9660 volume".into())),
        };
    };

    let mut disc = GameDisc { label: crate::disc::volume_label(&pvd), ..Default::default() };

    let sectors = root_size.div_ceil(SECTOR as u32) as usize;
    let root = read(root_lba as u64, sectors)?;
    let entries = dir_entries(&root, root_size as usize);
    disc.root = entries.iter().map(|e| e.name.clone()).collect();

    if let Some(cnf) = entries.iter().find(|e| e.name.eq_ignore_ascii_case("SYSTEM.CNF")) {
        let sectors = (cnf.size as usize).div_ceil(SECTOR).max(1);
        if let Ok(bytes) = read(cnf.lba as u64, sectors) {
            let text = String::from_utf8_lossy(&bytes[..(cnf.size as usize).min(bytes.len())]);
            disc.serial = serial_in(&text);
        }
    }
    Ok(disc)
}

/// The catalogue serial out of a PlayStation `SYSTEM.CNF`.
///
/// The file names the executable to boot - `cdrom0:\SLUS_202.02;1` - and that
/// name is the serial, written the way a filename has to be written. Turning it
/// back into the form the catalogues use means undoing that: the underscore was
/// a hyphen and the dot was nothing at all.
pub fn serial_in(system_cnf: &str) -> Option<String> {
    for line in system_cnf.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        // BOOT on a PlayStation, BOOT2 on a PlayStation 2.
        if !key.trim().to_ascii_uppercase().starts_with("BOOT") {
            continue;
        }
        // Strip the device and any path in front of it.
        let file = value.trim().rsplit(['\\', '/', ':']).next()?.trim();
        // And the ISO 9660 version suffix behind it.
        let stem = file.split(';').next()?.trim();
        let serial: String = stem
            .chars()
            .filter(|c| *c != '.')
            .map(|c| if c == '_' { '-' } else { c.to_ascii_uppercase() })
            .collect();
        // A boot line that names something else entirely is not a serial.
        if serial.len() >= 8 && serial.contains('-') {
            return Some(serial);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_playstation_2_disc_gives_up_its_serial() {
        let cnf = "BOOT2 = cdrom0:\\SLUS_202.02;1\nVER = 1.00\nVMODE = NTSC\n";
        assert_eq!(serial_in(cnf).as_deref(), Some("SLUS-20202"));
    }

    #[test]
    fn a_playstation_1_disc_uses_the_older_key() {
        let cnf = "BOOT = cdrom:\\SLUS_007.77;1\nTCB = 4\nEVENT = 10\nSTACK = 801FFFF0\n";
        assert_eq!(serial_in(cnf).as_deref(), Some("SLUS-00777"));
    }

    #[test]
    fn european_and_japanese_serials_come_out_the_same_way() {
        assert_eq!(serial_in("BOOT2 = cdrom0:\\SLES_512.34;1").as_deref(), Some("SLES-51234"));
        assert_eq!(serial_in("BOOT = cdrom:\\SLPS_001.23;1").as_deref(), Some("SLPS-00123"));
    }

    #[test]
    fn spacing_and_case_are_however_the_publisher_left_them() {
        assert_eq!(serial_in("boot2=cdrom0:\\slus_202.02;1").as_deref(), Some("SLUS-20202"));
        assert_eq!(
            serial_in("  BOOT2   =   cdrom0:\\SLUS_202.02;1  ").as_deref(),
            Some("SLUS-20202")
        );
    }

    #[test]
    fn a_file_with_no_boot_line_has_no_serial() {
        assert_eq!(serial_in("VER = 1.00\nVMODE = PAL\n"), None);
        assert_eq!(serial_in(""), None);
    }

    #[test]
    fn a_boot_line_naming_something_that_is_not_a_serial_is_refused() {
        // Homebrew and demo discs boot from all sorts of things, and calling
        // one of those a catalogue serial would send it to the wrong name.
        assert_eq!(serial_in("BOOT2 = cdrom0:\\MAIN.ELF;1"), None);
        assert_eq!(serial_in("BOOT = cdrom:\\PSX.EXE;1"), None);
    }

    #[test]
    fn a_disc_is_described_by_whatever_it_actually_offered() {
        let both = GameDisc {
            label: Some("SLUS_202.02".into()),
            serial: Some("SLUS-20202".into()),
            root: Vec::new(),
        };
        assert_eq!(both.describe(), "SLUS_202.02 (SLUS-20202)");
        assert_eq!(
            GameDisc { label: Some("HALF_LIFE".into()), ..Default::default() }.describe(),
            "HALF_LIFE"
        );
        assert_eq!(GameDisc::default().describe(), "unnamed disc");
    }
}

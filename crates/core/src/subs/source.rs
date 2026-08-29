//! Getting VobSub data out of whatever the user points us at.
//!
//! A Matroska file is read directly - see [`crate::subs::matroska`] - because
//! the alternative was `mkvextract`, and MKVToolNix requires Qt for every one
//! of its tools. Anything else is remuxed to Matroska with ffmpeg first, which
//! is one process rather than two and needs nothing beyond what is already
//! here.

use crate::host::{Command, Runner};
use crate::subs::vobsub::{self, Idx};
use crate::{Error, Result};
use std::path::{Path, PathBuf};

pub struct Source {
    pub idx: Idx,
    pub sub: Vec<u8>,
    /// Set when the subtitles came from Matroska, where a block is a whole SPU
    /// and there is nothing to seek within.
    pub packets: Option<Vec<crate::subs::matroska::Packet>>,
    _tmp: Option<TempDir>,
}

impl Source {
    /// Decode whichever way this source stores its subtitles.
    pub fn events(&self) -> Vec<crate::subs::vobsub::Event> {
        match &self.packets {
            // From Matroska, where a block is a whole SPU
            Some(packets) => crate::subs::vobsub::decode_packets(packets),
            // From a .idx/.sub pair, where the index says where each starts
            None => crate::subs::vobsub::decode_all(&self.idx, &self.sub),
        }
    }
}

pub struct TempDir(pub PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn temp_dir(tag: &str) -> Result<TempDir> {
    let mut p = std::env::temp_dir();
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    p.push(format!("riplika-{}-{}-{}", tag, std::process::id(), n));
    std::fs::create_dir_all(&p).map_err(|e| Error(format!("{}: {e}", p.display())))?;
    Ok(TempDir(p))
}

/// Copy one subtitle stream into a Matroska file of its own.
///
/// Only needed when the input is not already Matroska. `-c:s copy` because
/// re-encoding a bitmap subtitle would change the very pixels being recognised.
pub fn remux_command(input: &Path, stream: usize, mkv: &Path) -> Command {
    Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y", "-i"])
        .path(input)
        .args(["-map", &format!("0:s:{stream}"), "-c:s", "copy"])
        .path(mkv)
}

/// Is this file already Matroska?
///
/// By its magic rather than its name: a rip is `.mkv`, but a file that arrived
/// some other way may not be, and the four bytes are certain either way.
pub fn is_matroska(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic).is_ok() && magic == [0x1A, 0x45, 0xDF, 0xA3]
}

/// Load VobSub from a `.idx` (with its `.sub` beside it) or from a video file.
pub fn load(runner: &dyn Runner, path: &Path, stream: usize) -> Result<Source> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();

    if ext == "idx" {
        let idx = vobsub::parse_idx(path)?;
        let sub_path = path.with_extension("sub");
        let sub =
            std::fs::read(&sub_path).map_err(|e| Error(format!("{}: {e}", sub_path.display())))?;
        return Ok(Source { idx, sub, packets: None, _tmp: None });
    }

    // Already Matroska: read it where it lies. A rip is Matroska, so this is
    // the usual case, and it saves copying a subtitle track out first.
    if is_matroska(path) {
        let data = std::fs::read(path).map_err(|e| Error(format!("{}: {e}", path.display())))?;
        let track = crate::subs::matroska::read_vobsub(&data, stream)?;
        return Ok(Source {
            idx: Idx { palette: track.palette, events: Vec::new() },
            sub: Vec::new(),
            packets: Some(track.packets),
            _tmp: None,
        });
    }

    let tmp = temp_dir("sub")?;
    let mkv = tmp.0.join("s.mkv");
    runner.require(&remux_command(path, stream, &mkv))?;
    let data = std::fs::read(&mkv).map_err(|e| Error(format!("{}: {e}", mkv.display())))?;
    let track = crate::subs::matroska::read_vobsub(&data, 0)?;

    Ok(Source {
        idx: Idx { palette: track.palette, events: Vec::new() },
        sub: Vec::new(),
        packets: Some(track.packets),
        _tmp: Some(tmp),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remux_selects_the_requested_stream_and_copies_it() {
        let ff = remux_command(Path::new("/rip/t00.mp4"), 2, Path::new("/tmp/s.mkv"));
        assert_eq!(ff.value_of("-map"), Some("0:s:2"));
        // re-encoding a bitmap subtitle would change the pixels we recognise
        assert_eq!(ff.value_of("-c:s"), Some("copy"));
        assert!(ff.has("-nostdin"));
    }

    #[test]
    fn matroska_is_recognised_by_its_magic_not_its_name() {
        let dir = std::env::temp_dir().join(format!("riplika-magic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mkv = dir.join("named-wrongly.dat");
        std::fs::write(&mkv, [0x1A, 0x45, 0xDF, 0xA3, 0, 0]).unwrap();
        let other = dir.join("looks-like.mkv");
        std::fs::write(&other, b"not matroska at all").unwrap();
        assert!(is_matroska(&mkv));
        assert!(!is_matroska(&other));
        assert!(!is_matroska(&dir.join("absent")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

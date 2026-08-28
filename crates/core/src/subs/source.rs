//! Getting VobSub data out of whatever the user points us at.
//!
//! Shelling out to ffmpeg and mkvextract rather than binding libav: container
//! handling is the part most likely to change under us, and these two are the
//! reference implementations of it.

use crate::host::{Command, Runner};
use crate::subs::vobsub::{self, Idx};
use crate::{Error, Result};
use std::path::{Path, PathBuf};

pub struct Source {
    pub idx: Idx,
    pub sub: Vec<u8>,
    _tmp: Option<TempDir>,
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

/// Commands that pull one subtitle stream out into a standalone `.idx`/`.sub`.
///
/// Two steps because neither tool does both: ffmpeg can select a stream from
/// any container but will not write VobSub's split index/data pair, and
/// mkvextract writes the pair but only from Matroska.
pub fn extract_commands(input: &Path, stream: usize, mkv: &Path, idx: &Path) -> [Command; 2] {
    [
        Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-y", "-i"])
            .path(input)
            .args(["-map", &format!("0:s:{stream}"), "-c:s", "copy"])
            .path(mkv),
        Command::new("mkvextract")
            .arg("tracks")
            .path(mkv)
            .arg(format!("0:{}", idx.display())),
    ]
}

/// Load VobSub from a `.idx` (with its `.sub` beside it) or from a video file.
pub fn load(runner: &dyn Runner, path: &Path, stream: usize) -> Result<Source> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    if ext == "idx" {
        let idx = vobsub::parse_idx(path)?;
        let sub_path = path.with_extension("sub");
        let sub =
            std::fs::read(&sub_path).map_err(|e| Error(format!("{}: {e}", sub_path.display())))?;
        return Ok(Source { idx, sub, _tmp: None });
    }

    let tmp = temp_dir("sub")?;
    let mkv = tmp.0.join("s.mkv");
    let idx_path = tmp.0.join("v.idx");
    for cmd in extract_commands(path, stream, &mkv, &idx_path) {
        runner.require(&cmd)?;
    }

    let idx = vobsub::parse_idx(&idx_path)?;
    let sub_path = tmp.0.join("v.sub");
    let sub = std::fs::read(&sub_path).map_err(|e| Error(format!("{}: {e}", sub_path.display())))?;

    Ok(Source {
        idx,
        sub,
        _tmp: Some(tmp),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extraction_selects_the_requested_stream_and_copies_it() {
        let [ff, mk] = extract_commands(
            Path::new("/rip/t00.mkv"),
            2,
            Path::new("/tmp/s.mkv"),
            Path::new("/tmp/v.idx"),
        );
        assert_eq!(ff.value_of("-map"), Some("0:s:2"));
        // re-encoding a bitmap subtitle would change the pixels we recognise
        assert_eq!(ff.value_of("-c:s"), Some("copy"));
        assert!(ff.has("-nostdin"));
        assert_eq!(mk.args.last().unwrap(), "0:/tmp/v.idx");
    }
}

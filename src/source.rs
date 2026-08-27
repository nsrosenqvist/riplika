//! Getting VobSub data out of whatever the user points us at.
//!
//! Shelling out to ffmpeg/mkvextract rather than binding libav: the container
//! handling is the part most likely to change under us, and these tools are
//! already the reference implementations.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::vobsub::{self, Idx};

pub struct Source {
    pub idx: Idx,
    pub sub: Vec<u8>,
    _tmp: Option<TempDir>,
}

pub struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_dir(tag: &str) -> Result<TempDir, String> {
    let mut p = std::env::temp_dir();
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    p.push(format!("ripper-{}-{}-{}", tag, std::process::id(), n));
    std::fs::create_dir_all(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    Ok(TempDir(p))
}

fn run(cmd: &mut Command) -> Result<(), String> {
    let out = cmd
        .output()
        .map_err(|e| format!("{:?}: {e}", cmd.get_program()))?;
    if !out.status.success() {
        return Err(format!(
            "{:?} failed: {}",
            cmd.get_program(),
            String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("")
        ));
    }
    Ok(())
}

/// Index of the English *bitmap* subtitle stream, counting subtitle streams only.
///
/// A file may already carry a text track (that is often the point of running
/// this), so language alone is not enough - we need the VobSub/PGS one.
fn english_sub_index(path: &Path) -> usize {
    let out = Command::new("ffprobe")
        .args([
            "-v", "error", "-select_streams", "s",
            "-show_entries", "stream=codec_name:stream_tags=language",
            "-of", "csv=p=0",
        ])
        .arg(path)
        .output();
    let Ok(out) = out else { return 0 };
    let text = String::from_utf8_lossy(&out.stdout);
    let rows: Vec<(String, String)> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut it = l.split(',');
            let codec = it.next().unwrap_or("").trim().to_string();
            let lang = it.next().unwrap_or("").trim().to_string();
            (codec, lang)
        })
        .collect();
    let bitmap = |c: &str| c == "dvd_subtitle" || c == "hdmv_pgs_subtitle";
    rows.iter()
        .position(|(c, l)| bitmap(c) && l == "eng")
        .or_else(|| rows.iter().position(|(c, _)| bitmap(c)))
        .unwrap_or(0)
}

/// Load VobSub from a `.idx` (with its `.sub` beside it) or straight from a
/// video file, picking the English bitmap track.
pub fn load(path: &Path, stream: Option<usize>) -> Result<Source, String> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    if ext == "idx" {
        let idx = vobsub::parse_idx(path)?;
        let sub_path = path.with_extension("sub");
        let sub = std::fs::read(&sub_path).map_err(|e| format!("{}: {e}", sub_path.display()))?;
        return Ok(Source {
            idx,
            sub,
            _tmp: None,
        });
    }

    let tmp = temp_dir("sub")?;
    let mkv = tmp.0.join("s.mkv");
    let n = stream.unwrap_or_else(|| english_sub_index(path));

    run(Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(path)
        .args(["-map", &format!("0:s:{n}"), "-c:s", "copy"])
        .arg(&mkv))?;

    let idx_path = tmp.0.join("v.idx");
    run(Command::new("mkvextract")
        .arg("tracks")
        .arg(&mkv)
        .arg(format!("0:{}", idx_path.display())))?;

    let idx = vobsub::parse_idx(&idx_path)?;
    let sub_path = tmp.0.join("v.sub");
    let sub = std::fs::read(&sub_path).map_err(|e| format!("{}: {e}", sub_path.display()))?;

    Ok(Source {
        idx,
        sub,
        _tmp: Some(tmp),
    })
}

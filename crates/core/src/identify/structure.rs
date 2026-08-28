//! Working out what a ripped disc actually contains, from the disc alone.
//!
//! MakeMKV hands back a pile of titles with meaningless names, in an order that
//! is not broadcast order. The disc does know, though, because of the "play all"
//! title: a title that replays the episodes back to back, and whose chapter list
//! is therefore their chapter lists concatenated. Decomposing it recovers both
//! which titles are episodes and what order they go in - with no network and no
//! guessing.
//!
//! What is left over is either an extended cut of an episode (same content,
//! longer) or an extra. Telling those apart needs to look at the pictures, so
//! that part hashes frames; everything else here is arithmetic on chapter
//! durations.

use crate::host::{Command, Runner};
use crate::model::Millis;
use crate::Result;
use std::path::Path;

/// The shape of one title: enough to reason about, without the video.
#[derive(Debug, Clone, PartialEq)]
pub struct TitleShape {
    /// How the caller refers to this title - a filename, usually.
    pub key: String,
    /// Position on the disc. Play-alls are considered in this order, because
    /// disc layout follows broadcast order while durations do not: a
    /// five-episode run would otherwise sort ahead of the two-episode premiere
    /// that precedes it.
    pub order: u32,
    pub duration: Millis,
    pub chapters: Vec<Millis>,
}

/// Chapter boundaries that differ by less than this are the same boundary.
/// Durations come back through a float seconds field, so exact equality is not
/// safe even when the underlying frames are identical.
const CHAPTER_TOLERANCE: Millis = 100;

/// How a disc's titles sort out.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Structure {
    /// Play-all titles, each with the titles it decomposes into.
    pub play_alls: Vec<(String, Vec<String>)>,
    /// Episodes, in the order the play-alls put them.
    pub episodes: Vec<String>,
    /// Episode-length titles no play-all claims. Candidates for extended cuts.
    pub loose: Vec<String>,
    /// Everything else.
    pub extras: Vec<String>,
}

/// What counts as episode-length. Wide, because a "22-minute" comedy runs
/// anywhere from 20 to 24 and a drama twice that.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EpisodeRange {
    pub min: Millis,
    pub max: Millis,
}

impl Default for EpisodeRange {
    fn default() -> Self {
        EpisodeRange {
            min: 15 * 60 * 1000,
            max: 45 * 60 * 1000,
        }
    }
}

impl EpisodeRange {
    pub fn contains(&self, d: Millis) -> bool {
        d >= self.min && d <= self.max
    }
}

fn chapters_match(a: &[Millis], b: &[Millis]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.abs_diff(*y) <= CHAPTER_TOLERANCE)
}

/// Sort a disc's titles into episodes, play-alls and extras.
pub fn decompose(titles: &[TitleShape], range: EpisodeRange) -> Structure {
    // Only episode-length titles with chapters can be *parts* of a play-all.
    let parts: Vec<&TitleShape> = titles
        .iter()
        .filter(|t| range.contains(t.duration) && !t.chapters.is_empty())
        .collect();

    let mut play_alls: Vec<(String, Vec<String>)> = Vec::new();
    for t in titles {
        if t.chapters.is_empty() {
            continue;
        }
        // Greedily peel known chapter lists off the front.
        let mut rest: &[Millis] = &t.chapters;
        let mut seq: Vec<String> = Vec::new();
        loop {
            let mut matched = false;
            for p in &parts {
                if p.key == t.key {
                    continue; // a title trivially decomposes into itself
                }
                if seq.contains(&p.key) {
                    // A play-all plays each episode once. Without this, two
                    // episodes with identical chapter layouts - common when a
                    // disc uses one act break per episode - both resolve to
                    // whichever comes first, and the second is lost.
                    continue;
                }
                let n = p.chapters.len();
                if n > 0 && n <= rest.len() && chapters_match(&rest[..n], &p.chapters) {
                    seq.push(p.key.clone());
                    rest = &rest[n..];
                    matched = true;
                    break;
                }
            }
            if !matched {
                break;
            }
        }
        // Two or more parts, fully consumed, and not itself: anything less is a
        // coincidence rather than a play-all.
        if seq.len() >= 2 && rest.is_empty() && !seq.contains(&t.key) {
            play_alls.push((t.key.clone(), seq));
        }
    }
    play_alls.sort_by_key(|(k, _)| titles.iter().find(|t| &t.key == k).map(|t| t.order).unwrap_or(u32::MAX));

    let mut episodes: Vec<String> = Vec::new();
    for (_, seq) in &play_alls {
        for k in seq {
            if !episodes.contains(k) {
                episodes.push(k.clone());
            }
        }
    }

    let is_play_all = |k: &str| play_alls.iter().any(|(p, _)| p == k);
    let mut loose = Vec::new();
    let mut extras = Vec::new();
    for t in titles {
        if episodes.contains(&t.key) || is_play_all(&t.key) {
            continue;
        }
        if range.contains(t.duration) {
            loose.push(t.key.clone());
        } else {
            extras.push(t.key.clone());
        }
    }

    Structure {
        play_alls,
        episodes,
        loose,
        extras,
    }
}

/// When a disc has no play-all, fall back to duration clustering.
///
/// Episodes on a disc run to a house length, so the titles within a couple of
/// minutes of the most common length are the episodes and the rest are extras.
/// Order can then only come from the disc layout, which is usually right.
pub fn episodes_by_duration(titles: &[TitleShape], range: EpisodeRange) -> Vec<String> {
    let candidates: Vec<&TitleShape> = titles.iter().filter(|t| range.contains(t.duration)).collect();
    if candidates.is_empty() {
        return Vec::new();
    }
    // The cluster with the most members, within two minutes of its seed.
    let window: Millis = 2 * 60 * 1000;
    let best = candidates
        .iter()
        .max_by_key(|seed| {
            candidates
                .iter()
                .filter(|t| t.duration.abs_diff(seed.duration) <= window)
                .count()
        })
        .unwrap();
    let mut hits: Vec<&&TitleShape> = candidates
        .iter()
        .filter(|t| t.duration.abs_diff(best.duration) <= window)
        .collect();
    hits.sort_by_key(|t| t.order);
    hits.iter().map(|t| t.key.clone()).collect()
}

/// One frame reduced to 256 bits: above or below the frame's mean brightness.
pub type FrameHash = u128;

/// Sample a frame a second, scaled right down and greyscaled.
///
/// The frames go to a file rather than to stdout. Captured output is text, and
/// putting raw greyscale through a UTF-8 conversion replaces every invalid byte
/// with U+FFFD - which both corrupts the pixels and changes the length, so the
/// hashes come out of alignment and nothing ever matches anything.
pub fn hash_command(path: &Path, fps: u32, size: u32, dest: &Path) -> Command {
    Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y", "-i"])
        .path(path)
        .args([
            "-vf",
            &format!("fps={fps},scale={size}:{size},format=gray"),
            "-f",
            "rawvideo",
        ])
        .path(dest)
}

/// Turn raw grey bytes into one hash per frame.
pub fn hash_frames(raw: &[u8], size: usize) -> Vec<FrameHash> {
    let n = size * size;
    // 128 bits is the widest integer available, so sample that many pixels
    // evenly across the frame rather than taking the first 128.
    let step = (n / 128).max(1);
    raw.chunks_exact(n)
        .map(|frame| {
            let mean = frame.iter().map(|b| *b as u32).sum::<u32>() / n as u32;
            let mut h: FrameHash = 0;
            for i in 0..128 {
                let p = frame[(i * step) % n];
                h = (h << 1) | FrameHash::from(p as u32 > mean);
            }
            h
        })
        .collect()
}

/// Share of `a`'s frames that appear somewhere in `b`.
///
/// Asymmetric on purpose: an extended cut contains the whole episode plus more,
/// so the episode's frames are nearly all present in the longer title while the
/// reverse is not true.
pub fn similarity(a: &[FrameHash], b: &[FrameHash], tolerance: u32) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let hits = a
        .iter()
        .filter(|h| b.iter().any(|x| (*h ^ x).count_ones() <= tolerance))
        .count();
    hits as f32 / a.len() as f32
}

/// Hamming distance below which two sampled frames are the same shot.
pub const HASH_TOLERANCE: u32 = 8;

/// Enough shared frames to call it the same content.
///
/// Low because the sampling is one frame a second against a *re-encoded*
/// version of the same footage; a strict threshold rejects genuine matches.
pub const SAME_CONTENT: f32 = 0.15;

/// Hash a file's frames.
pub fn hash_file(runner: &dyn Runner, path: &Path) -> Result<Vec<FrameHash>> {
    let size = 16;
    let tmp = crate::subs::source::temp_dir("hash")?;
    let raw = tmp.0.join("frames.gray");
    runner.require(&hash_command(path, 1, size, &raw))?;
    let data = std::fs::read(&raw).unwrap_or_default();
    Ok(hash_frames(&data, size as usize))
}

/// Which loose titles are longer versions of which episodes.
pub fn find_extended_cuts(
    runner: &dyn Runner,
    dir: &Path,
    loose: &[String],
    episodes: &[String],
) -> Result<Vec<(String, String, f32)>> {
    // Hash each episode once. Decoding them again for every loose title turns a
    // two-minute step into a twenty-minute one on a full disc.
    let mut cache: std::collections::HashMap<&String, Vec<FrameHash>> =
        std::collections::HashMap::new();
    for e in episodes {
        cache.insert(e, hash_file(runner, &dir.join(e))?);
    }

    let mut out = Vec::new();
    for l in loose {
        let lh = hash_file(runner, &dir.join(l))?;
        let mut best: Option<(String, f32)> = None;
        for e in episodes {
            let eh = &cache[e];
            let s = similarity(eh, &lh, HASH_TOLERANCE);
            if best.as_ref().is_none_or(|(_, b)| s > *b) {
                best = Some((e.clone(), s));
            }
        }
        if let Some((e, s)) = best
            && s >= SAME_CONTENT {
                out.push((l.clone(), e, s));
            }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(key: &str, order: u32, duration: Millis, chapters: &[Millis]) -> TitleShape {
        TitleShape {
            key: key.into(),
            order,
            duration,
            chapters: chapters.to_vec(),
        }
    }

    /// Two episodes, and a play-all that is both of them end to end.
    fn simple_disc() -> Vec<TitleShape> {
        vec![
            t("play.mkv", 0, 2_550_000, &[300_000, 500_000, 475_000, 300_000, 500_000, 475_000]),
            t("ep1.mkv", 1, 1_275_000, &[300_000, 500_000, 475_000]),
            t("ep2.mkv", 2, 1_275_000, &[300_000, 500_000, 475_000]),
            t("extra.mkv", 3, 180_000, &[180_000]),
        ]
    }

    #[test]
    fn a_play_all_is_decomposed_into_its_episodes() {
        let s = decompose(&simple_disc(), EpisodeRange::default());
        assert_eq!(s.play_alls.len(), 1);
        assert_eq!(s.play_alls[0].0, "play.mkv");
        assert_eq!(s.episodes, vec!["ep1.mkv", "ep2.mkv"]);
        assert_eq!(s.extras, vec!["extra.mkv"]);
    }

    #[test]
    fn a_title_does_not_decompose_into_itself() {
        // without the self-exclusion every title is trivially its own play-all
        let titles = vec![t("only.mkv", 0, 1_275_000, &[600_000, 675_000])];
        assert!(decompose(&titles, EpisodeRange::default()).play_alls.is_empty());
    }

    #[test]
    fn a_two_episode_play_all_is_found_despite_being_episode_length_itself() {
        // gating play-all detection on duration missed these: a two-episode
        // run is 43 minutes, well inside the range one extended episode covers
        let titles = vec![
            t("play.mkv", 0, 2_550_000, &[1_275_000, 1_275_000]),
            t("ep1.mkv", 1, 1_275_000, &[1_275_000]),
            t("ep2.mkv", 2, 1_275_000, &[1_275_000]),
        ];
        let s = decompose(&titles, EpisodeRange { min: 900_000, max: 2_700_000 });
        assert_eq!(s.episodes.len(), 2);
    }

    #[test]
    fn chapter_boundaries_match_within_a_rounding_tolerance() {
        // ffprobe reports seconds as a float string; the last digit moves
        let titles = vec![
            t("play.mkv", 0, 2_550_000, &[300_050, 974_960, 300_000, 975_000]),
            t("a.mkv", 1, 1_275_000, &[300_000, 975_000]),
            t("b.mkv", 2, 1_275_000, &[300_000, 975_000]),
        ];
        assert_eq!(decompose(&titles, EpisodeRange::default()).episodes.len(), 2);
    }

    #[test]
    fn a_partial_decomposition_is_rejected() {
        // leftover chapters mean this is not simply those episodes replayed
        let titles = vec![
            t("play.mkv", 0, 2_550_000, &[300_000, 975_000, 111_111]),
            t("a.mkv", 1, 1_275_000, &[300_000, 975_000]),
            t("b.mkv", 2, 1_275_000, &[300_000, 975_000]),
        ];
        assert!(decompose(&titles, EpisodeRange::default()).play_alls.is_empty());
    }

    #[test]
    fn episode_order_follows_disc_layout_not_duration() {
        let titles = vec![
            // the five-episode run sits later on the disc than the premiere
            t("pa2.mkv", 5, 3_825_000, &[1_275_000, 1_275_000, 1_275_000]),
            t("pa1.mkv", 0, 2_550_000, &[1_275_000, 1_275_000]),
            t("a.mkv", 1, 1_275_000, &[1_275_000]),
            t("b.mkv", 2, 1_275_000, &[1_275_000]),
            t("c.mkv", 3, 1_275_000, &[1_275_000]),
        ];
        let s = decompose(&titles, EpisodeRange::default());
        // pa1 comes first on the disc, so its episodes come first
        assert_eq!(s.play_alls[0].0, "pa1.mkv");
        assert_eq!(s.episodes[0], "a.mkv");
    }

    #[test]
    fn an_unclaimed_episode_length_title_is_loose_not_an_extra() {
        let mut titles = simple_disc();
        titles.push(t("long.mkv", 4, 1_500_000, &[1_500_000]));
        let s = decompose(&titles, EpisodeRange::default());
        assert_eq!(s.loose, vec!["long.mkv"]);
        assert!(!s.extras.contains(&"long.mkv".to_string()));
    }

    #[test]
    fn duration_clustering_covers_discs_with_no_play_all() {
        let titles = vec![
            t("a.mkv", 0, 1_275_000, &[]),
            t("b.mkv", 1, 1_290_000, &[]),
            t("c.mkv", 2, 1_260_000, &[]),
            t("bonus.mkv", 3, 2_400_000, &[]),
        ];
        assert_eq!(
            episodes_by_duration(&titles, EpisodeRange::default()),
            vec!["a.mkv", "b.mkv", "c.mkv"]
        );
    }

    #[test]
    fn identical_frames_hash_identically() {
        let frame_a = vec![0u8; 256];
        let mut frame_b = vec![0u8; 256];
        frame_b[0] = 255;
        let ha = hash_frames(&frame_a, 16);
        let hb = hash_frames(&frame_b, 16);
        assert_eq!(ha.len(), 1);
        assert_ne!(ha[0], hb[0]);
        assert_eq!(hash_frames(&frame_a, 16), ha);
    }

    #[test]
    fn similarity_is_asymmetric_so_a_longer_cut_still_matches() {
        let episode: Vec<FrameHash> = (0..100).map(|i| i as FrameHash).collect();
        let mut extended = episode.clone();
        extended.extend((1000..1100).map(|i| i as FrameHash));
        // every episode frame is in the extended cut
        assert!(similarity(&episode, &extended, 0) > 0.99);
        // but only half the extended cut is in the episode
        assert!(similarity(&extended, &episode, 0) < 0.6);
    }

    #[test]
    fn unrelated_content_scores_below_the_threshold() {
        let a: Vec<FrameHash> = (0..100).map(|i| (i as FrameHash) << 64).collect();
        let b: Vec<FrameHash> = (0..100).map(|i| (i as FrameHash) << 3 | 0x5555).collect();
        assert!(similarity(&a, &b, HASH_TOLERANCE) < SAME_CONTENT);
    }

    #[test]
    fn empty_input_is_zero_similarity_not_a_panic() {
        assert_eq!(similarity(&[], &[1, 2], 8), 0.0);
        assert_eq!(similarity(&[1, 2], &[], 8), 0.0);
    }
}

#[cfg(test)]
mod binary_tests {
    use super::*;

    #[test]
    fn frames_are_read_from_a_file_not_from_captured_output() {
        // captured output is text: raw greyscale through a UTF-8 conversion
        // becomes U+FFFD wherever a byte is not valid, which both corrupts the
        // pixels and changes the length
        let c = hash_command(Path::new("/rip/a.mkv"), 1, 16, Path::new("/tmp/f.gray"));
        assert_eq!(c.args.last().unwrap(), "/tmp/f.gray");
        assert!(!c.has("-"), "{}", c.display());
    }

    #[test]
    fn a_truncated_final_frame_is_discarded_rather_than_hashed() {
        // ffmpeg killed partway leaves a partial frame; hashing it would
        // produce a value that matches nothing and drags the score down
        let raw = vec![7u8; 256 + 100];
        assert_eq!(hash_frames(&raw, 16).len(), 1);
    }

    #[test]
    fn hashing_is_stable_across_calls() {
        let raw: Vec<u8> = (0..512).map(|i| (i % 256) as u8).collect();
        assert_eq!(hash_frames(&raw, 16), hash_frames(&raw, 16));
        assert_eq!(hash_frames(&raw, 16).len(), 2);
    }
}

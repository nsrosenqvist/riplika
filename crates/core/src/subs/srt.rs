//! SRT reading and writing.
//!
//! Cue timings always come from the subtitle stream itself, never from
//! recognition, so a rebuilt SRT is sample-accurate against the source.

#[derive(Debug, Clone, PartialEq)]
pub struct Cue {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

pub fn fmt_ts(ms: u64) -> String {
    let h = ms / 3_600_000;
    let m = (ms % 3_600_000) / 60_000;
    let s = (ms % 60_000) / 1000;
    format!("{:02}:{:02}:{:02},{:03}", h, m, s, ms % 1000)
}

pub fn parse_ts(t: &str) -> Option<u64> {
    let t = t.trim();
    let (hms, ms) = t.split_once(',').or_else(|| t.split_once('.'))?;
    let p: Vec<&str> = hms.split(':').collect();
    if p.len() != 3 {
        return None;
    }
    let h: u64 = p[0].trim().parse().ok()?;
    let m: u64 = p[1].trim().parse().ok()?;
    let s: u64 = p[2].trim().parse().ok()?;
    let ms: u64 = ms.trim().parse().ok()?;
    Some(((h * 60 + m) * 60 + s) * 1000 + ms)
}

pub fn write(cues: &[Cue]) -> String {
    let mut out = String::new();
    for (i, c) in cues.iter().enumerate() {
        out.push_str(&format!(
            "{}\n{} --> {}\n{}\n\n",
            i + 1,
            fmt_ts(c.start_ms),
            fmt_ts(c.end_ms),
            c.text
        ));
    }
    out
}

pub fn parse(s: &str) -> Vec<Cue> {
    let s = s.trim_start_matches('\u{feff}');
    let mut out = Vec::new();
    for block in s.split("\n\n") {
        let lines: Vec<&str> = block.lines().collect();
        if lines.len() < 3 {
            continue;
        }
        let Some((a, b)) = lines[1].split_once("-->") else {
            continue;
        };
        let (Some(start), Some(end)) = (parse_ts(a), parse_ts(b)) else {
            continue;
        };
        out.push(Cue {
            start_ms: start,
            end_ms: end,
            text: lines[2..].join("\n"),
        });
    }
    out
}

/// Fill in missing end times and stop cues overlapping the next one.
pub fn tidy(cues: &mut [Cue], ends: &[Option<u64>]) {
    for i in 0..cues.len() {
        if ends.get(i).copied().flatten().is_none() {
            cues[i].end_ms = if i + 1 < cues.len() {
                cues[i + 1].start_ms.saturating_sub(1)
            } else {
                cues[i].start_ms + 3000
            };
        }
        if i + 1 < cues.len() && cues[i].end_ms >= cues[i + 1].start_ms {
            cues[i].end_ms = cues[i + 1].start_ms.saturating_sub(1);
        }
        if cues[i].end_ms <= cues[i].start_ms {
            cues[i].end_ms = cues[i].start_ms + 800;
        }
    }
}

//! The glyph-table side of the tool: building a table, reviewing it, and
//! recognising with it.
//!
//! These are the commands that exist because recognition is *deterministic*.
//! A table is built once per disc font, checked by eye once, and then every
//! episode decodes by exact lookup. The reviewing commands matter as much as
//! the building ones: the one manual step is where the mistakes come from.

use ripper_core::host::RealRunner;
use ripper_core::subs::{segment, source, srt, table, vobsub};
use segment::SegOpts;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use table::Table;

pub fn build(
    inputs: &[PathBuf],
    table_path: &Path,
    reference: Option<&Path>,
    min_agreement: f32,
    name: Option<String>,
    stream: usize,
) -> Result<(), String> {
    let mut t = if table_path.exists() {
        Table::load(table_path).map_err(|e| e.to_string())?
    } else {
        Table::default()
    };
    if let Some(n) = name {
        t.source = n;
    }
    if t.version == 0 {
        t.version = 1;
    }

    let opts = SegOpts::default();
    let (mut n_ev, mut n_gl, mut n_votes, mut n_aligned, mut n_skipped) = (0, 0, 0, 0, 0);
    // per glyph: gaps observed with no space after, and with a space after
    let mut gapobs: BTreeMap<usize, (Vec<i32>, Vec<i32>)> = BTreeMap::new();

    for input in inputs {
        let src = match source::load(&RealRunner::default(), input, stream) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  skip {}: {e}", input.display());
                continue;
            }
        };
        let events = vobsub::decode_all(&src.idx, &src.sub);

        // reference cues keyed by start time - our SRTs share the stream's timing
        let refmap: BTreeMap<u64, String> = match reference {
            Some(dir) => {
                let stem = input.file_stem().unwrap_or_default().to_string_lossy();
                let p = dir.join(format!("{stem}.srt"));
                match std::fs::read_to_string(&p) {
                    Ok(s) => srt::parse(&s)
                        .into_iter()
                        .map(|c| (c.start_ms, c.text))
                        .collect(),
                    Err(_) => {
                        eprintln!("  no reference for {}", stem);
                        BTreeMap::new()
                    }
                }
            }
            None => BTreeMap::new(),
        };

        for ev in &events {
            n_ev += 1;
            let lines = segment::segment(&ev.spu, &src.idx.palette, &opts);
            let idxs: Vec<Vec<usize>> = lines
                .iter()
                .map(|l| l.glyphs.iter().map(|g| t.observe(g)).collect())
                .collect();
            let linegaps: Vec<Vec<i32>> = lines.iter().map(segment::gaps).collect();
            n_gl += idxs.iter().map(|v| v.len()).sum::<usize>();

            let Some(text) = refmap.get(&ev.start_ms) else {
                continue;
            };
            // Vote only where the shapes and the reference agree exactly on
            // structure; a guess here would poison the table.
            let rlines: Vec<&str> = text.lines().collect();
            if rlines.len() != idxs.len() {
                n_skipped += 1;
                continue;
            }
            let ok = rlines.iter().zip(&idxs).all(|(r, gs)| {
                r.chars().filter(|c| !c.is_whitespace()).count() == gs.len()
            });
            if !ok {
                n_skipped += 1;
                continue;
            }
            n_aligned += 1;
            for ((r, gs), lg) in rlines.iter().zip(&idxs).zip(&linegaps) {
                // Walk the reference including its spaces, so we learn not just
                // what each glyph is but whether a space follows it.
                let mut k = 0usize;
                let mut space_pending = false;
                for c in r.chars() {
                    if c.is_whitespace() {
                        space_pending = true;
                        continue;
                    }
                    if k >= gs.len() {
                        break;
                    }
                    t.vote(gs[k], &c.to_string());
                    n_votes += 1;
                    if k > 0
                        && let Some(&g) = lg.get(k - 1) {
                            let e = gapobs.entry(gs[k - 1]).or_default();
                            if space_pending { e.1.push(g) } else { e.0.push(g) }
                        }
                    space_pending = false;
                    k += 1;
                }
            }
        }
        println!("  {}", input.file_name().unwrap_or_default().to_string_lossy());
    }

    // Turn the gap observations into a per-glyph threshold: midway between the
    // typical within-word gap and the typical gap that carried a space.
    let mut learned = 0;
    for (gi, (mut no, mut yes)) in gapobs {
        if no.len() < 6 || yes.len() < 6 {
            continue;
        }
        no.sort_unstable();
        yes.sort_unstable();
        let lo = no[no.len() * 3 / 4];
        let hi = yes[yes.len() / 4];
        if hi > lo {
            t.glyphs[gi].gap = Some((lo + hi + 1) / 2);
            learned += 1;
        }
    }
    let (set, ambiguous, shaky) = t.apply_votes(min_agreement);
    t.reindex();
    t.save(table_path).map_err(|e| e.to_string())?;

    println!();
    println!("events segmented   : {n_ev}");
    println!("glyph instances    : {n_gl}");
    println!("distinct glyphs    : {}", t.glyphs.len());
    if reference.is_some() {
        println!("cues aligned       : {n_aligned} (skipped {n_skipped})");
        println!("votes cast         : {n_votes}");
        println!("labelled           : {set}");
        println!("ambiguity classes  : {ambiguous}  (font draws them identically)");
        println!("undecided          : {shaky}");
    }
    println!("per-glyph spacing  : {learned} glyphs");
    println!("unlabelled         : {}", t.unlabelled());
    println!("table              : {}", table_path.display());
    Ok(())
}

/// Identify a disc by the durations of the titles on it.
///
/// A DVD carries no usable identifier - the volume label may be just
/// "PARKS_AND_RECREATION", with no season or disc number - but the set of title
/// lengths is highly distinctive and survives ripping, so it can be matched
/// against a catalogue without the disc in hand.
pub fn fingerprint(inputs: &[PathBuf], min_seconds: u64) -> Result<(), String> {
    let mut files: Vec<PathBuf> = Vec::new();
    for p in inputs {
        if p.is_dir() {
            let rd = std::fs::read_dir(p).map_err(|e| format!("{}: {e}", p.display()))?;
            for e in rd.flatten() {
                let q = e.path();
                if q.extension().is_some_and(|x| x == "mkv" || x == "mp4") {
                    files.push(q);
                }
            }
        } else {
            files.push(p.clone());
        }
    }
    if files.is_empty() {
        return Err("no input files".into());
    }

    let mut secs: Vec<u64> = Vec::new();
    for f in &files {
        let out = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
            .arg(f)
            .output()
            .map_err(|e| format!("ffprobe: {e}"))?;
        let d: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0.0);
        let d = d.round() as u64;
        if d >= min_seconds {
            secs.push(d);
        }
    }
    secs.sort_unstable_by(|a, b| b.cmp(a));

    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for d in &secs {
        for b in d.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
    }
    let total: u64 = secs.iter().sum();

    println!("titles >= {min_seconds}s : {}", secs.len());
    println!("total runtime     : {}h{:02}m", total / 3600, (total % 3600) / 60);
    println!("fingerprint       : {h:016x}");
    println!("durations         :");
    for d in &secs {
        println!("  {:02}:{:02}:{:02}", d / 3600, (d % 3600) / 60, d % 60);
    }
    Ok(())
}

/// Flag labels that look wrong.
///
/// Reviewing glyphs by eye is the one manual step, and the mistake it invites
/// is case: `o` and `O` are the same shape at different sizes, and a contact
/// sheet that scales every glyph to one cell hides exactly that. Height tells
/// them apart, so check it.
pub fn check(t: &Table) -> Result<(), String> {
    const XH: &str = "acemnorsuvwxz";
    const CAP: &str = "ACEMNORSUVWXZ";

    let mut xs: Vec<i32> = Vec::new();
    let mut cs: Vec<i32> = Vec::new();
    for g in &t.glyphs {
        let Some(l) = g.text.as_deref() else { continue };
        if l.chars().count() != 1 {
            continue;
        }
        let c = l.chars().next().unwrap();
        if XH.contains(c) {
            xs.push(g.h);
        } else if CAP.contains(c) {
            cs.push(g.h);
        }
    }
    let median = |v: &mut Vec<i32>| {
        v.sort_unstable();
        v.get(v.len() / 2).copied().unwrap_or(0)
    };
    let (xh, cap) = (median(&mut xs), median(&mut cs));

    let mut issues = 0;
    if xh > 0 && cap > xh {
        println!("x-height {xh}px, cap-height {cap}px");
        for g in &t.glyphs {
            let Some(l) = g.text.as_deref() else { continue };
            if l.chars().count() != 1 {
                continue;
            }
            let c = l.chars().next().unwrap();
            // rare entries are usually a letter that merged with a mark, which
            // legitimately changes its height; only flag glyphs seen often
            if g.count < 10 {
                continue;
            }
            if CAP.contains(c) && g.h <= xh + 1 {
                println!("  {l:?} is {}px tall - that is x-height, so probably {:?}  (n={})",
                         g.h, c.to_lowercase().to_string(), g.count);
                issues += 1;
            }
            if XH.contains(c) && g.h >= cap - 1 {
                println!("  {l:?} is {}px tall - that is cap-height, so probably {:?}  (n={})",
                         g.h, c.to_uppercase().to_string(), g.count);
                issues += 1;
            }
        }
    }

    let unl = t.unlabelled();
    if unl > 0 {
        println!("{unl} glyphs still unlabelled");
        issues += unl;
    }
    let multi: Vec<&table::Entry> = t
        .glyphs
        .iter()
        .filter(|g| g.text.as_deref().is_some_and(|l| l.chars().count() > 1 && !l.contains('|')))
        .collect();
    if !multi.is_empty() {
        println!("{} glyphs carry multi-character labels (letters that merged):", multi.len());
        for g in multi {
            println!("  {:?}  (n={})", g.text.as_deref().unwrap_or(""), g.count);
        }
    }
    let amb = t.glyphs.iter().filter(|g| g.text.as_deref().is_some_and(|l| l.contains('|'))).count();
    if amb > 0 {
        println!("{amb} ambiguity classes - resolved from context at decode time");
    }
    if issues == 0 {
        println!("no problems found");
    }
    Ok(())
}

pub fn verify(produced: &Path, reference: &Path) -> Result<(), String> {
    let a = srt::parse(
        &std::fs::read_to_string(produced).map_err(|e| format!("{}: {e}", produced.display()))?,
    );
    let b = srt::parse(
        &std::fs::read_to_string(reference).map_err(|e| format!("{}: {e}", reference.display()))?,
    );

    let bm: BTreeMap<u64, &srt::Cue> = b.iter().map(|c| (c.start_ms, c)).collect();
    let (mut matched, mut text_ok, mut off_grid) = (0, 0, 0);
    let (mut chars, mut char_diff) = (0usize, 0usize);
    let mut samples = Vec::new();

    for c in &a {
        match bm.get(&c.start_ms) {
            Some(r) => {
                matched += 1;
                if r.text == c.text {
                    text_ok += 1;
                } else if samples.len() < 8 {
                    samples.push((c.start_ms, r.text.clone(), c.text.clone()));
                }
                let (x, y): (Vec<char>, Vec<char>) =
                    (r.text.chars().collect(), c.text.chars().collect());
                chars += x.len().max(y.len());
                char_diff += x.len().max(y.len()) - x.iter().zip(&y).filter(|(p, q)| p == q).count();
            }
            None => off_grid += 1,
        }
    }

    println!("produced cues      : {}", a.len());
    println!("reference cues     : {}", b.len());
    println!("timing matches     : {matched}  (off-grid: {off_grid})");
    if matched > 0 {
        println!(
            "exact text matches : {text_ok}  ({:.2}%)",
            100.0 * text_ok as f32 / matched as f32
        );
    }
    if chars > 0 {
        println!(
            "character accuracy : {:.3}%  ({char_diff} differing of {chars})",
            100.0 * (chars - char_diff) as f32 / chars as f32
        );
    }
    for (t, r, p) in &samples {
        println!("\n  @{}\n    ref: {}\n    got: {}", srt::fmt_ts(*t), r.replace('\n', " / "), p.replace('\n', " / "));
    }
    Ok(())
}

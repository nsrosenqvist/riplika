//! ripper - deterministic DVD subtitle recognition.
//!
//! DVD subtitles are a rendered bitmap font: every "e" on a disc is the same
//! pixels. So instead of running statistical OCR over every frame, we segment
//! the bitmaps into glyphs once, label the few hundred distinct shapes, and
//! then decode by exact lookup. Timings always come from the subtitle stream,
//! so output is sample-accurate with the source by construction.

#[cfg(test)]
mod tests;

mod recognize;
mod resolve;
mod segment;
mod sheet;
mod source;
mod srt;
mod table;
mod vobsub;

use clap::{Parser, Subcommand};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use segment::SegOpts;
use table::Table;

#[derive(Parser)]
#[command(name = "ripper", about = "Deterministic DVD subtitle recognition", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Segment inputs into glyphs and build (or extend) a glyph table.
    ///
    /// With --reference, labels are voted from already-trusted SRTs whose cue
    /// times line up with the subtitle stream; otherwise the table comes out
    /// unlabelled and is filled in via `sheet` + `label`.
    Build {
        /// Video files or .idx files.
        inputs: Vec<PathBuf>,
        #[arg(long, default_value = "glyphs.json")]
        table: PathBuf,
        /// Directory of reference .srt files named after each input.
        #[arg(long)]
        reference: Option<PathBuf>,
        /// Minimum share of votes needed to accept a label.
        #[arg(long, default_value_t = 0.90)]
        min_agreement: f32,
        #[arg(long)]
        name: Option<String>,
        /// Subtitle stream index, as ffmpeg counts subtitle streams.
        #[arg(long)]
        stream: Option<usize>,
    },
    /// Write an HTML page for reviewing and correcting labels.
    Sheet {
        #[arg(long, default_value = "glyphs.json")]
        table: PathBuf,
        #[arg(long, default_value = "glyphs.html")]
        out: PathBuf,
        #[arg(long, default_value_t = 4)]
        zoom: usize,
    },
    /// Apply a corrections file ({glyph key: text}) to the table.
    Label {
        #[arg(long, default_value = "glyphs.json")]
        table: PathBuf,
        /// JSON produced by the review sheet.
        corrections: PathBuf,
    },
    /// Recognize subtitles and write an SRT.
    Ocr {
        input: PathBuf,
        #[arg(long, default_value = "glyphs.json")]
        table: PathBuf,
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Character emitted where no glyph matched.
        #[arg(long, default_value_t = '\u{25a1}')]
        placeholder: char,
        /// Wordlist used to resolve ambiguous glyphs (I vs l).
        #[arg(long)]
        words: Option<PathBuf>,
        /// Subtitle language. Only affects ambiguity resolution; anything other
        /// than "en" disables the English-only rules.
        #[arg(long, default_value = "en")]
        lang: String,
        /// Subtitle stream index, as ffmpeg counts subtitle streams.
        #[arg(long)]
        stream: Option<usize>,
    },
    /// Print the segmentation of one subtitle, for diagnosing bad output.
    Inspect {
        input: PathBuf,
        /// Timestamp in milliseconds; the nearest cue is shown.
        #[arg(long)]
        at: u64,
        #[arg(long, default_value = "glyphs.json")]
        table: PathBuf,
        /// Subtitle stream index, as ffmpeg counts subtitle streams.
        #[arg(long)]
        stream: Option<usize>,
    },
    /// Compare a produced SRT against a reference one.
    Verify {
        produced: PathBuf,
        reference: PathBuf,
    },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    match Cli::parse().cmd {
        Cmd::Build {
            inputs,
            table,
            reference,
            min_agreement,
            name,
            stream,
        } => build(&inputs, &table, reference.as_deref(), min_agreement, name, stream),
        Cmd::Sheet { table, out, zoom } => {
            let t = Table::load(&table)?;
            let html = sheet::render(&t, zoom);
            std::fs::write(&out, html).map_err(|e| format!("{}: {e}", out.display()))?;
            println!(
                "{} glyphs ({} labelled, {} to review) -> {}",
                t.glyphs.len(),
                t.labelled(),
                t.unlabelled(),
                out.display()
            );
            Ok(())
        }
        Cmd::Label {
            table,
            corrections,
        } => {
            let mut t = Table::load(&table)?;
            let s = std::fs::read_to_string(&corrections)
                .map_err(|e| format!("{}: {e}", corrections.display()))?;
            let fixes: BTreeMap<String, String> =
                serde_json::from_str(&s).map_err(|e| format!("{}: {e}", corrections.display()))?;
            let mut n = 0;
            for g in t.glyphs.iter_mut() {
                if let Some(v) = fixes.get(&g.key) {
                    if g.text.as_deref() != Some(v.as_str()) {
                        n += 1;
                    }
                    g.text = Some(v.clone());
                }
            }
            t.save(&table)?;
            println!("applied {n} changes; {} still unlabelled", t.unlabelled());
            Ok(())
        }
        Cmd::Ocr {
            input,
            table,
            out,
            placeholder,
            words,
            lang,
            stream,
        } => {
            let t = Table::load(&table)?;
            let r = resolve::Resolver::load_lang(words.as_deref(), &lang);
            let (text, stats) = ocr_one(&input, &t, &r, placeholder, stream)?;
            let dest = out.unwrap_or_else(|| input.with_extension("srt"));
            std::fs::write(&dest, text).map_err(|e| format!("{}: {e}", dest.display()))?;
            println!(
                "{} cues, word-gap {}px, {} unknown glyph instances ({} distinct) -> {}",
                stats.cues,
                stats.space_gap,
                stats.unknown,
                stats.distinct_unknown.len(),
                dest.display()
            );
            Ok(())
        }
        Cmd::Inspect { input, at, table, stream } => {
            let t = Table::load(&table).unwrap_or_default();
            let src = source::load(&input, stream)?;
            let events = vobsub::decode_all(&src.idx, &src.sub);
            let opts = SegOpts::default();
            let Some(ev) = events
                .iter()
                .min_by_key(|e| e.start_ms.abs_diff(at))
            else {
                return Err("no subtitle events".into());
            };
            println!("cue at {} ms  ({}x{} bitmap)", ev.start_ms, ev.spu.w, ev.spu.h);
            for (li, line) in segment::segment(&ev.spu, &src.idx.palette, &opts)
                .iter()
                .enumerate()
            {
                println!(
                    "line {li}: top={} bottom={} height={}",
                    line.top,
                    line.bottom,
                    line.height()
                );
                let gaps = segment::gaps(line);
                for (i, g) in line.glyphs.iter().enumerate() {
                    let key = g.key();
                    let e = t.get(&key);
                    println!(
                        "   {:>3} x={:<4} y={:<4} {:>2}x{:<2} gap_after={:<4} {:?} thr={:?}",
                        i,
                        g.x,
                        g.y,
                        g.w,
                        g.h,
                        gaps.get(i).map(|v| v.to_string()).unwrap_or("-".into()),
                        e.and_then(|e| e.text.clone()).unwrap_or("?".into()),
                        e.and_then(|e| e.gap),
                    );
                }
            }
            Ok(())
        }
        Cmd::Verify {
            produced,
            reference,
        } => verify(&produced, &reference),
    }
}

#[derive(Default)]
struct OcrStats {
    cues: usize,
    unknown: usize,
    space_gap: i32,
    distinct_unknown: BTreeMap<String, u64>,
}

fn ocr_one(
    input: &Path,
    t: &Table,
    resolver: &resolve::Resolver,
    placeholder: char,
    stream: Option<usize>,
) -> Result<(String, OcrStats), String> {
    let src = source::load(input, stream)?;
    let events = vobsub::decode_all(&src.idx, &src.sub);
    let opts = SegOpts::default();

    // segment everything first so the word-gap can be measured from the whole file
    let segmented: Vec<Vec<segment::Line>> = events
        .iter()
        .map(|ev| segment::segment(&ev.spu, &src.idx.palette, &opts))
        .collect();
    let fallback = segmented
        .iter()
        .flatten()
        .next()
        .map(|l| segment::space_threshold(l, &opts))
        .unwrap_or(6);
    let space_gap = recognize::estimate_space_gap(&segmented, fallback);

    let mut cues = Vec::new();
    let mut ends = Vec::new();
    let mut stats = OcrStats::default();
    stats.space_gap = space_gap;

    for (ev, lines) in events.iter().zip(&segmented) {
        let r = recognize::lines_to_text(lines, t, resolver, space_gap, placeholder);
        if r.text.trim().is_empty() {
            continue;
        }
        for k in &r.unknown {
            *stats.distinct_unknown.entry(k.clone()).or_insert(0) += 1;
            stats.unknown += 1;
        }
        cues.push(srt::Cue {
            start_ms: ev.start_ms,
            end_ms: ev.end_ms.unwrap_or(ev.start_ms + 2000),
            text: r.text,
        });
        ends.push(ev.end_ms);
    }

    srt::tidy(&mut cues, &ends);
    stats.cues = cues.len();
    Ok((srt::write(&cues), stats))
}

fn build(
    inputs: &[PathBuf],
    table_path: &Path,
    reference: Option<&Path>,
    min_agreement: f32,
    name: Option<String>,
    stream: Option<usize>,
) -> Result<(), String> {
    let mut t = if table_path.exists() {
        Table::load(table_path)?
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
        let src = match source::load(input, stream) {
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
                    if k > 0 {
                        if let Some(&g) = lg.get(k - 1) {
                            let e = gapobs.entry(gs[k - 1]).or_default();
                            if space_pending { e.1.push(g) } else { e.0.push(g) }
                        }
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
    t.save(table_path)?;

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

fn verify(produced: &Path, reference: &Path) -> Result<(), String> {
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

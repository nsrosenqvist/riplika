//! riplika - turn a disc into a tagged, subtitled library.

mod glyphs;
mod run;

use clap::{Parser, Subcommand};
use riplika_core::model::{Container, Quality};
use riplika_core::subs::table::Table;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "riplika", about = "Rip, identify, transcode and subtitle discs", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args, Clone)]
struct Output {
    /// Where the finished files go.
    #[arg(short, long, default_value = ".")]
    out: PathBuf,
    /// Picture quality: high, medium or low.
    #[arg(long, default_value = "medium")]
    video: String,
    /// Sound: high keeps the original untouched, medium and low re-encode.
    #[arg(long, default_value = "high")]
    audio: String,
    /// Container: mp4 or mkv.
    #[arg(long, default_value = "mp4")]
    container: String,
    /// Languages to keep, in preference order, e.g. "english,swedish".
    /// The first one listed becomes the default track.
    #[arg(long, default_value = "")]
    languages: String,
    /// Add an AAC stereo track beside the original, for browser clients.
    #[arg(long)]
    dual_audio: bool,
    /// Keep VobSub bitmaps even after they have been recognised.
    #[arg(long)]
    keep_bitmap_subs: bool,
    /// Keep commentary audio tracks.
    #[arg(long)]
    keep_commentary: bool,
    /// Glyph table for subtitle recognition.
    #[arg(long)]
    table: Option<PathBuf>,
    /// Directory of <code>.txt wordlists.
    #[arg(long)]
    words: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List optical drives and what is in them.
    Drives,
    /// Show what is on a disc, without ripping it.
    Scan {
        /// Drive, e.g. disc:0. Defaults to the first with a disc in it.
        drive: Option<String>,
    },
    /// Work out what a disc is.
    Identify { drive: Option<String> },
    /// Look a title up in the catalogues by hand.
    Search {
        query: String,
        #[arg(long)]
        season: Option<u32>,
    },
    /// Rip a disc and produce finished files: the whole pipeline.
    Rip {
        drive: Option<String>,
        /// Where the raw rip goes. Kept afterwards so it can be re-run.
        #[arg(long, default_value = "/tmp/riplika-rip")]
        rip_dir: PathBuf,
        /// Skip identification and use this title.
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        season: Option<u32>,
        /// Which disc of the season this is, for episode numbering.
        #[arg(long)]
        disc: Option<u32>,
        /// Print what would be done and stop.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        output: Output,
    },
    /// Process an already-ripped directory, skipping the disc.
    Process {
        dir: PathBuf,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        season: Option<u32>,
        #[arg(long)]
        disc: Option<u32>,
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        output: Output,
    },

    /// Segment inputs into glyphs and build (or extend) a glyph table.
    ///
    /// With --reference, labels are voted from already-trusted SRTs whose cue
    /// times line up with the subtitle stream; otherwise the table comes out
    /// unlabelled and is filled in via `sheet` + `label`.
    Build {
        inputs: Vec<PathBuf>,
        #[arg(long, default_value = "glyphs.json")]
        table: PathBuf,
        #[arg(long)]
        reference: Option<PathBuf>,
        #[arg(long, default_value_t = 0.90)]
        min_agreement: f32,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value_t = 0)]
        stream: usize,
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
        corrections: PathBuf,
    },
    /// Recognise subtitles and write an SRT.
    Ocr {
        input: PathBuf,
        #[arg(long, default_value = "glyphs.json")]
        table: PathBuf,
        #[arg(short, long)]
        out: Option<PathBuf>,
        #[arg(long, default_value_t = '\u{25a1}')]
        placeholder: char,
        #[arg(long)]
        words: Option<PathBuf>,
        /// Subtitle language, which selects the ambiguity rules.
        #[arg(long, default_value = "en")]
        lang: String,
        #[arg(long, default_value_t = 0)]
        stream: usize,
    },
    /// Print the segmentation of one subtitle, for diagnosing bad output.
    Inspect {
        input: PathBuf,
        #[arg(long)]
        at: u64,
        #[arg(long, default_value = "glyphs.json")]
        table: PathBuf,
        #[arg(long, default_value_t = 0)]
        stream: usize,
    },
    /// Fingerprint a disc from its title durations.
    Fingerprint {
        inputs: Vec<PathBuf>,
        #[arg(long, default_value_t = 120)]
        min_seconds: u64,
    },
    /// Look for labels that are probably wrong.
    Check {
        #[arg(long, default_value = "glyphs.json")]
        table: PathBuf,
    },
    /// Compare a produced SRT against a reference one.
    Verify { produced: PathBuf, reference: PathBuf },
}

impl Output {
    fn settings(&self) -> Result<riplika_core::model::JobSettings, String> {
        Ok(riplika_core::model::JobSettings {
            output_dir: self.out.clone(),
            video: Quality::parse(&self.video)
                .ok_or("video quality must be high, medium or low")?,
            audio: Quality::parse(&self.audio)
                .ok_or("audio quality must be high, medium or low")?,
            container: match self.container.to_ascii_lowercase().as_str() {
                "mp4" => Container::Mp4,
                "mkv" | "matroska" => Container::Mkv,
                _ => return Err("container must be mp4 or mkv".into()),
            },
            languages: riplika_core::lang::LanguageSet::parse(&self.languages),
            dual_audio: self.dual_audio,
            keep_bitmap_subs: self.keep_bitmap_subs,
            drop_commentary: !self.keep_commentary,
            words_dir: self.words.clone(),
            glyph_table: self.table.clone(),
        })
    }
}

fn main() {
    // Restore the default SIGPIPE handling that Rust disables at startup.
    // Without this, `riplika check | head` kills the pipe and the next println!
    // panics with "failed printing to stdout: Broken pipe".
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    if let Err(e) = dispatch() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn dispatch() -> Result<(), String> {
    match Cli::parse().cmd {
        Cmd::Drives => run::drives(),
        Cmd::Scan { drive } => run::scan(drive.as_deref()),
        Cmd::Identify { drive } => run::identify(drive.as_deref()),
        Cmd::Search { query, season } => run::search(&query, season),
        Cmd::Rip { drive, rip_dir, title, season, disc, dry_run, output } => run::rip(
            drive.as_deref(),
            &rip_dir,
            title.as_deref(),
            season,
            disc,
            dry_run,
            output.settings()?,
        ),
        Cmd::Process { dir, title, season, disc, dry_run, output } => run::process(
            &dir,
            title.as_deref(),
            season,
            disc,
            dry_run,
            output.settings()?,
        ),

        Cmd::Build { inputs, table, reference, min_agreement, name, stream } => {
            glyphs::build(&inputs, &table, reference.as_deref(), min_agreement, name, stream)
        }
        Cmd::Sheet { table, out, zoom } => {
            let t = Table::load(&table).map_err(|e| e.to_string())?;
            let html = riplika_core::subs::sheet::render(&t, zoom);
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
        Cmd::Label { table, corrections } => {
            let mut t = Table::load(&table).map_err(|e| e.to_string())?;
            let s = std::fs::read_to_string(&corrections)
                .map_err(|e| format!("{}: {e}", corrections.display()))?;
            let fixes: BTreeMap<String, String> = serde_json::from_str(&s)
                .map_err(|e| format!("{}: {e}", corrections.display()))?;
            let mut n = 0;
            for g in t.glyphs.iter_mut() {
                if let Some(v) = fixes.get(&g.key) {
                    if g.text.as_deref() != Some(v.as_str()) {
                        n += 1;
                    }
                    g.text = Some(v.clone());
                }
            }
            t.save(&table).map_err(|e| e.to_string())?;
            println!("applied {n} changes; {} still unlabelled", t.unlabelled());
            Ok(())
        }
        Cmd::Ocr { input, table, out, placeholder, words, lang, stream } => {
            run::ocr(&input, &table, out.as_deref(), placeholder, words.as_deref(), &lang, stream)
        }
        Cmd::Inspect { input, at, table, stream } => run::inspect(&input, at, &table, stream),
        Cmd::Fingerprint { inputs, min_seconds } => glyphs::fingerprint(&inputs, min_seconds),
        Cmd::Check { table } => glyphs::check(&Table::load(&table).map_err(|e| e.to_string())?),
        Cmd::Verify { produced, reference } => glyphs::verify(&produced, &reference),
    }
}

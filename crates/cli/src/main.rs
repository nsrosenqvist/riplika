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

/// Which program reads the disc.
#[derive(clap::Args, Clone)]
struct Source {
    /// Disc reader: auto, dvd or makemkv.
    ///
    /// `dvd` uses ffmpeg's dvdvideo demuxer over libdvdread/libdvdnav/libdvdcss
    /// and needs nothing proprietary, but reads DVDs only. `makemkv` also reads
    /// Blu-ray, where there is no free equivalent. `auto` picks `dvd` for a
    /// DVD and `makemkv` otherwise.
    #[arg(long, default_value = "auto")]
    reader: String,
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
    /// Skip the longer cuts of episodes some discs carry.
    #[arg(long)]
    no_extended: bool,
    /// Skip bonus material: featurettes, deleted scenes, gag reels.
    ///
    /// A season disc can carry thirty of these against seven episodes, so this
    /// is most of the reading as well as most of the files.
    #[arg(long)]
    no_extras: bool,
    /// Read each title twice for chapter marks accurate to the frame.
    ///
    /// Roughly doubles how long the disc takes to read. Without it the marks
    /// run about a tenth of a per cent long - under two seconds by the end of
    /// an episode.
    #[arg(long)]
    accurate_chapters: bool,
    /// Glyph table for subtitle recognition.
    #[arg(long)]
    table: Option<PathBuf>,
    /// Directory of <code>.txt wordlists.
    #[arg(long)]
    words: Option<PathBuf>,
    /// How to name episodes, e.g. "{show} - S{season}E{episode} - {title}".
    ///
    /// Tokens: {show} {season} {episode} {title} {year} {date}. Numbers are two
    /// digits; {season:3} asks for more.
    #[arg(long)]
    template: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List optical drives and what is in them.
    Drives {
        #[command(flatten)]
        source: Source,
    },
    /// Say what kind of disc is in the drive, and what it is.
    Disc {
        /// Drive to look at, by device or MakeMKV id.
        #[arg(long)]
        drive: Option<String>,
    },
    /// Rip a music CD.
    RipCd {
        /// Drive to read, by device or MakeMKV id.
        #[arg(long)]
        drive: Option<String>,
        /// Where the album goes. Defaults to ~/Music.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Read only this track. For trying it out without a whole disc.
        #[arg(long)]
        track: Option<u8>,
        /// flac or mp3. Defaults to what the settings say.
        #[arg(long)]
        format: Option<String>,
        /// Take the names off the disc rather than asking a catalogue.
        #[arg(long)]
        from_disc: bool,
    },
    /// Dump a game disc to an image and identify it.
    RipGame {
        /// Drive to read, by device or MakeMKV id.
        #[arg(long)]
        drive: Option<String>,
        /// Where the image goes. Defaults to ~/Games.
        #[arg(long)]
        out: Option<PathBuf>,
        /// A datfile, or a directory of them. Defaults to the configured one.
        #[arg(long)]
        dat: Option<PathBuf>,
    },
    /// Identify a dumped game image against Redump datfiles.
    CheckDump {
        /// The image to check.
        image: PathBuf,
        /// A datfile, or a directory of them. Defaults to the configured one.
        #[arg(long)]
        dat: Option<PathBuf>,
    },
    /// Show what is on a disc, without ripping it.
    Scan {
        /// Drive, e.g. disc:0 or /dev/sr0. Defaults to the one with a disc in it.
        drive: Option<String>,
        #[command(flatten)]
        source: Source,
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
        ///
        /// Defaults to the cache directory, not /tmp: a season disc's raw rip
        /// is tens of gigabytes, and /tmp is memory on most systems now.
        #[arg(long)]
        rip_dir: Option<PathBuf>,
        #[command(flatten)]
        source: Source,
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
    /// Recover a damaged disc into an image, a sector at a time.
    ///
    /// Reads the easy data first and works on the damage afterwards, keeping a
    /// map so it can be stopped, the disc cleaned, and resumed.
    Rescue {
        /// Device, e.g. /dev/sr0.
        device: PathBuf,
        /// Where the image goes. A .map file is written beside it.
        image: PathBuf,
        /// Video title set to rescue. Omit for the whole disc.
        #[arg(long)]
        vts: Option<u8>,
        /// Program chains within that title set, e.g. 2-8. Omit for all.
        #[arg(long)]
        chains: Option<String>,
        /// Show what would be read and stop.
        #[arg(long)]
        dry_run: bool,
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
        /// Glyph table. Defaults to the installed one.
        #[arg(long)]
        table: Option<PathBuf>,
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
        /// Glyph table. Defaults to the installed one.
        #[arg(long)]
        table: Option<PathBuf>,
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
        let prefs = riplika_core::prefs::Preferences::load();
        Ok(riplika_core::model::JobSettings {
            output_dir: self.out.clone(),
            video: Quality::parse(&self.video)
                .ok_or("video quality must be high, medium or low")?,
            audio: Quality::parse(&self.audio)
                .ok_or("audio quality must be high, medium or low")?,
            accurate_chapters: self.accurate_chapters,
            music_format: prefs.music_format,
            music_quality: prefs.music_quality,
            music_template: Some(prefs.music_template.clone()).filter(|t| !t.trim().is_empty()),
            container: match self.container.to_ascii_lowercase().as_str() {
                "mp4" => Container::Mp4,
                "mkv" | "matroska" => Container::Mkv,
                _ => return Err("container must be mp4 or mkv".into()),
            },
            languages: riplika_core::lang::LanguageSet::parse(&self.languages),
            dual_audio: self.dual_audio,
            keep_bitmap_subs: self.keep_bitmap_subs,
            include_extended_cuts: !self.no_extended,
            include_extras: !self.no_extras,
            drop_commentary: !self.keep_commentary,
            // Asking Preferences, not just taking the flag. Without this the
            // whole pipeline ran with no glyph table unless told where one was,
            // so every subtitle stayed a bitmap - which is the thing
            // recognition exists to avoid, since a bitmap subtitle makes a
            // player burn it into the picture and re-encode. `ocr` was fixed to
            // look for the installed table; the pipeline was not.
            words_dir: self.words.clone().or_else(|| prefs.words_dir()),
            glyph_table: self.table.clone().or_else(|| prefs.glyph_table()),
            episode_template: self.template.clone(),
        })
    }
}

/// Where to find the glyph table: what was asked for, else the installed one,
/// else the working directory.
///
/// The last is for someone in the middle of building a table, who has one in
/// front of them and has not installed it yet.
fn resolve_table(given: Option<PathBuf>) -> PathBuf {
    given.unwrap_or_else(|| {
        let installed = riplika_core::prefs::Preferences::default_glyph_table();
        if installed.exists() { installed } else { PathBuf::from("glyphs.json") }
    })
}

/// Where to put the raw rip.
///
/// Not /tmp, which is what this used to be. On this machine and most others
/// running systemd, /tmp is a tmpfs - memory - and a season disc's raw rip is
/// tens of gigabytes. Ripping Parks and Recreation series six disc one filled
/// fourteen gigabytes of RAM and died a third of the way through, having taken
/// the machine's memory with it. The cache directory is on disk, which is where
/// something this size belongs, and it is what Preferences already meant by a
/// default rip directory.
fn resolve_rip_dir(given: Option<PathBuf>) -> PathBuf {
    given.unwrap_or_else(|| riplika_core::prefs::Preferences::load().rip_dir())
}

fn resolve_words(given: Option<PathBuf>) -> Option<PathBuf> {
    given.or_else(|| {
        let installed = riplika_core::prefs::Preferences::default_words_dir();
        installed.is_dir().then_some(installed)
    })
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
        Cmd::Drives { source } => run::drives(&source.reader),
        Cmd::Disc { drive } => run::disc(drive.as_deref()),
        Cmd::CheckDump { image, dat } => run::check_dump(&image, dat.as_deref()),
        Cmd::RipGame { drive, out, dat } => run::rip_game(drive.as_deref(), out, dat.as_deref()),
        Cmd::RipCd { drive, out, track, format, from_disc } => {
            run::rip_cd(drive.as_deref(), out, track, format.as_deref(), from_disc)
        }
        Cmd::Scan { drive, source } => run::scan(drive.as_deref(), &source.reader),
        Cmd::Identify { drive } => run::identify(drive.as_deref()),
        Cmd::Search { query, season } => run::search(&query, season),
        Cmd::Rip { drive, rip_dir, source, title, season, disc, dry_run, output } => run::rip(
            drive.as_deref(),
            &resolve_rip_dir(rip_dir),
            title.as_deref(),
            season,
            disc,
            dry_run,
            &source.reader,
            output.settings()?,
        ),
        Cmd::Rescue { device, image, vts, chains, dry_run } => {
            run::rescue(&device, &image, vts, chains.as_deref(), dry_run)
        }
        Cmd::Process { dir, title, season, disc, dry_run, output } => {
            run::process(&dir, title.as_deref(), season, disc, dry_run, output.settings()?)
        }

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
            t.save(&table).map_err(|e| e.to_string())?;
            println!("applied {n} changes; {} still unlabelled", t.unlabelled());
            Ok(())
        }
        Cmd::Ocr { input, table, out, placeholder, words, lang, stream } => run::ocr(
            &input,
            &resolve_table(table),
            out.as_deref(),
            placeholder,
            resolve_words(words).as_deref(),
            &lang,
            stream,
        ),
        Cmd::Inspect { input, at, table, stream } => {
            run::inspect(&input, at, &resolve_table(table), stream)
        }
        Cmd::Fingerprint { inputs, min_seconds } => glyphs::fingerprint(&inputs, min_seconds),
        Cmd::Check { table } => glyphs::check(&Table::load(&table).map_err(|e| e.to_string())?),
        Cmd::Verify { produced, reference } => glyphs::verify(&produced, &reference),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_raw_rip_does_not_default_into_memory() {
        // This defaulted to /tmp, which on this machine and most others
        // running systemd is a tmpfs. Ripping a season disc filled fourteen
        // gigabytes of RAM and died a third of the way through. Whatever the
        // default is, it must not be the system temporary folder.
        let chosen = resolve_rip_dir(None);
        assert!(
            !chosen.starts_with("/tmp"),
            "the raw rip would go to {}, which is memory",
            chosen.display()
        );
    }

    #[test]
    fn a_folder_given_by_hand_is_used_as_given() {
        let mine = PathBuf::from("/mnt/scratch/rip");
        assert_eq!(resolve_rip_dir(Some(mine.clone())), mine);
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;
    use clap::Parser;

    /// `Output` is flattened into each command, so it needs a parser around it
    /// before it can be built from arguments.
    #[derive(clap::Parser)]
    struct Wrap {
        #[command(flatten)]
        out: Output,
    }

    #[test]
    fn the_pipeline_looks_for_the_installed_glyph_table() {
        // Without this the whole pipeline ran with no table unless told where
        // one was, and every subtitle stayed a bitmap - which makes a player
        // burn it into the picture and re-encode, the exact thing recognition
        // exists to avoid. `ocr` had been fixed to look; the pipeline had not.
        let installed = riplika_core::prefs::Preferences::default_glyph_table();
        let out = Wrap::parse_from(["riplika"]).out;
        let chosen = out.settings().unwrap().glyph_table;
        if installed.exists() {
            assert_eq!(chosen, Some(installed), "the installed table was not found");
        } else {
            assert_eq!(chosen, None, "invented a table that is not there");
        }
    }

    #[test]
    fn a_table_given_by_hand_still_wins() {
        let mine = PathBuf::from("/mnt/tables/mine.json");
        let out = Wrap::parse_from(["riplika", "--table", "/mnt/tables/mine.json"]).out;
        assert_eq!(out.settings().unwrap().glyph_table, Some(mine));
    }
}

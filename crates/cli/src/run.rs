//! Wiring the pipeline to a terminal.
//!
//! All this module does is choose real implementations of the ports, turn
//! events into lines, and turn a few flags into settings. Deciding anything is
//! the library's job - which is what lets the GUI be a sibling of this file
//! rather than a rewrite of it.

use riplika_core::host::{Cancel, Fs, RealFs, RealRunner};
use riplika_core::identify::catalogue::{Catalogue, Catalogues, Tmdb, TvMaze, UreqHttp};
use riplika_core::job::{Event, Pipeline, Ports, Report, Stage};
use riplika_core::media::FfProbe;
use riplika_core::model::{Candidate, Drive, JobSettings, Item, Media, Role};
use riplika_core::rip::{dvd::DvdVideo, MakeMkv, Ripper};
use riplika_core::subs;
use std::path::{Path, PathBuf};

fn hms(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

fn mib(bytes: u64) -> String {
    format!("{} MB", bytes / 1_048_576)
}

/// Everything the pipeline talks to, built for real use.
struct Real {
    runner: RealRunner,
    fs: RealFs,
    http: UreqHttp,
    cancel: Cancel,
}

impl Real {
    fn new() -> Self {
        let cancel = Cancel::new();
        Real {
            runner: RealRunner::new(cancel.clone()),
            fs: RealFs,
            http: UreqHttp,
            cancel,
        }
    }

    /// TVmaze covers television and needs no key; TMDB also covers film but
    /// needs one, so it joins in only when `TMDB_API_KEY` is set.
    fn catalogues(&self) -> Catalogues<'_> {
        let mut v: Vec<Box<dyn Catalogue + '_>> = vec![Box::new(TvMaze { http: &self.http })];
        if let Some(t) = Tmdb::from_env(&self.http) {
            v.push(Box::new(t));
        }
        Catalogues(v)
    }
}

/// Choose the disc reader.
///
/// `dvd` needs nothing proprietary but reads DVDs only; `makemkv` also reads
/// Blu-ray, where libaacs ships no keys and there is no free equivalent. `auto`
/// prefers the free one and falls back.
enum Reader<'a> {
    Dvd(DvdVideo<'a>),
    MakeMkv(MakeMkv<'a>),
    /// The free reader, with MakeMKV held in reserve.
    ///
    /// libdvdcss does the player-key exchange with the drive, which an RPC-2
    /// drive can refuse when the disc's region does not match the one the drive
    /// is set to - and a drive can only be set to one region. MakeMKV talks to
    /// the drive itself and does not care. It also retries unreadable sectors,
    /// where libdvdread gives up, so scratched discs and the deliberately
    /// corrupt sectors some copy protections write are its territory too.
    ///
    /// Losing that silently would be the worst outcome, so a scan that shows
    /// any sign of trouble is handed straight over.
    Auto(DvdVideo<'a>, MakeMkv<'a>),
}

impl<'a> Reader<'a> {
    fn choose(which: &str, runner: &'a riplika_core::host::RealRunner) -> Result<Self, String> {
        match which.trim().to_ascii_lowercase().as_str() {
            "dvd" | "dvdvideo" | "ffmpeg" => Ok(Reader::Dvd(DvdVideo::new(runner))),
            "makemkv" => Ok(Reader::MakeMkv(MakeMkv::new(runner))),
            "auto" => {
                // A DVD has a VIDEO_TS directory; anything else needs MakeMKV.
                let dvd = DvdVideo::new(runner);
                let is_dvd = dvd
                    .drives()
                    .map(|ds| {
                        ds.iter().any(|d| {
                            riplika_core::rip::iso::device_reader(std::path::Path::new(&d.device))
                                .and_then(|mut r| riplika_core::rip::iso::title_table(&mut r))
                                .is_ok()
                        })
                    })
                    .unwrap_or(false);
                Ok(if is_dvd {
                    Reader::Auto(dvd, MakeMkv::new(runner))
                } else {
                    Reader::MakeMkv(MakeMkv::new(runner))
                })
            }
            other => Err(format!(
                "unknown reader {other:?}; use auto, dvd or makemkv"
            )),
        }
    }

    fn as_ripper(&self) -> &dyn Ripper {
        match self {
            Reader::Dvd(d) | Reader::Auto(d, _) => d,
            Reader::MakeMkv(m) => m,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Reader::Dvd(_) => "ffmpeg dvdvideo",
            Reader::MakeMkv(_) => "makemkv",
            Reader::Auto(..) => "ffmpeg dvdvideo (makemkv in reserve)",
        }
    }

    /// Scan, falling back to MakeMKV if the free path cannot be trusted.
    fn scan_disc(&self, drive: &Drive) -> Result<riplika_core::model::DiscScan, String> {
        let (dvd, fallback) = match self {
            Reader::MakeMkv(m) => return m.scan(drive).map_err(|e| e.to_string()),
            Reader::Dvd(d) => (d, None),
            Reader::Auto(d, m) => (d, Some(m)),
        };
        match dvd.scan_checked(drive) {
            Ok((scan, health)) if health.is_trustworthy() => Ok(scan),
            Ok((_, health)) => {
                eprintln!("  the free reader could not read this disc fully:");
                eprintln!("    {}", health.complaint());
                match fallback {
                    Some(m) => {
                        eprintln!("  handing it to makemkv, which works around this");
                        m.scan(drive).map_err(|e| e.to_string())
                    }
                    None => Err(format!(
                        "{} - retry without --reader dvd to use makemkv",
                        health.complaint()
                    )),
                }
            }
            Err(e) => match fallback {
                Some(m) => {
                    eprintln!("  the free reader failed ({e}); handing it to makemkv");
                    m.scan(drive).map_err(|e| e.to_string())
                }
                None => Err(e.to_string()),
            },
        }
    }
}

fn pick_drive(ripper: &dyn Ripper, want: Option<&str>) -> Result<Drive, String> {
    let drives = ripper.drives().map_err(|e| e.to_string())?;
    if drives.is_empty() {
        return Err("no optical drives found".into());
    }
    match want {
        Some(w) => drives
            .into_iter()
            .find(|d| d.id == w || d.device == w)
            .ok_or_else(|| format!("no drive matching {w:?}")),
        None => {
            let loaded: Vec<Drive> = drives.iter().filter(|d| d.has_disc()).cloned().collect();
            match loaded.len() {
                0 => Err("no disc in any drive".into()),
                1 => Ok(loaded.into_iter().next().unwrap()),
                // Picking one at random would be a coin flip on which disc gets
                // read for the next forty minutes.
                _ => Err(format!(
                    "several drives have discs; name one:\n{}",
                    loaded
                        .iter()
                        .map(|d| format!("  {}  {}", d.id, d.disc_label.as_deref().unwrap_or("")))
                        .collect::<Vec<_>>()
                        .join("\n")
                )),
            }
        }
    }
}

pub fn drives(reader: &str) -> Result<(), String> {
    let real = Real::new();
    let r = Reader::choose(reader, &real.runner)?;
    eprintln!("reader: {}", r.name());
    for d in r.as_ripper().drives().map_err(|e| e.to_string())? {
        println!(
            "{:8} {:12} {:32} {}",
            d.id,
            d.device,
            d.name,
            d.disc_label.as_deref().unwrap_or("(empty)")
        );
    }
    Ok(())
}

pub fn scan(drive: Option<&str>, reader: &str) -> Result<(), String> {
    let real = Real::new();
    let r = Reader::choose(reader, &real.runner)?;
    eprintln!("reader: {}", r.name());
    let d = pick_drive(r.as_ripper(), drive)?;
    let scan = r.scan_disc(&d)?;
    println!("{}  ({} titles)\n", scan.label, scan.titles.len());
    for t in &scan.titles {
        let audio = t.tracks.iter().filter(|x| x.kind == riplika_core::model::TrackKind::Audio).count();
        let subs = t.tracks.iter().filter(|x| x.kind == riplika_core::model::TrackKind::Subtitle).count();
        println!(
            "  {:>3}  {:>9}  {:>2} chapters  {:>2} audio  {:>2} subs  {}",
            t.id,
            hms(t.duration),
            t.chapter_count,
            audio,
            subs,
            t.output_name
        );
    }
    Ok(())
}

fn print_candidates(cands: &[Candidate]) {
    if cands.is_empty() {
        println!("no match - say what it is with --title and --season");
        return;
    }
    for (i, c) in cands.iter().take(5).enumerate() {
        println!(
            "{} {:>3.0}%  {}",
            if i == 0 { "->" } else { "  " },
            c.confidence * 100.0,
            c.media.describe()
        );
        for r in &c.reasons {
            println!("        {r}");
        }
    }
}

pub fn identify(drive: Option<&str>) -> Result<(), String> {
    let real = Real::new();
    let mk = MakeMkv::new(&real.runner);
    let d = pick_drive(&mk, drive)?;
    let scan = mk.scan(&d).map_err(|e| e.to_string())?;
    let cat = real.catalogues();
    let cands = riplika_core::identify::identify(&scan, &cat).map_err(|e| e.to_string())?;
    println!("{}\n", scan.label);
    print_candidates(&cands);
    Ok(())
}

pub fn search(query: &str, season: Option<u32>) -> Result<(), String> {
    let real = Real::new();
    let cat = real.catalogues();
    let cands = riplika_core::identify::search(&cat, query, season).map_err(|e| e.to_string())?;
    print_candidates(&cands);
    Ok(())
}

/// Render events as they arrive.
///
/// Progress is rewritten in place on one line, so a long rip does not scroll a
/// thousand lines of percentages past everything worth reading.
fn reporter() -> impl FnMut(Event) {
    use std::io::Write;
    let mut last_stage: Option<Stage> = None;
    let mut on_progress_line = false;
    move |e: Event| {
        let clear = |on: &mut bool| {
            if *on {
                print!("\r\x1b[K");
                *on = false;
            }
        };
        match e {
            Event::Stage(s) => {
                if last_stage != Some(s) {
                    clear(&mut on_progress_line);
                    println!("{}", s.label());
                    last_stage = Some(s);
                }
            }
            Event::Progress { fraction, message, .. } => {
                print!(
                    "\r\x1b[K  {:>3.0}%  {}",
                    fraction * 100.0,
                    message.unwrap_or_default()
                );
                let _ = std::io::stdout().flush();
                on_progress_line = true;
            }
            Event::ItemStarted { index, total, name } => {
                clear(&mut on_progress_line);
                println!("  [{}/{}] {name}", index + 1, total);
            }
            Event::ItemFinished { destination, bytes, .. } => {
                clear(&mut on_progress_line);
                println!(
                    "        wrote {} ({})",
                    destination.file_name().unwrap_or_default().to_string_lossy(),
                    mib(bytes)
                );
            }
            Event::Subtitle { language, cues, unknown, recognised, .. } => {
                clear(&mut on_progress_line);
                if recognised {
                    let note = if unknown > 0 {
                        format!(", {unknown} unrecognised glyphs")
                    } else {
                        String::new()
                    };
                    println!("        subs {language}: {cues} cues{note}");
                } else {
                    println!("        subs {language}: not recognised, bitmap kept");
                }
            }
            Event::Warning(w) => {
                clear(&mut on_progress_line);
                eprintln!("  warning: {w}");
            }
        }
    }
}

fn show_plan(items: &[Item]) {
    println!("\nplan:");
    for i in items {
        let source = i.source.file_name().unwrap_or_default().to_string_lossy();
        match (&i.role, &i.destination) {
            (Role::PlayAll, _) => println!("  {source:16} play-all, not written"),
            (_, Some(d)) => println!("  {source:16} {:>9}  -> {}", hms(i.duration), d.display()),
            (_, None) => println!("  {source:16} skipped"),
        }
    }
}

fn show_report(r: &Report) {
    println!(
        "\n{} files, {}",
        r.produced.len(),
        mib(r.total_bytes())
    );
    for p in &r.produced {
        let langs: Vec<&str> = p.subtitles.iter().map(|s| s.language.name.as_str()).collect();
        println!(
            "  {:60} {:>8}  subs: {}",
            p.destination.file_name().unwrap_or_default().to_string_lossy(),
            mib(p.bytes),
            if langs.is_empty() { "none".to_string() } else { langs.join(", ") }
        );
    }
    for (f, why) in &r.skipped {
        println!(
            "  FAILED {}: {why}",
            f.file_name().unwrap_or_default().to_string_lossy()
        );
    }
}

/// Resolve what the disc is: what was asked for, or the best guess.
fn decide_media(
    given_title: Option<&str>,
    season: Option<u32>,
    cat: &dyn Catalogue,
    scan: Option<&riplika_core::model::DiscScan>,
) -> Result<Media, String> {
    if let Some(t) = given_title {
        let cands = riplika_core::identify::search(cat, t, season).map_err(|e| e.to_string())?;
        return cands
            .into_iter()
            .next()
            .map(|c| c.media)
            .ok_or_else(|| format!("nothing found for {t:?}"));
    }
    let scan = scan.ok_or("nothing to identify from; pass --title")?;
    let cands = riplika_core::identify::identify(scan, cat).map_err(|e| e.to_string())?;
    println!("{}\n", scan.label);
    print_candidates(&cands);
    println!();
    cands
        .into_iter()
        .next()
        .map(|c| c.media)
        .ok_or_else(|| "could not identify the disc; pass --title".to_string())
}

#[allow(clippy::too_many_arguments)]
pub fn rip(
    drive: Option<&str>,
    rip_dir: &Path,
    title: Option<&str>,
    season: Option<u32>,
    disc: Option<u32>,
    dry_run: bool,
    settings: JobSettings,
) -> Result<(), String> {
    let real = Real::new();
    let mk = MakeMkv::new(&real.runner);
    let prober = FfProbe(&real.runner);
    let cat = real.catalogues();
    let d = pick_drive(&mk, drive)?;

    let pipeline = Pipeline::new(
        Ports {
            runner: &real.runner,
            prober: &prober,
            ripper: &mk,
            catalogue: &cat,
            fs: &real.fs,
            cancel: real.cancel.clone(),
        },
        settings,
    );

    let mut events = reporter();
    let scan = pipeline.scan(&d, &mut events).map_err(|e| e.to_string())?;
    let media = decide_media(title, season, &cat, Some(&scan))?;
    let disc = disc.or_else(|| riplika_core::identify::label::parse(&scan.label).disc);

    let files = pipeline.rip(&scan, rip_dir, &mut events).map_err(|e| e.to_string())?;
    let items = pipeline
        .organise(&files, &media, disc, &mut events)
        .map_err(|e| e.to_string())?;
    show_plan(&items);
    if dry_run {
        return Ok(());
    }
    let report = pipeline
        .produce(&items, &media, &mut events)
        .map_err(|e| e.to_string())?;
    show_report(&report);
    if !report.is_complete() {
        return Err(format!("{} titles failed", report.skipped.len()));
    }
    Ok(())
}

pub fn process(
    dir: &Path,
    title: Option<&str>,
    season: Option<u32>,
    disc: Option<u32>,
    dry_run: bool,
    settings: JobSettings,
) -> Result<(), String> {
    let real = Real::new();
    let prober = FfProbe(&real.runner);
    let cat = real.catalogues();
    let mk = MakeMkv::new(&real.runner);

    let mut files: Vec<PathBuf> = real
        .fs
        .list(dir)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|f| {
            f.extension()
                .map(|e| e == "mkv" || e == "mp4")
                .unwrap_or(false)
        })
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(format!("no .mkv or .mp4 files in {}", dir.display()));
    }

    let media = decide_media(title, season, &cat, None)?;
    let pipeline = Pipeline::new(
        Ports {
            runner: &real.runner,
            prober: &prober,
            ripper: &mk,
            catalogue: &cat,
            fs: &real.fs,
            cancel: real.cancel.clone(),
        },
        settings,
    );

    let mut events = reporter();
    let items = pipeline
        .organise(&files, &media, disc, &mut events)
        .map_err(|e| e.to_string())?;
    show_plan(&items);
    if dry_run {
        return Ok(());
    }
    let report = pipeline
        .produce(&items, &media, &mut events)
        .map_err(|e| e.to_string())?;
    show_report(&report);
    if !report.is_complete() {
        return Err(format!("{} titles failed", report.skipped.len()));
    }
    Ok(())
}

pub fn ocr(
    input: &Path,
    table: &Path,
    out: Option<&Path>,
    placeholder: char,
    words: Option<&Path>,
    lang: &str,
    stream: usize,
) -> Result<(), String> {
    let runner = RealRunner::default();
    let t = subs::table::Table::load(table).map_err(|e| e.to_string())?;
    let r = subs::resolve::Resolver::load_lang(words, lang);
    if !r.has_wordlist() {
        eprintln!(
            "note: no wordlist for '{lang}' - ambiguous glyphs will use structural \
             rules only. Pass --words for better results."
        );
    }
    let rec = subs::recognise(&runner, input, stream, &t, &r, placeholder)
        .map_err(|e| e.to_string())?;
    let dest = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| input.with_extension("srt"));
    std::fs::write(&dest, &rec.srt).map_err(|e| format!("{}: {e}", dest.display()))?;
    println!(
        "{} cues, word-gap {}px, {} unknown glyph instances ({} distinct) -> {}",
        rec.cues,
        rec.space_gap,
        rec.unknown,
        rec.distinct_unknown.len(),
        dest.display()
    );
    Ok(())
}

pub fn inspect(input: &Path, at: u64, table: &Path, stream: usize) -> Result<(), String> {
    use riplika_core::subs::{segment, source, vobsub};
    let runner = RealRunner::default();
    let t = subs::table::Table::load(table).unwrap_or_default();
    let src = source::load(&runner, input, stream).map_err(|e| e.to_string())?;
    let events = vobsub::decode_all(&src.idx, &src.sub);
    let opts = segment::SegOpts::default();
    let Some(ev) = events.iter().min_by_key(|e| e.start_ms.abs_diff(at)) else {
        return Err("no subtitle events".into());
    };
    println!("cue at {} ms  ({}x{} bitmap)", ev.start_ms, ev.spu.w, ev.spu.h);
    for (li, line) in segment::segment(&ev.spu, &src.idx.palette, &opts).iter().enumerate() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use riplika_core::model::Drive;
    use riplika_core::rip::FakeRipper;
    use riplika_core::model::DiscScan;

    fn drive(id: &str, label: Option<&str>) -> Drive {
        Drive {
            id: id.into(),
            device: format!("/dev/{id}"),
            name: "drive".into(),
            disc_label: label.map(str::to_string),
        }
    }

    struct Drives(Vec<Drive>);
    impl Ripper for Drives {
        fn drives(&self) -> riplika_core::Result<Vec<Drive>> {
            Ok(self.0.clone())
        }
        fn scan(&self, _: &Drive) -> riplika_core::Result<DiscScan> {
            Err("not used".into())
        }
        fn rip(
            &self,
            _: &Drive,
            _: &[riplika_core::model::DiscTitle],
            _: &Path,
            _: &mut dyn FnMut(f32, Option<&str>),
        ) -> riplika_core::Result<Vec<PathBuf>> {
            Err("not used".into())
        }
    }

    #[test]
    fn the_only_loaded_drive_is_chosen_without_being_named() {
        let d = Drives(vec![drive("disc:0", None), drive("disc:1", Some("MOVIE"))]);
        assert_eq!(pick_drive(&d, None).unwrap().id, "disc:1");
    }

    #[test]
    fn two_loaded_drives_must_be_disambiguated_rather_than_guessed() {
        // reading the wrong disc costs forty minutes
        let d = Drives(vec![drive("disc:0", Some("A")), drive("disc:1", Some("B"))]);
        let e = pick_drive(&d, None).unwrap_err();
        assert!(e.contains("several drives"), "{e}");
    }

    #[test]
    fn a_drive_can_be_named_by_id_or_by_device() {
        let d = Drives(vec![drive("disc:0", Some("A"))]);
        assert_eq!(pick_drive(&d, Some("disc:0")).unwrap().id, "disc:0");
        assert_eq!(pick_drive(&d, Some("/dev/disc:0")).unwrap().id, "disc:0");
        assert!(pick_drive(&d, Some("disc:9")).is_err());
    }

    #[test]
    fn an_empty_drive_is_reported_as_such() {
        let d = Drives(vec![drive("disc:0", None)]);
        assert!(pick_drive(&d, None).unwrap_err().contains("no disc"));
        assert!(pick_drive(&Drives(vec![]), None).unwrap_err().contains("no optical drives"));
    }

    #[test]
    fn a_named_but_empty_drive_is_still_selectable() {
        // scanning it will fail with a clearer message than "no disc in any drive"
        let d = Drives(vec![drive("disc:0", None)]);
        assert!(pick_drive(&d, Some("disc:0")).is_ok());
    }

    #[test]
    fn durations_render_as_hours_minutes_seconds() {
        assert_eq!(hms(1_275_000), "0:21:15");
        assert_eq!(hms(5_098_000), "1:24:58");
    }

    #[test]
    fn fake_ripper_satisfies_the_same_trait_the_cli_uses() {
        let scan = DiscScan {
            drive: drive("disc:0", Some("X")),
            label: "X".into(),
            titles: vec![],
        };
        let f = FakeRipper::new(scan);
        assert_eq!(f.drives().unwrap().len(), 1);
    }
}

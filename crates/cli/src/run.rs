//! Wiring the pipeline to a terminal.
//!
//! All this module does is choose real implementations of the ports, turn
//! events into lines, and turn a few flags into settings. Deciding anything is
//! the library's job - which is what lets the GUI be a sibling of this file
//! rather than a rewrite of it.

use riplika_core::host::{Cancel, Fs, RealFs, RealRunner};
use riplika_core::identify::catalogue::{Catalogue, Catalogues, Tmdb, TvMaze, UreqHttp, Wikidata};
use riplika_core::job::{Event, Pipeline, Ports, Report, Stage};
use riplika_core::joblog::JobLog;
use riplika_core::media::FfProbe;
use riplika_core::model::{Candidate, Drive, Item, JobSettings, Media, Role};
use riplika_core::prefs::Preferences;
use riplika_core::rip::{Auto, MakeMkv, Ripper, dvd::DvdVideo};
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
        Real { runner: RealRunner::new(cancel.clone()), fs: RealFs, http: UreqHttp, cancel }
    }

    /// TVmaze covers television and needs no key; TMDB also covers film but
    /// needs one, so it joins in only when `TMDB_API_KEY` is set.
    /// In preference order, and asked in that order until one answers.
    ///
    /// TMDB first when a key is configured: it is the better data, it covers
    /// film and television both, and it is what a media server will consult
    /// about the same files afterwards. Without a key, TVmaze answers for
    /// television and Wikidata for film - between them, no key is needed for
    /// anything.
    fn catalogues(&self) -> Catalogues<'_> {
        let mut v: Vec<Box<dyn Catalogue + '_>> = Vec::new();
        if let Some(t) = Tmdb::configured(&self.http) {
            v.push(Box::new(t));
        }
        v.push(Box::new(TvMaze { http: &self.http }));
        v.push(Box::new(Wikidata { http: &self.http }));
        Catalogues(v)
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

/// Build the disc reader named on the command line.
///
/// Shared with the window through `rip::Auto`, so the two cannot drift apart
/// about when MakeMKV gets involved.
fn reader<'a>(
    which: &str,
    runner: &'a riplika_core::host::RealRunner,
) -> Result<Box<dyn Ripper + 'a>, String> {
    match which.trim().to_ascii_lowercase().as_str() {
        "dvd" | "dvdvideo" | "ffmpeg" => Ok(Box::new(DvdVideo::new(runner))),
        "makemkv" => {
            if !Preferences::makemkv_available() {
                return Err("makemkvcon is not installed".into());
            }
            Ok(Box::new(MakeMkv::new(runner)))
        }
        "auto" => Ok(Box::new(
            Auto::new(runner, Preferences::makemkv_available()).on_fallback(|m| eprintln!("  {m}")),
        )),
        other => Err(format!("unknown reader {other:?}; use auto, dvd or makemkv")),
    }
}

pub fn drives(which: &str) -> Result<(), String> {
    let real = Real::new();
    let r = reader(which, &real.runner)?;
    for d in r.drives().map_err(|e| e.to_string())? {
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

pub fn scan(drive: Option<&str>, which: &str) -> Result<(), String> {
    let real = Real::new();
    let r = reader(which, &real.runner)?;
    let d = pick_drive(r.as_ref(), drive)?;
    // The scan probes each title in turn; on a full disc that is minutes, so
    // it says where it has got to rather than sitting silent.
    let mut last = String::new();
    let scan = r
        .scan(&d, &mut |fraction, message| {
            use std::io::Write;
            let line = format!("\r\x1b[K  {:>3.0}%  {}", fraction * 100.0, message.unwrap_or(""));
            if line != last {
                print!("{line}");
                let _ = std::io::stdout().flush();
                last = line;
            }
        })
        .map_err(|e| e.to_string())?;
    println!("\r\x1b[K");
    println!("{}  ({} titles)\n", scan.label, scan.titles.len());
    for t in &scan.titles {
        let audio =
            t.tracks.iter().filter(|x| x.kind == riplika_core::model::TrackKind::Audio).count();
        let subs =
            t.tracks.iter().filter(|x| x.kind == riplika_core::model::TrackKind::Subtitle).count();
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
    let scan = mk.scan(&d, &mut |_, _| {}).map_err(|e| e.to_string())?;
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
                print!("\r\x1b[K  {:>3.0}%  {}", fraction * 100.0, message.unwrap_or_default());
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
            Event::Plan(p) => {
                clear(&mut on_progress_line);
                for line in p.lines() {
                    println!("  {line}");
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
    println!("\n{} files, {}", r.produced.len(), mib(r.total_bytes()));
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
        println!("  FAILED {}: {why}", f.file_name().unwrap_or_default().to_string_lossy());
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
    which_reader: &str,
    settings: JobSettings,
) -> Result<(), String> {
    let real = Real::new();
    // The same reader selection `scan` uses. Hardcoding MakeMKV here meant the
    // free path was built, tested and then never reached by the one command
    // that matters.
    let mk = reader(which_reader, &real.runner)?;
    let prober = FfProbe(&real.runner);
    let cat = real.catalogues();
    let d = pick_drive(mk.as_ref(), drive)?;

    let pipeline = Pipeline::new(
        Ports {
            runner: &real.runner,
            prober: &prober,
            ripper: mk.as_ref(),
            catalogue: &cat,
            fs: &real.fs,
            cancel: real.cancel.clone(),
        },
        settings,
    );

    let mut events = reporter();
    let scan = pipeline.scan(&d, &mut events).map_err(|e| e.to_string())?;

    // One file per disc, so a season can be read back afterwards.
    let mut log = JobLog::for_disc(&scan, &riplika_core::joblog::now());
    println!("  log: {}", log.path().display());
    let mut events = |e: Event| {
        log.record(&e);
        events(e);
    };

    let media = decide_media(title, season, &cat, Some(&scan))?;
    let disc = disc.or_else(|| riplika_core::identify::label::parse(&scan.label).disc);

    if dry_run {
        // Ripping first and then stopping is not a dry run - it is the whole
        // disc read for nothing. When the scanner gave chapter durations the
        // mapping can be worked out from the scan alone.
        match pipeline.preview(&scan, &media, disc, rip_dir) {
            Some(items) => {
                show_plan(&items);
                println!(
                    "\n(dry run: nothing read. Extended cuts cannot be spotted \
                     without the files, so they are not shown.)"
                );
            }
            None => {
                println!(
                    "\n{} titles; this reader reports no chapter durations,",
                    scan.titles.len()
                );
                println!("so the episode mapping cannot be worked out without ripping first.");
                for t in &scan.titles {
                    println!("  {:>3}  {:>9}  {}", t.id, hms(t.duration), t.output_name);
                }
            }
        }
        return Ok(());
    }

    // Decide what is worth reading before reading it: a play-all is the same
    // video a second time.
    let plan = pipeline.preview(&scan, &media, disc, rip_dir);
    let titles = pipeline.titles_to_rip(&scan, plan.as_deref());
    if titles.len() < scan.titles.len() {
        println!(
            "  skipping {} play-all title(s), already covered by the episodes",
            scan.titles.len() - titles.len()
        );
    }
    let files = pipeline.rip(&scan, &titles, rip_dir, &mut events).map_err(|e| e.to_string())?;
    let items = pipeline.organise(&files, &media, disc, &mut events).map_err(|e| e.to_string())?;
    show_plan(&items);
    let report = pipeline.produce(&items, &media, &mut events).map_err(|e| e.to_string())?;
    log.finish(&summarise(&report));
    show_report(&report);
    if !report.is_complete() {
        return Err(format!("{} titles failed", report.skipped.len()));
    }
    Ok(())
}

fn summarise(r: &Report) -> String {
    let mut lines = vec![format!("{} files, {}", r.produced.len(), mib(r.total_bytes()))];
    for p in &r.produced {
        lines.push(format!(
            "  {}  {}",
            p.destination.file_name().unwrap_or_default().to_string_lossy(),
            mib(p.bytes)
        ));
    }
    for (f, why) in &r.skipped {
        lines.push(format!(
            "  FAILED {}: {why}",
            f.file_name().unwrap_or_default().to_string_lossy()
        ));
    }
    lines.join("\n")
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
    // Never used: this command works from files that are already on disk.
    let mk = MakeMkv::new(&real.runner);

    let mut files: Vec<PathBuf> = real
        .fs
        .list(dir)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|f| f.extension().map(|e| e == "mkv" || e == "mp4").unwrap_or(false))
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
    let mut log = JobLog::for_folder(dir, files.len(), &riplika_core::joblog::now());
    println!("  log: {}", log.path().display());
    let mut events = |e: Event| {
        log.record(&e);
        events(e);
    };

    let items = pipeline.organise(&files, &media, disc, &mut events).map_err(|e| e.to_string())?;
    show_plan(&items);
    if dry_run {
        log.finish("dry run: nothing was read");
        return Ok(());
    }
    let report = pipeline.produce(&items, &media, &mut events).map_err(|e| e.to_string())?;
    log.finish(&summarise(&report));
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
    let rec =
        subs::recognise(&runner, input, stream, &t, &r, placeholder).map_err(|e| e.to_string())?;
    let dest = out.map(Path::to_path_buf).unwrap_or_else(|| input.with_extension("srt"));
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
    use riplika_core::subs::{segment, source};
    let runner = RealRunner::default();
    let t = subs::table::Table::load(table).unwrap_or_default();
    let src = source::load(&runner, input, stream).map_err(|e| e.to_string())?;
    let events = src.events();
    let opts = segment::SegOpts::default();
    let Some(ev) = events.iter().min_by_key(|e| e.start_ms.abs_diff(at)) else {
        return Err("no subtitle events".into());
    };
    println!("cue at {} ms  ({}x{} bitmap)", ev.start_ms, ev.spu.w, ev.spu.h);
    for (li, line) in segment::segment(&ev.spu, &src.idx.palette, &opts).iter().enumerate() {
        println!("line {li}: top={} bottom={} height={}", line.top, line.bottom, line.height());
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
    use riplika_core::model::DiscScan;
    use riplika_core::model::Drive;
    use riplika_core::rip::FakeRipper;

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
        fn scan(
            &self,
            _: &Drive,
            _: &mut dyn FnMut(f32, Option<&str>),
        ) -> riplika_core::Result<DiscScan> {
            Err("not used".into())
        }
        fn rip(
            &self,
            _: &Drive,
            _: &[riplika_core::model::DiscTitle],
            _: &Path,
            _: &mut dyn FnMut(f32, Option<&str>),
        ) -> riplika_core::Result<riplika_core::rip::RipOutcome> {
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
        let scan =
            DiscScan { drive: drive("disc:0", Some("X")), label: "X".into(), titles: vec![] };
        let f = FakeRipper::new(scan);
        assert_eq!(f.drives().unwrap().len(), 1);
    }
}

/// Parse a chain selection like `2-8` or `1,3,5`.
fn parse_chains(spec: &str) -> Result<Vec<u32>, String> {
    let mut out = Vec::new();
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b) = (
                    a.trim().parse::<u32>().map_err(|_| format!("bad chain {part:?}"))?,
                    b.trim().parse::<u32>().map_err(|_| format!("bad chain {part:?}"))?,
                );
                if b < a {
                    return Err(format!("bad chain range {part:?}"));
                }
                out.extend(a..=b);
            }
            None => out.push(part.parse().map_err(|_| format!("bad chain {part:?}"))?),
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

pub fn rescue(
    device: &Path,
    image: &Path,
    vts: Option<u8>,
    chains: Option<&str>,
    dry_run: bool,
) -> Result<(), String> {
    use riplika_core::rescue;
    use riplika_core::rip::iso;

    if !rescue::dvdcss::available() {
        return Err("libdvdcss is not installed, so a damaged disc cannot be rescued".into());
    }

    // Work out which sectors are wanted. Rescuing a whole disc reads a lot that
    // is never watched; rescuing the chains you want skips the menus, the
    // duplicated play-alls and anything else you did not ask for.
    let (ranges, plain, what) = match vts {
        Some(vts) => {
            let mut read = iso::device_reader(device).map_err(|e| e.to_string())?;
            let set = iso::title_set(&mut read, vts).map_err(|e| e.to_string())?;
            let wanted = match chains {
                Some(spec) => parse_chains(spec)?,
                None => set.chains.iter().map(|c| c.number).collect(),
            };
            let mut ranges = Vec::new();
            for n in &wanted {
                match set.chains.iter().find(|c| c.number == *n) {
                    Some(c) => {
                        println!(
                            "  chain {:>2}: {:>3}m{:02}  {:.2} GB",
                            c.number,
                            c.seconds / 60,
                            c.seconds % 60,
                            c.sectors() as f64 * rescue::SECTOR as f64 / 1e9
                        );
                        ranges.extend(set.absolute(c));
                    }
                    None => return Err(format!("VTS {vts} has no chain {n}")),
                }
            }
            // Without the descriptors and IFOs the image is data with nothing
            // to navigate it; they are a few megabytes, so always included.
            let mut r = iso::device_reader(device).map_err(|e| e.to_string())?;
            let meta = iso::metadata_ranges(&mut r).map_err(|e| e.to_string())?;
            ranges.extend(meta.iter().copied());
            (iso::merge_ranges(&ranges), meta, format!("VTS {vts}"))
        }
        None => {
            let size = std::fs::metadata(device)
                .map(|m| m.len())
                .map_err(|e| format!("{}: {e}", device.display()))?;
            let sectors = size / rescue::SECTOR as u64;
            if sectors == 0 {
                return Err(format!("{}: cannot tell how large the disc is", device.display()));
            }
            let mut r = iso::device_reader(device).map_err(|e| e.to_string())?;
            let meta = iso::metadata_ranges(&mut r).unwrap_or_default();
            (vec![(0, sectors)], meta, "the whole disc".to_string())
        }
    };

    let total: u64 = ranges.iter().map(|(a, b)| b - a).sum();
    println!(
        "rescuing {what}: {} run(s), {:.2} GB to read -> {}",
        ranges.len(),
        total as f64 * rescue::SECTOR as f64 / 1e9,
        image.display()
    );
    if dry_run {
        return Ok(());
    }

    let map_path = image.with_extension("map");
    let mut last = String::new();
    let map = rescue::rescue_ranges(device, &ranges, &plain, image, &map_path, &mut |p| {
        use std::io::Write;
        let line = format!(
            "\r\x1b[K  {:<9} {:>5.1}%  {:.2} GB recovered, {} bad sectors",
            p.pass,
            p.fraction * 100.0,
            p.recovered_sectors as f64 * rescue::SECTOR as f64 / 1e9,
            p.bad_sectors
        );
        if line != last {
            print!("{line}");
            let _ = std::io::stdout().flush();
            last = line;
        }
    })
    .map_err(|e| e.to_string())?;

    println!("\n{}", map.summary(rescue::SECTOR as u64));
    println!("map: {}", map_path.display());
    if map.count(rescue::map::State::Bad) > 0 {
        println!(
            "unrecoverable sectors were filled with padding, so the image is \
             still demuxable; clean the disc and run this again to retry them"
        );
    }
    Ok(())
}

#[cfg(test)]
mod rescue_tests {
    use super::*;

    #[test]
    fn chain_selections_parse_in_the_forms_a_person_types() {
        assert_eq!(parse_chains("2-8").unwrap(), vec![2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(parse_chains("1,3,5").unwrap(), vec![1, 3, 5]);
        assert_eq!(parse_chains("2-4,9").unwrap(), vec![2, 3, 4, 9]);
        assert_eq!(parse_chains(" 7 ").unwrap(), vec![7]);
    }

    #[test]
    fn duplicates_and_overlaps_collapse() {
        assert_eq!(parse_chains("2-4,3-5").unwrap(), vec![2, 3, 4, 5]);
    }

    #[test]
    fn nonsense_is_rejected_rather_than_silently_read_as_nothing() {
        assert!(parse_chains("eight").is_err());
        assert!(parse_chains("8-2").is_err());
        assert!(parse_chains("2-").is_err());
    }
}

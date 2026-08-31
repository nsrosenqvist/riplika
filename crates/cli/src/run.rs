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
    accurate_chapters: bool,
) -> Result<Box<dyn Ripper + 'a>, String> {
    match which.trim().to_ascii_lowercase().as_str() {
        "dvd" | "dvdvideo" | "ffmpeg" => {
            let mut d = DvdVideo::new(runner);
            d.accurate_chapters = accurate_chapters;
            Ok(Box::new(d))
        }
        "makemkv" => {
            if !Preferences::makemkv_available() {
                return Err("makemkvcon is not installed".into());
            }
            Ok(Box::new(MakeMkv::new(runner)))
        }
        "auto" => Ok(Box::new(
            Auto::new(runner, Preferences::makemkv_available())
                .with_accurate_chapters(accurate_chapters)
                .on_fallback(|w| eprintln!("  {}", w.text())),
        )),
        other => Err(format!("unknown reader {other:?}; use auto, dvd or makemkv")),
    }
}

pub fn drives(which: &str) -> Result<(), String> {
    let real = Real::new();
    // No titles are read here, so chapter accuracy has nothing to apply to.
    let r = reader(which, &real.runner, false)?;
    for d in r.drives().map_err(|e| e.to_string())? {
        println!("{:8} {:12} {:32} {}", d.id, d.device, d.name, d.describe_disc());
    }
    Ok(())
}

/// Say what is in the drive.
///
/// For a music CD this goes as far as naming the album, because the disc's own
/// table of contents identifies it exactly - there is nothing to scan and
/// nothing to guess at, so there is no reason to make the user rip it first to
/// find out whether we know what it is.
pub fn disc(drive: Option<&str>) -> Result<(), String> {
    use riplika_core::disc::DiscKind;
    use riplika_core::identify::music::{MusicBrainz, MusicCatalogue};

    let real = Real::new();
    let d = pick_drive(&DvdVideo::new(&real.runner), drive)?;
    let kind = riplika_core::disc::identify(Path::new(&d.device));
    println!("{}  {}", d.device, kind.describe());

    if let DiscKind::Data(Some(toc)) = &kind
        && toc.is_audio()
    {
        let gaps = riplika_core::disc::pregaps(Path::new(&d.device), toc);
        let mode = riplika_core::disc::data_mode(Path::new(&d.device), toc);
        let spans = riplika_core::cue::layout(toc, &gaps);
        println!("mode      {mode:?}");
        println!(
            "{:>5}  {:>8} {:>8} {:>7}  {:>12}",
            "track", "start", "sectors", "pregap", "bytes"
        );
        for span in &spans {
            println!(
                "{:>5}  {:>8} {:>8} {:>7}  {:>12}",
                span.number,
                span.start,
                span.sectors(),
                span.pregap,
                span.bytes()
            );
        }
        return Ok(());
    }
    let DiscKind::Audio(toc) = &kind else { return Ok(()) };
    println!("disc id   {}", toc.musicbrainz_id());
    match riplika_core::cdtext::read(Path::new(&d.device)) {
        Some(text) => println!(
            "cd-text   {} - {}, {} track(s) named",
            text.performer.as_deref().unwrap_or("?"),
            text.album.as_deref().unwrap_or("?"),
            text.tracks.iter().filter(|t| t.title.is_some()).count()
        ),
        None => println!("cd-text   none"),
    }

    let mb = MusicBrainz::new(&real.http);
    let albums = match mb.by_disc_id(&toc.musicbrainz_id()) {
        Ok(a) => a,
        // Not knowing the album is not a reason to fail: the disc is still
        // rippable, it just gets named by hand.
        Err(e) => {
            println!("\n{} could not identify it: {e}", mb.name());
            return Ok(());
        }
    };
    if albums.is_empty() {
        println!("\nNo release in {} matches this disc.", mb.name());
        return Ok(());
    }
    for a in &albums {
        let detail = a.detail();
        println!(
            "\n{} - {}{}",
            a.artist,
            a.title,
            if detail.is_empty() { String::new() } else { format!("  ({detail})") }
        );
        if let Some(cat) = &a.catalogue_number {
            println!("  {}", cat);
        }
        for t in &a.tracks {
            let secs = t.duration.unwrap_or(0) / 1000;
            let who = t.artist.as_deref().map(|w| format!("{w} - ")).unwrap_or_default();
            println!("  {:>2}. {who}{}  [{}:{:02}]", t.number, t.title, secs / 60, secs % 60);
        }
    }
    Ok(())
}

/// Rip a music CD.
pub fn rip_cd(
    drive: Option<&str>,
    out: Option<PathBuf>,
    only: Option<u8>,
    format: Option<&str>,
    from_disc: bool,
) -> Result<(), String> {
    use riplika_core::job::Event;
    use riplika_core::musicjob;

    let real = Real::new();
    let prefs = riplika_core::prefs::Preferences::load();
    let d = pick_drive(&DvdVideo::new(&real.runner), drive)?;
    let device = PathBuf::from(&d.device);

    // Warnings during identification say why a disc came back unnamed, which
    // is the one thing worth knowing when it does.
    let mut complain = |e: Event| {
        if let Event::Warning(w) = e {
            println!("{}", w.text());
        }
    };
    let found = if from_disc {
        musicjob::identify_from_disc(&device, &mut complain)
    } else {
        musicjob::identify(&device, &real.http, &mut complain)
    }
    .map_err(|e| e.to_string())?;
    if found.from_cd_text {
        println!("(named from the disc itself; no date, label or cover art to be had)");
    }
    let album = match found.albums.first().cloned() {
        Some(a) => a,
        None if found.lookup_failed.is_some() => {
            return Err(
                "the catalogue could not be reached, so the disc was never asked about".into()
            );
        }
        None => return Err("no release matches this disc".into()),
    };
    println!("{} - {}\n", album.artist, album.title);

    // `--track` is for trying it out without sitting through a whole disc.
    let selection = only.map(|n| vec![n]);

    let mut settings = prefs.to_settings(
        out.unwrap_or_else(|| prefs.output_for(riplika_core::prefs::Library::Music)),
        prefs.languages(),
    );
    settings.music_format = match format {
        Some("flac") => riplika_core::prefs::AudioFormat::Flac,
        Some("mp3") => riplika_core::prefs::AudioFormat::Mp3,
        Some(other) => return Err(format!("format must be flac or mp3, not {other:?}")),
        None => prefs.music_format,
    };

    let ports = musicjob::Ports {
        runner: &real.runner,
        fs: &real.fs,
        http: &real.http,
        cancel: real.cancel.clone(),
    };
    let mut say = |e: Event| match e {
        Event::ItemStarted { index, total, name } => {
            print!("  {:>2}/{total} {name} ... ", index + 1);
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
        Event::ItemFinished { bytes, .. } => println!("{} MB", bytes / 1_000_000),
        Event::Warning(w) => println!("\n  {}", w.text()),
        _ => {}
    };
    let scratch = riplika_core::subs::source::temp_dir("cdrip").map_err(|e| e.to_string())?;
    let report = musicjob::rip(
        &ports,
        &musicjob::Disc { device: &device, toc: &found.toc, tracks: selection.as_deref() },
        &album,
        &settings,
        &scratch.0,
        &mut say,
    )
    .map_err(|e| e.to_string())?;
    println!("\n{}", settings.output_dir.display());
    if !report.is_complete() {
        return Err(format!("{} track(s) could not be written", report.skipped.len()));
    }
    Ok(())
}

/// List the datfiles there are, or fetch one.
pub fn dats(fetch: Option<&str>, out: Option<PathBuf>) -> Result<(), String> {
    use riplika_core::host::{Command, Runner};
    use riplika_core::identify::catalogue::Http;
    use riplika_core::redump;

    let real = Real::new();
    let prefs = riplika_core::prefs::Preferences::load();
    let dir = out
        .or_else(|| prefs.dat_dir())
        .unwrap_or_else(riplika_core::prefs::Preferences::default_dat_dir);

    let Some(system) = fetch else {
        println!("{}\n", dir.display());
        for (_, dat) in redump::load_all(&real.fs, &dir) {
            println!("  {:<48} {} disc(s)", dat.name, dat.len());
        }
        println!("\nTo add one:  riplika dats --fetch <system>");
        for (slug, name) in redump::SYSTEMS {
            println!("  {slug:<6} {name}");
        }
        return Ok(());
    };

    // An unlisted slug is passed through rather than refused: redump.org
    // covers more systems than a disc drive can read, and somebody who knows
    // the name should not be argued with.
    let known = redump::system_name(system).unwrap_or(system);
    let url = redump::datfile_url(system);
    println!("fetching {known} from {url}");
    let zipped = real.http.get_bytes(&url).map_err(|e| e.to_string())?;
    if zipped.len() < 1024 {
        return Err(format!("{url} gave back {} bytes; is {system:?} a system?", zipped.len()));
    }

    real.fs.create_dir_all(&dir).map_err(|e| e.to_string())?;
    let archive = dir.join(format!("{system}.zip"));
    real.fs.write(&archive, &zipped).map_err(|e| e.to_string())?;

    // Redump serves a zip. Rather than carry an unzipper for one file, use
    // whichever the machine has - and if it has neither, say where the archive
    // is instead of failing silently.
    let extracted = ["unzip", "bsdtar"].iter().find_map(|tool| {
        let cmd = match *tool {
            "unzip" => Command::new("unzip").args(["-o", "-q"]).path(&archive).arg("-d").path(&dir),
            _ => Command::new("bsdtar").args(["-x", "-f"]).path(&archive).arg("-C").path(&dir),
        };
        real.runner.run(&cmd).ok().filter(|o| o.ok()).map(|_| *tool)
    });
    match extracted {
        Some(tool) => {
            let _ = real.fs.remove_file(&archive);
            println!("extracted with {tool}");
        }
        None => {
            println!("\nNo unzip or bsdtar here. The archive is at:\n  {}", archive.display());
            println!("Extract it into that folder and riplika will find it.");
            return Ok(());
        }
    }

    // Everything in the folder rather than just the new one: matching a
    // datfile back to the slug that fetched it means guessing at how redump
    // punctuates a name, and getting that wrong prints nothing at all.
    println!();
    for (_, dat) in redump::load_all(&real.fs, &dir) {
        println!("  {:<48} {} disc(s)", dat.name, dat.len());
    }
    println!("{}", dir.display());
    Ok(())
}

/// Dump a game disc and work out what it was.
pub fn rip_game(
    drive: Option<&str>,
    out: Option<PathBuf>,
    dat: Option<&Path>,
    offset: Option<i32>,
) -> Result<(), String> {
    use riplika_core::disc::DiscKind;
    use riplika_core::job::{Event, Stage};
    use riplika_core::{game, gamejob};

    let real = Real::new();
    let prefs = riplika_core::prefs::Preferences::load();
    let d = pick_drive(&DvdVideo::new(&real.runner), drive)?;
    let device = PathBuf::from(&d.device);

    let kind = riplika_core::disc::identify(&device);
    if !matches!(kind, DiscKind::Data(_) | DiscKind::BluRay) {
        return Err(format!("not a data disc: {} is holding {}", d.device, kind.describe()));
    }

    // What the disc says before anything is read off it. For a PlayStation
    // that is a real identification; for a PC disc it is a volume label and a
    // hope.
    let mut source = riplika_core::rescue::PlainDisc::open(&device).map_err(|e| e.to_string())?;
    let inspected = game::inspect(&mut |lba, count| {
        use riplika_core::rescue::SectorSource;
        source.read(lba, count as u64).map_err(|_| riplika_core::Error("read failed".into()))
    })
    .unwrap_or_default();
    println!("{}", inspected.describe());
    if let Some(serial) = &inspected.serial {
        println!("  PlayStation serial {serial}");
    }

    let root = out.unwrap_or_else(|| prefs.output_for(riplika_core::prefs::Library::Games));

    let dats = load_dats(&real, dat.map(Path::to_path_buf).or_else(|| prefs.dat_dir()))?;
    println!("{} disc(s) known from datfiles\n", dats.iter().map(|(_, d)| d.len()).sum::<usize>());

    // Dumped beside the destination and moved into place once named, since the
    // name is not known until the bytes are.
    let staging = root.join("Unidentified").join(gamejob::suggested_stem(None, &inspected));
    let mut last = String::new();
    let mut say = |e: Event| match e {
        Event::Progress { stage, fraction, message } => {
            let what = match stage {
                Stage::Verify => "hashing".to_string(),
                _ => message.unwrap_or_else(|| "reading".into()),
            };
            let line = format!("  {what} {:.0}%", fraction * 100.0);
            if line != last {
                print!("\r{line}   ");
                let _ = std::io::Write::flush(&mut std::io::stdout());
                last = line;
            }
        }
        // On its own line: progress redraws over itself, and a warning that
        // gets drawn over is a warning nobody saw.
        Event::Warning(w) => {
            println!("\r  {}   ", w.text());
            last = String::new();
        }
        _ => {}
    };
    let read_offset = offset.unwrap_or(prefs.read_offset);
    let dumped = gamejob::dump(&device, &staging, &real.fs, read_offset, &real.cancel, &mut say)
        .map_err(|e| e.to_string())?;
    println!();

    for track in &dumped.tracks {
        let label = if dumped.tracks.len() > 1 {
            format!("track {:02}", track.number)
        } else {
            "image".into()
        };
        println!(
            "  {label}  {:>12}  {}  {}",
            track.digests.bytes,
            track.digests.crc32_hex(),
            track.digests.sha1_hex()
        );
    }
    if let Some(why) = gamejob::shortfall(&dumped) {
        println!("\n{why}");
    }
    if dumped.padded > 0 {
        println!(
            "\n{} sample(s) at the end are silence: correcting the read offset points past the\n\
             lead-out, and this drive will not read there. The last track cannot match a datfile.",
            dumped.padded
        );
    }

    // A disc of several tracks is only right when all of them are: one track
    // matching proves nothing about a boundary cut in the wrong place.
    let matched = gamejob::identify_all(&dats, &dumped);
    match &matched {
        Some((dat, game)) => println!("\n{}\n  {}", game.name, dat.name),
        // Not the same thing as an unknown disc, and until this was checked
        // both came out as "no datfile has this".
        None if dumped.tracks.len() > 1 => match gamejob::nearly(&dats, &dumped) {
            Some((dat, partial)) => {
                println!("\n{}\n  {}", gamejob::near_miss(&partial), dat.name);
            }
            None => println!("\nNo datfile has this disc."),
        },
        None => println!("\nNo datfile has this image."),
    }
    // Only a match can say what the disc really is, so it is read into a
    // holding folder first and moved once there is a name to move it under.
    let filed = gamejob::file_away(
        &real.fs,
        &dumped,
        &root,
        matched.as_ref().map(|(dat, _)| dat.name.as_str()),
        &matched
            .as_ref()
            .map(|(_, game)| game.name.clone())
            .unwrap_or_else(|| inspected.describe()),
    )
    .map_err(|e| e.to_string())?;
    for path in filed.cue.iter().chain(filed.tracks.iter().map(|t| &t.path)) {
        println!("  {}", path.display());
    }
    Ok(())
}

fn load_dats(
    real: &Real,
    where_from: Option<PathBuf>,
) -> Result<Vec<(PathBuf, riplika_core::redump::Dat)>, String> {
    use riplika_core::redump;
    let Some(where_from) = where_from else { return Ok(Vec::new()) };
    if where_from.is_dir() {
        return Ok(redump::load_all(&real.fs, &where_from));
    }
    let bytes = real.fs.read(&where_from).map_err(|e| e.to_string())?;
    let dat = redump::parse(&String::from_utf8_lossy(&bytes)).map_err(|e| e.to_string())?;
    Ok(vec![(where_from, dat)])
}

/// Say what a dumped image is, and whether it is whole.
///
/// One act, not two: a hit in a preservation datfile names the disc *and*
/// proves the dump is byte-for-byte right.
pub fn check_dump(image: &Path, dat: Option<&Path>) -> Result<(), String> {
    use riplika_core::hash;

    let real = Real::new();
    let prefs = riplika_core::prefs::Preferences::load();
    let where_from = dat
        .map(Path::to_path_buf)
        .or_else(|| prefs.dat_dir())
        .ok_or("no datfiles: pass --dat, or put them in the configured folder")?;

    let dats = load_dats(&real, Some(where_from.clone()))?;
    if dats.is_empty() {
        return Err(format!("no datfiles found in {}", where_from.display()));
    }
    let discs: usize = dats.iter().map(|(_, d)| d.len()).sum();
    println!("{} datfile(s), {discs} disc(s) known\n", dats.len());

    print!("hashing {} ... ", image.display());
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let digests = hash::of_file(&real.fs, image, &mut |_, _| {}).map_err(|e| e.to_string())?;
    println!("done");
    println!("  size   {}", digests.bytes);
    println!("  crc32  {}", digests.crc32_hex());
    println!("  sha1   {}", digests.sha1_hex());

    for (path, dat) in &dats {
        if let Some(found) = dat.find(&digests) {
            println!("\n{}", found.game.name);
            println!("  {} - {}", dat.name, found.rom.name);
            println!("  verified against {}", path.display());
            return Ok(());
        }
    }
    println!("\nNo datfile has this image.");
    match integrity(&real, image) {
        // The image's own error detection settles which of the two it is,
        // and they call for opposite responses: send one in, re-read the other.
        Some(checked) if checked.is_sound() => {
            println!(
                "Every one of its {} data sectors agrees with the error detection written\n\
                 into it, so the read is right and no datfile covers this disc.",
                checked.sound
            );
            if checked.unchecked > 0 {
                println!(
                    "  ({} sector(s) carry no error detection and were not checked)",
                    checked.unchecked
                );
            }
        }
        Some(checked) => {
            println!(
                "{} of its {} sectors disagree with the error detection written into them,\n\
                 so this is a bad read rather than an unknown disc.",
                checked.corrupt + checked.misplaced,
                checked.sectors
            );
        }
        None => {
            println!("That means the dump is not a known-good one - either the disc is not");
            println!("covered, or the read did not come out byte-for-byte right.");
        }
    }
    Ok(())
}

/// Check an image against the error detection inside its own sectors.
///
/// Answers nothing for anything that is not a track of raw data sectors - an
/// audio track has no such detection, and calling it corrupt would be a lie.
fn integrity(real: &Real, image: &Path) -> Option<riplika_core::edc::Checked> {
    use riplika_core::edc;
    let first = real.fs.read_range(image, 0, edc::SECTOR).ok()?;
    if !edc::looks_like_data(&first) {
        return None;
    }
    print!("checking every sector against its own error detection ... ");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    let out = edc::of_file(&real.fs, image, 0, &mut |_, _| {}).ok();
    println!("done");
    out
}

pub fn scan(drive: Option<&str>, which: &str) -> Result<(), String> {
    let real = Real::new();
    // A scan reads chapter times from the IFO tables, never from the video.
    let r = reader(which, &real.runner, false)?;
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
                eprintln!("  warning: {}", w.text());
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
        let found = riplika_core::identify::search(cat, t, season)
            .unwrap_or_default()
            .into_iter()
            .next()
            .map(|c| c.media);
        return Ok(match found {
            Some(m) => with_given_season(m, season),
            // The catalogues do not have everything, and a disc the user has
            // named is not a disc that cannot be ripped. Episodes come out as
            // "Episode 3" and can be renamed; refusing would leave the disc
            // unread, which is worse.
            None => {
                println!("  {t:?} is not in the catalogues; using it as given");
                riplika_core::identify::unverified(t, season)
            }
        });
    }
    let scan = scan.ok_or("nothing to identify from; pass --title")?;
    let cands = riplika_core::identify::identify(scan, cat).map_err(|e| e.to_string())?;
    println!("{}\n", scan.label);
    print_candidates(&cands);
    println!();
    let media = cands
        .into_iter()
        .next()
        .map(|c| c.media)
        .ok_or_else(|| "could not identify the disc; pass --title".to_string())?;
    let identified_season = media.season();

    let chosen = with_given_season(media, season);
    if chosen.season() != identified_season {
        println!("  using season {} rather than the identified one", season.unwrap_or(0));
    }
    Ok(chosen)
}

/// Apply a season the user gave by hand, over the one identification guessed.
///
/// A season disc's label rarely says which season it is - "PARKS_AND_RECREATION"
/// says nothing - so identification has to pick one, and for a season 6 disc it
/// picked season 1 and named eight episodes after season 1's. That is what
/// `--season` is for, and it was only being applied on the path where the title
/// was also given by hand: on the path that needs it, it did nothing at all.
fn with_given_season(media: Media, season: Option<u32>) -> Media {
    match season {
        Some(n) if media.season() != Some(n) => media.with_season(n),
        _ => media,
    }
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
    let mk = reader(which_reader, &real.runner, settings.accurate_chapters)?;
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
        // The same sentence the log and the window use, rather than a third
        // copy of it - this one still said "play-all title(s)".
        println!(
            "  {}",
            riplika_core::Warning::PlayAllsSkipped { titles: scan.titles.len() - titles.len() }
                .text()
        );
    }
    let files = pipeline.rip(&scan, &titles, rip_dir, &mut events).map_err(|e| e.to_string())?;
    let items =
        pipeline.organise(&files, None, &media, disc, &mut events).map_err(|e| e.to_string())?;
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

    let items =
        pipeline.organise(&files, None, &media, disc, &mut events).map_err(|e| e.to_string())?;
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

    fn series(season: u32) -> Media {
        Media::Series {
            title: "Parks and Recreation".into(),
            year: Some(2009),
            season,
            provider_id: None,
        }
    }

    #[test]
    fn a_season_given_by_hand_beats_the_one_identification_guessed() {
        // A season disc label rarely says which season it is, so this disc -
        // PARKS_AND_RECREATION, season 6 - identified as season 1 and named
        // eight episodes after season 1's. --season existed for exactly this
        // and was applied only when the title was also given by hand.
        assert_eq!(with_given_season(series(1), Some(6)).season(), Some(6));
    }

    #[test]
    fn saying_nothing_leaves_the_identified_season_alone() {
        assert_eq!(with_given_season(series(1), None).season(), Some(1));
    }

    #[test]
    fn saying_the_season_it_already_found_changes_nothing() {
        assert_eq!(with_given_season(series(6), Some(6)).season(), Some(6));
    }

    #[test]
    fn a_film_has_no_season_to_set() {
        let film = Media::Movie { title: "Heat".into(), year: Some(1995), provider_id: None };
        assert_eq!(with_given_season(film.clone(), Some(6)), film);
    }

    fn drive(id: &str, label: Option<&str>) -> Drive {
        Drive {
            id: id.into(),
            device: format!("/dev/{id}"),
            name: "drive".into(),
            disc_label: label.map(str::to_string),
            kind: None,
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

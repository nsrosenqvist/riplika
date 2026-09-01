//! Running the pipeline off the main thread.
//!
//! GTK's main loop must never block: a forty-minute rip on it would freeze the
//! window, including the cancel button. So each phase runs on its own thread
//! and reports back through a channel that the main loop drains on a timer.
//!
//! The phases are separate threads rather than one, because the user has to be
//! able to intervene between them - to correct a wrong identification, or to
//! change the quality after seeing how many episodes there are.

use riplika_core::Warning;
use riplika_core::gamejob;
use riplika_core::host::{Cancel, RealFs, RealRunner, Runner};
use riplika_core::identify::catalogue::{Catalogue, Catalogues, Tmdb, TvMaze, UreqHttp, Wikidata};
use riplika_core::identify::music::Album;
use riplika_core::job::{Event, Pipeline, Ports, Report};
use riplika_core::joblog::JobLog;
use riplika_core::media::FfProbe;
use riplika_core::model::{Candidate, DiscScan, Drive, Item, JobSettings, Media, Role};
use riplika_core::musicjob;
use riplika_core::rip::Auto;
use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};

/// What a worker sends back to the window.
pub enum Msg {
    Drives(Vec<Drive>),
    /// The drive opened, so what was known about the disc no longer holds.
    Ejected,
    Scanned(Box<DiscScan>),
    /// A music disc, and what MusicBrainz says it is.
    Music(Box<musicjob::Found>),
    /// A data disc, and the little it says about itself.
    Game(Box<riplika_core::game::GameDisc>),
    /// What the disc in the drive turned out to be, asked just now.
    Kind(Box<riplika_core::disc::DiscKind>),
    /// Datfiles were fetched because there were none, and how many.
    DatfilesReady(usize),
    /// A picture arrived, for whatever the page has settled on.
    Poster(std::path::PathBuf),
    Candidates(Vec<Candidate>),
    /// Releases a search by name turned up, to choose between.
    Releases(Vec<riplika_core::identify::music::Match>),
    /// One release fetched in full, once it has been chosen.
    Release(Box<riplika_core::identify::music::Album>),
    Organised(Vec<Item>),
    Event(Event),
    Finished(Box<Report>),
    Failed(String),
}

pub struct Channel {
    pub rx: Receiver<Msg>,
    tx: Sender<Msg>,
}

impl Default for Channel {
    fn default() -> Self {
        let (tx, rx) = channel();
        Channel { rx, tx }
    }
}

impl Channel {
    pub fn sender(&self) -> Sender<Msg> {
        self.tx.clone()
    }
}

/// The concrete implementations, built inside whichever thread needs them.
struct Real {
    runner: RealRunner,
    fs: RealFs,
    http: UreqHttp,
}

impl Real {
    fn new(cancel: Cancel) -> Self {
        Real { runner: RealRunner::new(cancel), fs: RealFs, http: UreqHttp }
    }

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

fn report(tx: &Sender<Msg>, r: riplika_core::Result<()>) {
    if let Err(e) = r {
        let _ = tx.send(Msg::Failed(e.to_string()));
    }
}

/// List the drives.
pub fn list_drives(allow_makemkv: bool, tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let real = Real::new(Cancel::new());
        let mk = Auto::new(&real.runner, allow_makemkv);
        match riplika_core::rip::Ripper::drives(&mk) {
            Ok(d) => {
                let _ = tx.send(Msg::Drives(d));
            }
            Err(e) => {
                let _ = tx.send(Msg::Failed(e.to_string()));
            }
        }
    });
}

/// Ask the drive what it is holding, right now.
///
/// The drive list carries a kind too, but that was read whenever the list was
/// last built - before this disc was put in, if the tray has been opened
/// since. Choosing a pipeline from it sent a music CD down the game path and
/// left it there, so the question is asked again at the moment it matters.
pub fn identify_disc(device: String, tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let kind = riplika_core::disc::identify(std::path::Path::new(&device));
        let _ = tx.send(Msg::Kind(Box::new(kind)));
    });
}

/// Read the disc and work out what it is.
pub fn analyse(drive: Drive, allow_makemkv: bool, cancel: Cancel, tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let real = Real::new(cancel.clone());
        let notify = tx.clone();
        // A scan reads chapter times from the IFO tables, never from the
        // video, so there is nothing here for exact marks to apply to.
        let mk = Auto::new(&real.runner, allow_makemkv).on_fallback(move |w| {
            let _ = notify.send(Msg::Event(Event::Warning(w)));
        });
        let prober = FfProbe(&real.runner);
        let cat = real.catalogues();
        let p = Pipeline::new(
            Ports {
                runner: &real.runner,
                prober: &prober,
                ripper: &mk,
                catalogue: &cat,
                fs: &real.fs,
                cancel,
            },
            JobSettings::default(),
        );
        let t = tx.clone();
        let mut events = move |e: Event| {
            let _ = t.send(Msg::Event(e));
        };
        report(
            &tx,
            (|| {
                let scan = p.scan(&drive, &mut events)?;
                let candidates = p.identify(&scan, &mut events);
                let _ = tx.send(Msg::Scanned(Box::new(scan)));
                let _ = tx.send(Msg::Candidates(candidates));
                Ok(())
            })(),
        );
    });
}

/// Open the drive.
///
/// On a thread, because a drive still reading will not answer until it stops -
/// and the window must not stop with it.
/// Wait until the drive admits it is empty, or give up waiting.
///
/// `eject` returns when the drive has accepted the request, not when the tray
/// has opened and the kernel has noticed. Listing the drives at that moment
/// reads the disc on its way out and puts it straight back on the page, which
/// is what left the last disc's details there after ejecting it.
///
/// Bounded, because a drive that will not open must not hang the window - and
/// reporting the eject anyway is right: the request was made and accepted, and
/// the listing that follows will say what is actually in there.
fn wait_for_the_tray(device: &str) -> bool {
    use riplika_core::disc::DiscKind;
    let path = std::path::Path::new(device);
    for _ in 0..25 {
        if riplika_core::disc::identify(path) == DiscKind::Empty {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    false
}

pub fn eject(device: String, tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let real = Real::new(Cancel::new());
        // Unmount first, and do not care whether it worked: nothing may have
        // been mounted, and if something was and this failed, the eject below
        // reports the real problem rather than this one.
        let path = std::path::Path::new(&device);
        let _ = real.runner.run(&riplika_core::rip::unmount_command(path));
        let cmd = riplika_core::rip::eject_command(path);
        match real.runner.run(&cmd) {
            Ok(out) if out.ok() => {
                if wait_for_the_tray(&device) {
                    let _ = tx.send(Msg::Ejected);
                } else {
                    // Accepted and nothing happened: something else is using
                    // the drive, or it will not open. Saying so beats clearing
                    // the page and then listing the same disc straight back
                    // onto it, which is what it looks like from the outside.
                    let _ = tx.send(Msg::Failed(
                        "the drive took the request but the disc is still in it".into(),
                    ));
                }
            }
            Ok(out) => {
                // Nearly always the same cause, and the message the drive
                // gives - "busy" - does not say what to do about it: the
                // desktop mounted the disc when it went in, and the kernel
                // will not open a tray under a mounted filesystem. From
                // inside a sandbox that mount cannot even be seen, let alone
                // undone, so the remedy has to be somebody else's.
                let _ = tx.send(Msg::Failed(format!(
                    "could not open the drive ({}). The disc is probably still \
                     mounted - eject it from Files, or unmount it first.",
                    out.last_error()
                )));
            }
            Err(e) => {
                let _ = tx.send(Msg::Failed(format!("could not open the drive: {e}")));
            }
        }
    });
}

/// Look a title up by hand, for when the guess was wrong.
pub fn search(query: String, season: Option<u32>, tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let real = Real::new(Cancel::new());
        let cat = real.catalogues();
        match riplika_core::identify::search(&cat, &query, season) {
            Ok(c) => {
                let _ = tx.send(Msg::Candidates(c));
            }
            Err(e) => {
                let _ = tx.send(Msg::Failed(e.to_string()));
            }
        }
    });
}

/// Ask MusicBrainz what releases go by a name.
///
/// Two requests make a search: this is the first, and [`fetch_release`] is the
/// second. Split because the search endpoint carries no track listings, and
/// fetching every result's would be twenty-five requests to a service that
/// allows one a second.
pub fn search_music(query: String, tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let real = Real::new(Cancel::new());
        let mb = riplika_core::identify::music::MusicBrainz::new(&real.http);
        use riplika_core::identify::music::MusicCatalogue;
        match mb.search(&query) {
            Ok(found) => {
                let _ = tx.send(Msg::Releases(found));
            }
            Err(e) => {
                let _ = tx.send(Msg::Failed(e.to_string()));
            }
        }
    });
}

/// Fetch one release in full, once the reader has said which.
pub fn fetch_release(id: String, tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let real = Real::new(Cancel::new());
        let mb = riplika_core::identify::music::MusicBrainz::new(&real.http);
        use riplika_core::identify::music::MusicCatalogue;
        match mb.by_release_id(&id) {
            Ok(Some(album)) => {
                let _ = tx.send(Msg::Release(Box::new(album)));
            }
            Ok(None) => {
                let _ = tx.send(Msg::Failed("MusicBrainz has no such release".into()));
            }
            Err(e) => {
                let _ = tx.send(Msg::Failed(e.to_string()));
            }
        }
    });
}

/// Read a music CD and ask what it is.
///
/// Quick where the video one is slow: there is no structure to probe, only a
/// table of contents to hash and one request to make.
pub fn analyse_music(device: String, tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let real = Real::new(Cancel::new());
        let t = tx.clone();
        let mut events = move |e: Event| {
            let _ = t.send(Msg::Event(e));
        };
        report(
            &tx,
            (|| {
                let found = musicjob::identify(Path::new(&device), &real.http, &mut events)?;
                let _ = tx.send(Msg::Music(Box::new(found)));
                Ok(())
            })(),
        );
    });
}

/// Rip a music CD.
pub fn run_music(
    device: String,
    found: musicjob::Found,
    album: Album,
    settings: JobSettings,
    cancel: Cancel,
    tx: Sender<Msg>,
) {
    std::thread::spawn(move || {
        let real = Real::new(cancel.clone());
        let ports =
            musicjob::Ports { runner: &real.runner, fs: &real.fs, http: &real.http, cancel };
        let t = tx.clone();
        let mut events = move |e: Event| {
            let _ = t.send(Msg::Event(e));
        };
        report(
            &tx,
            (|| {
                let scratch = riplika_core::subs::source::temp_dir("cdrip")?;
                let report = musicjob::rip(
                    &ports,
                    &musicjob::Disc::whole(Path::new(&device), &found.toc),
                    &album,
                    &settings,
                    &scratch.0,
                    &mut events,
                )?;
                let _ = tx.send(Msg::Finished(Box::new(report)));
                Ok(())
            })(),
        );
    });
}

/// Look at a data disc. Quick: a volume label and, on a PlayStation, a serial.
/// Make sure there are datfiles, fetching them if there are not.
///
/// Nobody should have to know that a dump is named by hashing it against a
/// database, let alone go and find the database first. The application knows
/// it needs them, knows where they come from and has the network to get them,
/// so it gets them.
///
/// Started when a game disc is recognised rather than when the dump finishes,
/// because the dump takes minutes and this takes seconds: by the time there is
/// something to identify, there is something to identify it against.
/// Fetch a picture of what was identified, if there is one to fetch.
///
/// Decoration: nothing waits for it and nothing goes wrong without it, so it
/// says nothing at all when it cannot help and the page keeps the kind icon.
pub fn poster(url: String, tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let dir = riplika_core::prefs::Preferences::cache_dir().join("art");
        if let Some(path) = riplika_core::art::cached(
            &RealFs,
            &riplika_core::identify::catalogue::UreqHttp,
            &dir,
            &url,
        ) {
            let _ = tx.send(Msg::Poster(path));
        }
    });
}

pub fn ensure_datfiles(tx: Sender<Msg>) {
    use riplika_core::job::Event;
    use riplika_core::model::Warning;

    std::thread::spawn(move || {
        let prefs = riplika_core::prefs::Preferences::load();
        let dir =
            prefs.dat_dir.clone().unwrap_or_else(riplika_core::prefs::Preferences::default_dat_dir);
        let fs = RealFs;
        // Already have some: leave them alone. They go out of date, but
        // re-downloading on every disc would be rude to redump.org and slow
        // for no gain - the Download button is there for refreshing them.
        if !riplika_core::redump::load_all(&fs, &dir).is_empty() {
            return;
        }
        let runner = RealRunner::new(Cancel::new());
        let http = riplika_core::identify::catalogue::UreqHttp;
        let mut got = 0;
        for (slug, name) in riplika_core::redump::SYSTEMS {
            match riplika_core::redump::fetch(&fs, &runner, &http, slug, &dir) {
                Ok(_) => got += 1,
                Err(e) => {
                    let _ = tx.send(Msg::Event(Event::Warning(Warning::CouldNotIdentify {
                        why: format!("could not fetch the {name} datfile: {e}"),
                    })));
                }
            }
        }
        if got > 0 {
            let _ = tx.send(Msg::DatfilesReady(got));
        }
    });
}

pub fn analyse_game(device: String, tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let disc = riplika_core::rescue::PlainDisc::open(Path::new(&device))
            .and_then(|mut source| {
                riplika_core::game::inspect(&mut |lba, count| {
                    use riplika_core::rescue::SectorSource;
                    source
                        .read(lba, count as u64)
                        .map_err(|_| riplika_core::Error("read failed".into()))
                })
            })
            // A disc that will not say anything is still dumpable; the name
            // comes from the hash afterwards, if it comes at all.
            .unwrap_or_default();
        let _ = tx.send(Msg::Game(Box::new(disc)));
    });
}

/// Dump a game disc and find out what it was.
pub fn run_game(
    device: String,
    disc: riplika_core::game::GameDisc,
    root: PathBuf,
    dat_dir: Option<PathBuf>,
    read_offset: i32,
    cancel: Cancel,
    tx: Sender<Msg>,
) {
    std::thread::spawn(move || {
        let real = Real::new(cancel.clone());
        let t = tx.clone();
        let mut events = move |e: Event| {
            let _ = t.send(Msg::Event(e));
        };
        report(
            &tx,
            (|| {
                let staging = root.join("Unidentified").join(gamejob::suggested_stem(None, &disc));
                let dumped = gamejob::dump(
                    Path::new(&device),
                    &staging,
                    &real.fs,
                    read_offset,
                    &cancel,
                    &mut events,
                )?;
                // Loaded here rather than before the dump: a dump takes
                // minutes, the datfiles are fetched in the background while it
                // runs, and reading them at the start would use the empty
                // folder that was there when the disc went in.
                let dats = dat_dir
                    .map(|d| riplika_core::redump::load_all(&real.fs, &d))
                    .unwrap_or_default();

                // Not FreeReaderIncomplete: that one says "using MakeMKV",
                // which has nothing to do with a game disc and told anybody
                // reading it something that was not happening.
                if let Some(why) = gamejob::shortfall(&dumped) {
                    events(Event::Warning(Warning::DumpIncomplete { why }));
                }

                // A disc of several tracks is only right when all of them
                // are: one track matching proves nothing about a boundary cut
                // in the wrong place.
                let matched = gamejob::identify_all(&dats, &dumped);
                // A near miss is not an unknown disc: it names the disc and
                // the tracks that let it down, which is a read to do again
                // rather than a pressing nobody has catalogued.
                if matched.is_none()
                    && let Some((_, partial)) = gamejob::nearly(&dats, &dumped)
                {
                    events(Event::Warning(Warning::DumpIncomplete {
                        why: gamejob::near_miss(&partial),
                    }));
                }
                let name = matched
                    .as_ref()
                    .map(|(_, game)| game.name.clone())
                    .unwrap_or_else(|| disc.describe());
                let filed = gamejob::file_away(
                    &real.fs,
                    &dumped,
                    &root,
                    matched.as_ref().map(|(dat, _)| dat.name.as_str()),
                    &name,
                )?;
                let dest = filed.path().map(Path::to_path_buf).unwrap_or(staging);
                events(Event::ItemFinished {
                    index: 0,
                    destination: dest.clone(),
                    bytes: filed.bytes(),
                });

                let mut report = Report::default();
                report.produced.push(riplika_core::job::Produced {
                    item: Item {
                        source: PathBuf::from(&device),
                        role: Role::Feature,
                        title: name,
                        air_date: None,
                        duration: 0,
                        destination: Some(dest.clone()),
                    },
                    // The path, not an empty one: the results screen titles
                    // each row by this file's name. For a disc of several
                    // tracks that is the cue sheet, which is the disc.
                    destination: dest,
                    bytes: filed.bytes(),
                    subtitles: Vec::new(),
                });
                let _ = tx.send(Msg::Finished(Box::new(report)));
                Ok(())
            })(),
        );
    });
}

/// Rip, sort out and produce - the long one.
#[allow(clippy::too_many_arguments)]
pub fn run(
    scan: DiscScan,
    media: Media,
    disc: Option<u32>,
    rip_dir: PathBuf,
    settings: JobSettings,
    allow_makemkv: bool,
    stamp: String,
    cancel: Cancel,
    tx: Sender<Msg>,
) {
    std::thread::spawn(move || {
        let real = Real::new(cancel.clone());
        let notify = tx.clone();
        let mk = Auto::new(&real.runner, allow_makemkv)
            .with_accurate_chapters(settings.accurate_chapters)
            .on_fallback(move |w| {
                let _ = notify.send(Msg::Event(Event::Warning(w)));
            });
        let prober = FfProbe(&real.runner);
        let cat = real.catalogues();
        let p = Pipeline::new(
            Ports {
                runner: &real.runner,
                prober: &prober,
                ripper: &mk,
                catalogue: &cat,
                fs: &real.fs,
                cancel,
            },
            settings,
        );
        // One file per disc, so a season can be read back afterwards.
        let mut log = JobLog::for_disc(&scan, &stamp);
        let t = tx.clone();
        let mut events = move |e: Event| {
            log.record(&e);
            let _ = t.send(Msg::Event(e));
        };
        report(
            &tx,
            (|| {
                // Work out which titles are actually wanted before reading
                // them: a play-all replays episodes that are on the disc
                // individually, so reading it reads the same video twice.
                let plan = p.preview(&scan, &media, disc, &rip_dir);
                let titles = p.titles_to_rip(&scan, plan.as_deref());
                if let Some(items) = &plan {
                    let _ = tx.send(Msg::Organised(items.clone()));
                }
                if titles.len() < scan.titles.len() {
                    events(Event::Warning(Warning::PlayAllsSkipped {
                        titles: scan.titles.len() - titles.len(),
                    }));
                }
                let files = p.rip(&scan, &titles, &rip_dir, &mut events)?;
                let items = p.organise(&files, Some(&scan), &media, disc, &mut events)?;
                let _ = tx.send(Msg::Organised(items.clone()));
                let report = p.produce(&items, &media, &mut events)?;
                // Eight gigabytes of intermediate files, once the episodes
                // exist. Nothing else in the window ever gets round to this.
                p.discard_rip(&rip_dir, &report, &mut events);
                let _ = tx.send(Msg::Finished(Box::new(report)));
                Ok(())
            })(),
        );
    });
}

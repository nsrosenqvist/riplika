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
use riplika_core::host::{Cancel, Fs, RealFs, RealRunner, Runner};
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
    Candidates(Vec<Candidate>),
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
pub fn eject(device: String, tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let real = Real::new(Cancel::new());
        let cmd = riplika_core::rip::eject_command(std::path::Path::new(&device));
        match real.runner.run(&cmd) {
            Ok(out) if out.ok() => {
                let _ = tx.send(Msg::Ejected);
            }
            Ok(out) => {
                let _ =
                    tx.send(Msg::Failed(format!("could not open the drive: {}", out.last_error())));
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
                let dats = dat_dir
                    .map(|d| riplika_core::redump::load_all(&real.fs, &d))
                    .unwrap_or_default();
                let staging = root.join("Unidentified").join(gamejob::suggested_name(None, &disc));
                let dumped =
                    gamejob::dump(Path::new(&device), &staging, &real.fs, &cancel, &mut events)?;
                if let Some(why) = gamejob::shortfall(&dumped) {
                    events(Event::Warning(Warning::FreeReaderIncomplete { why }));
                }

                let matched = gamejob::identify(&dats, &dumped.digests);
                let system = matched.as_ref().map(|(dat, _)| dat.name.clone());
                let dest = gamejob::destination(
                    &root,
                    matched.as_ref().map(|(_, f)| f),
                    system.as_deref(),
                    &disc,
                );
                if dest != staging {
                    if let Some(dir) = dest.parent() {
                        real.fs.create_dir_all(dir)?;
                    }
                    real.fs.rename(&staging, &dest)?;
                }
                let name = matched
                    .as_ref()
                    .map(|(_, f)| f.game.name.clone())
                    .unwrap_or_else(|| disc.describe());
                events(Event::ItemFinished {
                    index: 0,
                    destination: dest.clone(),
                    bytes: dumped.digests.bytes,
                });

                let mut report = Report::default();
                report.produced.push(riplika_core::job::Produced {
                    item: Item {
                        source: PathBuf::from(&device),
                        role: Role::Feature,
                        title: name,
                        air_date: None,
                        duration: 0,
                        destination: Some(dest),
                    },
                    destination: PathBuf::new(),
                    bytes: dumped.digests.bytes,
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
                let _ = tx.send(Msg::Finished(Box::new(report)));
                Ok(())
            })(),
        );
    });
}

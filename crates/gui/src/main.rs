//! riplika - a window over the disc pipeline.
//!
//! Four steps, in the order the work happens: pick a drive, confirm what the
//! disc is, choose how to encode it, watch it run. The middle step is the one
//! that justifies a window at all - identification is a guess, and a guess is
//! only safe if it is easy to overrule.

mod prefs_dialog;
mod worker;

use adw::prelude::*;
use gtk::glib;
use riplika_core::job::{Event, Report};
use riplika_core::lang::{self, LanguageSet};
use riplika_core::prefs::Preferences;
use riplika_core::model::*;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;
use worker::Msg;

const APP_ID: &str = "com.nsrosenqvist.Riplika";

/// Which step we are on. The navigation view mirrors this.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Drive,
    Identify,
    Settings,
    Progress,
    Results,
}

impl Step {
    fn tag(self) -> &'static str {
        match self {
            Step::Drive => "drive",
            Step::Identify => "identify",
            Step::Settings => "settings",
            Step::Progress => "progress",
            Step::Results => "results",
        }
    }
}

struct State {
    drives: Vec<Drive>,
    drive: Option<Drive>,
    scan: Option<DiscScan>,
    candidates: Vec<Candidate>,
    chosen: Option<Media>,
    items: Vec<Item>,
    cancel: riplika_core::host::Cancel,
}

impl Default for State {
    fn default() -> Self {
        State {
            drives: Vec::new(),
            drive: None,
            scan: None,
            candidates: Vec::new(),
            chosen: None,
            items: Vec::new(),
            cancel: riplika_core::host::Cancel::new(),
        }
    }
}

/// Every widget the message loop needs to reach.
struct Ui {
    nav: adw::NavigationView,
    toasts: adw::ToastOverlay,
    drive_list: gtk::ListBox,
    drive_next: gtk::Button,
    candidate_list: gtk::ListBox,
    identify_next: gtk::Button,
    search_entry: adw::EntryRow,
    season_entry: adw::EntryRow,
    video: adw::ComboRow,
    audio: adw::ComboRow,
    container: adw::ComboRow,
    language_group: adw::PreferencesGroup,
    language_rows: RefCell<Vec<(String, adw::SwitchRow)>>,
    output_dir: adw::ActionRow,
    disc_entry: adw::EntryRow,
    stage_label: gtk::Label,
    progress: gtk::ProgressBar,
    log: gtk::ListBox,
    cancel_button: gtk::Button,
    results: adw::PreferencesGroup,
    results_status: adw::StatusPage,
}

struct App {
    ui: Ui,
    state: RefCell<State>,
    prefs: Rc<prefs_dialog::Store>,
}


fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build);
    app.run()
}

fn quality_at(row: &adw::ComboRow) -> Quality {
    match row.selected() {
        0 => Quality::High,
        2 => Quality::Low,
        _ => Quality::Medium,
    }
}

fn mib(bytes: u64) -> String {
    format!("{} MB", bytes / 1_048_576)
}

fn hms(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

fn page(tag: &str, title: &str, child: &impl IsA<gtk::Widget>) -> adw::NavigationPage {
    let view = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    // Reachable from every step: the languages you prefer are most obviously
    // wrong at the moment the rip page shows them ticked the wrong way.
    let prefs_button = gtk::Button::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text("Preferences")
        .name("preferences")
        .build();
    header.pack_end(&prefs_button);
    view.add_top_bar(&header);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(child)
        .build();
    view.set_content(Some(&scroll));
    
    adw::NavigationPage::builder()
        .tag(tag)
        .title(title)
        .child(&view)
        .build()
}

/// A page body with the usual margins.
fn body() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build()
}

fn build(app: &adw::Application) {
    let ui = build_ui();
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Riplika")
        .default_width(720)
        .default_height(720)
        .content(&ui.toasts)
        .build();

    let app_state = Rc::new(App {
        ui,
        state: RefCell::new(State::default()),
        prefs: Rc::new(prefs_dialog::Store::new(Preferences::load())),
    });

    wire(&app_state, &window);
    window.present();
}

fn build_ui() -> Ui {
    let nav = adw::NavigationView::new();
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&nav));

    // --- step one: the drive ---------------------------------------------
    let drive_body = body();
    let drive_group = adw::PreferencesGroup::builder()
        .title("Drive")
        .description("Pick the drive holding the disc")
        .build();
    let drive_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(vec!["boxed-list".to_string()])
        .build();
    drive_group.add(&drive_list);
    let refresh = gtk::Button::with_label("Refresh");
    let drive_next = gtk::Button::builder()
        .label("Analyse disc")
        .sensitive(false)
        .css_classes(vec!["suggested-action".to_string()])
        .build();
    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .build();
    buttons.append(&refresh);
    buttons.append(&drive_next);
    drive_body.append(&drive_group);
    drive_body.append(&buttons);
    refresh.set_widget_name("refresh");
    nav.add(&page(Step::Drive.tag(), "Riplika", &drive_body));

    // --- step two: what is it? -------------------------------------------
    let id_body = body();
    let id_group = adw::PreferencesGroup::builder()
        .title("Identified as")
        .description("Choose another if this is wrong")
        .build();
    let candidate_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(vec!["boxed-list".to_string()])
        .build();
    id_group.add(&candidate_list);

    let search_group = adw::PreferencesGroup::builder().title("Search instead").build();
    let search_entry = adw::EntryRow::builder().title("Title").build();
    let season_entry = adw::EntryRow::builder().title("Season (blank for a film)").build();
    let search_button = gtk::Button::builder()
        .label("Search")
        .name("search")
        .halign(gtk::Align::End)
        .build();
    search_group.add(&search_entry);
    search_group.add(&season_entry);

    let disc_group = adw::PreferencesGroup::builder()
        .title("Disc number")
        .description("Sets where episode numbering starts; read from the label when it says")
        .build();
    let disc_entry = adw::EntryRow::builder().title("Disc").build();
    disc_group.add(&disc_entry);

    let identify_next = gtk::Button::builder()
        .label("Continue")
        .sensitive(false)
        .css_classes(vec!["suggested-action".to_string()])
        .halign(gtk::Align::End)
        .build();
    id_body.append(&id_group);
    id_body.append(&search_group);
    id_body.append(&search_button);
    id_body.append(&disc_group);
    id_body.append(&identify_next);
    nav.add(&page(Step::Identify.tag(), "What is this?", &id_body));

    // --- step three: how to encode it ------------------------------------
    let set_body = body();
    let quality = adw::PreferencesGroup::builder().title("Quality").build();
    let tiers = gtk::StringList::new(&["High", "Medium", "Low"]);
    let video = adw::ComboRow::builder()
        .title("Picture")
        .subtitle("Medium is the sweet spot for DVD: about 170 MB an episode")
        .model(&tiers)
        .selected(1)
        .build();
    let audio_tiers = gtk::StringList::new(&["High", "Medium", "Low"]);
    let audio = adw::ComboRow::builder()
        .title("Sound")
        .subtitle("High keeps the original AC3 untouched; browsers cannot decode it")
        .model(&audio_tiers)
        .selected(0)
        .build();
    let containers = gtk::StringList::new(&["MP4", "Matroska"]);
    let container = adw::ComboRow::builder()
        .title("Container")
        .model(&containers)
        .selected(0)
        .build();
    quality.add(&video);
    quality.add(&audio);
    quality.add(&container);

    // Built once the disc has been scanned, from the languages actually on it.
    // Offering a text field instead means guessing at spellings and finding out
    // afterwards that nothing matched.
    let language_group = adw::PreferencesGroup::builder()
        .title("Languages")
        .description("What this disc carries. Your preferred languages start ticked; the first becomes the default track.")
        .build();

    let folders = adw::PreferencesGroup::builder().title("Output").build();
    let output_dir = adw::ActionRow::builder().title("Folder").activatable(true).build();
    folders.add(&output_dir);

    let start = gtk::Button::builder()
        .label("Start")
        .name("start")
        .css_classes(vec!["suggested-action".to_string()])
        .halign(gtk::Align::End)
        .build();
    set_body.append(&quality);
    set_body.append(&language_group);
    set_body.append(&folders);
    set_body.append(&start);
    nav.add(&page(Step::Settings.tag(), "Settings", &set_body));

    // --- step four: watching it happen -----------------------------------
    let prog_body = body();
    let stage_label = gtk::Label::builder().label("Starting").xalign(0.0).build();
    stage_label.add_css_class("title-2");
    let progress = gtk::ProgressBar::builder().show_text(true).build();
    let log = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(vec!["boxed-list".to_string()])
        .build();
    let cancel_button = gtk::Button::builder()
        .label("Cancel")
        .css_classes(vec!["destructive-action".to_string()])
        .halign(gtk::Align::End)
        .build();
    prog_body.append(&stage_label);
    prog_body.append(&progress);
    prog_body.append(&log);
    prog_body.append(&cancel_button);
    nav.add(&page(Step::Progress.tag(), "Working", &prog_body));

    // --- and what came out ------------------------------------------------
    let res_body = body();
    let results_status = adw::StatusPage::builder().title("Done").build();
    let results = adw::PreferencesGroup::builder().title("Files").build();
    res_body.append(&results_status);
    res_body.append(&results);
    nav.add(&page(Step::Results.tag(), "Results", &res_body));

    Ui {
        nav,
        toasts,
        drive_list,
        drive_next,
        candidate_list,
        identify_next,
        search_entry,
        season_entry,
        video,
        audio,
        container,
        language_group,
        language_rows: RefCell::new(Vec::new()),
        output_dir,
        disc_entry,
        stage_label,
        progress,
        log,
        cancel_button,
        results,
        results_status,
    }
}

impl App {
    fn toast(&self, text: &str) {
        self.ui.toasts.add_toast(adw::Toast::new(text));
    }

    fn go(&self, step: Step) {
        self.ui.nav.push_by_tag(step.tag());
    }

    fn log_line(&self, text: &str) {
        let row = adw::ActionRow::builder().title(text).build();
        self.ui.log.append(&row);
        // Keep the list short: a full disc emits thousands of lines, and a
        // list box that long makes the window crawl.
        while self.ui.log.row_at_index(60).is_some() {
            if let Some(first) = self.ui.log.row_at_index(0) {
                self.ui.log.remove(&first);
            }
        }
    }

    /// Which languages are ticked, in the order they are shown.
    ///
    /// Order is the point: the rows are laid out with the preferred languages
    /// first, so reading them top to bottom gives the preference order, and the
    /// first one ends up the default track.
    fn chosen_languages(&self) -> LanguageSet {
        LanguageSet(
            self.ui
                .language_rows
                .borrow()
                .iter()
                .filter(|(_, row)| row.is_active())
                .map(|(code, _)| lang::parse(code))
                .collect(),
        )
    }

    fn settings(&self) -> JobSettings {
        let prefs = self.prefs.prefs.borrow();
        let output = prefs
            .output_dir
            .clone()
            .unwrap_or_else(|| glib::home_dir().join("Videos"));
        let mut s = prefs.to_settings(output, self.chosen_languages());
        // the rip page can override the persisted quality for this disc
        s.video = quality_at(&self.ui.video);
        s.audio = quality_at(&self.ui.audio);
        s.container = if self.ui.container.selected() == 1 {
            Container::Mkv
        } else {
            Container::Mp4
        };
        s
    }

    /// Rebuild the language switches for the disc that was just scanned.
    fn show_languages(&self, available: &[String]) {
        for (_, row) in self.ui.language_rows.borrow().iter() {
            self.ui.language_group.remove(row);
        }
        self.ui.language_rows.borrow_mut().clear();

        if available.is_empty() {
            let row = adw::SwitchRow::builder()
                .title("No language tracks found")
                .sensitive(false)
                .build();
            self.ui.language_group.add(&row);
            return;
        }
        for (code, wanted) in self.prefs.prefs.borrow().preselect(available) {
            let language = lang::parse(&code);
            let row = adw::SwitchRow::builder()
                .title(&language.name)
                // The code is worth showing: a disc may tag the same language
                // two ways, and this is what distinguishes the rows.
                .subtitle(&code)
                .build();
            row.set_active(wanted);
            self.ui.language_group.add(&row);
            self.ui.language_rows.borrow_mut().push((code, row));
        }
    }

    fn refresh_paths(&self) {
        let prefs = self.prefs.prefs.borrow();
        let output = prefs
            .output_dir
            .clone()
            .unwrap_or_else(|| glib::home_dir().join("Videos"));
        self.ui.output_dir.set_subtitle(&output.to_string_lossy());
        self.ui.video.set_selected(match prefs.video {
            Quality::High => 0,
            Quality::Medium => 1,
            Quality::Low => 2,
        });
        self.ui.audio.set_selected(match prefs.audio {
            Quality::High => 0,
            Quality::Medium => 1,
            Quality::Low => 2,
        });
        self.ui.container.set_selected(match prefs.container {
            Container::Mp4 => 0,
            Container::Mkv => 1,
        });
    }

    fn show_drives(&self, drives: &[Drive]) {
        while let Some(r) = self.ui.drive_list.row_at_index(0) {
            self.ui.drive_list.remove(&r);
        }
        for d in drives {
            let row = adw::ActionRow::builder()
                .title(&d.name)
                .subtitle(format!(
                    "{}   {}",
                    d.device,
                    d.disc_label.as_deref().unwrap_or("no disc")
                ))
                .build();
            row.set_sensitive(d.has_disc());
            self.ui.drive_list.append(&row);
        }
        self.state.borrow_mut().drives = drives.to_vec();
        // Select the only usable drive rather than making the user click it.
        let loaded: Vec<usize> = drives
            .iter()
            .enumerate()
            .filter(|(_, d)| d.has_disc())
            .map(|(i, _)| i)
            .collect();
        if let [only] = loaded[..]
            && let Some(row) = self.ui.drive_list.row_at_index(only as i32) {
                self.ui.drive_list.select_row(Some(&row));
            }
        self.ui.drive_next.set_sensitive(!loaded.is_empty());
    }

    fn show_candidates(&self, cands: &[Candidate]) {
        while let Some(r) = self.ui.candidate_list.row_at_index(0) {
            self.ui.candidate_list.remove(&r);
        }
        for c in cands {
            let row = adw::ActionRow::builder()
                .title(c.media.describe())
                .subtitle(c.reasons.join("\n"))
                .build();
            let pct = gtk::Label::new(Some(&format!("{:.0}%", c.confidence * 100.0)));
            pct.add_css_class("dim-label");
            row.add_suffix(&pct);
            self.ui.candidate_list.append(&row);
        }
        self.state.borrow_mut().candidates = cands.to_vec();
        if !cands.is_empty()
            && let Some(row) = self.ui.candidate_list.row_at_index(0) {
                self.ui.candidate_list.select_row(Some(&row));
            }
        self.ui.identify_next.set_sensitive(!cands.is_empty());
    }

    fn show_report(&self, r: &Report) {
        while let Some(c) = self.ui.results.first_child() {
            self.ui.results.remove(&c);
        }
        for p in &r.produced {
            let langs: Vec<&str> = p.subtitles.iter().map(|s| s.language.name.as_str()).collect();
            let row = adw::ActionRow::builder()
                .title(
                    p.destination
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string(),
                )
                .subtitle(format!(
                    "{}   subtitles: {}",
                    mib(p.bytes),
                    if langs.is_empty() { "none".into() } else { langs.join(", ") }
                ))
                .build();
            self.ui.results.add(&row);
        }
        for (f, why) in &r.skipped {
            let row = adw::ActionRow::builder()
                .title(f.file_name().unwrap_or_default().to_string_lossy().to_string())
                .subtitle(why)
                .css_classes(vec!["error".to_string()])
                .build();
            self.ui.results.add(&row);
        }
        self.ui.results_status.set_title(if r.is_complete() {
            "Done"
        } else {
            "Finished with problems"
        });
        self.ui.results_status.set_description(Some(&format!(
            "{} files, {}{}",
            r.produced.len(),
            mib(r.total_bytes()),
            if r.skipped.is_empty() {
                String::new()
            } else {
                format!(", {} failed", r.skipped.len())
            }
        )));
    }

    /// Show the plan while it runs, so the naming can be checked early rather
    /// than after an hour of encoding.
    fn show_plan(&self, items: &[Item]) {
        for i in items {
            match (&i.role, &i.destination) {
                (Role::PlayAll, _) => {
                    self.log_line(&format!(
                        "{}: play-all, not written",
                        i.source.file_name().unwrap_or_default().to_string_lossy()
                    ));
                }
                (_, Some(d)) => self.log_line(&format!(
                    "{} -> {}",
                    hms(i.duration),
                    d.file_name().unwrap_or_default().to_string_lossy()
                )),
                _ => {}
            }
        }
        self.state.borrow_mut().items = items.to_vec();
    }

    fn handle(&self, msg: Msg) {
        match msg {
            Msg::Drives(d) => self.show_drives(&d),
            Msg::Scanned(scan) => {
                let disc = riplika_core::identify::label::parse(&scan.label).disc;
                if let Some(d) = disc {
                    self.ui.disc_entry.set_text(&d.to_string());
                }
                self.ui.search_entry.set_text(
                    &riplika_core::identify::label::parse(&scan.label).title,
                );
                self.show_languages(&scan.all_languages());
                self.state.borrow_mut().scan = Some(*scan);
            }
            Msg::Candidates(c) => {
                self.show_candidates(&c);
                if self.ui.nav.visible_page().map(|p| p.tag().unwrap_or_default().to_string())
                    != Some(Step::Identify.tag().into())
                {
                    self.go(Step::Identify);
                }
            }
            Msg::Organised(items) => self.show_plan(&items),
            Msg::Event(e) => self.handle_event(e),
            Msg::Finished(r) => {
                self.show_report(&r);
                self.go(Step::Results);
            }
            Msg::Failed(e) => {
                self.toast(&e);
                self.log_line(&format!("failed: {e}"));
                self.ui.cancel_button.set_label("Close");
            }
        }
    }

    fn handle_event(&self, e: Event) {
        match e {
            Event::Stage(s) => {
                self.ui.stage_label.set_label(s.label());
                self.ui.progress.set_fraction(0.0);
            }
            Event::Progress { fraction, message, .. } => {
                self.ui.progress.set_fraction(fraction as f64);
                if let Some(m) = message {
                    self.ui.progress.set_text(Some(&m));
                }
            }
            Event::ItemStarted { index, total, name } => {
                self.ui.progress.set_fraction(index as f64 / total.max(1) as f64);
                self.log_line(&format!("[{}/{}] {name}", index + 1, total));
            }
            Event::ItemFinished { destination, bytes, .. } => {
                self.log_line(&format!(
                    "wrote {} ({})",
                    destination.file_name().unwrap_or_default().to_string_lossy(),
                    mib(bytes)
                ));
            }
            Event::Subtitle { language, cues, recognised, unknown, .. } => {
                self.log_line(&if recognised {
                    format!("subtitles {language}: {cues} cues, {unknown} unrecognised glyphs")
                } else {
                    format!("subtitles {language}: not recognised, bitmap kept")
                });
            }
            Event::Warning(w) => self.log_line(&format!("warning: {w}")),
        }
    }
}

/// Pick a folder, then hand the choice back.
fn choose_folder<F: Fn(PathBuf) + 'static>(window: &adw::ApplicationWindow, title: &str, then: F) {
    let dialog = gtk::FileDialog::builder().title(title).build();
    dialog.select_folder(Some(window), gtk::gio::Cancellable::NONE, move |res| {
        if let Ok(file) = res
            && let Some(p) = file.path() {
                then(p);
            }
    });
}

fn wire(app: &Rc<App>, window: &adw::ApplicationWindow) {
    let channel = worker::Channel::default();
    let tx = channel.sender();

    app.refresh_paths();
    worker::list_drives(app.prefs.prefs.borrow().use_makemkv(), tx.clone());

    // Drain the worker channel on the main loop. Polling rather than an async
    // channel because the pipeline is plain blocking code on plain threads,
    // and this keeps every widget touch on the thread GTK requires.
    {
        let app = Rc::clone(app);
        glib::timeout_add_local(Duration::from_millis(80), move || {
            while let Ok(msg) = channel.rx.try_recv() {
                app.handle(msg);
            }
            glib::ControlFlow::Continue
        });
    }

    // Step one -------------------------------------------------------------
    {
        let app = Rc::clone(app);
        let list = app.ui.drive_list.clone();
        list.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            let i = row.index() as usize;
            let d = app.state.borrow().drives.get(i).cloned();
            app.state.borrow_mut().drive = d;
        });
    }
    if let Some(refresh) = find_button(&app.ui.drive_next, "refresh") {
        let tx = tx.clone();
        refresh.connect_clicked(move |_| worker::list_drives(true, tx.clone()));
    }
    {
        let app = Rc::clone(app);
        let tx = tx.clone();
        app.clone().ui.drive_next.connect_clicked(move |_| {
            let drive = app.state.borrow().drive.clone();
            let Some(drive) = drive else {
                app.toast("Select a drive first");
                return;
            };
            app.ui.stage_label.set_label("Scanning disc");
            app.toast("Reading the disc - this takes a minute");
            let cancel = app.state.borrow().cancel.clone();
            let allow = app.prefs.prefs.borrow().use_makemkv();
            worker::analyse(drive, allow, cancel, tx.clone());
        });
    }

    // Step two -------------------------------------------------------------
    {
        let app = Rc::clone(app);
        app.clone().ui.identify_next.connect_clicked(move |_| {
            let i = app
                .ui
                .candidate_list
                .selected_row()
                .map(|r| r.index() as usize);
            let chosen = i.and_then(|i| app.state.borrow().candidates.get(i).cloned());
            match chosen {
                Some(c) => {
                    app.state.borrow_mut().chosen = Some(c.media);
                    app.go(Step::Settings);
                }
                None => app.toast("Choose what this disc is, or search for it"),
            }
        });
    }
    if let Some(search) = find_button(&app.ui.identify_next, "search") {
        let app = Rc::clone(app);
        let tx = tx.clone();
        search.connect_clicked(move |_| {
            let q = app.ui.search_entry.text().to_string();
            if q.trim().is_empty() {
                app.toast("Type something to search for");
                return;
            }
            let season = app.ui.season_entry.text().trim().parse::<u32>().ok();
            worker::search(q, season, tx.clone());
        });
    }

    // Step three -----------------------------------------------------------
    {
        let app = Rc::clone(app);
        let window = window.clone();
        app.clone().ui.output_dir.connect_activated(move |_| {
            let app2 = Rc::clone(&app);
            choose_folder(&window, "Output folder", move |p| {
                app2.prefs.prefs.borrow_mut().output_dir = Some(p);
                app2.prefs.save();
                app2.refresh_paths();
            });
        });
    }
    if let Some(start) = find_button(&app.ui.output_dir, "start") {
        let app = Rc::clone(app);
        let tx = tx.clone();
        start.connect_clicked(move |_| {
            let (scan, media) = {
                let s = app.state.borrow();
                (s.scan.clone(), s.chosen.clone())
            };
            let (Some(scan), Some(media)) = (scan, media) else {
                app.toast("Nothing to rip yet");
                return;
            };
            let disc = app.ui.disc_entry.text().trim().parse::<u32>().ok();
            let settings = app.settings();
            let rip_dir = app
                .prefs
                .prefs
                .borrow()
                .rip_dir
                .clone()
                .unwrap_or_else(|| std::env::temp_dir().join("riplika-rip"));
            if settings.glyph_table.is_none() {
                app.toast("No glyph table: subtitles will stay as bitmaps");
            }
            // A fresh token, so a cancelled run does not poison the next one.
            app.state.borrow_mut().cancel = riplika_core::host::Cancel::new();
            let cancel = app.state.borrow().cancel.clone();
            app.ui.cancel_button.set_label("Cancel");
            app.go(Step::Progress);
            let allow = app.prefs.prefs.borrow().use_makemkv();
            worker::run(scan, media, disc, rip_dir, settings, allow, cancel, tx.clone());
        });
    }

    // Preferences ----------------------------------------------------------
    for button in find_buttons(&app.ui.output_dir, "preferences") {
        let app = Rc::clone(app);
        let window = window.clone();
        button.connect_clicked(move |_| {
            let app2 = Rc::clone(&app);
            prefs_dialog::present(&window, Rc::clone(&app.prefs), move || {
                // Re-tick the rip page from the new preferences, but only while
                // a disc is loaded and the choice has not been acted on yet.
                if let Some(scan) = app2.state.borrow().scan.as_ref() {
                    app2.show_languages(&scan.all_languages());
                }
                app2.refresh_paths();
            });
        });
    }

    // Step four ------------------------------------------------------------
    {
        let app = Rc::clone(app);
        app.clone().ui.cancel_button.connect_clicked(move |b| {
            if b.label().map(|l| l == "Close").unwrap_or(false) {
                app.ui.nav.pop();
                return;
            }
            app.state.borrow().cancel.cancel();
            app.toast("Stopping after the current step");
            b.set_label("Close");
        });
    }
}

/// Every button with this name in the window.
///
/// The preferences button is repeated once per page, so a single lookup would
/// wire up whichever happened to be found first and leave the rest dead.
fn find_buttons(anchor: &impl IsA<gtk::Widget>, name: &str) -> Vec<gtk::Button> {
    let mut root: gtk::Widget = anchor.clone().upcast();
    while let Some(p) = root.parent() {
        root = p;
    }
    fn walk(w: &gtk::Widget, name: &str, out: &mut Vec<gtk::Button>) {
        if w.widget_name() == name
            && let Ok(b) = w.clone().downcast::<gtk::Button>() {
                out.push(b);
            }
        let mut child = w.first_child();
        while let Some(c) = child {
            walk(&c, name, out);
            child = c.next_sibling();
        }
    }
    let mut out = Vec::new();
    walk(&root, name, &mut out);
    out
}

/// Find a button by name anywhere in the window.
///
/// The pages are built in one pass and wired in another, so this walks the tree
/// rather than threading every button through the `Ui` struct - there are only
/// a handful and they all have names.
fn find_button(anchor: &impl IsA<gtk::Widget>, name: &str) -> Option<gtk::Button> {
    let mut root: gtk::Widget = anchor.clone().upcast();
    while let Some(p) = root.parent() {
        root = p;
    }
    fn walk(w: &gtk::Widget, name: &str) -> Option<gtk::Button> {
        if w.widget_name() == name
            && let Ok(b) = w.clone().downcast::<gtk::Button>() {
                return Some(b);
            }
        let mut child = w.first_child();
        while let Some(c) = child {
            if let Some(found) = walk(&c, name) {
                return Some(found);
            }
            child = c.next_sibling();
        }
        None
    }
    walk(&root, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_render_in_whole_megabytes() {
        assert_eq!(mib(250_770_926), "239 MB");
        assert_eq!(mib(0), "0 MB");
    }

    #[test]
    fn durations_render_as_hours_minutes_seconds() {
        assert_eq!(hms(1_291_000), "0:21:31");
    }

    #[test]
    fn every_step_has_a_distinct_tag() {
        let tags: Vec<&str> = [
            Step::Drive,
            Step::Identify,
            Step::Settings,
            Step::Progress,
            Step::Results,
        ]
        .iter()
        .map(|s| s.tag())
        .collect();
        let mut sorted = tags.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), tags.len());
    }

    #[test]
    fn preferred_languages_decide_what_starts_ticked() {
        // the rip page offers what the disc has; preferences decide the ticks
        let prefs = Preferences {
            preferred_languages: vec!["swe".into(), "eng".into()],
            ..Preferences::default()
        };
        let on_disc: Vec<String> = ["eng", "spa", "swe"].iter().map(|s| s.to_string()).collect();
        let rows = prefs.preselect(&on_disc);
        // preferred ones first and ticked, in preference order
        assert_eq!(rows[0], ("swe".to_string(), true));
        assert_eq!(rows[1], ("eng".to_string(), true));
        assert_eq!(rows[2], ("spa".to_string(), false));
    }

    #[test]
    fn the_makemkv_option_cannot_be_switched_on_when_it_is_absent() {
        // an option that cannot be honoured must not look as though it can
        let prefs = Preferences { makemkv_fallback: true, ..Preferences::default() };
        assert_eq!(prefs.use_makemkv(), Preferences::makemkv_available());
    }

    #[test]
    fn the_rip_folder_defaults_somewhere_other_than_the_library() {
        // ripping into the library would leave raw titles among the episodes
        let prefs = Preferences::default();
        let output = prefs.output_dir.clone().unwrap_or_else(|| glib::home_dir().join("Videos"));
        let rip = prefs
            .rip_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("riplika-rip"));
        assert_ne!(output, rip);
    }

    use riplika_core::naming;

    #[test]
    fn naming_matches_what_the_results_page_will_show() {
        // the GUI shows file names, so it must agree with the library's rules
        let media = Media::Series {
            title: "Parks and Recreation".into(),
            year: Some(2009),
            season: 7,
            provider_id: None,
        };
        let item = Item {
            source: PathBuf::from("/rip/a.mkv"),
            role: Role::Episode { season: 7, number: 2 },
            title: "Ron & Jammy".into(),
            air_date: None,
            duration: 0,
            destination: None,
        };
        assert_eq!(
            naming::file_name(&media, &item, Container::Mp4),
            "Parks and Recreation - S07E02 - Ron & Jammy.mp4"
        );
    }
}

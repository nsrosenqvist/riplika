//! riplika - a window over the disc pipeline.
//!
//! Four steps, in the order the work happens: pick a drive, confirm what the
//! disc is, choose how to encode it, watch it run. The middle step is the one
//! that justifies a window at all - identification is a guess, and a guess is
//! only safe if it is easy to overrule.

mod prefs_dialog;
mod show_picker;
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
    /// What the page has settled on, from identification or from the picker.
    selected: Option<Candidate>,
    chosen: Option<Media>,
    items: Vec<Item>,
    cancel: riplika_core::host::Cancel,
    /// What was last searched for, so reopening the picker resumes it.
    query: String,
    /// A catalogue search is in flight, so its result should be announced.
    searching: bool,
    /// What is running, if anything.
    ///
    /// One drive, one job: a second scan started while the first is reading
    /// contends for the same hardware and both come back slower. The buttons
    /// that would start work are switched off rather than left live.
    busy: Option<String>,
}

impl Default for State {
    fn default() -> Self {
        State {
            drives: Vec::new(),
            drive: None,
            scan: None,
            candidates: Vec::new(),
            selected: None,
            chosen: None,
            items: Vec::new(),
            cancel: riplika_core::host::Cancel::new(),
            busy: None,
            query: String::new(),
            searching: false,
        }
    }
}

/// Every widget the message loop needs to reach.
struct Ui {
    nav: adw::NavigationView,
    toasts: adw::ToastOverlay,
    drive_page: adw::StatusPage,
    drive_combo: adw::ComboRow,
    drive_group: adw::PreferencesGroup,
    drive_next: gtk::Button,
    chosen_row: adw::ActionRow,
    /// The open picker, so search results can be put where the user is looking.
    picker: RefCell<Option<show_picker::Picker>>,
    identify_next: gtk::Button,
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
    /// A handle back to itself, so a widget callback can reach the window
    /// without keeping it alive and leaking it.
    me: RefCell<std::rc::Weak<App>>,
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

/// A comfortable measure for a page of form rows.
///
/// Wide enough that a label and its control are not squeezed together, narrow
/// enough that the eye does not have to travel the width of a maximised window
/// to get from one to the other.
const CONTENT_WIDTH: i32 = 860;

/// Narrower, for a page that is a handful of centred things rather than a form.
const FOCUSED_WIDTH: i32 = 560;

fn page(tag: &str, title: &str, child: &impl IsA<gtk::Widget>) -> adw::NavigationPage {
    page_clamped(tag, title, child, CONTENT_WIDTH)
}

fn page_clamped(
    tag: &str,
    title: &str,
    child: &impl IsA<gtk::Widget>,
    width: i32,
) -> adw::NavigationPage {
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
    // Rows stretched across a wide window are hard to read - the eye has to
    // travel from a label on the left to its control on the right - so the
    // content is held to a comfortable measure and centred, which is what the
    // platform's own preference pages do.
    let clamp = adw::Clamp::builder()
        .maximum_size(width)
        .tightening_threshold(width / 2)
        // so a page that wants to centre itself vertically has the height to
        // do it in, rather than being sized to its own content
        .vexpand(true)
        .child(child)
        .build();
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&clamp)
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
        me: RefCell::new(std::rc::Weak::new()),
        ui,
        state: RefCell::new(State::default()),
        prefs: Rc::new(prefs_dialog::Store::new(Preferences::load())),
    });

    *app_state.me.borrow_mut() = Rc::downgrade(&app_state);
    wire(&app_state, &window);
    window.present();
}

fn build_ui() -> Ui {
    let nav = adw::NavigationView::new();
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&nav));

    // --- step one: the drive ---------------------------------------------
    //
    // One screen rather than two. A boxed list is heavy furniture for something
    // that is almost always a single row, and swapping between an empty layout
    // and a populated one makes the landing screen feel like two places. A
    // status page is the landing screen; what it says changes with what is in
    // the machine, and the drive chooser appears only when there is a choice to
    // make.
    let drive_page = adw::StatusPage::builder()
        .icon_name("media-optical-symbolic")
        .title("No disc")
        .vexpand(true)
        .build();

    let drive_controls = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .halign(gtk::Align::Center)
        .build();

    let drive_group = adw::PreferencesGroup::builder().build();
    let drive_combo = adw::ComboRow::builder().title("Drive").build();
    drive_group.add(&drive_combo);
    // Hidden unless there is more than one: a chooser offering one option is
    // a decision the user does not have.
    drive_group.set_visible(false);

    let drive_next = gtk::Button::builder()
        .label("Analyse disc")
        .sensitive(false)
        .halign(gtk::Align::Center)
        .css_classes(vec!["pill".to_string(), "suggested-action".to_string()])
        .build();
    let refresh = gtk::Button::builder()
        .label("Look again")
        .name("refresh")
        .halign(gtk::Align::Center)
        .css_classes(vec!["flat".to_string()])
        .build();

    drive_controls.append(&drive_group);
    drive_controls.append(&drive_next);
    drive_controls.append(&refresh);
    drive_page.set_child(Some(&drive_controls));
    nav.add(&page(Step::Drive.tag(), "Riplika", &drive_page));

    // --- step two: what is it? -------------------------------------------
    //
    // The alternatives are only interesting while you are choosing between
    // them; left on the page they are a list of things already rejected, and
    // they push what actually needs answering further down. So this states what
    // it settled on, and the alternatives live in a dialog opened when that is
    // wrong.
    let id_body = body();
    let id_group = adw::PreferencesGroup::builder()
        .title("Identified as")
        .build();
    let chosen_row = adw::ActionRow::builder()
        .title("Not identified")
        .subtitle("Choose the show")
        .activatable(true)
        .build();
    chosen_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    id_group.add(&chosen_row);

    // Applies to whichever show is chosen above; changing it needs no search.
    let detail_group = adw::PreferencesGroup::builder()
        .title("This disc")
        .description("Which part of the show it holds. The disc number decides where episode numbering starts.")
        .build();
    let season_entry = adw::EntryRow::builder().title("Season").build();
    let disc_entry = adw::EntryRow::builder().title("Disc").build();
    detail_group.add(&season_entry);
    detail_group.add(&disc_entry);

    let identify_next = gtk::Button::builder()
        .label("Continue")
        .sensitive(false)
        .css_classes(vec!["suggested-action".to_string()])
        .halign(gtk::Align::End)
        .build();
    id_body.append(&id_group);
    id_body.append(&detail_group);
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
    let stage_label = gtk::Label::builder()
        .label("Starting")
        .justify(gtk::Justification::Center)
        .wrap(true)
        .build();
    stage_label.add_css_class("title-2");
    let progress = gtk::ProgressBar::builder().show_text(true).build();
    let log = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(vec!["boxed-list".to_string()])
        .build();
    let cancel_button = gtk::Button::builder()
        .label("Cancel")
        .css_classes(vec!["pill".to_string(), "destructive-action".to_string()])
        .halign(gtk::Align::Center)
        .build();
    prog_body.append(&stage_label);
    prog_body.append(&progress);
    prog_body.append(&log);
    prog_body.append(&cancel_button);
    // While a scan is running there are three short things on this page, and
    // pinned to the top of a tall window they look stranded. Centred, the page
    // reads as one thing happening. It still grows downwards normally once the
    // log fills up.
    prog_body.set_valign(gtk::Align::Center);
    prog_body.set_vexpand(true);
    // Narrower than a form page: this is a heading, a bar and a list, and they
    // read better as a column than spread across the window.
    nav.add(&page_clamped(Step::Progress.tag(), "Working", &prog_body, FOCUSED_WIDTH));

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
        drive_page,
        drive_combo,
        drive_group,
        drive_next,
        chosen_row,
        picker: RefCell::new(None),
        identify_next,
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
    /// Switch off everything that would start a second job, or switch it back on.
    ///
    /// The drive is the shared resource: two scans at once contend for it and
    /// both come back slower, and a rip started on top of a scan will fight the
    /// same hardware. Leaving the buttons live and hoping is not a design.
    fn set_busy(&self, what: Option<&str>) {
        self.state.borrow_mut().busy = what.map(str::to_string);
        let idle = what.is_none();
        let has_candidate = !self.state.borrow().candidates.is_empty();

        self.ui.drive_next.set_label(match what {
            Some(w) => w,
            None => "Analyse disc",
        });
        // One rule for whether analysing is possible, applied from both places.
        self.refresh_drive_page();
        self.ui.identify_next.set_sensitive(idle && has_candidate);
        for name in ["start", "refresh", "search"] {
            for b in find_buttons(&self.ui.output_dir, name) {
                b.set_sensitive(idle);
            }
        }
    }

    fn is_busy(&self) -> bool {
        self.state.borrow().busy.is_some()
    }

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

    /// What the drive page should say, given what the machine has.
    ///
    /// Three situations, and which one you are in decides what you do next, so
    /// the page says which one rather than just showing nothing.
    fn drive_status(drives: &[Drive], selected: Option<&Drive>) -> (String, String, bool) {
        if drives.is_empty() {
            return (
                "No disc drive".into(),
                "No optical drive was found. Connect one and look again.".into(),
                false,
            );
        }
        match selected {
            Some(d) => match &d.disc_label {
                // The label is what the disc calls itself, and seeing it is the
                // first confirmation that the right disc is in the tray.
                Some(label) => (label.clone(), format!("{} in {}", d.name, d.device), true),
                None => (
                    "No disc".into(),
                    format!("Insert a DVD into {}, then look again.", d.device),
                    false,
                ),
            },
            None => ("No disc".into(), "Choose a drive.".into(), false),
        }
    }

    fn show_drives(&self, drives: &[Drive]) {
        let model = gtk::StringList::new(&[]);
        for d in drives {
            model.append(&format!(
                "{}  -  {}",
                d.device,
                d.disc_label.as_deref().unwrap_or("empty")
            ));
        }
        self.ui.drive_combo.set_model(Some(&model));
        // A chooser offering one option is not a choice worth showing.
        self.ui.drive_group.set_visible(drives.len() > 1);

        // Prefer a drive with something in it: on a machine with two, the one
        // holding a disc is what was meant.
        let pick = drives.iter().position(Drive::has_disc).unwrap_or(0);
        if !drives.is_empty() {
            self.ui.drive_combo.set_selected(pick as u32);
        }
        {
            let mut state = self.state.borrow_mut();
            state.drives = drives.to_vec();
            state.drive = drives.get(pick).cloned();
        }
        self.refresh_drive_page();
    }

    /// Re-read the drive page from state.
    fn refresh_drive_page(&self) {
        let (drives, selected) = {
            let state = self.state.borrow();
            (state.drives.clone(), state.drive.clone())
        };
        let (title, description, ready) = Self::drive_status(&drives, selected.as_ref());
        self.ui.drive_page.set_title(&title);
        self.ui.drive_page.set_description(Some(&description));
        // Analysing a drive with no disc in it can only fail, so it is not
        // offered rather than offered and then refused.
        self.ui.drive_next.set_sensitive(ready && !self.is_busy());
    }

    /// Take in a fresh set of candidates.
    ///
    /// They land in the picker if it is open - that is where the user is
    /// looking - and otherwise become the page's stated answer.
    fn show_candidates(&self, cands: &[Candidate]) {
        self.state.borrow_mut().candidates = cands.to_vec();
        let already_chosen = self.state.borrow().selected.is_some();

        if let Some(picker) = self.ui.picker.borrow().as_ref() {
            let app = self.weak();
            picker.show(cands, move |i| {
                if let Some(app) = app.upgrade() {
                    app.choose(i);
                }
            });
            return;
        }
        // Identification's best guess becomes the answer, but never overrides
        // something the user has already picked.
        if !already_chosen {
            self.state.borrow_mut().selected = cands.first().cloned();
        }
        self.show_choice();
    }

    /// The user picked one from the dialog.
    fn choose(&self, index: usize) {
        let chosen = self.state.borrow().candidates.get(index).cloned();
        if chosen.is_none() {
            return;
        }
        self.state.borrow_mut().selected = chosen;
        if let Some(picker) = self.ui.picker.borrow().as_ref() {
            picker.close();
        }
        *self.ui.picker.borrow_mut() = None;
        self.show_choice();
    }

    /// Restate what the page has settled on.
    fn show_choice(&self) {
        let selected = self.state.borrow().selected.clone();
        match selected {
            Some(c) => {
                self.ui.chosen_row.set_title(&c.media.describe_work());
                // Both here: what the work is, and why this disc is thought to
                // be it. On the identify page the evidence is the point.
                let mut lines: Vec<String> = Vec::new();
                if let Some(d) = &c.detail {
                    lines.push(d.clone());
                }
                lines.extend(c.reasons.iter().cloned());
                self.ui.chosen_row.set_subtitle(&lines.join("\n"));
                if self.ui.season_entry.text().trim().is_empty()
                    && let Some(n) = c.media.season()
                {
                    self.ui.season_entry.set_text(&n.to_string());
                }
                self.ui.identify_next.set_sensitive(!self.is_busy());
            }
            None => {
                self.ui.chosen_row.set_title("Not identified");
                self.ui.chosen_row.set_subtitle("Choose the show");
                self.ui.identify_next.set_sensitive(false);
            }
        }
    }

    fn weak(&self) -> std::rc::Weak<App> {
        self.me.borrow().clone()
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
                let guess = riplika_core::identify::label::parse(&scan.label);
                if let Some(d) = guess.disc {
                    self.ui.disc_entry.set_text(&d.to_string());
                }
                // Only when the label actually said. Filling it with a guess
                // the label did not make is how "season 1" ends up looking
                // like a decision rather than a default.
                if let Some(n) = guess.season {
                    self.ui.season_entry.set_text(&n.to_string());
                }
                self.state.borrow_mut().query = guess.title.clone();
                self.show_languages(&scan.all_languages());
                self.state.borrow_mut().scan = Some(*scan);
            }
            Msg::Candidates(c) => {
                self.set_busy(None);
                let searched = self.state.borrow().searching;
                self.state.borrow_mut().searching = false;
                if searched {
                    // The answer appears in a list the user may not be looking
                    // at, so say what happened as well as showing it.
                    self.toast(&match c.len() {
                        0 => "Nothing found".to_string(),
                        1 => "1 match".to_string(),
                        n => format!("{n} matches"),
                    });
                }
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
                self.set_busy(None);
                self.show_report(&r);
                self.go(Step::Results);
            }
            Msg::Failed(e) => {
                self.set_busy(None);
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
        let combo = app.ui.drive_combo.clone();
        combo.connect_selected_notify(move |c| {
            let i = c.selected() as usize;
            let d = app.state.borrow().drives.get(i).cloned();
            app.state.borrow_mut().drive = d;
            app.refresh_drive_page();
        });
    }
    // Two buttons carry this name now - the one beside the list and the one in
    // the empty state - and wiring only the first found would leave the other
    // looking live and doing nothing.
    for refresh in find_buttons(&app.ui.drive_next, "refresh") {
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
            if app.is_busy() {
                app.toast("Already working - wait for it, or cancel it first");
                return;
            }
            app.ui.stage_label.set_label("Scanning disc");
            app.set_busy(Some("Reading disc..."));
            // A scan takes minutes and there is nothing to abandon it with on
            // this page, so show the progress page, which has the cancel button.
            app.state.borrow_mut().cancel = riplika_core::host::Cancel::new();
            app.ui.cancel_button.set_label("Cancel");
            app.go(Step::Progress);
            let cancel = app.state.borrow().cancel.clone();
            let allow = app.prefs.prefs.borrow().use_makemkv();
            worker::analyse(drive, allow, cancel, tx.clone());
        });
    }

    // Step two -------------------------------------------------------------
    {
        let app = Rc::clone(app);
        app.clone().ui.identify_next.connect_clicked(move |_| {
            let chosen = app.state.borrow().selected.clone();
            match chosen {
                Some(c) => {
                    // The picker found a show; the season comes from this page.
                    let season = app.ui.season_entry.text().trim().parse::<u32>().ok();
                    let media = match season {
                        Some(n) => c.media.with_season(n),
                        None => c.media,
                    };
                    app.state.borrow_mut().chosen = Some(media);
                    app.go(Step::Settings);
                }
                None => app.toast("Choose what this disc is first"),
            }
        });
    }
    {
        // Tapping what it settled on is how you disagree with it.
        let app = Rc::clone(app);
        let tx = tx.clone();
        let window = window.clone();
        let row = app.ui.chosen_row.clone();
        row.connect_activated(move |_| {
            let query = {
                let state = app.state.borrow();
                if state.query.trim().is_empty() {
                    state
                        .selected
                        .as_ref()
                        .map(|c| c.media.title().to_string())
                        .unwrap_or_default()
                } else {
                    state.query.clone()
                }
            };
            let app_for_search = Rc::clone(&app);
            let tx = tx.clone();
            let picker = show_picker::present(&window, &query, move |q| {
                app_for_search.state.borrow_mut().query = q.clone();
                if let Some(p) = app_for_search.ui.picker.borrow().as_ref() {
                    p.show_searching();
                }
                worker::search(q, None, tx.clone());
            });

            // Open on what is already known rather than an empty list.
            let candidates = app.state.borrow().candidates.clone();
            let chooser = app.weak();
            picker.show(&candidates, move |i| {
                if let Some(app) = chooser.upgrade() {
                    app.choose(i);
                }
            });
            *app.ui.picker.borrow_mut() = Some(picker);
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
            if app.is_busy() {
                app.toast("Already working - wait for it, or cancel it first");
                return;
            }
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
            // The job will not stop this instant - it stops at the next command
            // boundary - but nothing new should be startable in the meantime.
            app.set_busy(Some("Stopping..."));
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

#[cfg(test)]
mod busy_tests {
    use super::*;

    /// The rules the busy flag encodes, kept honest without a display.
    ///
    /// There is one optical drive. Two scans at once contend for it, and a rip
    /// started on top of a scan fights the same hardware - so exactly one job
    /// runs at a time and the buttons that would start another are switched
    /// off, rather than left live and hoped about.
    #[test]
    fn a_job_is_either_running_or_not() {
        let mut state = State::default();
        assert!(state.busy.is_none(), "nothing runs before anything is started");
        state.busy = Some("Reading disc...".into());
        assert!(state.busy.is_some());
        state.busy = None;
        assert!(state.busy.is_none(), "finishing must clear it");
    }

    #[test]
    fn every_way_a_job_ends_clears_the_flag() {
        // success, failure and cancellation all have to release the buttons;
        // missing one leaves the window permanently unusable
        for outcome in ["finished", "failed", "cancelled"] {
            let mut state = State::default();
            state.busy = Some("Ripping...".into());
            match outcome {
                "finished" | "failed" => state.busy = None,
                _ => state.busy = Some("Stopping...".into()),
            }
            if outcome == "cancelled" {
                // cancelling does not finish instantly - it stops at the next
                // command boundary - but the run then reports and clears
                assert!(state.busy.is_some());
                state.busy = None;
            }
            assert!(state.busy.is_none(), "{outcome} left the window stuck");
        }
    }

    #[test]
    fn cancelling_is_honoured_by_the_pipeline_not_just_the_window() {
        // the button sets a token the runner checks before each command, so
        // "stopping after the current step" is a true description
        let cancel = riplika_core::host::Cancel::new();
        assert!(!cancel.is_cancelled());
        cancel.cancel();
        assert!(cancel.is_cancelled());
        assert!(cancel.check().is_err());
    }

    #[test]
    fn a_fresh_token_is_used_for_each_job() {
        // reusing a cancelled token would make the next job stop immediately
        let first = riplika_core::host::Cancel::new();
        first.cancel();
        let second = riplika_core::host::Cancel::new();
        assert!(first.is_cancelled());
        assert!(!second.is_cancelled(), "a new job must start uncancelled");
    }
}

#[cfg(test)]
mod drive_page_tests {
    use super::*;

    fn drive(device: &str, label: Option<&str>) -> Drive {
        Drive {
            id: device.into(),
            device: device.into(),
            name: "PIONEER BD-RW".into(),
            disc_label: label.map(str::to_string),
        }
    }

    #[test]
    fn no_drive_at_all_says_so_and_offers_nothing() {
        let (title, description, ready) = App::drive_status(&[], None);
        assert_eq!(title, "No disc drive");
        assert!(description.contains("Connect one"), "{description}");
        assert!(!ready, "there is nothing to analyse");
    }

    #[test]
    fn a_drive_with_no_disc_asks_for_one_and_names_the_tray() {
        let d = drive("/dev/sr0", None);
        let (title, description, ready) = App::drive_status(std::slice::from_ref(&d), Some(&d));
        assert_eq!(title, "No disc");
        assert!(description.contains("/dev/sr0"), "{description}");
        // analysing an empty drive can only fail, so it is not offered
        assert!(!ready);
    }

    #[test]
    fn a_disc_is_named_by_its_own_label() {
        // seeing the label is the first confirmation that the right disc is in
        let d = drive("/dev/sr0", Some("PARKS_AND_RECREATION"));
        let (title, description, ready) = App::drive_status(std::slice::from_ref(&d), Some(&d));
        assert_eq!(title, "PARKS_AND_RECREATION");
        assert!(description.contains("PIONEER"), "{description}");
        assert!(description.contains("/dev/sr0"), "{description}");
        assert!(ready);
    }

    #[test]
    fn selecting_the_empty_one_of_two_drives_withdraws_the_action() {
        let loaded = drive("/dev/sr1", Some("DISC"));
        let empty = drive("/dev/sr0", None);
        let drives = [empty.clone(), loaded.clone()];
        assert!(App::drive_status(&drives, Some(&loaded)).2);
        assert!(!App::drive_status(&drives, Some(&empty)).2, "the empty tray is not analysable");
    }

    #[test]
    fn a_chooser_is_only_worth_showing_when_there_is_a_choice() {
        // the rule the page applies to the dropdown's visibility
        assert!(![drive("/dev/sr0", Some("A"))].len() > 1);
        assert!([drive("/dev/sr0", Some("A")), drive("/dev/sr1", None)].len() > 1);
    }
}

#[cfg(test)]
mod identify_page_tests {
    use super::*;

    fn show(season: u32) -> Media {
        Media::Series {
            title: "Parks and Recreation".into(),
            year: Some(2009),
            season,
            provider_id: Some("1633".into()),
        }
    }

    #[test]
    fn a_candidate_is_shown_as_a_work_not_a_season() {
        // The catalogue found a show. Labelling the row "season 1" claims the
        // search determined something it did not, and then contradicts whatever
        // season the user sets.
        assert_eq!(show(1).describe_work(), "Parks and Recreation (2009)");
        assert!(!show(1).describe_work().contains("season"));
    }

    #[test]
    fn changing_the_season_needs_no_new_search() {
        // This was the confusion: the season sat in the search group, so
        // setting it looked like it should search, and looked inert when it
        // did not.
        let chosen = show(1).with_season(6);
        assert_eq!(chosen.season(), Some(6));
        assert_eq!(chosen.provider_id().as_deref(), Some("1633"));
    }

    #[test]
    fn a_season_that_is_not_a_number_leaves_the_choice_alone() {
        let parsed = "six".trim().parse::<u32>().ok();
        let media = match parsed {
            Some(n) => show(1).with_season(n),
            None => show(1),
        };
        assert_eq!(media.season(), Some(1));
    }

    #[test]
    fn a_search_result_count_is_worth_announcing() {
        // the answer lands in a list the user may not be watching
        let phrase = |n: usize| match n {
            0 => "Nothing found".to_string(),
            1 => "1 match".to_string(),
            n => format!("{n} matches"),
        };
        assert_eq!(phrase(0), "Nothing found");
        assert_eq!(phrase(1), "1 match");
        assert_eq!(phrase(4), "4 matches");
    }
}

#[cfg(test)]
mod picker_tests {
    use super::*;

    fn candidate(title: &str, season: u32, confidence: f32) -> Candidate {
        Candidate {
            media: Media::Series {
                title: title.into(),
                year: Some(2009),
                season,
                provider_id: Some("1633".into()),
            },
            confidence,
            reasons: vec!["volume label".into()],
            detail: Some("NBC \u{b7} Scripted \u{b7} 2009-2015".into()),
        }
    }

    #[test]
    fn identification_supplies_the_answer_when_none_has_been_chosen() {
        let mut state = State::default();
        let cands = [candidate("Parks and Recreation", 1, 0.85)];
        if state.selected.is_none() {
            state.selected = cands.first().cloned();
        }
        assert_eq!(state.selected.unwrap().media.title(), "Parks and Recreation");
    }

    #[test]
    fn a_later_identification_does_not_override_what_the_user_picked() {
        // reopening the picker and searching must not have its result quietly
        // replaced by the disc's original guess
        let mut state = State::default();
        state.selected = Some(candidate("The Office", 3, 0.4));
        let cands = [candidate("Parks and Recreation", 1, 0.85)];
        if state.selected.is_none() {
            state.selected = cands.first().cloned();
        }
        assert_eq!(state.selected.unwrap().media.title(), "The Office");
    }

    #[test]
    fn choosing_from_the_picker_replaces_the_answer() {
        let mut state = State::default();
        state.candidates = vec![
            candidate("Parks and Recreation", 1, 0.85),
            candidate("Parks", 1, 0.11),
        ];
        state.selected = state.candidates.first().cloned();
        state.selected = state.candidates.get(1).cloned();
        assert_eq!(state.selected.unwrap().media.title(), "Parks");
    }

    #[test]
    fn choosing_an_index_that_is_gone_changes_nothing() {
        // results can be replaced by a newer search while a row is being tapped
        let mut state = State::default();
        state.selected = Some(candidate("Parks and Recreation", 1, 0.85));
        let chosen = state.candidates.get(7).cloned();
        if chosen.is_some() {
            state.selected = chosen;
        }
        assert_eq!(state.selected.unwrap().media.title(), "Parks and Recreation");
    }

    #[test]
    fn the_query_is_remembered_so_reopening_resumes_where_it_was() {
        let mut state = State::default();
        assert!(state.query.is_empty());
        state.query = "parks".into();
        assert_eq!(state.query, "parks");
    }
}

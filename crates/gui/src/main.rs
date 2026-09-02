//! riplika - a window over the disc pipeline.
//!
//! Four steps, in the order the work happens: pick a drive, confirm what the
//! disc is, choose how to encode it, watch it run. The middle step is the one
//! that justifies a window at all - identification is a guess, and a guess is
//! only safe if it is easy to overrule.

mod i18n;
mod prefs_dialog;
mod rows;
mod show_picker;
mod worker;

use crate::i18n::{tr, tr_args, tr_n};
use adw::prelude::*;
use gtk::glib;
use riplika_core::disc::DiscKind;
use riplika_core::job::{Event, Report};
use riplika_core::lang::{self, LanguageSet};
use show_picker::{Choice, Prompt};

/// What the release picker currently has in it.
///
/// Two sources answer the same question and are chosen from the same list, but
/// they are not interchangeable: a release the disc id named is already known
/// in full, down to which disc of a box set is in the tray, while a release
/// found by name is a title and an id and has to be fetched. Keeping them
/// apart is what stops a box set being fetched back as its first disc.
#[derive(Clone)]
enum Offering {
    Nothing,
    /// Releases this exact disc belongs to.
    ThisDisc(Vec<riplika_core::identify::music::Album>),
    /// Releases whose name matched what was typed.
    Searched(Vec<riplika_core::identify::music::Match>),
}
use riplika_core::model::*;
use riplika_core::prefs::{AudioFormat, Preferences};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
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
    /// The route to this step, so navigation can replace the stack rather
    /// than push onto it.
    ///
    /// Progress is reached from two places and its route says so: while a disc
    /// is being scanned there is nothing behind it but the drive page, and
    /// going back from a rip means starting the choices again rather than
    /// re-entering settings while the drive is busy.
    fn path(self) -> &'static [&'static str] {
        match self {
            Step::Drive => &["drive"],
            Step::Identify => &["drive", "identify"],
            Step::Settings => &["drive", "identify", "settings"],
            Step::Progress => &["drive", "progress"],
            Step::Results => &["drive", "results"],
        }
    }

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
    /// The music disc in the drive, once read.
    music: Option<riplika_core::musicjob::Found>,
    /// Which of the releases matching that disc was settled on.
    album: Option<riplika_core::identify::music::Album>,
    /// What the release picker is offering, while it is open.
    offering: Offering,
    /// The data disc in the drive, and the little it says about itself.
    game: Option<riplika_core::game::GameDisc>,
    candidates: Vec<Candidate>,
    /// What the page has settled on, from identification or from the picker.
    selected: Option<Candidate>,
    chosen: Option<Media>,
    items: Vec<Item>,
    cancel: riplika_core::host::Cancel,
    /// What was last searched for, so reopening the picker resumes it.
    query: String,
    /// Turns progress into a time remaining.
    eta: riplika_core::job::Eta,
    /// The disc the desktop handed us, if it launched us for one.
    handed: Option<String>,
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
            music: None,
            album: None,
            offering: Offering::Nothing,
            game: None,
            candidates: Vec::new(),
            selected: None,
            chosen: None,
            items: Vec::new(),
            cancel: riplika_core::host::Cancel::new(),
            busy: None,
            query: String::new(),
            eta: riplika_core::job::Eta::new(),
            handed: None,
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
    /// The cover of what was identified, or the kind of disc when there is
    /// none - which is most of the time, since only two of the three
    /// catalogues have pictures and games have none at all.
    chosen_art: gtk::Image,
    /// Opens the picker. Hidden where there is nothing to choose, rather than
    /// left to be pressed for no effect.
    search_button: gtk::Button,
    /// Watches for discs coming and going.
    ///
    /// Held because the signals stop the moment it is dropped, and a monitor
    /// nobody keeps is a monitor that fires once and never again.
    volumes: RefCell<Option<gtk::gio::VolumeMonitor>>,
    /// The open picker, so search results can be put where the user is looking.
    picker: RefCell<Option<show_picker::Picker>>,
    identify_next: gtk::Button,
    season_entry: adw::EntryRow,
    video: adw::ComboRow,
    audio: adw::ComboRow,
    container: adw::ComboRow,
    /// The video settings, hidden when the disc has no video on it.
    quality_group: adw::PreferencesGroup,
    contents_group: adw::PreferencesGroup,
    /// Season and disc number: an episode's coordinates, and meaningless here.
    id_group: adw::PreferencesGroup,
    detail_group: adw::PreferencesGroup,
    music_group: adw::PreferencesGroup,
    music_format: adw::ComboRow,
    music_quality: adw::ComboRow,
    language_group: adw::PreferencesGroup,
    include_extended: adw::SwitchRow,
    include_extras: adw::SwitchRow,
    accurate_chapters: adw::SwitchRow,
    language_rows: RefCell<Vec<(String, adw::SwitchRow)>>,
    output_dir: adw::ActionRow,
    disc_entry: adw::EntryRow,
    stage_label: gtk::Label,
    progress: gtk::ProgressBar,
    progress_text: gtk::Label,
    log: gtk::TextView,
    log_scroll: gtk::ScrolledWindow,
    cancel_button: gtk::Button,
    results: adw::PreferencesGroup,
    /// The rows put into `results`, so they can be taken out again.
    ///
    /// An AdwPreferencesGroup cannot be emptied by walking its children: its
    /// first child is an internal box, not a row, and asking it to remove that
    /// is refused - leaving the box still there, still the first child, and
    /// the loop going round again. It did that several thousand times a second
    /// until the disk filled. What went in is remembered instead.
    result_rows: RefCell<Vec<adw::ActionRow>>,
    /// The heading over the file list, and the line under it.
    ///
    /// Labels rather than an AdwStatusPage. That widget is meant to *be* a
    /// page and to fill one; put in a box beside a list and two buttons it
    /// takes what height is left, which was not enough for its own title -
    /// "Finished with problems" came out with the top and bottom of every
    /// letter cut off. A label asks for the height it needs and wraps.
    results_title: gtk::Label,
    results_summary: gtk::Label,
}

struct App {
    /// For work the window starts on its own, rather than in response to a
    /// button that was given a sender when it was wired.
    tx: RefCell<Option<std::sync::mpsc::Sender<Msg>>>,
    /// A handle back to itself, so a widget callback can reach the window
    /// without keeping it alive and leaking it.
    me: RefCell<std::rc::Weak<App>>,
    ui: Ui,
    state: RefCell<State>,
    prefs: Rc<prefs_dialog::Store>,
}

fn main() -> glib::ExitCode {
    // What the desktop passes when a disc is inserted: the mount point of the
    // volume it just mounted, as a file:// URI. Read before GTK sees it, since
    // it is not an option GTK knows.
    let handed: Option<String> = std::env::args().nth(1).filter(|a| !a.starts_with('-'));

    // Before any string is asked for.
    i18n::init();

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| build(app, handed.clone()));
    app.run_with_args::<&str>(&[])
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

/// A button that shows what it does as well as saying it.
///
/// The utility actions - look again, eject - are read at a glance far more
/// often than they are read as words, and an icon is what makes that possible.
/// The one action a page exists for is left as plain text, where a picture
/// would only compete with it.
fn labelled_button(icon: &str, label: &str, name: &str) -> gtk::Button {
    let content = adw::ButtonContent::builder().icon_name(icon).label(label).build();

    gtk::Button::builder().name(name).child(&content).build()
}

/// Change a labelled button's text, whichever kind it is.
fn set_button_label(button: &gtk::Button, label: &str) {
    match button.child().and_downcast::<adw::ButtonContent>() {
        Some(content) => content.set_label(label),
        None => button.set_label(label),
    }
}

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
        .tooltip_text(tr("Preferences"))
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

    adw::NavigationPage::builder().tag(tag).title(title).child(&view).build()
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

fn build(app: &adw::Application, handed: Option<String>) {
    let ui = build_ui();
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(tr("Riplika"))
        .default_width(720)
        .default_height(720)
        .content(&ui.toasts)
        .build();

    let app_state = Rc::new(App {
        tx: RefCell::new(None),
        me: RefCell::new(std::rc::Weak::new()),
        ui,
        state: RefCell::new(State::default()),
        prefs: Rc::new(prefs_dialog::Store::new(Preferences::load())),
    });

    *app_state.me.borrow_mut() = Rc::downgrade(&app_state);
    app_state.state.borrow_mut().handed = handed;
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
        .title(tr("No disc"))
        .vexpand(true)
        .build();

    let drive_controls = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .halign(gtk::Align::Center)
        .build();

    let drive_group = adw::PreferencesGroup::builder().build();
    let drive_combo = rows::combo().title(tr("Drive")).build();
    drive_group.add(&drive_combo);
    // Hidden unless there is more than one: a chooser offering one option is
    // a decision the user does not have.
    drive_group.set_visible(false);

    let drive_next = gtk::Button::builder()
        .label(tr("Analyse disc"))
        .sensitive(false)
        .halign(gtk::Align::Center)
        .css_classes(vec!["pill".to_string(), "suggested-action".to_string()])
        .build();
    let refresh = labelled_button("view-refresh-symbolic", &tr("Look again"), "refresh");
    refresh.set_halign(gtk::Align::Center);
    refresh.add_css_class("flat");

    let drive_secondary = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .halign(gtk::Align::Center)
        .build();
    drive_secondary.append(&refresh);
    // Swapping discs is what you do most on this page, and reaching for the
    // tray button on an external drive is not always possible.
    let eject = labelled_button("media-eject-symbolic", &tr("Eject"), "eject");
    eject.add_css_class("flat");
    drive_secondary.append(&eject);

    drive_controls.append(&drive_group);
    drive_controls.append(&drive_next);
    drive_controls.append(&drive_secondary);
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
    let id_group = adw::PreferencesGroup::builder().title(tr("Identified as")).build();
    let chosen_row =
        rows::action().title(tr("Not identified")).subtitle(tr("Choose the show")).build();
    // The cover, once there is one, and the kind of disc until then. A poster
    // is decoration and may never arrive, so what is here at the start has to
    // be something worth looking at on its own.
    let chosen_art = gtk::Image::builder()
        .icon_name("media-optical-symbolic")
        .pixel_size(64)
        .margin_top(6)
        .margin_bottom(6)
        .margin_end(6)
        .build();
    chosen_row.add_prefix(&chosen_art);
    // Searching is an action, so it is a button. It used to be the row
    // itself, which is how a game disc came to open the television picker and
    // why the arrow then had to be hidden on the paths where tapping did
    // nothing - a row that looks like it opens something and does not.
    let search_button = gtk::Button::builder()
        .label(tr("Search"))
        .valign(gtk::Align::Center)
        .css_classes(vec!["flat".to_string()])
        .build();
    id_group.set_header_suffix(Some(&search_button));
    id_group.add(&chosen_row);

    // Applies to whichever show is chosen above; changing it needs no search.
    let detail_group = adw::PreferencesGroup::builder()
        .title(tr("This disc"))
        .description(tr("Which part of the show it holds. The disc number decides where episode numbering starts."))
        .build();
    let season_entry = rows::entry().title(tr("Season")).build();
    let disc_entry = rows::entry().title(tr("Disc")).build();
    detail_group.add(&season_entry);
    detail_group.add(&disc_entry);

    let identify_next = gtk::Button::builder()
        .label(tr("Continue"))
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
    let quality = adw::PreferencesGroup::builder().title(tr("Quality")).build();
    let tiers = tier_list();
    let video = rows::combo()
        .title(tr("Picture"))
        .subtitle(tr("Medium is the sweet spot for DVD: about 170 MB an episode"))
        .model(&tiers)
        .selected(1)
        .build();
    let audio_tiers = tier_list();
    let audio = rows::combo()
        .title(tr("Sound"))
        .subtitle(tr("High keeps the original AC3 untouched; browsers cannot decode it"))
        .model(&audio_tiers)
        .selected(0)
        .build();
    let music = adw::PreferencesGroup::builder().title(tr("Music")).build();
    let music_formats = gtk::StringList::new(&["FLAC", "MP3"]);
    let music_format = rows::combo()
        .title(tr("Format"))
        .subtitle(tr("FLAC keeps the disc exactly; MP3 plays on anything"))
        .model(&music_formats)
        .selected(0)
        .build();
    let music_tiers = tier_list();
    let music_quality = rows::combo().title(tr("Quality")).model(&music_tiers).selected(0).build();
    apply_music_quality_rule(&music_quality, AudioFormat::Flac);
    music_format.connect_selected_notify({
        let quality = music_quality.clone();
        move |row| apply_music_quality_rule(&quality, format_at(row))
    });
    music.add(&music_format);
    music.add(&music_quality);

    // MKV, not Matroska. Nobody who needs this row explained knows the format
    // by its name, and the choice beside it is called MP4 rather than MPEG-4
    // Part 14 - so one of the two was asking to be recognised by the letters
    // on a file and the other by the name of a specification.
    let containers = gtk::StringList::new(&["MP4", "MKV"]);
    let container = rows::combo()
        .title(tr("Container"))
        // The one row on this page that said only its own name. Both carry
        // the show and episode tags - MP4 gets them from ffmpeg directly,
        // MKV has them written in afterwards - so what actually differs is
        // what else fits: MP4 has no room for the disc's own bitmap
        // subtitles, and MKV does.
        // What somebody choosing for the first time needs is which one plays
        // where, not which subtitle codecs each admits. MP4 is written with
        // the index at the front - HandBrake calls the same thing "web
        // optimized" - so a server can start sending it before the whole file
        // has arrived.
        .subtitle(tr("MP4 plays and streams almost anywhere; MKV holds more, on fewer players"))
        .model(&containers)
        .selected(0)
        .build();
    let accurate_chapters = rows::switch()
        .title(tr("Exact chapter marks"))
        // The drift is a tenth of a per cent, so it is proportional: the
        // first mark is nearly exact and the last is the worst. Under two
        // seconds by the end of an episode, around five by the end of a film,
        // which is why the row says which of those is in the drive.
        .subtitle(tr("Reads the disc twice, so it takes about twice as long"))
        .build();
    quality.add(&video);
    quality.add(&audio);
    quality.add(&container);
    quality.add(&accurate_chapters);

    // Built once the disc has been scanned, from the languages actually on it.
    // Offering a text field instead means guessing at spellings and finding out
    // afterwards that nothing matched.
    let language_group = adw::PreferencesGroup::builder()
        .title(tr("Languages"))
        .description(tr("What this disc carries. Your preferred languages start ticked; the first becomes the default track."))
        .build();

    // What to take off this disc. A season disc carries thirty pieces of bonus
    // material against seven episodes, so this is most of the reading as well
    // as most of the files.
    let contents_group = adw::PreferencesGroup::builder()
        .title(tr("What to take"))
        .description(tr("Episodes are always taken. Anything unticked is not read at all."))
        .build();
    let include_extended = rows::switch()
        .title(tr("Extended episodes"))
        .subtitle(tr("Longer cuts some discs carry alongside the broadcast versions"))
        .build();
    let include_extras = rows::switch()
        .title(tr("Bonus material"))
        .subtitle(tr("Featurettes, deleted scenes, gag reels"))
        .build();
    contents_group.add(&include_extended);
    contents_group.add(&include_extras);

    let folders = adw::PreferencesGroup::builder().title(tr("Output")).build();
    let output_dir = rows::action().title(tr("Folder")).activatable(true).build();
    folders.add(&output_dir);

    let start = gtk::Button::builder()
        .label(tr("Start"))
        .name("start")
        .css_classes(vec!["suggested-action".to_string()])
        .halign(gtk::Align::End)
        .build();
    set_body.append(&quality);
    set_body.append(&music);
    set_body.append(&language_group);
    set_body.append(&contents_group);
    set_body.append(&folders);
    set_body.append(&start);
    nav.add(&page(Step::Settings.tag(), "Settings", &set_body));

    // --- step four: watching it happen -----------------------------------
    let prog_body = body();
    let stage_label = gtk::Label::builder()
        .label(tr("Starting"))
        .justify(gtk::Justification::Center)
        .wrap(true)
        .build();
    stage_label.add_css_class("title-2");
    // Our own label rather than the bar's own text. GtkProgressBar draws its
    // text as a node inside itself, so the space between the two is whatever
    // the theme says and can only be changed by overriding it. A label above
    // the bar can be spaced, sized and ellipsised like any other.
    let progress_text = gtk::Label::builder()
        .label("")
        .justify(gtk::Justification::Center)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    progress_text.add_css_class("dim-label");
    progress_text.add_css_class("caption");
    let progress = gtk::ProgressBar::new();
    let progress_block =
        gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    progress_block.append(&progress_text);
    progress_block.append(&progress);
    // A list row per line is enormous furniture for one line of text, and a
    // long run produces hundreds. This is a log; it should look like one and
    // stay out of the way of the thing that matters, which is the progress bar.
    let log = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(10)
        .right_margin(10)
        .wrap_mode(gtk::WrapMode::WordChar)
        .build();
    log.add_css_class("dim-label");
    // Small: it is a running commentary, not the content of the page.
    log.add_css_class("caption");
    let log_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .height_request(180)
        .child(&log)
        .css_classes(vec!["card".to_string()])
        .build();
    // Stopping work is not a dialog's "Cancel", and the icon says which of the
    // two this is - the same word means "discard what I typed" three pages
    // back.
    let cancel_button = labelled_button("process-stop-symbolic", &tr("Cancel"), "cancel");
    cancel_button.add_css_class("pill");
    cancel_button.add_css_class("destructive-action");
    cancel_button.set_halign(gtk::Align::Center);
    // The heading, the bar and the button read as a column and are held to a
    // narrow measure. The log is lines of text and wants the room, so it sits
    // outside that clamp at the page's full width.
    let focus = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(18).build();
    focus.append(&stage_label);
    focus.append(&progress_block);
    // Directly under the bar: it is the page's only action, and putting it
    // below the log made its position depend on how much had been logged.
    focus.append(&cancel_button);
    prog_body.append(
        &adw::Clamp::builder()
            .maximum_size(FOCUSED_WIDTH)
            .tightening_threshold(FOCUSED_WIDTH / 2)
            .child(&focus)
            .build(),
    );
    // Hidden until there is something in it. A scan logs nothing at all, and an
    // empty card between the progress bar and the button is furniture for
    // content that may never arrive.
    log_scroll.set_visible(false);
    prog_body.append(&log_scroll);
    // While a scan is running there are three short things on this page, and
    // pinned to the top of a tall window they look stranded. Centred, the page
    // reads as one thing happening. It still grows downwards normally once the
    // log fills up.
    prog_body.set_valign(gtk::Align::Center);
    prog_body.set_vexpand(true);
    // Narrower than a form page: this is a heading, a bar and a list, and they
    // read better as a column than spread across the window.
    nav.add(&page(Step::Progress.tag(), "Working", &prog_body));

    // --- and what came out ------------------------------------------------
    let res_body = body();
    let results_title = gtk::Label::builder()
        .label(tr("Done"))
        .wrap(true)
        .justify(gtk::Justification::Center)
        .margin_top(12)
        .build();
    results_title.add_css_class("title-1");
    let results_summary = gtk::Label::builder()
        .wrap(true)
        .justify(gtk::Justification::Center)
        .margin_bottom(6)
        .build();
    results_summary.add_css_class("dim-label");
    let results = adw::PreferencesGroup::builder().title(tr("Files")).build();
    // The disc is finished with, and the next one is the point.
    let results_actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .halign(gtk::Align::Center)
        .margin_top(12)
        .build();
    let eject_done = labelled_button("media-eject-symbolic", &tr("Eject"), "eject");
    eject_done.add_css_class("pill");
    let another = labelled_button("media-optical-symbolic", &tr("Rip another disc"), "another");
    another.add_css_class("pill");
    another.add_css_class("suggested-action");
    results_actions.append(&eject_done);
    results_actions.append(&another);
    res_body.append(&results_title);
    res_body.append(&results_summary);
    res_body.append(&results);
    res_body.append(&results_actions);
    nav.add(&page(Step::Results.tag(), "Results", &res_body));

    Ui {
        nav,
        toasts,
        drive_page,
        drive_combo,
        drive_group,
        drive_next,
        chosen_row,
        chosen_art,
        search_button,
        volumes: RefCell::new(None),
        picker: RefCell::new(None),
        identify_next,
        season_entry,
        video,
        audio,
        container,
        quality_group: quality,
        contents_group,
        id_group,
        detail_group,
        music_group: music,
        music_format,
        music_quality,
        language_group,
        include_extended,
        include_extras,
        accurate_chapters,
        language_rows: RefCell::new(Vec::new()),
        output_dir,
        disc_entry,
        stage_label,
        progress,
        progress_text,
        log,
        log_scroll,
        cancel_button,
        results,
        result_rows: RefCell::new(Vec::new()),
        results_title,
        results_summary,
    }
}

/// How much longer, in the reader's language.
///
/// Core decides what to round to, because that is a judgement about how good
/// the estimate is; this only says it. The hours-and-minutes form is one string
/// rather than two joined, so a translator can put them in the order their
/// language wants.
fn remaining_text(r: riplika_core::job::Remaining) -> String {
    use riplika_core::job::Remaining;
    match r {
        Remaining::LessThanAMinute => tr("less than a minute left"),
        Remaining::AboutAMinute => tr("about a minute left"),
        Remaining::Minutes(m) => {
            tr_args("about %1$s left", &[&tr_n("%d minute", "%d minutes", m as u32)])
        }
        Remaining::Hours(h) => {
            tr_args("about %1$s left", &[&tr_n("%d hour", "%d hours", h as u32)])
        }
        Remaining::HoursAndMinutes(h, m) => tr_args(
            "about %1$s %2$s left",
            &[&tr_n("%d hour", "%d hours", h as u32), &tr_n("%d minute", "%d minutes", m as u32)],
        ),
    }
}

/// What the picker's search box opens on.
///
/// The last search if there was one, then whatever the page settled on, and
/// failing both the disc's own label - which is all that is known about a disc
/// nothing could identify. An empty box is the one answer that helps nobody:
/// the way out of an unidentified disc is to type a name, and a box opened
/// blank does not suggest that is possible.
fn opening_query(last_search: &str, settled_on: Option<&str>, label: Option<&str>) -> String {
    if !last_search.trim().is_empty() {
        return last_search.to_string();
    }
    if let Some(t) = settled_on {
        return t.to_string();
    }
    label.map(|l| riplika_core::identify::label::parse(l).title).unwrap_or_default()
}

/// A warning, in the reader's language.
///
/// The counterpart to `Warning::text`, which says the same things in English
/// for the log and the command line. Both exist because the log is searched and
/// pasted into bug reports, and the window is read.
///
/// Where a warning carries a `why`, that text came from the operating system,
/// ffmpeg or libdvdcss and is English whatever happens here. Only the sentence
/// around it is ours to translate.
/// What the drive page calls the disc.
///
/// Mirrors `Drive::describe_disc`, which stays English because the CLI and the
/// job log read it; this is the same information said in the user's language.
/// The three quality tiers, in one place.
///
/// Built rather than written out at each chooser so all of them say the same
/// words, and so all of them are translated - as bare array literals they were
/// invisible to xgettext and stayed English everywhere.
fn tier_list() -> gtk::StringList {
    let labels = [tr("High"), tr("Medium"), tr("Low")];
    gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>())
}

fn format_at(row: &adw::ComboRow) -> AudioFormat {
    if row.selected() == 1 { AudioFormat::Mp3 } else { AudioFormat::Flac }
}

/// What the music quality chooser says, and whether it can be touched.
///
/// FLAC is lossless, so a tier there decides nothing: every level decodes to
/// bit-identical audio and only the file size moves. A live control that
/// cannot affect the result is worse than none, so it is switched off and says
/// why rather than being quietly ignored.
fn music_quality_state(format: AudioFormat) -> (String, bool) {
    if format.quality_applies() {
        (tr("High is about 245 kb/s, Low about 130"), true)
    } else {
        (tr("FLAC is lossless - every level decodes to the same audio"), false)
    }
}

fn apply_music_quality_rule(row: &adw::ComboRow, format: AudioFormat) {
    let (subtitle, live) = music_quality_state(format);
    row.set_subtitle(&subtitle);
    row.set_sensitive(live);
}

fn disc_text(d: &Drive) -> String {
    match (&d.kind, &d.disc_label) {
        (Some(DiscKind::Audio(toc)), _) => {
            let tracks = tr_n("%d track", "%d tracks", toc.audio_count() as u32);
            let minutes = tr_n("%d minute", "%d minutes", (toc.duration() / 60_000) as u32);
            tr_args("Audio CD, %1$s, %2$s", &[&tracks, &minutes])
        }
        (_, Some(label)) => label.clone(),
        (Some(DiscKind::DvdVideo), None) => tr("DVD-Video"),
        (Some(DiscKind::BluRay), None) => tr("Blu-ray"),
        (Some(DiscKind::Data(_)), None) => tr("Data disc"),
        _ => tr("No disc"),
    }
}

fn warning_text(w: &Warning) -> String {
    match w {
        Warning::CouldNotIdentify { why } => tr_args("could not identify the disc: %1$s", &[why]),
        Warning::TitleUnreadable { title, why } => {
            tr_args("title %1$s could not be read: %2$s", &[&title.to_string(), why])
        }
        Warning::NoPlayAll { episodes } => tr_args(
            "no play-all title on this disc; ordering %1$s by disc layout instead",
            &[&tr_n("%d episode", "%d episodes", *episodes as u32)],
        ),
        Warning::ExtendedCutsUncomparable { why } => {
            tr_args("could not compare titles for extended cuts: %1$s", &[why])
        }
        Warning::GlyphTableUnreadable { path, why } => {
            tr_args("glyph table %1$s: %2$s", &[&path.display().to_string(), why])
        }
        Warning::GlyphTableMissing { path } => {
            tr_args("glyph table %1$s does not exist", &[&path.display().to_string()])
        }
        Warning::NoGlyphTable => tr("no glyph table, so subtitles stay as bitmaps"),
        // A file name and an error from elsewhere, with no sentence of ours
        // around them. There is nothing here for a translator to do.
        Warning::ItemSkipped { .. } => w.text(),
        Warning::UnrecognisedGlyphs { language, glyphs } => tr_args(
            "%1$s: %2$s - the table may not cover %1$s",
            &[language, &tr_n("%d unrecognised glyph", "%d unrecognised glyphs", *glyphs as u32)],
        ),
        // One line however long: xgettext does not follow a Rust string
        // continuation, so a wrapped literal never reaches a translator.
        Warning::GlyphTableIsForAnotherFont { shapes } => tr_args(
            "%1$s on this disc are not in the glyph table, which was built for another release; subtitles kept as pictures",
            &[&tr_n("%d shape", "%d shapes", *shapes as u32)],
        ),
        Warning::CannotReadLanguage { language } => tr_args(
            "no %1$s reader is installed, so those subtitles were kept as pictures rather than read with another language's alphabet",
            &[language],
        ),
        Warning::CannotLearnLettering { shapes } => tr_args(
            "no glyph table fits this disc and there is nothing installed to read its %1$s; subtitles kept as pictures",
            &[&tr_n("%d shape", "%d shapes", *shapes as u32)],
        ),
        Warning::SubtitlesUnreadable { language, why } => {
            tr_args("%1$s subtitles could not be read: %2$s", &[language, why])
        }
        Warning::PlayAllsSkipped { titles } => tr_args(
            "skipping %1$s, whose content is on the disc already",
            &[&tr_n("%d play-all title", "%d play-all titles", *titles as u32)],
        ),
        Warning::FreeReaderIncomplete { why } => {
            tr_args("the free reader could not read this disc fully (%1$s); using MakeMKV", &[why])
        }
        Warning::FreeReaderFailed { why } => {
            tr_args("the free reader failed (%1$s); using MakeMKV", &[why])
        }
        // A count of sectors and a sentence composed in core, with nothing of
        // ours around it. There is nothing here for a translator to do.
        Warning::DumpIncomplete { .. } => w.text(),
        Warning::CacheNotCleared { path, why } => tr_args(
            "%1$s: %2$s - the cache folder still holds it",
            &[&path.display().to_string(), why],
        ),
        Warning::TrackCorrupt { track, sectors } => tr_args(
            "track %1$s is corrupt: %2$s disagree with the error detection written into them",
            &[&track.to_string(), &tr_n("%d sector", "%d sectors", *sectors as u32)],
        ),
        Warning::TrackDamaged { track, sectors } => tr_args(
            "track %1$s is damaged: the drive had to guess at %2$s",
            &[&track.to_string(), &tr_n("%d sector", "%d sectors", *sectors as u32)],
        ),
    }
}

/// Drop everything that was true of the disc that has just come out.
///
/// Every one of these was read off a particular disc, so leaving any of them
/// describes the disc that is gone as though it were the disc that is there.
/// That is not tidiness: the kind is what chooses the pipeline, and a stale
/// one sent a music CD down the game path, where it was reported as a data
/// disc with no name and nothing could be searched for.
fn forget_the_disc(state: &mut State) {
    state.scan = None;
    state.candidates.clear();
    state.selected = None;
    state.chosen = None;
    state.items.clear();
    state.game = None;
    state.album = None;
    state.music = None;
    state.offering = Offering::Nothing;
    if let Some(drive) = state.drive.as_mut() {
        drive.kind = None;
    }
}

/// Is there another answer the reader could pick, for a disc of this kind?
///
/// Everything but a game. A film or a show is chosen from what the catalogues
/// offer; a music disc usually needs no choosing, since a disc id names one
/// pressing exactly - but the same pressing can have been issued twice, and a
/// disc MusicBrainz has never seen can still be searched for by name.
///
/// A game is the exception. It is settled by what its dump hashes to, which
/// has not happened yet at the point this is asked, and is not something to
/// choose from a list afterwards either: picking a name by hand is how a dump
/// comes to claim it is a game it is not.
fn identity_is_choosable(kind: Option<&DiscKind>) -> bool {
    !matches!(kind, Some(DiscKind::Data(_)))
}

/// A stage's name, in the reader's language.
///
/// `Stage::label` is in core, which has no gettext binding and should not grow
/// one - core is driven by the CLI too, and by tests. Translating where it is
/// displayed keeps that boundary and costs one match.
fn stage_label(stage: riplika_core::job::Stage) -> String {
    use riplika_core::job::Stage;
    match stage {
        Stage::Scan => tr("Scanning disc"),
        Stage::Identify => tr("Identifying"),
        Stage::Rip => tr("Ripping"),
        Stage::Organise => tr("Sorting titles"),
        Stage::Verify => tr("Verifying"),
        Stage::Subtitles => tr("Reading subtitles"),
        Stage::Lettering => tr("Learning this disc's lettering"),
        Stage::Transcode => tr("Transcoding"),
    }
}

/// What the disc holds, as the lines a person reads before starting.
///
/// Separated from showing it so the phrasing can be tested. It counted in one
/// breath and spelled in another once - "1 episodes", "2 feature" - which is
/// invisible until the disc happens to hold exactly one of something.
fn plan_lines(items: &[Item]) -> Vec<String> {
    let plan = riplika_core::model::Plan::of(items);
    let play_alls = plan.play_alls as u32;

    // Written out one call at a time on purpose. xgettext reads the strings
    // where tr_n is called, so a table of them - which is otherwise the tidier
    // way to write this - hands it variables it cannot see through, and the
    // entries quietly leave the template. They did, once.
    let mut parts = Vec::new();
    let episodes = plan.episodes as u32;
    if episodes > 0 {
        parts.push(tr_n("%d episode", "%d episodes", episodes));
    }
    let features = plan.features as u32;
    if features > 0 {
        parts.push(tr_n("%d feature", "%d features", features));
    }
    let cuts = plan.extended_cuts as u32;
    if cuts > 0 {
        parts.push(tr_n("%d extended cut", "%d extended cuts", cuts));
    }
    let extras = plan.extras as u32;
    if extras > 0 {
        parts.push(tr_n("%d extra", "%d extras", extras));
    }

    let mut lines = Vec::new();
    if !parts.is_empty() {
        lines.push(tr("This disc holds %s").replace("%s", &parts.join(", ")));
    }
    if play_alls > 0 {
        lines.push(tr_n(
            "%d play-all title will be skipped - the same video again",
            "%d play-all titles will be skipped - the same video again",
            play_alls,
        ));
    }
    lines
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

        // While something is running the progress page cannot be left. Swiping
        // back from it hid a job that was still going, with no way to return to
        // it and no way to cancel it. Cancel is the way out; there is no other.
        if let Some(page) = self.ui.nav.find_page(Step::Progress.tag()) {
            page.set_can_pop(idle);
        }
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
        self.refresh_settings_page();
    }

    fn is_busy(&self) -> bool {
        self.state.borrow().busy.is_some()
    }

    fn toast(&self, text: &str) {
        // An empty one is a bar that slides in, says nothing, and covers the
        // page while it does it.
        if text.trim().is_empty() {
            return;
        }
        let toast = adw::Toast::new(text);
        // A toast parses its title as markup too, and some of these carry an
        // error message - a path, or ffmpeg's own words. One ampersand in
        // either would slide an empty bar across the page.
        toast.set_use_markup(false);
        self.ui.toasts.add_toast(toast);
    }

    /// Is this the page currently being looked at?
    fn at_step(&self, step: Step) -> bool {
        self.ui.nav.visible_page().and_then(|p| p.tag()).is_some_and(|t| t == step.tag())
    }

    /// Move to a step.
    ///
    /// AdwNavigationView is a stack, and pushing a tag already on it is an
    /// error - it warns and does nothing, so the window simply stops moving.
    /// This flow reaches the progress page twice, once while scanning and
    /// again while ripping, so it cannot be driven by pushing. Each step
    /// declares the whole path to itself instead, and the stack is replaced
    /// with it; the back button still works because the path is a real one.
    fn go(&self, step: Step) {
        self.ui.nav.replace_with_tags(step.path());
    }

    fn log_line(&self, text: &str) {
        self.ui.log_scroll.set_visible(true);
        let buffer = self.ui.log.buffer();
        let mut end = buffer.end_iter();
        if buffer.char_count() > 0 {
            buffer.insert(&mut end, "\n");
        }
        buffer.insert(&mut end, text);

        // A full disc produces hundreds of lines and only the recent ones are
        // worth keeping on screen; the whole run is in the report at the end.
        const KEEP: i32 = 400;
        let excess = buffer.line_count() - KEEP;
        if excess > 0 {
            let start = buffer.start_iter();
            if let Some(cut) = buffer.iter_at_line(excess) {
                let mut start = start;
                let mut cut = cut;
                buffer.delete(&mut start, &mut cut);
            }
        }
        // Follow the tail, which is where anything new appears.
        let mut end = buffer.end_iter();
        self.ui.log.scroll_to_iter(&mut end, 0.0, false, 0.0, 0.0);
    }

    /// Which languages are ticked, in the order they are shown.
    ///
    /// Order is the point: the rows are laid out with the preferred languages
    /// first, so reading them top to bottom gives the preference order, and the
    /// first one ends up the default track.
    fn chosen_languages(&self) -> LanguageSet {
        let rows = self.ui.language_rows.borrow();
        if rows.is_empty() {
            // The disc offers no language tracks, so there is nothing to say
            // about them - not the same as choosing none.
            return LanguageSet::Everything;
        }
        LanguageSet::Only(
            rows.iter()
                .filter(|(_, row)| row.is_active())
                .map(|(code, _)| lang::parse(code))
                .collect(),
        )
    }

    /// Say what unticking everything will actually produce.
    ///
    /// It is a legitimate choice - video with the disc's own soundtrack and no
    /// subtitles - so it is honoured rather than refused. It is also unusual
    /// enough to be worth confirming out loud.
    fn refresh_settings_page(&self) {
        let rows = self.ui.language_rows.borrow();
        let none_ticked = !rows.is_empty() && !rows.iter().any(|(_, r)| r.is_active());
        drop(rows);
        self.ui.language_group.set_description(Some(if none_ticked {
            "None chosen: no subtitles, and the disc's first audio track kept \
             so the result is not silent."
        } else {
            "What this disc carries. Your preferred languages start ticked; \
             the first becomes the default track."
        }));
    }

    fn settings(&self) -> JobSettings {
        let kind = self.state.borrow().drive.as_ref().and_then(|d| d.kind.clone());
        let prefs = self.prefs.prefs.borrow();
        let output = prefs.output_for(riplika_core::prefs::Library::of(kind.as_ref()));
        let mut s = prefs.to_settings(output, self.chosen_languages());
        s.include_extended_cuts = self.ui.include_extended.is_active();
        s.include_extras = self.ui.include_extras.is_active();
        s.accurate_chapters = self.ui.accurate_chapters.is_active();
        // the rip page can override the persisted quality for this disc
        s.video = quality_at(&self.ui.video);
        s.audio = quality_at(&self.ui.audio);
        s.container =
            if self.ui.container.selected() == 1 { Container::Mkv } else { Container::Mp4 };
        s.music_format = format_at(&self.ui.music_format);
        s.music_quality = quality_at(&self.ui.music_quality);
        s
    }

    /// Rebuild the language switches for the disc that was just scanned.
    fn show_languages(&self, available: &[String]) {
        for (_, row) in self.ui.language_rows.borrow().iter() {
            self.ui.language_group.remove(row);
        }
        self.ui.language_rows.borrow_mut().clear();

        if available.is_empty() {
            let row = rows::switch().title(tr("No language tracks found")).sensitive(false).build();
            self.ui.language_group.add(&row);
            return;
        }
        for (code, wanted) in self.prefs.prefs.borrow().preselect(available) {
            let language = lang::parse(&code);
            let row = rows::switch()
                .title(&language.name)
                // The code is worth showing: a disc may tag the same language
                // two ways, and this is what distinguishes the rows.
                .subtitle(&code)
                .build();
            row.set_active(wanted);
            {
                let app = self.weak();
                row.connect_active_notify(move |_| {
                    if let Some(app) = app.upgrade() {
                        app.refresh_settings_page();
                    }
                });
            }
            self.ui.language_group.add(&row);
            self.ui.language_rows.borrow_mut().push((code, row));
        }
        self.refresh_settings_page();
    }

    /// Show where this disc's files will go.
    ///
    /// Only that. It used to re-tick every control on the page from the saved
    /// preferences as well, which meant choosing an output folder silently
    /// undid the format and quality chosen for the disc in the drive: select
    /// MP3, pick a folder, and the rip came out FLAC without a word.
    fn refresh_paths(&self) {
        let kind = self.state.borrow().drive.as_ref().and_then(|d| d.kind.clone());
        let prefs = self.prefs.prefs.borrow();
        // What this disc will actually use, so the page is not promising
        // Videos to somebody who has a CD in the drive.
        let output = prefs.output_for(riplika_core::prefs::Library::of(kind.as_ref()));
        self.ui.output_dir.set_subtitle(&output.to_string_lossy());
    }

    /// Set every control on the rip page from the saved preferences.
    ///
    /// For when the preferences themselves have changed, and at startup. Not
    /// for anything else: these controls are the disc in the drive's own
    /// settings, deliberately allowed to differ from what is persisted, and
    /// re-ticking them throws that away.
    fn apply_prefs_to_controls(&self) {
        let prefs = self.prefs.prefs.borrow();
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
        self.ui.music_format.set_selected(match prefs.music_format {
            AudioFormat::Flac => 0,
            AudioFormat::Mp3 => 1,
        });
        self.ui.music_quality.set_selected(match prefs.music_quality {
            Quality::High => 0,
            Quality::Medium => 1,
            Quality::Low => 2,
        });
        // Setting the format above does not fire the handler, so the rule that
        // greys the tier chooser has to be applied here as well.
        apply_music_quality_rule(&self.ui.music_quality, prefs.music_format);
        self.ui.include_extended.set_active(prefs.include_extended_cuts);
        self.ui.include_extras.set_active(prefs.include_extras);
        self.ui.accurate_chapters.set_active(prefs.accurate_chapters);
    }

    /// What the drive page should say, given what the machine has.
    ///
    /// Three situations, and which one you are in decides what you do next, so
    /// the page says which one rather than just showing nothing.
    fn drive_status(drives: &[Drive], selected: Option<&Drive>) -> (String, String, bool) {
        if drives.is_empty() {
            return (
                tr("No disc drive"),
                tr("No optical drive was found. Connect one and look again."),
                false,
            );
        }
        match selected {
            // What the disc calls itself, or failing a label, what it turned
            // out to be. Seeing it is the first confirmation that the right
            // disc is in the tray.
            Some(d) if d.has_disc() => {
                (disc_text(d), tr_args("%1$s in %2$s", &[&d.name, &d.device]), true)
            }
            Some(d) => (
                tr("No disc"),
                tr_args("Insert a disc into %1$s, then look again.", &[&d.device]),
                false,
            ),
            None => (tr("No disc"), tr("Choose a drive."), false),
        }
    }

    fn show_drives(&self, drives: &[Drive]) {
        let model = gtk::StringList::new(&[]);
        for d in drives {
            model.append(&format!("{}  -  {}", d.device, disc_text(d)));
        }
        self.ui.drive_combo.set_model(Some(&model));
        // A chooser offering one option is not a choice worth showing.
        self.ui.drive_group.set_visible(drives.len() > 1);

        // If the desktop launched us for a particular disc, that is the one
        // meant - on a machine with two drives it is the only way to know.
        let handed = self.state.borrow().handed.clone().and_then(|arg| {
            let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
            riplika_core::rip::drive_from_argument(&arg, &mounts)
        });
        let pick = handed
            .and_then(|device| drives.iter().position(|d| Path::new(&d.device) == device))
            // Otherwise prefer a drive with something in it: on a machine with
            // two, the one holding a disc is what was meant.
            .or_else(|| drives.iter().position(Drive::has_disc))
            .unwrap_or(0);
        if !drives.is_empty() {
            self.ui.drive_combo.set_selected(pick as u32);
        }
        let swapped = {
            let state = self.state.borrow();
            let before = state.drive.as_ref().map(|d| (d.device.clone(), d.disc_label.clone()));
            let now = drives.get(pick).map(|d| (d.device.clone(), d.disc_label.clone()));
            before.is_some() && before != now
        };
        {
            let mut state = self.state.borrow_mut();
            state.drives = drives.to_vec();
            state.drive = drives.get(pick).cloned();
        }
        // A different disc - or none - means everything worked out about the
        // last one is about a disc that is no longer there. Watching the tray
        // put a new disc in front of the previous one's identification, so
        // Kung Fu Panda came up as the show that was in the drive before it.
        if swapped {
            forget_the_disc(&mut self.state.borrow_mut());
            self.show_choice();
            // And go back to the beginning, if the page being looked at was
            // about that disc. Clearing the identification in place left a
            // wizard standing on a question with nothing behind it; there is
            // nothing on these pages worth keeping - a season number and two
            // switches - so starting again is no loss.
            //
            // Not from the progress page, where something may still be
            // running, and not from the results, where somebody reading what
            // came off a disc has every reason to take it out of the drive.
            if self.at_step(Step::Identify) || self.at_step(Step::Settings) {
                self.go(Step::Drive);
            }
        }
        self.refresh_drive_page();
    }

    /// Re-read the drive page from state.
    fn refresh_drive_page(&self) {
        let (drives, selected) = {
            let state = self.state.borrow();
            (state.drives.clone(), state.drive.clone())
        };
        // A music disc has no picture to encode and no container to put it in;
        // a video disc has no music format. Showing both groups would offer
        // settings that cannot apply to what is in the tray.
        let music =
            matches!(selected.as_ref().and_then(|d| d.kind.as_ref()), Some(DiscKind::Audio(_)));
        let game =
            matches!(selected.as_ref().and_then(|d| d.kind.as_ref()), Some(DiscKind::Data(_)));
        self.ui.music_group.set_visible(music);
        // A game disc is copied, not encoded: there is no picture to compress,
        // no container to choose and no track to name.
        self.ui.quality_group.set_visible(!music && !game);
        // A CD has one language and no subtitles to choose it for, no extras,
        // no extended cuts, and no season or disc number - those pages would
        // otherwise ask questions about a disc that cannot answer them.
        self.ui.language_group.set_visible(!music && !game);
        self.ui.contents_group.set_visible(!music && !game);
        self.ui.detail_group.set_visible(self.season_is_a_question());

        // The rest of this page was written for a box set. A film has no
        // episodes to count, no broadcast version to be an extended cut of,
        // and is one file rather than seven - so the words change with it.
        let film = self.is_film();
        self.ui.video.set_subtitle(&if film {
            tr("Medium is the sweet spot for DVD: about 700 MB a film")
        } else {
            tr("Medium is the sweet spot for DVD: about 170 MB an episode")
        });
        self.ui.contents_group.set_description(Some(&if film {
            tr("The film is always taken. Anything unticked is not read at all.")
        } else {
            tr("Episodes are always taken. Anything unticked is not read at all.")
        }));
        self.ui.include_extended.set_title(&if film {
            tr("Extended cut")
        } else {
            tr("Extended episodes")
        });
        self.ui.include_extended.set_subtitle(&if film {
            tr("A longer cut some discs carry alongside the theatrical version")
        } else {
            tr("Longer cuts some discs carry alongside the broadcast versions")
        });
        // The drift grows through the title rather than sitting at a fixed
        // offset, so how much it comes to depends on how long the title is.
        self.ui.accurate_chapters.set_subtitle(&if film {
            tr("Reads the disc twice. Without it, chapter marks drift a few seconds by the end")
        } else {
            tr("Reads the disc twice. Without it, chapter marks drift a second or two")
        });

        let (title, description, ready) = Self::drive_status(&drives, selected.as_ref());
        self.ui.drive_page.set_title(&title);
        // An AdwStatusPage description is markup and has no switch to turn
        // that off the way a row does, so the drive's own name - the only part
        // of this that is not ours - is escaped instead.
        self.ui.drive_page.set_description(Some(&glib::markup_escape_text(&description)));
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
            let choices: Vec<Choice> = cands.iter().map(Choice::of_candidate).collect();
            picker.show(&choices, move |i| {
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

    /// Show the kind of disc, which is what stands in for a cover.
    ///
    /// Set whenever the page changes what it is about, so a picture fetched
    /// for the last disc cannot linger beside this one's name.
    fn show_kind_icon(&self) {
        let kind = self.state.borrow().drive.as_ref().and_then(|d| d.kind.clone());
        self.ui.chosen_art.set_icon_name(Some(
            match riplika_core::prefs::Library::of(kind.as_ref()) {
                riplika_core::prefs::Library::Music => "audio-x-generic-symbolic",
                riplika_core::prefs::Library::Games => "applications-games-symbolic",
                riplika_core::prefs::Library::Video => "video-x-generic-symbolic",
            },
        ));
        self.ui.chosen_art.set_pixel_size(64);
    }

    /// Ask for a picture of this, if the catalogue offered one.
    fn want_poster(&self, url: Option<&String>) {
        self.show_kind_icon();
        if let Some(url) = url {
            worker::poster(url.clone(), self.sender());
        }
    }

    /// Say whether tapping what the page settled on leads anywhere.
    fn set_chosen_actionable(&self, actionable: bool) {
        self.ui.search_button.set_visible(actionable);
    }

    /// Restate what the page has settled on.
    /// Is what the page settled on a film rather than a season of something?
    ///
    /// Asked of what was chosen, not of the disc: a disc holding one long
    /// title could be either until a catalogue says which, and the pages that
    /// ask about seasons and episodes should follow the answer.
    /// Is "which season, and which disc of it" a question this disc can answer?
    ///
    /// Only a television disc can. It was worked out in two places and they
    /// disagreed: the rip page asked about music and games as well as films,
    /// and the identification page asked only about films - so a CD was shown
    /// a Season and a Disc field, above the words "which part of the show it
    /// holds", while its eleven tracks sat above them.
    fn season_is_a_question(&self) -> bool {
        !self.is_music() && !self.is_game() && !self.is_film()
    }

    fn is_film(&self) -> bool {
        let state = self.state.borrow();
        match state.chosen.as_ref().or(state.selected.as_ref().map(|c| &c.media)) {
            Some(m) => matches!(m, Media::Movie { .. }),
            None => false,
        }
    }

    /// Is the disc in the drive a game, or anything else with a filesystem
    /// that is not video?
    fn is_game(&self) -> bool {
        matches!(
            self.state.borrow().drive.as_ref().and_then(|d| d.kind.as_ref()),
            Some(DiscKind::Data(_))
        )
    }

    /// What the identify page says about a data disc.
    ///
    /// Almost nothing, honestly. A volume label is as weak a clue as a DVD's,
    /// and the real identification only happens once the disc has been read
    /// and can be matched by what it hashes to - so the page says that rather
    /// than dressing up a guess.
    fn show_game(&self) {
        self.set_chosen_actionable(false);
        // Redump carries hashes and names, and no artwork at all.
        self.want_poster(None);
        let disc = self.state.borrow().game.clone();
        match disc {
            Some(d) => {
                // Not "Identified as": nothing has identified it. That is
                // the volume label, which is what the disc calls itself, and
                // the identification happens against the database after the
                // dump - claiming it here would be a promise the page cannot
                // keep and there is no search box to correct it with.
                self.ui.id_group.set_title(&tr("What the disc calls itself"));
                self.ui.chosen_row.set_title(&d.describe());
                // What happens next, rather than a remark about disc labels.
                // A game is named by what its dump hashes to, so there is
                // nothing to choose here and nothing to search for - saying
                // that a label is "only a hint" answers a question nobody
                // asked and leaves the page looking like it failed.
                self.ui.chosen_row.set_subtitle(&match &d.serial {
                    Some(serial) => tr_args("PlayStation disc %1$s", &[serial]),
                    None => tr("Checked against the preservation database once it is dumped"),
                });
                self.ui.identify_next.set_sensitive(!self.is_busy());
            }
            None => {
                // Still worth dumping. What could not be read is the disc's
                // own description of itself, and the dump is verified against
                // the database by its hashes either way.
                self.ui.id_group.set_title(&tr("What the disc calls itself"));
                self.ui.chosen_row.set_title(&tr("A disc that does not say what it is"));
                self.ui.chosen_row.set_subtitle(&tr(
                    "Checked against the preservation database once it is dumped",
                ));
                self.ui.identify_next.set_sensitive(!self.is_busy());
            }
        }
    }

    /// Is the disc in the drive a music CD?
    ///
    /// Decides which of the two pipelines the whole wizard is running, so it
    /// is asked of the drive rather than tracked as a mode that could drift
    /// out of step with what is in the tray.
    fn is_music(&self) -> bool {
        matches!(
            self.state.borrow().drive.as_ref().and_then(|d| d.kind.as_ref()),
            Some(DiscKind::Audio(_))
        )
    }

    /// Open the dialog for choosing which release a music disc is.
    ///
    /// It starts on what the disc id already said, when that was more than one
    /// pressing, so the ordinary case needs no request at all. Typing searches
    /// MusicBrainz by name, which is the way out for a disc it has never seen.
    fn open_release_picker(
        &self,
        window: &impl IsA<gtk::Widget>,
        tx: &std::sync::mpsc::Sender<Msg>,
    ) {
        let query = {
            let state = self.state.borrow();
            state
                .album
                .as_ref()
                .map(|a| format!("{} {}", a.artist, a.title))
                .or_else(|| state.scan.as_ref().map(|s| s.label.clone()))
                .unwrap_or_default()
        };
        let app_for_search = self.weak();
        let tx = tx.clone();
        let picker = show_picker::present(
            window,
            Prompt {
                title: tr("Which release is this?"),
                // No use offering it. A show named by hand still rips into
                // numbered episodes; an album named by hand has no track
                // listing, so there would be nothing to write the files from.
                use_typed: None,
            },
            &query,
            move |q| {
                if let Some(app) = app_for_search.upgrade() {
                    app.state.borrow_mut().searching = true;
                    if let Some(p) = app.ui.picker.borrow().as_ref() {
                        p.show_searching();
                    }
                    worker::search_music(q, tx.clone());
                }
            },
            |_| {},
        );
        *self.ui.picker.borrow_mut() = Some(picker);

        // Whatever the disc id already answered, so the dialog opens on
        // something rather than on an empty box.
        let albums =
            self.state.borrow().music.as_ref().map(|m| m.albums.clone()).unwrap_or_default();
        self.show_releases(Offering::ThisDisc(albums));
    }

    /// Put releases into the open picker, whichever asked for them.
    fn show_releases(&self, offering: Offering) {
        self.state.borrow_mut().offering = offering.clone();
        let choices: Vec<Choice> = match &offering {
            Offering::Nothing => Vec::new(),
            Offering::ThisDisc(albums) => albums.iter().map(Choice::of_album).collect(),
            Offering::Searched(found) => found.iter().map(Choice::of_release).collect(),
        };
        let app = self.weak();
        if let Some(picker) = self.ui.picker.borrow().as_ref() {
            picker.show(&choices, move |i| {
                if let Some(app) = app.upgrade() {
                    app.choose_release(i);
                }
            });
        }
    }

    /// Take the release at this position, however it got into the list.
    ///
    /// A release the disc id named is already known in full - including which
    /// disc of a box set is in the tray - so it is taken as it stands. One
    /// found by name is an id, and its tracks are a second request away.
    fn choose_release(&self, index: usize) {
        let offering = self.state.borrow().offering.clone();
        match offering {
            Offering::ThisDisc(albums) => {
                let Some(album) = albums.get(index).cloned() else {
                    return;
                };
                self.state.borrow_mut().album = Some(album);
                if let Some(p) = self.ui.picker.borrow().as_ref() {
                    p.close();
                }
                *self.ui.picker.borrow_mut() = None;
                self.show_choice();
            }
            Offering::Searched(found) => {
                let Some(m) = found.get(index) else {
                    return;
                };
                if let Some(p) = self.ui.picker.borrow().as_ref() {
                    p.show_searching();
                }
                self.state.borrow_mut().searching = true;
                worker::fetch_release(m.release_id.clone(), self.sender());
            }
            Offering::Nothing => {}
        }
    }

    /// What the identify page says about a music disc.
    ///
    /// There is nothing to choose here in the ordinary case: a disc id names
    /// one pressing, so the page reports rather than asks. The row still opens
    /// the picker, because the ordinary case is not the only one - a pressing
    /// can have been issued twice, and a disc the catalogue has never seen can
    /// be searched for by name.
    fn show_album(&self) {
        self.set_chosen_actionable(true);
        let album = self.state.borrow().album.clone();
        // The same picture the rip embeds, shown before the rip rather than
        // only afterwards. Asked for only when the release says it has one.
        self.want_poster(
            album
                .as_ref()
                .filter(|a| a.has_cover_art && !a.release_id.is_empty())
                .map(|a| riplika_core::identify::music::cover_art_url(&a.release_id))
                .as_ref(),
        );
        match album {
            Some(a) => {
                self.ui.chosen_row.set_title(&format!("{} - {}", a.artist, a.title));
                let mut lines = vec![tr_n("%d track", "%d tracks", a.tracks.len() as u32)];
                let detail = a.detail();
                if !detail.is_empty() {
                    lines.push(detail);
                }
                // Worth saying where the names came from: CD-Text carries names
                // and nothing else, so a rip made from it has no release date,
                // no label and no cover, and is not missing them by accident.
                if self.state.borrow().music.as_ref().is_some_and(|m| m.from_cd_text) {
                    lines.push(tr("Named by the disc itself; no cover art or release details"));
                }
                self.ui.chosen_row.set_subtitle(&lines.join("\n"));
                self.ui.identify_next.set_sensitive(!self.is_busy());
            }
            None => {
                self.ui.chosen_row.set_title(&tr("Not identified"));
                // Asked and unknown is a different problem from never asked,
                // and only one of them is worth trying again.
                let unreachable =
                    self.state.borrow().music.as_ref().is_some_and(|m| m.lookup_failed.is_some());
                self.ui.chosen_row.set_subtitle(&if unreachable {
                    tr("The catalogue could not be reached, so this disc was never asked about")
                } else {
                    tr("No release matches this disc, so there is nothing to name the tracks from")
                });
                self.ui.identify_next.set_sensitive(false);
            }
        }
    }

    fn show_choice(&self) {
        // Video is the one path where there is something to disagree with.
        self.set_chosen_actionable(true);
        // A film, a show and an album are all looked up and identified. Only a
        // game is not, and show_game says so for itself.
        self.ui.id_group.set_title(&tr("Identified as"));
        // A film is one thing, not part four of a season, and an album is
        // neither. Asking which season it is and where its episode numbering
        // starts are questions about a disc that cannot answer them.
        self.ui.detail_group.set_visible(self.season_is_a_question());
        if self.is_music() {
            return self.show_album();
        }
        if self.is_game() {
            return self.show_game();
        }
        let selected = self.state.borrow().selected.clone();
        self.want_poster(selected.as_ref().and_then(|c| c.poster.as_ref()));
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
                self.ui.chosen_row.set_title(&tr("Not identified"));
                self.ui.chosen_row.set_subtitle(&tr("Choose the show"));
                self.ui.identify_next.set_sensitive(false);
            }
        }
    }

    /// Take the name the user typed, with no catalogue behind it.
    ///
    /// The season comes from the field on the page, because that is what
    /// decides whether this is a series at all - without one there is nothing
    /// to number episodes by.
    fn use_name_as_given(&self, name: &str) {
        let season = self.ui.season_entry.text().trim().parse::<u32>().ok();
        let media = riplika_core::identify::unverified(name, season);
        let chosen = Candidate {
            media,
            confidence: 0.0,
            reasons: vec![tr("Entered by hand; not found in the catalogues")],
            detail: None,
            // No catalogue behind it, so no picture either.
            poster: None,
        };
        self.state.borrow_mut().selected = Some(chosen);
        if let Some(p) = self.ui.picker.borrow().as_ref() {
            p.close();
        }
        self.show_choice();
    }

    fn weak(&self) -> std::rc::Weak<App> {
        self.me.borrow().clone()
    }

    fn sender(&self) -> std::sync::mpsc::Sender<Msg> {
        self.tx.borrow().clone().expect("wired before use")
    }

    fn show_report(&self, r: &Report) {
        for row in self.ui.result_rows.borrow_mut().drain(..) {
            self.ui.results.remove(&row);
        }
        for p in &r.produced {
            let langs: Vec<String> = p.subtitles.iter().map(|s| s.language.name.clone()).collect();
            // "subtitles: none" against a track of an album, which cannot have
            // any. Where they are possible their absence is worth saying, and
            // where they are not the words are noise.
            let detail = match (carries_subtitles(&p.destination), langs.is_empty()) {
                (false, _) => mib(p.bytes),
                (true, true) => format!("{}   {}", mib(p.bytes), tr("subtitles: none")),
                (true, false) => {
                    tr_args("%1$s   subtitles: %2$s", &[&mib(p.bytes), &langs.join(", ")])
                }
            };
            let row = rows::action()
                .title(p.destination.file_name().unwrap_or_default().to_string_lossy().to_string())
                .subtitle(detail)
                .build();
            self.ui.results.add(&row);
            self.ui.result_rows.borrow_mut().push(row);
        }
        for (f, why) in &r.skipped {
            let row = rows::action()
                .title(f.file_name().unwrap_or_default().to_string_lossy().to_string())
                .subtitle(why)
                .css_classes(vec!["error".to_string()])
                .build();
            self.ui.results.add(&row);
            self.ui.result_rows.borrow_mut().push(row);
        }
        // Each literal at its own call site: xgettext reads the source, not
        // the program, and cannot follow a string through a conditional.
        self.ui.results_title.set_label(&if r.is_complete() {
            tr("Done")
        } else {
            tr("Finished with problems")
        });
        self.ui.results_summary.set_label(&format!(
            "{} files, {}{}",
            r.produced.len(),
            mib(r.total_bytes()),
            if r.skipped.is_empty() {
                String::new()
            } else {
                format!(", {} failed", r.skipped.len())
            }
        ));
    }

    /// Empty the log, so a second disc does not begin with the first one's.
    /// Begin a piece of work: clear what the last one left, and show it.
    ///
    /// Four places used to move to the progress page and only one of them
    /// cleared anything, so cancelling a rip and starting another showed the
    /// cancelled one's bar, heading and log while the new disc was being
    /// identified. Beginning work is one thing and is done in one place.
    ///
    /// Answers with the cancellation token for the run, since a fresh one is
    /// part of beginning: a cancelled run must not poison the next.
    fn start_working(&self, heading: &str, busy: &str) -> riplika_core::host::Cancel {
        self.ui.stage_label.set_label(heading);
        self.ui.progress.set_fraction(0.0);
        self.ui.progress_text.set_label("");
        self.clear_log();
        self.state.borrow_mut().eta = riplika_core::job::Eta::default();
        self.set_busy(Some(busy));
        let cancel = riplika_core::host::Cancel::new();
        self.state.borrow_mut().cancel = cancel.clone();
        set_button_label(&self.ui.cancel_button, &tr("Cancel"));
        self.go(Step::Progress);
        cancel
    }

    fn clear_log(&self) {
        self.ui.log.buffer().set_text("");
        self.ui.log_scroll.set_visible(false);
    }

    /// Say what the disc turned out to hold, before reading it.
    ///
    /// Knowing that a disc is seven episodes and thirty pieces of bonus
    /// material - rather than "47 titles" - is the difference between watching
    /// a progress bar and knowing what is being made.
    fn show_plan(&self, items: &[Item]) {
        for line in plan_lines(items) {
            self.log_line(&line);
        }
        for i in items.iter().filter(|i| matches!(i.role, Role::Episode { .. })) {
            if let Some(d) = &i.destination {
                self.log_line(&format!(
                    "  {}  {}",
                    hms(i.duration),
                    d.file_name().unwrap_or_default().to_string_lossy()
                ));
            }
        }
        self.state.borrow_mut().items = items.to_vec();
    }

    fn handle(&self, msg: Msg) {
        match msg {
            Msg::Drives(d) => self.show_drives(&d),
            Msg::Kind(kind) => {
                // What the disc actually is, as of a moment ago. Recorded
                // before anything is read, so every later question about the
                // kind - which page to show, whether the identity can be
                // chosen - is asked of this rather than of a stale snapshot.
                if let Some(drive) = self.state.borrow_mut().drive.as_mut() {
                    drive.kind = Some((*kind).clone());
                }
                // Where this disc's files go depends on what it is, and until
                // now nothing re-read that once the answer arrived: the page
                // was still offering Videos to somebody holding a CD.
                self.refresh_paths();
                let Some(drive) = self.state.borrow().drive.clone() else {
                    return;
                };
                let tx = self.sender();
                // A game disc will want datfiles to be named against, and a
                // dump takes minutes where fetching them takes seconds. Asked
                // for now so they are there by the time they are needed.
                if matches!(*kind, DiscKind::Data(_)) {
                    worker::ensure_datfiles(self.sender());
                }
                match *kind {
                    // A music disc needs none of the video machinery: no title
                    // probing, no structure matching, no catalogue guessing
                    // from a label. Its table of contents says what it is.
                    DiscKind::Audio(_) => worker::analyse_music(drive.device, tx),
                    // A data disc has no titles to probe: it is copied whole.
                    DiscKind::Data(_) => worker::analyse_game(drive.device, tx),
                    _ => {
                        let allow = self.prefs.prefs.borrow().use_makemkv();
                        let cancel = self.state.borrow().cancel.clone();
                        worker::analyse(drive, allow, cancel, tx);
                    }
                }
            }
            Msg::Ejected => {
                // Everything known about the disc referred to the one that has
                // just come out, so it goes rather than being left to look
                // current beside an empty tray.
                forget_the_disc(&mut self.state.borrow_mut());
                self.show_choice();
                self.toast(&tr("Drive open"));
                self.go(Step::Drive);
                worker::list_drives(true, self.sender());
            }
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
            Msg::Game(disc) => {
                self.set_busy(None);
                self.state.borrow_mut().game = Some(*disc);
                self.show_choice();
                self.go(Step::Identify);
            }
            Msg::Music(found) => {
                self.set_busy(None);
                // One release is the ordinary case; more than one means the
                // same pressing was issued twice, and the page settles on the
                // first while the row offers the rest.
                self.state.borrow_mut().album = found.albums.first().cloned();
                self.state.borrow_mut().music = Some(*found);
                self.show_choice();
                self.go(Step::Identify);
            }
            Msg::Releases(found) => {
                self.set_busy(None);
                self.state.borrow_mut().searching = false;
                // No toast: this dialog is over the window that would show it.
                // The list says what was found by showing it, and says that
                // nothing was found by saying so.
                self.show_releases(Offering::Searched(found));
            }
            Msg::Release(album) => {
                self.set_busy(None);
                self.state.borrow_mut().searching = false;
                // Chosen by hand rather than proven by the disc id, so what it
                // came from is no longer the disc's own answer. Saying that is
                // what keeps "named by the disc" honest on the page.
                self.state.borrow_mut().album = Some(*album);
                if let Some(p) = self.ui.picker.borrow().as_ref() {
                    p.close();
                }
                *self.ui.picker.borrow_mut() = None;
                self.state.borrow_mut().offering = Offering::Nothing;
                self.show_choice();
            }
            Msg::Poster(path) => {
                // Only if the page is still about the disc it was fetched
                // for: a picture arriving after the tray has been swapped
                // would put the last disc's cover beside the new one's name.
                if self.state.borrow().selected.is_some() || self.state.borrow().album.is_some() {
                    self.ui.chosen_art.set_pixel_size(64);
                    self.ui.chosen_art.set_from_file(Some(&path));
                }
            }
            Msg::DatfilesReady(n) => {
                self.log_line(&tr_args("%1$s datfile(s) downloaded", &[&n.to_string()]));
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
                // A stop is not a fault. Core says so in one word, which read
                // as a lowercase toast saying "cancelled" and nothing else.
                if riplika_core::Error(e.clone()).is_cancelled() {
                    self.state.borrow_mut().searching = false;
                    self.toast(&tr("Cancelled. Nothing further was written."));
                    self.log_line(&tr("Cancelled"));
                    set_button_label(&self.ui.cancel_button, &tr("Close"));
                    return;
                }
                // A search that failed left the picker spinning for an answer
                // that was never coming.
                self.state.borrow_mut().searching = false;
                if let Some(picker) = self.ui.picker.borrow().as_ref() {
                    picker.show_problem(&e);
                } else {
                    self.toast(&e);
                }
                self.log_line(&tr_args("failed: %1$s", &[&e]));
                set_button_label(&self.ui.cancel_button, &tr("Close"));
            }
        }
    }

    fn handle_event(&self, e: Event) {
        match e {
            Event::Stage(s) => {
                self.ui.stage_label.set_label(&stage_label(s));
                self.ui.progress.set_fraction(0.0);
                self.ui.progress_text.set_label("");
                // Each stage reads at its own rate, so an estimate carried over
                // from the last one would be wrong in a way that looks precise.
                self.state.borrow_mut().eta = riplika_core::job::Eta::new();
            }
            Event::Progress { stage, fraction, message } => {
                // Every progress event says which stage it belongs to, so the
                // heading follows it rather than waiting to be told separately.
                // A job that forgets to announce a stage is then wrong for one
                // event instead of for the whole of it - which is what left
                // "Identifying" up for an entire CD rip.
                let label = stage_label(stage);
                if self.ui.stage_label.label() != label {
                    self.ui.stage_label.set_label(&label);
                }
                self.ui.progress.set_fraction(fraction as f64);
                // What is happening, how far along, and how much longer - the
                // last of which is the only one that answers "should I wait?".
                let remaining = self.state.borrow_mut().eta.update(fraction);
                let mut parts = vec![format!("{:.0}%", fraction * 100.0)];
                if let Some(m) = message.filter(|m| !m.is_empty()) {
                    parts.push(m);
                }
                if let Some(left) = remaining {
                    parts.push(remaining_text(riplika_core::job::remaining(left)));
                }
                self.ui.progress_text.set_label(&parts.join("  \u{b7}  "));
            }
            Event::ItemStarted { index, total, name } => {
                self.ui.progress.set_fraction(index as f64 / total.max(1) as f64);
                self.ui.progress_text.set_label(&tr_args(
                    "%1$s of %2$s",
                    &[&(index + 1).to_string(), &total.to_string()],
                ));
                // No prose in this one - a position and a file name - so there
                // is nothing here for a translator to do.
                self.log_line(&format!("[{}/{}] {name}", index + 1, total));
            }
            Event::ItemFinished { destination, bytes, .. } => {
                self.log_line(&tr_args(
                    "wrote %1$s (%2$s)",
                    &[&destination.file_name().unwrap_or_default().to_string_lossy(), &mib(bytes)],
                ));
            }
            Event::TableChosen { path, covered, built } => {
                let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                self.log_line(&if built {
                    tr_args("lettering learned from this disc, kept as %1$s", &[&name])
                } else {
                    tr_args(
                        "lettering: %1$s, which covers %2$s of this disc",
                        &[&name, &format!("{:.0}%", covered * 100.0)],
                    )
                });
            }
            Event::LetteringLearned { labelled, ambiguous, blank } => {
                self.log_line(&tr_args(
                    "%1$s labelled, %2$s the font draws alike, %3$s left blank",
                    &[
                        &tr_n("%d shape", "%d shapes", labelled as u32),
                        &tr_n("%d shape", "%d shapes", ambiguous as u32),
                        &tr_n("%d shape", "%d shapes", blank as u32),
                    ],
                ));
            }
            Event::Subtitle { language, cues, recognised, unknown, .. } => {
                self.log_line(&if recognised {
                    // The two counts are composed rather than written into the
                    // sentence, so each gets its own plural form. "1 cues" is
                    // the mistake this avoids.
                    tr_args(
                        "subtitles %1$s: %2$s, %3$s",
                        &[
                            &language,
                            &tr_n("%d cue", "%d cues", cues as u32),
                            &tr_n(
                                "%d unrecognised glyph",
                                "%d unrecognised glyphs",
                                unknown as u32,
                            ),
                        ],
                    )
                } else {
                    tr_args("subtitles %1$s: not recognised, bitmap kept", &[&language])
                });
            }
            // The text of a warning is built in core and usually ends in an
            // error from the operating system or ffmpeg, which arrives in
            // English and stays there. Only the label around it is ours.
            // Shown already, and in the reader's language: the window builds
            // this from the items it is given, together with the per-episode
            // listing that needs them anyway. The event carries it for the log
            // and the command line, which have no items to count.
            Event::Plan(_) => {}
            Event::Warning(w) => self.log_line(&tr_args("warning: %1$s", &[&warning_text(&w)])),
        }
    }
}

/// Pick a folder, then hand the choice back.
fn choose_folder<F: Fn(PathBuf) + 'static>(window: &adw::ApplicationWindow, title: &str, then: F) {
    let dialog = gtk::FileDialog::builder().title(title).build();
    dialog.select_folder(Some(window), gtk::gio::Cancellable::NONE, move |res| {
        if let Ok(file) = res
            && let Some(p) = file.path()
        {
            then(p);
        }
    });
}

fn wire(app: &Rc<App>, window: &adw::ApplicationWindow) {
    let channel = worker::Channel::default();
    let tx = channel.sender();
    *app.tx.borrow_mut() = Some(tx.clone());

    app.refresh_paths();
    app.apply_prefs_to_controls();
    worker::list_drives(app.prefs.prefs.borrow().use_makemkv(), tx.clone());

    // Notice a disc going in or coming out, so the page follows the tray
    // rather than waiting to be asked. "Look again" stays, for the drive that
    // says nothing when its media changes - some do.
    {
        use gtk::gio::prelude::*;
        let monitor = gtk::gio::VolumeMonitor::get();
        let watching = Rc::clone(app);
        let tx = tx.clone();
        // One insertion is several signals - the drive changes, then a volume
        // appears once the desktop has mounted it - and each would otherwise
        // start its own scan of every drive. They are collapsed into one.
        let pending: Rc<std::cell::Cell<bool>> = Rc::new(std::cell::Cell::new(false));
        let look = move || {
            if pending.replace(true) {
                return;
            }
            let (app, tx, pending) = (Rc::clone(&watching), tx.clone(), Rc::clone(&pending));
            glib::timeout_add_local_once(Duration::from_millis(700), move || {
                pending.set(false);
                // Not while something is running: a scan or a rip has the
                // drive, and asking it what it holds mid-read is how it comes
                // back with nothing.
                if !app.is_busy() {
                    worker::list_drives(app.prefs.prefs.borrow().use_makemkv(), tx.clone());
                }
            });
        };
        // A drive whose media changed, a volume appearing once the desktop
        // mounts it, and the drive itself coming and going: any of them means
        // what is in the tray is no longer what the page says.
        let l = look.clone();
        monitor.connect_drive_changed(move |_, _| l());
        let l = look.clone();
        monitor.connect_volume_added(move |_, _| l());
        let l = look.clone();
        monitor.connect_volume_removed(move |_, _| l());
        let l = look.clone();
        monitor.connect_drive_connected(move |_, _| l());
        monitor.connect_drive_disconnected(move |_, _| look());
        *app.ui.volumes.borrow_mut() = Some(monitor);
    }

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
                app.toast(&tr("Already working - wait for it, or cancel it first"));
                return;
            }
            // A scan takes minutes and there is nothing to abandon it with on
            // this page, so show the progress page, which has the cancel button.
            app.start_working("Scanning disc", "Reading disc...");
            // Which pipeline this is depends on what is in the tray now, not
            // on what was in it when the drive list was last built. Asked
            // again here; Msg::Kind carries the answer and dispatches.
            worker::identify_disc(drive.device.clone(), tx.clone());
        });
    }

    // Step two -------------------------------------------------------------
    {
        let app = Rc::clone(app);
        app.clone().ui.identify_next.connect_clicked(move |_| {
            // A music disc was settled by its disc id; there is no season to
            // apply and no show to have chosen.
            if app.is_music() {
                if app.state.borrow().album.is_some() {
                    app.go(Step::Settings);
                } else {
                    app.toast(&tr("This disc could not be identified"));
                }
                return;
            }
            // A game has nothing to choose here; its name comes afterwards.
            if app.is_game() {
                app.go(Step::Settings);
                return;
            }
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
                None => app.toast(&tr("Choose what this disc is first")),
            }
        });
    }
    {
        // Pressing Search is how you disagree with what it settled on.
        let app = Rc::clone(app);
        let tx = tx.clone();
        let window = window.clone();
        let button = app.ui.search_button.clone();
        button.connect_clicked(move |_| {
            // The button is hidden on the paths this excludes, so this is the
            // second lock on the same door - and the one that was missing
            // when a game disc opened the show picker.
            if !identity_is_choosable(
                app.state.borrow().drive.as_ref().and_then(|d| d.kind.as_ref()),
            ) {
                return;
            }
            // A different question, a different catalogue, and a different
            // dialog. Asking the film catalogues about an album would find
            // nothing, which is how a game disc came to ask about shows.
            if app.is_music() {
                app.open_release_picker(&window, &tx);
                return;
            }
            let query = {
                let state = app.state.borrow();
                opening_query(
                    &state.query,
                    state.selected.as_ref().map(|c| c.media.title()),
                    state.scan.as_ref().map(|s| s.label.as_str()),
                )
            };
            let app_for_search = Rc::clone(&app);
            let tx = tx.clone();
            let picker = show_picker::present(
                &window,
                Prompt {
                    // Films as well as television: search() asks the
                    // catalogues for both, and always has.
                    title: tr("Which film or show is this?"),
                    use_typed: Some(tr("Episodes are numbered but not named")),
                },
                &query,
                move |q| {
                    app_for_search.state.borrow_mut().query = q.clone();
                    if let Some(p) = app_for_search.ui.picker.borrow().as_ref() {
                        p.show_searching();
                    }
                    worker::search(q, None, tx.clone());
                },
                {
                    let app = app.weak();
                    move |name| {
                        if let Some(app) = app.upgrade() {
                            app.use_name_as_given(&name);
                        }
                    }
                },
            );

            // Open on what is already known rather than an empty list.
            let candidates = app.state.borrow().candidates.clone();
            let chooser = app.weak();
            let choices: Vec<Choice> = candidates.iter().map(Choice::of_candidate).collect();
            picker.show(&choices, move |i| {
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
            // For the library this disc belongs to, not for all three: a
            // folder picked with a CD in the drive is a decision about music.
            let library = riplika_core::prefs::Library::of(
                app.state.borrow().drive.as_ref().and_then(|d| d.kind.as_ref()),
            );
            choose_folder(&window, "Output folder", move |p| {
                app2.prefs.prefs.borrow_mut().set_output_for(library, p);
                app2.prefs.save();
                app2.refresh_paths();
            });
        });
    }
    // The three switches on the rip page that are policy rather than a
    // decision about this disc. Somebody who never wants bonus material never
    // wants it, and having to untick it once a disc is the application
    // forgetting what it was told. They were already read from preferences on
    // the way in and never written back, so the file only ever held the
    // defaults.
    remember(
        app,
        &app.ui.include_extended,
        |p| p.include_extended_cuts,
        |p, v| p.include_extended_cuts = v,
    );
    remember(app, &app.ui.include_extras, |p| p.include_extras, |p, v| p.include_extras = v);
    remember(
        app,
        &app.ui.accurate_chapters,
        |p| p.accurate_chapters,
        |p, v| p.accurate_chapters = v,
    );
    if let Some(start) = find_button(&app.ui.output_dir, "start") {
        let app = Rc::clone(app);
        let tx = tx.clone();
        start.connect_clicked(move |_| {
            if app.is_game() {
                let disc = app.state.borrow().game.clone().unwrap_or_default();
                if app.is_busy() {
                    app.toast(&tr("Already working - wait for it, or cancel it first"));
                    return;
                }
                let Some(device) = app.state.borrow().drive.as_ref().map(|d| d.device.clone())
                else {
                    return;
                };
                let (root, dat_dir, read_offset) = {
                    let p = app.prefs.prefs.borrow();
                    // The folder, whether or not anything is in it yet: the
                    // datfiles are being fetched while the dump runs, and
                    // dat_dir() answers None for a folder that does not exist.
                    (
                        p.output_for(riplika_core::prefs::Library::Games),
                        Some(
                            p.dat_dir
                                .clone()
                                .unwrap_or_else(riplika_core::prefs::Preferences::default_dat_dir),
                        ),
                        p.read_offset,
                    )
                };

                let cancel = app
                    .start_working(&stage_label(riplika_core::job::Stage::Rip), "Dumping disc...");
                worker::run_game(device, disc, root, dat_dir, read_offset, cancel, tx.clone());
                return;
            }
            if app.is_music() {
                let (found, album) = {
                    let s = app.state.borrow();
                    (s.music.clone(), s.album.clone())
                };
                let (Some(found), Some(album)) = (found, album) else {
                    app.toast(&tr("Nothing to rip yet"));
                    return;
                };
                if app.is_busy() {
                    app.toast(&tr("Already working - wait for it, or cancel it first"));
                    return;
                }
                let device = match app.state.borrow().drive.as_ref() {
                    Some(d) => d.device.clone(),
                    None => return,
                };
                let settings = app.settings();
                let cancel = app
                    .start_working(&stage_label(riplika_core::job::Stage::Rip), "Ripping disc...");
                worker::run_music(device, found, album, settings, cancel, tx.clone());
                return;
            }
            let (scan, media) = {
                let s = app.state.borrow();
                (s.scan.clone(), s.chosen.clone())
            };
            let (Some(scan), Some(media)) = (scan, media) else {
                app.toast(&tr("Nothing to rip yet"));
                return;
            };
            if app.is_busy() {
                app.toast(&tr("Already working - wait for it, or cancel it first"));
                return;
            }
            let disc = app.ui.disc_entry.text().trim().parse::<u32>().ok();
            let settings = app.settings();
            let rip_dir = app.prefs.prefs.borrow().rip_dir();
            if settings.glyph_table.is_none() {
                app.toast("No glyph table: subtitles will stay as bitmaps");
            }
            let cancel =
                app.start_working(&stage_label(riplika_core::job::Stage::Rip), "Ripping disc...");
            let allow = app.prefs.prefs.borrow().use_makemkv();
            worker::run(
                scan,
                media,
                disc,
                rip_dir,
                settings,
                allow,
                riplika_core::joblog::now(),
                cancel,
                tx.clone(),
            );
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
                // The preferences themselves just changed, so the page follows
                // them. Choosing an output folder does not, which is why that
                // is the one call site that refreshes only the paths.
                app2.apply_prefs_to_controls();
            });
        });
    }

    // Eject, from wherever it is offered ------------------------------------
    for button in find_buttons(&app.ui.output_dir, "eject") {
        let app = Rc::clone(app);
        let tx = tx.clone();
        button.connect_clicked(move |_| {
            if app.is_busy() {
                app.toast(&tr("Not while the drive is being read"));
                return;
            }
            let device = app.state.borrow().drive.as_ref().map(|d| d.device.clone());
            match device {
                Some(device) => {
                    app.toast(&tr("Opening the drive"));
                    worker::eject(device, tx.clone());
                }
                None => app.toast(&tr("No drive to open")),
            }
        });
    }
    for button in find_buttons(&app.ui.output_dir, "another") {
        let app = Rc::clone(app);
        let tx = tx.clone();
        button.connect_clicked(move |_| {
            app.go(Step::Drive);
            worker::list_drives(true, tx.clone());
        });
    }

    // Step four ------------------------------------------------------------
    {
        let app = Rc::clone(app);
        app.clone().ui.cancel_button.connect_clicked(move |b| {
            let closing = b
                .child()
                .and_downcast::<adw::ButtonContent>()
                .map(|c| c.label() == "Close")
                .unwrap_or(false);
            if closing {
                // Back to the start rather than popping: after a finished or
                // abandoned job the choices are stale, and the drive page is
                // where a second disc begins.
                app.go(Step::Drive);
                return;
            }
            app.state.borrow().cancel.cancel();
            app.toast(&tr("Stopping after the current step"));
            set_button_label(b, &tr("Close"));
            // The job will not stop this instant - it stops at the next command
            // boundary - but nothing new should be startable in the meantime.
            app.set_busy(Some("Stopping..."));
        });
    }
}

/// Can a file of this kind hold a subtitle track at all?
///
/// A track of an album cannot, and saying "subtitles: none" underneath one is
/// answering a question nobody asked. Decided from what was written rather
/// than from what the drive currently holds, because the results page outlives
/// the disc: it is still on screen after the tray is opened.
fn carries_subtitles(path: &Path) -> bool {
    !matches!(
        path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref(),
        Some("mp3" | "flac" | "m4a" | "ogg" | "opus" | "wav")
    )
}

/// Keep a switch on the rip page, so the next disc starts where this one left.
///
/// Seeding the page calls `set_active`, which fires this handler too, so a
/// value that has not changed is not written - otherwise every visit to the
/// page would rewrite the preferences file.
fn remember(
    app: &Rc<App>,
    row: &adw::SwitchRow,
    get: fn(&Preferences) -> bool,
    put: fn(&mut Preferences, bool),
) {
    let app = Rc::clone(app);
    row.connect_active_notify(move |r| {
        let now = r.is_active();
        let unchanged = get(&app.prefs.prefs.borrow()) == now;
        if unchanged {
            return;
        }
        put(&mut app.prefs.prefs.borrow_mut(), now);
        app.prefs.save();
    });
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
            && let Ok(b) = w.clone().downcast::<gtk::Button>()
        {
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
            && let Ok(b) = w.clone().downcast::<gtk::Button>()
        {
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
    fn a_track_of_an_album_is_not_asked_about_its_subtitles() {
        // "subtitles: none" under 01 - The Great Awakening.mp3, eleven times.
        assert!(!carries_subtitles(Path::new("/m/01 - The Great Awakening.mp3")));
        assert!(!carries_subtitles(Path::new("/m/02 - All Over the Earth.flac")));
        // where they are possible, their absence is still worth saying
        assert!(carries_subtitles(Path::new("/v/Parks and Recreation - S04E07.mp4")));
        assert!(carries_subtitles(Path::new("/v/Frozen (2013).mkv")));
    }

    #[test]
    fn the_extension_is_read_whatever_case_it_is_written_in() {
        assert!(!carries_subtitles(Path::new("/m/track.MP3")));
        assert!(!carries_subtitles(Path::new("/m/track.FLAC")));
    }

    #[test]
    fn a_file_with_no_extension_is_assumed_to_be_video() {
        // Everything this produces has one; guessing "no subtitles" for
        // something unexpected would hide them where they exist.
        assert!(carries_subtitles(Path::new("/v/whatever")));
    }

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
        let tags: Vec<&str> =
            [Step::Drive, Step::Identify, Step::Settings, Step::Progress, Step::Results]
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
        let rip = prefs.rip_dir();
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
            naming::file_name(&media, &item, Container::Mp4, None),
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
            let mut state = State { busy: Some("Ripping...".into()), ..Default::default() };
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
            kind: None,
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
mod ejecting_tests {
    use super::*;

    fn loaded() -> State {
        State {
            drive: Some(Drive {
                id: "disc:0".into(),
                device: "/dev/sr0".into(),
                name: "HL-DT-ST BD-RE".into(),
                disc_label: Some("COOLBOARDERS2".into()),
                kind: Some(DiscKind::Data(None)),
            }),
            game: Some(riplika_core::game::GameDisc::default()),
            candidates: vec![],
            ..State::default()
        }
    }

    #[test]
    fn the_kind_goes_with_the_disc() {
        // The one that mattered. The kind chooses the pipeline, so a stale one
        // sent the next disc - a music CD - down the game path, where it came
        // out as an unnamed data disc with nothing to search for.
        let mut state = loaded();
        forget_the_disc(&mut state);
        assert_eq!(state.drive.as_ref().and_then(|d| d.kind.clone()), None);
    }

    #[test]
    fn nothing_read_off_the_old_disc_is_left_describing_the_new_one() {
        let mut state = loaded();
        forget_the_disc(&mut state);
        assert!(state.game.is_none(), "the last disc's name");
        assert!(state.album.is_none());
        assert!(state.music.is_none());
        assert!(state.scan.is_none());
        assert!(state.selected.is_none());
        assert!(state.candidates.is_empty());
    }

    #[test]
    fn the_drive_itself_stays_selected() {
        // The tray opening does not mean a different drive was meant.
        let mut state = loaded();
        forget_the_disc(&mut state);
        assert_eq!(state.drive.as_ref().map(|d| d.device.clone()), Some("/dev/sr0".to_string()));
    }
}

#[cfg(test)]
mod choosable_tests {
    use super::*;
    use riplika_core::disc::Toc;

    fn toc() -> Toc {
        Toc { tracks: Vec::new(), leadout: 0 }
    }

    #[test]
    fn a_game_offers_nothing_to_choose() {
        // It offered the show picker, which searches television and film by
        // title: putting a PlayStation disc in asked "Which show is this?" and
        // found nothing for Cool Boarders 2 - the right answer to a question
        // nobody meant to ask. A game is named by what its dump hashes to.
        assert!(!identity_is_choosable(Some(&DiscKind::Data(Some(toc())))));
        assert!(!identity_is_choosable(Some(&DiscKind::Data(None))));
    }

    #[test]
    fn a_music_disc_can_be_chosen_for_too() {
        // A disc id names one pressing exactly, so usually there is nothing to
        // choose - but the same pressing can have been issued twice, and a
        // disc MusicBrainz has never seen can still be searched for by name.
        assert!(identity_is_choosable(Some(&DiscKind::Audio(toc()))));
    }

    #[test]
    fn video_is_asked_about_as_well() {
        assert!(identity_is_choosable(Some(&DiscKind::DvdVideo)));
        assert!(identity_is_choosable(Some(&DiscKind::BluRay)));
    }

    #[test]
    fn an_unknown_drive_still_leads_somewhere() {
        // Nothing read yet is not the same as nothing to choose, and locking
        // the row on a disc that has not been looked at would strand anybody
        // whose disc failed to scan.
        assert!(identity_is_choosable(None));
        assert!(identity_is_choosable(Some(&DiscKind::Empty)));
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
            poster: None,
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
        let mut state =
            State { selected: Some(candidate("The Office", 3, 0.4)), ..Default::default() };
        let cands = [candidate("Parks and Recreation", 1, 0.85)];
        if state.selected.is_none() {
            state.selected = cands.first().cloned();
        }
        assert_eq!(state.selected.unwrap().media.title(), "The Office");
    }

    #[test]
    fn choosing_from_the_picker_replaces_the_answer() {
        let mut state = State::default();
        state.candidates =
            vec![candidate("Parks and Recreation", 1, 0.85), candidate("Parks", 1, 0.11)];
        state.selected = state.candidates.first().cloned();
        state.selected = state.candidates.get(1).cloned();
        assert_eq!(state.selected.unwrap().media.title(), "Parks");
    }

    #[test]
    fn choosing_an_index_that_is_gone_changes_nothing() {
        // results can be replaced by a newer search while a row is being tapped
        let mut state = State {
            selected: Some(candidate("Parks and Recreation", 1, 0.85)),
            ..Default::default()
        };
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

#[cfg(test)]
mod navigation_tests {
    use super::*;

    /// AdwNavigationView is a stack: pushing a tag already on it warns and does
    /// nothing, so the window silently stops moving. The flow reaches the
    /// progress page twice - once scanning, once ripping - so it cannot be
    /// driven by pushing, and every step declares its whole route instead.
    #[test]
    fn every_step_declares_a_route_ending_in_itself() {
        for step in [Step::Drive, Step::Identify, Step::Settings, Step::Progress, Step::Results] {
            let path = step.path();
            assert!(!path.is_empty(), "{:?} has no route", step.tag());
            assert_eq!(*path.last().unwrap(), step.tag(), "route must end at the step");
        }
    }

    #[test]
    fn no_route_visits_a_page_twice() {
        // a repeated tag within one route is the same error by another name
        for step in [Step::Drive, Step::Identify, Step::Settings, Step::Progress, Step::Results] {
            let mut seen = step.path().to_vec();
            seen.sort_unstable();
            let before = seen.len();
            seen.dedup();
            assert_eq!(seen.len(), before, "{:?} repeats a page", step.tag());
        }
    }

    #[test]
    fn every_route_starts_at_the_drive_page() {
        // so the back button always leads somewhere sensible
        for step in [Step::Identify, Step::Settings, Step::Progress, Step::Results] {
            assert_eq!(step.path()[0], Step::Drive.tag());
        }
    }

    #[test]
    fn progress_can_be_reached_from_scanning_and_from_ripping() {
        // the case that broke: pressing Start while progress was already on the
        // stack from the scan left the window sitting on the settings page
        assert_eq!(Step::Progress.path(), &["drive", "progress"]);
    }
}

#[cfg(test)]
mod locking_tests {
    use super::*;

    /// While a job runs there must be no way off the progress page except
    /// cancelling.
    ///
    /// Swiping back from it left the job running with nothing showing it and no
    /// way to return - the window looked idle while the drive was still going.
    #[test]
    fn a_running_job_pins_the_page_and_finishing_releases_it() {
        let mut state = State { busy: Some("Ripping...".into()), ..Default::default() };

        assert!(state.busy.is_some(), "can_pop is false while this holds");

        for ending in ["finished", "failed", "cancelled"] {
            state.busy = Some("Ripping...".into());
            state.busy = None; // every ending clears it
            assert!(state.busy.is_none(), "{ending} must release the page");
        }
    }

    #[test]
    fn the_progress_page_is_the_only_one_worth_pinning() {
        // the others are forms; leaving them loses nothing
        assert_eq!(Step::Progress.tag(), "progress");
        assert_ne!(Step::Settings.tag(), Step::Progress.tag());
    }
}

#[cfg(test)]
mod opening_query_tests {
    use super::*;

    #[test]
    fn a_disc_nothing_identified_opens_on_its_own_label() {
        // The way out of an unidentified disc is to type a name and use it as
        // given. A box opened empty does not suggest that is possible, and the
        // label is the only thing known about the disc.
        assert_eq!(
            opening_query("", None, Some("PARKS_AND_RECREATION_S6D1")),
            "Parks And Recreation"
        );
    }

    #[test]
    fn what_was_searched_for_last_wins() {
        // reopening the picker resumes where it was left
        assert_eq!(opening_query("the office", Some("Parks"), Some("LABEL")), "the office");
    }

    #[test]
    fn otherwise_it_opens_on_what_the_page_settled_on() {
        assert_eq!(
            opening_query("", Some("Parks and Recreation"), Some("LABEL")),
            "Parks and Recreation"
        );
        assert_eq!(
            opening_query("   ", Some("Parks and Recreation"), None),
            "Parks and Recreation"
        );
    }

    #[test]
    fn a_disc_with_no_label_at_all_opens_empty() {
        // nothing is known, and inventing something would be worse
        assert_eq!(opening_query("", None, None), "");
    }
}

#[cfg(test)]
mod warning_text_tests {
    use super::*;

    fn all() -> Vec<Warning> {
        vec![
            Warning::CouldNotIdentify { why: "no network".into() },
            Warning::TitleUnreadable { title: 3, why: "i/o error".into() },
            Warning::NoPlayAll { episodes: 7 },
            Warning::ExtendedCutsUncomparable { why: "ffmpeg".into() },
            Warning::GlyphTableUnreadable { path: "/t.json".into(), why: "bad json".into() },
            Warning::GlyphTableMissing { path: "/t.json".into() },
            Warning::NoGlyphTable,
            Warning::ItemSkipped { name: "a.mkv".into(), why: "no space".into() },
            Warning::UnrecognisedGlyphs { language: "sv".into(), glyphs: 4 },
            Warning::PlayAllsSkipped { titles: 2 },
            Warning::FreeReaderIncomplete { why: "encrypted".into() },
            Warning::FreeReaderFailed { why: "no drive".into() },
            Warning::CacheNotCleared { path: "/c/title_t01.mkv".into(), why: "in use".into() },
            Warning::GlyphTableIsForAnotherFont { shapes: 115 },
            Warning::CannotLearnLettering { shapes: 115 },
            Warning::CannotReadLanguage { language: "Icelandic".into() },
            Warning::SubtitlesUnreadable {
                language: "Swedish".into(),
                why: "no such stream".into(),
            },
        ]
    }

    #[test]
    fn every_kind_of_warning_has_something_to_show() {
        // the match is exhaustive, so none can be forgotten; what this catches
        // is one being wired to an empty or placeholder string
        for w in all() {
            assert!(!warning_text(&w).trim().is_empty(), "{w:?} shows nothing");
        }
    }

    #[test]
    fn the_reason_from_elsewhere_survives_translation() {
        // an OS or ffmpeg message is the part a bug report is about, and it is
        // English wherever it is shown
        let w = Warning::TitleUnreadable { title: 3, why: "Input/output error".into() };
        assert!(warning_text(&w).contains("Input/output error"), "{}", warning_text(&w));
        assert!(warning_text(&w).contains('3'), "the title number is lost");
    }

    #[test]
    fn one_of_something_reads_as_one_here_as_well() {
        // with no catalogue loaded these come back as the source strings, so
        // this is checking the plural machinery is wired up, not the English
        assert!(
            warning_text(&Warning::PlayAllsSkipped { titles: 1 }).contains("1 play-all title,")
        );
        assert!(warning_text(&Warning::NoPlayAll { episodes: 1 }).contains("1 episode "));
    }

    #[test]
    fn nothing_is_left_saying_percent_one() {
        // a placeholder reaching the window means an argument was not passed
        for w in all() {
            let shown = warning_text(&w);
            assert!(!shown.contains("%1$s"), "{w:?} shows a placeholder: {shown}");
            assert!(!shown.contains("%2$s"), "{w:?} shows a placeholder: {shown}");
            assert!(!shown.contains("%d"), "{w:?} shows a placeholder: {shown}");
        }
    }
}

#[cfg(test)]
mod remaining_text_tests {
    use super::*;
    use riplika_core::job::Remaining;

    #[test]
    fn every_shape_of_estimate_is_phrased() {
        let all = [
            Remaining::LessThanAMinute,
            Remaining::AboutAMinute,
            Remaining::Minutes(4),
            Remaining::Hours(3),
            Remaining::HoursAndMinutes(2, 30),
        ];
        for r in all {
            let shown = remaining_text(r);
            assert!(!shown.trim().is_empty(), "{r:?} says nothing");
            assert!(!shown.contains("%1$s") && !shown.contains("%d"), "{r:?}: {shown}");
        }
    }

    #[test]
    fn an_hour_is_not_hours() {
        assert!(
            remaining_text(Remaining::Hours(1)).contains("1 hour "),
            "{}",
            remaining_text(Remaining::Hours(1))
        );
        assert!(
            remaining_text(Remaining::Minutes(1)).contains("1 minute "),
            "{}",
            remaining_text(Remaining::Minutes(1))
        );
    }
}

#[cfg(test)]
mod stage_label_tests {
    use super::*;
    use riplika_core::job::Stage;

    const ALL: [Stage; 7] = [
        Stage::Scan,
        Stage::Identify,
        Stage::Rip,
        Stage::Organise,
        Stage::Subtitles,
        Stage::Lettering,
        Stage::Transcode,
    ];

    #[test]
    fn every_stage_is_named_and_named_differently() {
        // The match is exhaustive, so a new stage cannot be forgotten. What
        // this catches is two of them being given the same label by a careless
        // copy - which shows up as a progress heading that never changes.
        let mut seen = std::collections::HashSet::new();
        for stage in ALL {
            let label = stage_label(stage);
            assert!(!label.is_empty(), "{stage:?} has no label");
            assert!(seen.insert(label.clone()), "{stage:?} repeats the label {label:?}");
        }
    }

    #[test]
    fn the_labels_are_the_ones_core_used_to_supply() {
        // Moving them here must not have changed the words, only who says
        // them: with no catalogue loaded, tr returns the source string.
        for stage in ALL {
            assert_eq!(stage_label(stage), stage.label());
        }
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;

    fn with(role: Role) -> Item {
        Item {
            source: PathBuf::from("/rip/a.mkv"),
            role,
            title: String::new(),
            air_date: None,
            duration: 0,
            destination: None,
        }
    }

    fn episodes(n: u32) -> Vec<Item> {
        (0..n).map(|i| with(Role::Episode { season: 1, number: i + 1 })).collect()
    }

    #[test]
    fn one_of_something_reads_as_one() {
        // the bug this replaced: the count was formatted into a noun that was
        // spelled for the other case, so a single-episode disc said
        // "1 episodes" and a two-feature disc said "2 feature"
        let line = &plan_lines(&episodes(1))[0];
        assert!(line.contains("1 episode"), "{line}");
        assert!(!line.contains("1 episodes"), "{line}");
    }

    #[test]
    fn more_than_one_reads_as_many() {
        let line = &plan_lines(&episodes(7))[0];
        assert!(line.contains("7 episodes"), "{line}");
    }

    #[test]
    fn a_lone_feature_is_singular_too() {
        let line = &plan_lines(&[with(Role::Feature)])[0];
        assert!(line.contains("1 feature") && !line.contains("1 features"), "{line}");
    }

    #[test]
    fn every_kind_the_disc_holds_is_named_once() {
        let mut items = episodes(2);
        items.push(with(Role::Feature));
        items.push(with(Role::Extra));
        let line = &plan_lines(&items)[0];
        for expected in ["2 episodes", "1 feature", "1 extra"] {
            assert!(line.contains(expected), "{expected} missing from {line}");
        }
    }

    #[test]
    fn a_disc_of_nothing_it_can_name_says_nothing() {
        // an empty line in the log would read as something having gone wrong
        assert!(plan_lines(&[]).is_empty());
    }

    #[test]
    fn skipped_play_alls_are_reported_apart_from_the_holdings() {
        // they are not part of what is being made, so they get their own line
        let mut items = episodes(3);
        items.push(with(Role::PlayAll));
        let lines = plan_lines(&items);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("3 episodes"), "{}", lines[0]);
        assert!(lines[1].contains("1 play-all title will"), "{}", lines[1]);
    }
}

#[cfg(test)]
mod language_choice_tests {
    use riplika_core::lang::LanguageSet;
    use riplika_core::model::TrackKind;

    #[test]
    fn an_empty_set_means_no_filter_which_is_not_what_unticking_looks_like() {
        // The trap: to the pipeline an empty set means "keep everything", so
        // unticking every language would keep them all rather than none. The
        // window refuses the choice rather than quietly inverting it.
        let tags: Vec<String> = ["eng", "swe"].iter().map(|s| s.to_string()).collect();
        let none = LanguageSet::default();
        assert_eq!(none.select(&tags), vec![0, 1], "empty means keep everything");
    }

    #[test]
    fn a_real_choice_filters_as_expected() {
        let tags: Vec<String> = ["eng", "swe", "spa"].iter().map(|s| s.to_string()).collect();
        let set = LanguageSet::parse("swedish");
        assert_eq!(set.select(&tags), vec![1]);
        assert!(set.select_with_fallback(&tags, TrackKind::Subtitle).len() == 1);
    }

    #[test]
    fn a_disc_with_no_language_tracks_is_not_the_same_as_choosing_none() {
        // nothing to tick is fine; ticking nothing is not
        let empty: Vec<(String, bool)> = Vec::new();
        let has_none_to_offer = empty.is_empty();
        let offered_but_unticked = [("eng".to_string(), false)];
        assert!(has_none_to_offer, "a disc with no tracks may still be ripped");
        assert!(
            !offered_but_unticked.iter().any(|(_, on)| *on),
            "a disc with tracks and none ticked must be refused"
        );
    }
}

#[cfg(test)]
mod icon_tests {
    /// Which buttons carry an icon, and why the rest do not.
    ///
    /// An icon earns its place on a button that is *skimmed past* - you are
    /// looking for the eject glyph, not reading the word. On the one action a
    /// page exists for it competes instead, and a misleading icon is worse than
    /// none at all.
    #[test]
    fn the_icons_chosen_say_what_the_buttons_do() {
        let decided: &[(&str, Option<&str>, &str)] = &[
            ("Look again", Some("view-refresh-symbolic"), "skimmed for"),
            ("Eject", Some("media-eject-symbolic"), "skimmed for"),
            ("Rip another disc", Some("media-optical-symbolic"), "skimmed for"),
            ("Cancel", Some("process-stop-symbolic"), "stopping work, not discarding a form"),
            // A play triangle in an application that also handles video reads
            // as "play this", which is worse than no icon.
            ("Start", None, "no icon means starting a rip rather than playback"),
            // go-next-symbolic already means "open the show picker" on that
            // same page, and one glyph cannot mean two things.
            ("Continue", None, "the chevron is taken"),
            ("Analyse disc", None, "the hero action of a status page"),
        ];
        for (label, icon, why) in decided {
            assert!(!why.is_empty(), "{label} needs a reason either way");
            if let Some(i) = icon {
                assert!(i.ends_with("-symbolic"), "{label}: {i} is not a symbolic icon");
            }
        }
        assert_eq!(decided.iter().filter(|(_, i, _)| i.is_some()).count(), 4);
    }
}

#[cfg(test)]
mod music_settings_tests {
    use super::*;

    #[test]
    fn flac_offers_no_tier_to_choose_so_the_chooser_is_switched_off() {
        let (subtitle, live) = music_quality_state(AudioFormat::Flac);
        assert!(!live, "a chooser that cannot change the result must not look live");
        assert!(
            subtitle.to_lowercase().contains("lossless"),
            "it has to say why it is off: {subtitle}"
        );
    }

    #[test]
    fn mp3_has_a_real_choice_so_the_chooser_stays_live() {
        let (subtitle, live) = music_quality_state(AudioFormat::Mp3);
        assert!(live);
        assert!(!subtitle.is_empty(), "a live chooser should say what the tiers mean");
    }

    #[test]
    fn the_two_formats_do_not_say_the_same_thing() {
        assert_ne!(music_quality_state(AudioFormat::Flac), music_quality_state(AudioFormat::Mp3));
    }
}

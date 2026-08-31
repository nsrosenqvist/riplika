//! The preferences dialog: the settings that outlive one disc.
//!
//! The split from the per-rip settings page is deliberate. What goes here is
//! policy that is true of the whole library - which languages are worth
//! keeping, where the glyph table lives, whether commentary is wanted. What
//! stays on the rip page is what can differ between one disc and the next:
//! quality, the output folder, and which of *this* disc's languages to take.

use crate::i18n::{tr, tr_args};
use adw::prelude::*;
use riplika_core::host;
use riplika_core::lang;
use riplika_core::naming;
use riplika_core::prefs::Library;
use riplika_core::prefs::{MAKEMKV, Preferences};
use riplika_core::secret;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

/// Somewhere to keep preferences while the dialog edits them.
pub struct Store {
    pub prefs: RefCell<Preferences>,
}

impl Store {
    pub fn new(prefs: Preferences) -> Self {
        Store { prefs: RefCell::new(prefs) }
    }

    /// Save, reporting failure rather than losing it silently.
    pub fn save(&self) -> Option<String> {
        self.prefs.borrow().save().err().map(|e| e.to_string())
    }
}

fn folder_row(title: &str, subtitle: &str) -> adw::ActionRow {
    adw::ActionRow::builder().title(title).subtitle(subtitle).activatable(true).build()
}

fn describe(path: &Option<PathBuf>, empty: &str) -> String {
    path.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| empty.to_string())
}

/// Build and present the dialog.
///
/// `on_change` runs after every edit, so the rest of the window can follow -
/// the language pre-selection on the rip page depends on what is chosen here.
pub fn present<F>(parent: &impl IsA<gtk::Widget>, store: Rc<Store>, on_change: F)
where
    F: Fn() + Clone + 'static,
{
    let dialog = adw::PreferencesDialog::builder().title(tr("Preferences")).build();
    let page = adw::PreferencesPage::builder().title(tr("General")).build();

    // --- reading discs ----------------------------------------------------
    let reading = adw::PreferencesGroup::builder()
        .title(tr("Reading discs"))
        .description(
            "DVDs are read with libdvdread and libdvdcss, which need nothing \
             proprietary. MakeMKV is needed for Blu-ray, and for DVDs the free \
             reader cannot manage - a disc whose region does not match the \
             drive, or one that is scratched.",
        )
        .build();

    let installed = Preferences::makemkv_available();
    let makemkv = adw::SwitchRow::builder()
        .title(tr("Use MakeMKV when needed"))
        .subtitle(if installed {
            match host::which(MAKEMKV) {
                Some(p) => format!("Found at {}", p.display()),
                None => "Installed".into(),
            }
        } else {
            // Say why it cannot be switched on, rather than leaving a dead
            // control with no explanation.
            format!("Not available - {MAKEMKV} is not installed")
        })
        .build();
    // An option that cannot be honoured must not look as though it can: the
    // failure would otherwise surface forty minutes into a disc.
    makemkv.set_sensitive(installed);
    makemkv.set_active(installed && store.prefs.borrow().makemkv_fallback);
    reading.add(&makemkv);
    page.add(&reading);

    // --- languages --------------------------------------------------------
    let languages = adw::PreferencesGroup::builder()
        .title(tr("Preferred languages"))
        .description(
            "Ticked languages are selected by default when a disc offers them. \
             The order is the order you turn them on, and the first one becomes \
             the default track.",
        )
        .build();

    let expander = adw::ExpanderRow::builder().title(tr("Languages")).build();
    let summary = |p: &Preferences| {
        let names: Vec<String> = p.languages().wanted().iter().map(|l| l.name.clone()).collect();
        if names.is_empty() {
            "None - every language on a disc will be offered unticked".to_string()
        } else {
            names.join(", ")
        }
    };
    expander.set_subtitle(&summary(&store.prefs.borrow()));

    for language in lang::all() {
        let code = language.code.clone();
        let row = adw::SwitchRow::builder().title(&language.name).build();
        row.set_active(store.prefs.borrow().preferred_languages.contains(&code));
        let store2 = Rc::clone(&store);
        let expander2 = expander.clone();
        let on_change2 = on_change.clone();
        row.connect_active_notify(move |r| {
            {
                let mut p = store2.prefs.borrow_mut();
                if r.is_active() {
                    // Appending rather than inserting keeps the order the user
                    // chose, which is what decides the default track.
                    if !p.preferred_languages.contains(&code) {
                        p.preferred_languages.push(code.clone());
                    }
                } else {
                    p.preferred_languages.retain(|c| c != &code);
                }
            }
            expander2.set_subtitle(&summary(&store2.prefs.borrow()));
            store2.save();
            on_change2();
        });
        expander.add_row(&row);
    }
    languages.add(&expander);
    page.add(&languages);

    // --- naming -----------------------------------------------------------
    let naming_group = adw::PreferencesGroup::builder()
        .title(tr("Episode filenames"))
        .description(format!(
            "Tokens: {}",
            naming::TOKENS.iter().map(|(t, _)| *t).collect::<Vec<_>>().join("  ")
        ))
        .build();
    let template_row = adw::EntryRow::builder().title(tr("Pattern")).build();
    template_row.set_text(&store.prefs.borrow().episode_template);
    // What it will actually produce, updated as it is typed - the only way to
    // know a pattern does what you meant without ripping a disc to find out.
    let preview_row = adw::ActionRow::builder().title(tr("Preview")).build();
    preview_row.add_css_class("property");
    let container = store.prefs.borrow().container;
    preview_row.set_subtitle(&naming::preview(&template_row.text(), container));
    naming_group.add(&template_row);
    naming_group.add(&preview_row);
    page.add(&naming_group);

    // The same idea for music, with its own words: a track has no season and
    // an episode has no album, so one list of tokens could not serve both.
    let music_naming = adw::PreferencesGroup::builder()
        .title(tr("Track filenames"))
        .description(format!(
            "{}\nTokens: {}",
            tr(
                "A slash makes a folder, so {artist}/{album}/{track} - {title} lays out the library"
            ),
            naming::MUSIC_TOKENS.iter().map(|(t, _)| *t).collect::<Vec<_>>().join("  ")
        ))
        .build();
    let music_template_row = adw::EntryRow::builder().title(tr("Pattern")).build();
    music_template_row.set_text(&store.prefs.borrow().music_template);
    let music_preview_row = adw::ActionRow::builder().title(tr("Preview")).build();
    music_preview_row.add_css_class("property");
    let music_extension = store.prefs.borrow().music_format.target().extension();
    music_preview_row
        .set_subtitle(&naming::music_preview(&music_template_row.text(), music_extension));
    music_naming.add(&music_template_row);
    music_naming.add(&music_preview_row);
    page.add(&music_naming);

    // --- catalogues -------------------------------------------------------
    let catalogue_group = adw::PreferencesGroup::builder()
        .title(tr("Catalogues"))
        .description(
            "TVmaze answers for television and Wikidata for film, neither needing a key. \
             A TMDB key is used in preference to both: it is better data, and it is what a \
             media server consults about the same files.",
        )
        .build();
    let tmdb_row = adw::PasswordEntryRow::builder().title(tr("TMDB API key")).build();
    if let Some(k) = secret::tmdb_key() {
        tmdb_row.set_text(&k);
    }
    tmdb_row.set_show_apply_button(true);
    catalogue_group.add(&tmdb_row);
    page.add(&catalogue_group);

    // --- track policy -----------------------------------------------------
    let tracks = adw::PreferencesGroup::builder().title(tr("Tracks")).build();
    let dual = adw::SwitchRow::builder()
        .title(tr("Add a stereo AAC track"))
        .subtitle(tr("So browser clients do not make the server transcode AC3"))
        .build();
    dual.set_active(store.prefs.borrow().dual_audio);
    let bitmaps = adw::SwitchRow::builder()
        .title(tr("Keep VobSub bitmaps"))
        .subtitle(tr("Redundant once recognised, and selecting one forces a burn-in re-encode"))
        .build();
    bitmaps.set_active(store.prefs.borrow().keep_bitmap_subs);
    let commentary = adw::SwitchRow::builder().title(tr("Keep commentary tracks")).build();
    commentary.set_active(!store.prefs.borrow().drop_commentary);
    tracks.add(&dual);
    tracks.add(&bitmaps);
    tracks.add(&commentary);
    page.add(&tracks);

    // --- folders ----------------------------------------------------------
    // Only the working folder. The glyph table and the wordlists are
    // application data with a standard place to live, built once and then used
    // without being thought about - there is no answer a user could give that
    // would be better than the default, so they are not asked.
    //
    // Where a rip lands is a real question: it wants tens of gigabytes, and a
    // small home partition is a good reason to put it elsewhere.
    // One row per library, because there is one folder per library and a
    // single "output folder" could not say which it meant. It used to be
    // settable only from the rip page, where what it referred to depended on
    // what was in the drive - so a folder picked with a CD loaded quietly
    // became the answer for films as well.
    let libraries = adw::PreferencesGroup::builder()
        .title(tr("Libraries"))
        .description(tr("Where finished files go. Each kind of disc has its own."))
        .build();
    let video_dir = folder_row(
        "Video",
        &describe(&Some(store.prefs.borrow().output_for(Library::Video)), "Videos"),
    );
    let music_dir = folder_row(
        "Music",
        &describe(&Some(store.prefs.borrow().output_for(Library::Music)), "Music"),
    );
    let games_dir = folder_row(
        "Games",
        &describe(&Some(store.prefs.borrow().output_for(Library::Games)), "Games"),
    );
    libraries.add(&video_dir);
    libraries.add(&music_dir);
    libraries.add(&games_dir);
    page.add(&libraries);

    let folders = adw::PreferencesGroup::builder()
        .title(tr("Working folder"))
        .description(format!(
            "Where a disc lands before it is encoded - tens of gigabytes, deleted afterwards. \
             Subtitle data lives in {}.",
            Preferences::data_dir().display()
        ))
        .build();
    let rip = folder_row(
        "Folder",
        // Never the system temporary folder: a disc's raw rip is tens of
        // gigabytes and /tmp is memory on most systems now.
        &describe(&Some(store.prefs.borrow().rip_dir()), "Cache folder"),
    );
    folders.add(&rip);
    page.add(&folders);

    // Datfiles are asked about where the glyph table and wordlists are not,
    // and the difference is who makes them. Those two the program builds, so
    // there is no answer a user could give that beats the default. A datfile
    // comes from outside, covers one system, and somebody who has a collection
    // of them already keeps it somewhere.
    let games = adw::PreferencesGroup::builder()
        .title(tr("Game datfiles"))
        .description(
            "Redump datfiles name a game disc from what it hashes to, which is also what \
             proves the dump is whole. Without them a dump still works and is filed under \
             whatever the disc calls itself.",
        )
        .build();
    let dats = folder_row(
        "Folder",
        &describe(&store.prefs.borrow().dat_dir(), "None - dumps will not be named"),
    );
    games.add(&dats);

    // Somewhere to get them. Inside a flatpak this is the only way there is:
    // the sandbox has a data directory of its own, so datfiles fetched on the
    // host are not visible and the folder starts empty - "no datfiles found",
    // with nothing the reader can do about it.
    let systems = gtk::StringList::new(&[]);
    for (_, name) in riplika_core::redump::SYSTEMS {
        systems.append(name);
    }
    let system_row = adw::ComboRow::builder()
        .title(tr("System"))
        .subtitle(tr("Downloaded from redump.org into the folder above"))
        .model(&systems)
        .selected(0)
        .build();
    let fetch = gtk::Button::builder()
        .label(tr("Download"))
        .valign(gtk::Align::Center)
        .css_classes(vec!["suggested-action".to_string()])
        .build();
    system_row.add_suffix(&fetch);
    games.add(&system_row);
    page.add(&games);

    {
        let store = Rc::clone(&store);
        let system_row = system_row.clone();
        let fetch = fetch.clone();
        fetch.connect_clicked(move |button| {
            let Some((slug, _)) = riplika_core::redump::SYSTEMS.get(system_row.selected() as usize)
            else {
                return;
            };
            let dir = store
                .prefs
                .borrow()
                .dat_dir
                .clone()
                .unwrap_or_else(riplika_core::prefs::Preferences::default_dat_dir);
            // On this thread on purpose: it is one request and a small file,
            // and the dialog has nowhere to put a background task's answer.
            // The button says what is happening while it happens.
            button.set_sensitive(false);
            button.set_label(&tr("Downloading..."));
            let fs = riplika_core::host::RealFs;
            let runner = riplika_core::host::RealRunner::new(riplika_core::host::Cancel::new());
            let http = riplika_core::identify::catalogue::UreqHttp;
            let done = riplika_core::redump::fetch(&fs, &runner, &http, slug, &dir);
            button.set_sensitive(true);
            button.set_label(&tr("Download"));
            match done {
                Ok(f) if f.archive.is_none() => {
                    system_row.set_subtitle(&tr_args("%1$s is ready", &[&f.system]));
                }
                Ok(_) => {
                    system_row.set_subtitle(&tr("Downloaded, but nothing here could open it"));
                }
                Err(e) => system_row.set_subtitle(&e.to_string()),
            }
        });
    }

    dialog.add(&page);

    // --- wiring -----------------------------------------------------------
    {
        let store = Rc::clone(&store);
        let on_change = on_change.clone();
        makemkv.connect_active_notify(move |r| {
            store.prefs.borrow_mut().makemkv_fallback = r.is_active();
            store.save();
            on_change();
        });
    }
    {
        let store = Rc::clone(&store);
        let on_change = on_change.clone();
        dual.connect_active_notify(move |r| {
            store.prefs.borrow_mut().dual_audio = r.is_active();
            store.save();
            on_change();
        });
    }
    {
        let store = Rc::clone(&store);
        let on_change = on_change.clone();
        bitmaps.connect_active_notify(move |r| {
            store.prefs.borrow_mut().keep_bitmap_subs = r.is_active();
            store.save();
            on_change();
        });
    }
    {
        let store = Rc::clone(&store);
        let on_change = on_change.clone();
        commentary.connect_active_notify(move |r| {
            store.prefs.borrow_mut().drop_commentary = !r.is_active();
            store.save();
            on_change();
        });
    }

    {
        let store = Rc::clone(&store);
        let preview = preview_row.clone();
        let on_change = on_change.clone();
        template_row.connect_changed(move |e| {
            let pattern = e.text().to_string();
            let container = store.prefs.borrow().container;
            preview.set_subtitle(&naming::preview(&pattern, container));
            store.prefs.borrow_mut().episode_template = pattern;
            store.save();
            on_change();
        });
    }
    {
        let store = Rc::clone(&store);
        let preview = music_preview_row.clone();
        let on_change = on_change.clone();
        music_template_row.connect_changed(move |e| {
            let pattern = e.text().to_string();
            let extension = store.prefs.borrow().music_format.target().extension();
            preview.set_subtitle(&naming::music_preview(&pattern, extension));
            store.prefs.borrow_mut().music_template = pattern;
            store.save();
            on_change();
        });
    }
    {
        // Into the login keyring, not the config file. Encrypting it ourselves
        // would need the key to decrypt stored somewhere this process can reach
        // unasked - which is somewhere anyone reading the config can reach too.
        let toasts = dialog.clone();
        tmdb_row.connect_apply(move |e| {
            if let Err(err) = secret::store("tmdb", &e.text()) {
                toasts.set_title(&format!("Could not save the key: {err}"));
            }
        });
    }

    let pick = |row: &adw::ActionRow,
                dialog: &adw::PreferencesDialog,
                store: Rc<Store>,
                folder: bool,
                title: &'static str,
                empty: &'static str,
                set: fn(&mut Preferences, Option<PathBuf>),
                get: fn(&Preferences) -> Option<PathBuf>,
                on_change: F| {
        let row2 = row.clone();
        let dialog = dialog.clone();
        row.connect_activated(move |_| {
            let chooser = gtk::FileDialog::builder().title(title).build();
            let store = Rc::clone(&store);
            let row3 = row2.clone();
            let on_change = on_change.clone();
            let handle = move |res: Result<gtk::gio::File, gtk::glib::Error>| {
                if let Ok(f) = res
                    && let Some(path) = f.path()
                {
                    set(&mut store.prefs.borrow_mut(), Some(path));
                    store.save();
                    row3.set_subtitle(&describe(&get(&store.prefs.borrow()), empty));
                    on_change();
                }
            };
            let root = dialog.root().and_downcast::<gtk::Window>();
            if folder {
                chooser.select_folder(root.as_ref(), gtk::gio::Cancellable::NONE, handle);
            } else {
                chooser.open(root.as_ref(), gtk::gio::Cancellable::NONE, handle);
            }
        });
    };

    pick(
        &rip,
        &dialog,
        Rc::clone(&store),
        true,
        "Working folder",
        "System temporary folder",
        |p, v| p.rip_dir = v,
        |p| p.rip_dir.clone(),
        on_change.clone(),
    );

    for (row, library, title) in [
        (&video_dir, Library::Video, "Video library"),
        (&music_dir, Library::Music, "Music library"),
        (&games_dir, Library::Games, "Game library"),
    ] {
        let store2 = Rc::clone(&store);
        let row2 = row.clone();
        let dialog2 = dialog.clone();
        let on_change2 = on_change.clone();
        row.connect_activated(move |_| {
            let chooser = gtk::FileDialog::builder().title(title).build();
            let (store3, row3, on_change3) = (Rc::clone(&store2), row2.clone(), on_change2.clone());
            let handle = move |res: Result<gtk::gio::File, gtk::glib::Error>| {
                if let Ok(f) = res
                    && let Some(path) = f.path()
                {
                    store3.prefs.borrow_mut().set_output_for(library, path);
                    store3.save();
                    let now = store3.prefs.borrow().output_for(library);
                    row3.set_subtitle(&now.to_string_lossy());
                    on_change3();
                }
            };
            let root = dialog2.root().and_downcast::<gtk::Window>();
            chooser.select_folder(root.as_ref(), gtk::gio::Cancellable::NONE, handle);
        });
    }

    pick(
        &dats,
        &dialog,
        Rc::clone(&store),
        true,
        "Game datfiles",
        "None - dumps will not be named",
        |p, v| p.dat_dir = v,
        |p| p.dat_dir.clone(),
        on_change.clone(),
    );

    dialog.present(Some(parent));
}

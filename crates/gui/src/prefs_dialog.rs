//! The preferences dialog: the settings that outlive one disc.
//!
//! The split from the per-rip settings page is deliberate. What goes here is
//! policy that is true of the whole library - which languages are worth
//! keeping, where the glyph table lives, whether commentary is wanted. What
//! stays on the rip page is what can differ between one disc and the next:
//! quality, the output folder, and which of *this* disc's languages to take.

use adw::prelude::*;
use riplika_core::host;
use riplika_core::lang;
use riplika_core::prefs::{Preferences, MAKEMKV};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

/// Somewhere to keep preferences while the dialog edits them.
pub struct Store {
    pub prefs: RefCell<Preferences>,
}

impl Store {
    pub fn new(prefs: Preferences) -> Self {
        Store {
            prefs: RefCell::new(prefs),
        }
    }

    /// Save, reporting failure rather than losing it silently.
    pub fn save(&self) -> Option<String> {
        self.prefs.borrow().save().err().map(|e| e.to_string())
    }
}

fn folder_row(title: &str, subtitle: &str) -> adw::ActionRow {
    adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .activatable(true)
        .build()
}

fn describe(path: &Option<PathBuf>, empty: &str) -> String {
    path.as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| empty.to_string())
}

/// Build and present the dialog.
///
/// `on_change` runs after every edit, so the rest of the window can follow -
/// the language pre-selection on the rip page depends on what is chosen here.
pub fn present<F>(parent: &impl IsA<gtk::Widget>, store: Rc<Store>, on_change: F)
where
    F: Fn() + Clone + 'static,
{
    let dialog = adw::PreferencesDialog::builder()
        .title("Preferences")
        .build();
    let page = adw::PreferencesPage::builder().title("General").build();

    // --- reading discs ----------------------------------------------------
    let reading = adw::PreferencesGroup::builder()
        .title("Reading discs")
        .description(
            "DVDs are read with libdvdread and libdvdcss, which need nothing \
             proprietary. MakeMKV is needed for Blu-ray, and for DVDs the free \
             reader cannot manage - a disc whose region does not match the \
             drive, or one that is scratched.",
        )
        .build();

    let installed = Preferences::makemkv_available();
    let makemkv = adw::SwitchRow::builder()
        .title("Use MakeMKV when needed")
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
        .title("Preferred languages")
        .description(
            "Ticked languages are selected by default when a disc offers them. \
             The order is the order you turn them on, and the first one becomes \
             the default track.",
        )
        .build();

    let expander = adw::ExpanderRow::builder().title("Languages").build();
    let summary = |p: &Preferences| {
        let names: Vec<String> = p
            .languages()
            .0
            .iter()
            .map(|l| l.name.clone())
            .collect();
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

    // --- track policy -----------------------------------------------------
    let tracks = adw::PreferencesGroup::builder().title("Tracks").build();
    let dual = adw::SwitchRow::builder()
        .title("Add a stereo AAC track")
        .subtitle("So browser clients do not make the server transcode AC3")
        .build();
    dual.set_active(store.prefs.borrow().dual_audio);
    let bitmaps = adw::SwitchRow::builder()
        .title("Keep VobSub bitmaps")
        .subtitle("Redundant once recognised, and selecting one forces a burn-in re-encode")
        .build();
    bitmaps.set_active(store.prefs.borrow().keep_bitmap_subs);
    let commentary = adw::SwitchRow::builder().title("Keep commentary tracks").build();
    commentary.set_active(!store.prefs.borrow().drop_commentary);
    tracks.add(&dual);
    tracks.add(&bitmaps);
    tracks.add(&commentary);
    page.add(&tracks);

    // --- folders ----------------------------------------------------------
    let folders = adw::PreferencesGroup::builder()
        .title("Folders")
        .description("Where subtitle recognition finds what it needs")
        .build();
    let p = store.prefs.borrow();
    let table = folder_row(
        "Glyph table",
        &describe(&p.glyph_table, "None - subtitles will stay as bitmaps"),
    );
    let words = folder_row("Wordlists", &describe(&p.words_dir, "None"));
    let rip = folder_row(
        "Working folder",
        &describe(&p.rip_dir, "System temporary folder"),
    );
    drop(p);
    folders.add(&table);
    folders.add(&words);
    folders.add(&rip);
    page.add(&folders);

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
                    && let Some(path) = f.path() {
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
        &table, &dialog, Rc::clone(&store), false, "Glyph table",
        "None - subtitles will stay as bitmaps",
        |p, v| p.glyph_table = v, |p| p.glyph_table.clone(), on_change.clone(),
    );
    pick(
        &words, &dialog, Rc::clone(&store), true, "Wordlists", "None",
        |p, v| p.words_dir = v, |p| p.words_dir.clone(), on_change.clone(),
    );
    pick(
        &rip, &dialog, Rc::clone(&store), true, "Working folder",
        "System temporary folder",
        |p, v| p.rip_dir = v, |p| p.rip_dir.clone(), on_change.clone(),
    );

    dialog.present(Some(parent));
}

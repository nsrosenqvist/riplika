//! Choosing which work a disc is.
//!
//! The alternatives are only interesting while you are choosing between them.
//! Left on the page they are a list of things you have already rejected, and
//! they push everything that actually needs answering - the season, the disc
//! number - further down. So the page states what it settled on, and the
//! alternatives live in a dialog you open when that is wrong.
//!
//! Films, television and music all come through here. What differs between
//! them is what a row says and what the question at the top is, so those are
//! given to it rather than assumed: this knows how to let somebody choose from
//! a list, and nothing about what is in the list.

use crate::i18n::tr;
use crate::rows;
use adw::prelude::*;
use riplika_core::identify::music::Match;
use riplika_core::model::Candidate;

/// One row: what it is, and what distinguishes it from the row above.
pub struct Choice {
    pub title: String,
    pub subtitle: String,
    /// How sure the catalogue is, where it said. Nothing for a music search,
    /// which scores the *name* and would read as confidence about the disc.
    pub confidence: Option<f32>,
}

impl Choice {
    /// A film or a television series, as the catalogues answered.
    pub fn of_candidate(c: &Candidate) -> Choice {
        Choice {
            title: c.media.describe_work(),
            // What the work is, not that a search happened: the reasons are
            // evidence about *this disc*, which a search result has none of.
            subtitle: c.detail.clone().unwrap_or_else(|| c.reasons.join("\n")),
            confidence: Some(c.confidence),
        }
    }

    /// A release this exact disc belongs to, from the disc id.
    ///
    /// Certain in a way a searched release is not - the disc said so - so it
    /// says what pressing it is rather than how many tracks it claims.
    pub fn of_album(a: &riplika_core::identify::music::Album) -> Choice {
        Choice {
            title: format!("{} - {}", a.artist, a.title),
            subtitle: a.detail(),
            confidence: None,
        }
    }

    /// A release, as MusicBrainz answered a search by name.
    ///
    /// No confidence shown. The catalogue scores how well the *name* matched,
    /// and putting that on a row invites reading it as how sure it is that
    /// this is the disc in the drive - which a name search cannot know at all.
    /// What can tell them apart is the year, the country, the format and the
    /// track count, so those are the subtitle.
    pub fn of_release(m: &Match) -> Choice {
        Choice {
            title: format!("{} - {}", m.artist, m.title),
            subtitle: m.detail(),
            confidence: None,
        }
    }
}

/// What the dialog asks, and how it offers a way out when nothing matches.
pub struct Prompt {
    /// The question at the top.
    pub title: String,
    /// Offered when nothing fits: use what was typed, explained by this.
    ///
    /// Nothing where a bare name is no use. A show named by hand still rips
    /// into numbered episodes; an album named by hand has no track listing, so
    /// there would be nothing to write the files from.
    pub use_typed: Option<String>,
}

/// The parts of an open picker the window needs to reach, so results arriving
/// from a search can be put into the list that is currently on screen.
pub struct Picker {
    pub dialog: adw::Dialog,
    pub list: gtk::ListBox,
    /// What was typed, so a name with no match can still be used.
    search: gtk::SearchEntry,
    on_use: std::rc::Rc<dyn Fn(String)>,
    /// How this dialog explains using the typed name, or nothing if it does
    /// not offer that at all.
    use_typed: Option<String>,
}

impl Picker {
    /// Replace the contents of the list.
    pub fn show(&self, choices: &[Choice], on_choose: impl Fn(usize) + 'static) {
        self.clear();
        if choices.is_empty() {
            let row = rows::action()
                .title(tr("Nothing found"))
                .subtitle(tr("Try a different spelling, or part of the title"))
                .build();
            row.set_sensitive(false);
            self.list.append(&row);
            self.offer_the_typed_name();
            return;
        }
        let chooser = std::rc::Rc::new(on_choose);
        for (i, c) in choices.iter().enumerate() {
            let row =
                rows::action().title(&c.title).subtitle(&c.subtitle).activatable(true).build();
            if let Some(confidence) = c.confidence {
                let pct = gtk::Label::new(Some(&format!("{:.0}%", confidence * 100.0)));
                pct.add_css_class("dim-label");
                row.add_suffix(&pct);
            }
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            let chooser = std::rc::Rc::clone(&chooser);
            row.connect_activated(move |_| chooser(i));
            self.list.append(&row);
        }
        // Results that are all wrong are as much of a dead end as no results,
        // so the way out is offered either way.
        self.offer_the_typed_name();
    }

    /// Offer to use what was typed, with no catalogue behind it.
    ///
    /// The catalogues do not have everything, and a disc they cannot name is
    /// still a disc worth ripping: episodes come out numbered but not named,
    /// which can be corrected, where an unread disc cannot.
    fn offer_the_typed_name(&self) {
        let typed = self.search.text().trim().to_string();
        let Some(explanation) = self.use_typed.clone() else {
            return;
        };
        if typed.is_empty() {
            return;
        }
        let row = rows::action()
            .title(tr("Use this name anyway"))
            .subtitle(&explanation)
            .activatable(true)
            .build();
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        let on_use = std::rc::Rc::clone(&self.on_use);
        row.connect_activated(move |_| on_use(typed.clone()));
        self.list.append(&row);
    }

    /// Say that a search is under way, where its answer will appear.
    pub fn show_searching(&self) {
        self.clear();
        let row = rows::action().title(tr("Searching...")).build();
        row.add_suffix(&gtk::Spinner::builder().spinning(true).build());
        row.set_sensitive(false);
        self.list.append(&row);
    }

    /// Say why there is nothing to show, in the list rather than in a toast.
    ///
    /// A toast belongs to the window and this dialog is over the top of it, so
    /// a search that failed while this was open said so behind it. The list is
    /// where the answer was going to appear and where somebody who just
    /// searched is looking.
    pub fn show_problem(&self, why: &str) {
        self.clear();
        let row = rows::action().title(tr("That search did not work")).subtitle(why).build();
        row.set_sensitive(false);
        self.list.append(&row);
        self.offer_the_typed_name();
    }

    fn clear(&self) {
        while let Some(r) = self.list.row_at_index(0) {
            self.list.remove(&r);
        }
    }

    pub fn close(&self) {
        self.dialog.close();
    }
}

/// Open the picker.
///
/// `on_search` runs when the user asks for a different title; results come back
/// through the window, which calls [`Picker::show`] on the returned handle.
pub fn present(
    parent: &impl IsA<gtk::Widget>,
    prompt: Prompt,
    query: &str,
    on_search: impl Fn(String) + 'static,
    on_use: impl Fn(String) + 'static,
) -> Picker {
    let dialog =
        adw::Dialog::builder().title(&prompt.title).content_width(520).content_height(620).build();

    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();

    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search the catalogues by title")
        .text(query)
        .build();
    // Searching on every keystroke would fire a network request per letter, so
    // it waits for a pause - or for Return, which is what a person will press.
    search.set_search_delay(400);

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(vec!["boxed-list".to_string()])
        .build();
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();

    body.append(&search);
    body.append(&scroll);
    view.set_content(Some(&body));
    dialog.set_child(Some(&view));

    let picker = Picker {
        dialog,
        list,
        search: search.clone(),
        on_use: std::rc::Rc::new(on_use),
        use_typed: prompt.use_typed,
    };
    {
        // Long enough that typing a name is one request rather than one per
        // word. MusicBrainz allows one a second and refuses the rest, and a
        // refusal reads to somebody typing like the record not existing.
        search.set_search_delay(700);
        let run = std::rc::Rc::new(on_search);
        let r = std::rc::Rc::clone(&run);
        search.connect_search_changed(move |e| {
            let q = e.text().to_string();
            if !q.trim().is_empty() {
                r(q);
            }
        });
        search.connect_activate(move |e| {
            let q = e.text().to_string();
            if !q.trim().is_empty() {
                run(q);
            }
        });
    }

    picker.dialog.present(Some(parent));
    search.grab_focus();
    picker
}

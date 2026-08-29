//! Choosing which show a disc is.
//!
//! The alternatives are only interesting while you are choosing between them.
//! Left on the page they are a list of things you have already rejected, and
//! they push everything that actually needs answering - the season, the disc
//! number - further down. So the page states what it settled on, and the
//! alternatives live in a dialog you open when that is wrong.

use crate::i18n::tr;
use adw::prelude::*;
use riplika_core::model::Candidate;

/// The parts of an open picker the window needs to reach, so results arriving
/// from a search can be put into the list that is currently on screen.
pub struct Picker {
    pub dialog: adw::Dialog,
    pub list: gtk::ListBox,
}

impl Picker {
    /// Replace the contents of the list.
    pub fn show(&self, candidates: &[Candidate], on_choose: impl Fn(usize) + 'static) {
        self.clear();
        if candidates.is_empty() {
            let row = adw::ActionRow::builder()
                .title(tr("Nothing found"))
                .subtitle(tr("Try a different spelling, or part of the title"))
                .build();
            row.set_sensitive(false);
            self.list.append(&row);
            return;
        }
        let chooser = std::rc::Rc::new(on_choose);
        for (i, c) in candidates.iter().enumerate() {
            // What the work is, not that a search happened: the reasons are
            // evidence about *this disc*, which a search result has none of.
            let subtitle = c.detail.clone().unwrap_or_else(|| c.reasons.join("\n"));
            let row = adw::ActionRow::builder()
                .title(c.media.describe_work())
                .subtitle(&subtitle)
                .activatable(true)
                .build();
            let pct = gtk::Label::new(Some(&format!("{:.0}%", c.confidence * 100.0)));
            pct.add_css_class("dim-label");
            row.add_suffix(&pct);
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            let chooser = std::rc::Rc::clone(&chooser);
            row.connect_activated(move |_| chooser(i));
            self.list.append(&row);
        }
    }

    /// Say that a search is under way, where its answer will appear.
    pub fn show_searching(&self) {
        self.clear();
        let row = adw::ActionRow::builder().title(tr("Searching...")).build();
        row.add_suffix(&gtk::Spinner::builder().spinning(true).build());
        row.set_sensitive(false);
        self.list.append(&row);
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
    query: &str,
    on_search: impl Fn(String) + 'static,
) -> Picker {
    let dialog = adw::Dialog::builder()
        .title(tr("Which show is this?"))
        .content_width(520)
        .content_height(620)
        .build();

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

    let picker = Picker { dialog, list };
    {
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

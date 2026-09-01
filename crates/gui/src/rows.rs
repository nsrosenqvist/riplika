//! Rows whose text is data rather than markup.
//!
//! An AdwPreferencesRow parses its title and subtitle as Pango markup, and
//! everything this window puts in one is a file name, a show title, a language
//! or a path. "Parks and Recreation - S07E02 - Ron & Jammy.mp4" is not markup:
//! Pango stopped at the ampersand, threw the whole string away and drew a row
//! with no title at all, so an episode that had been written perfectly well
//! looked like it had gone missing.
//!
//! Blank is the worst possible failure for this - louder than a crash would
//! have been, because it says nothing and looks like data loss. So it is turned
//! off where a row is made, not remembered at each of the places one is filled
//! in, and `check.sh` keeps the builders here where the rule cannot be missed.
//!
//! Nothing in this window wants markup. If something ever does, it needs its
//! own constructor here saying so, and the text going into it has to be escaped
//! at the point it stops being data.

pub fn action() -> adw::builders::ActionRowBuilder {
    adw::ActionRow::builder().use_markup(false)
}

pub fn switch() -> adw::builders::SwitchRowBuilder {
    adw::SwitchRow::builder().use_markup(false)
}

pub fn combo() -> adw::builders::ComboRowBuilder {
    adw::ComboRow::builder().use_markup(false)
}

pub fn entry() -> adw::builders::EntryRowBuilder {
    adw::EntryRow::builder().use_markup(false)
}

pub fn password() -> adw::builders::PasswordEntryRowBuilder {
    adw::PasswordEntryRow::builder().use_markup(false)
}

pub fn expander() -> adw::builders::ExpanderRowBuilder {
    adw::ExpanderRow::builder().use_markup(false)
}

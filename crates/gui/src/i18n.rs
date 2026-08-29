//! Translation.
//!
//! Every string a person reads goes through [`tr`], so that `xgettext` can find
//! it and a translator can change it without touching Rust. Doing this after
//! the fact means walking the whole window and deciding, string by string, what
//! is prose and what is a device path - so it is worth doing before there is
//! more of it.
//!
//! English is a translation like any other. Keeping a `po/en.po` even though the
//! source strings are already English is what proves the machinery works: if
//! the catalogue is not being found, English is what silently keeps working and
//! nothing else does.

/// The catalogue these strings live in.
pub const DOMAIN: &str = "riplika";

/// Point gettext at the catalogues and set the domain.
///
/// Called once, before any string is asked for. An installed application finds
/// its catalogues under the usual prefix; one being run from a build directory
/// finds them beside the binary, so a translation can be tried without
/// installing anything.
pub fn init() {
    use gettextrs::{bind_textdomain_codeset, bindtextdomain, setlocale, textdomain, LocaleCategory};

    // Reads the environment's idea of the locale. Unsafe because it mutates
    // process-global state, which is why it happens once, at startup, before
    // any thread exists to race it.
    unsafe { setlocale(LocaleCategory::LcAll, "") };

    let candidates = [
        std::env::var_os("RIPLIKA_LOCALE_DIR").map(std::path::PathBuf::from),
        std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|p| p.join("locale"))),
        Some(std::path::PathBuf::from("/app/share/locale")),
        Some(std::path::PathBuf::from("/usr/share/locale")),
    ];
    for dir in candidates.into_iter().flatten() {
        if dir.is_dir() {
            let _ = bindtextdomain(DOMAIN, dir);
            break;
        }
    }
    let _ = bind_textdomain_codeset(DOMAIN, "UTF-8");
    let _ = textdomain(DOMAIN);
}

/// A translated string.
///
/// Named `tr` rather than `gettext` so it stays short enough to be used
/// everywhere without the call site becoming mostly ceremony.
pub fn tr(s: &str) -> String {
    gettextrs::gettext(s)
}

/// A translated string with a plural form.
///
/// A separate call because the rule differs by language: some have one form,
/// some two, some six, and choosing between them in Rust with an `if` would be
/// wrong everywhere except English.
pub fn tr_n(singular: &str, plural: &str, n: u32) -> String {
    gettextrs::ngettext(singular, plural, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_string_survives_with_no_catalogue_loaded() {
        // the failure mode that matters: if the catalogue is missing, the
        // source string is what a person should still read
        assert_eq!(tr("Analyse disc"), "Analyse disc");
    }

    #[test]
    fn plurals_pick_a_form() {
        assert_eq!(tr_n("%d file", "%d files", 1), "%d file");
        assert_eq!(tr_n("%d file", "%d files", 4), "%d files");
    }

    #[test]
    fn initialising_twice_is_harmless() {
        init();
        init();
        assert_eq!(tr("Eject"), "Eject");
    }
}

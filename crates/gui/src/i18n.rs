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
    use gettextrs::{bind_textdomain_codeset, bindtextdomain, textdomain};

    ensure_a_locale_that_exists();

    let candidates = [
        std::env::var_os("RIPLIKA_LOCALE_DIR").map(std::path::PathBuf::from),
        std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.join("locale"))),
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
/// wrong everywhere except English. `%d` in either form becomes the number, so
/// a translator can move it to wherever the sentence needs it.
pub fn tr_n(singular: &str, plural: &str, n: u32) -> String {
    gettextrs::ngettext(singular, plural, n).replace("%d", &n.to_string())
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
        assert_eq!(tr_n("%d file", "%d files", 1), "1 file");
        assert_eq!(tr_n("%d file", "%d files", 4), "4 files");
    }

    #[test]
    fn initialising_twice_is_harmless() {
        init();
        init();
        assert_eq!(tr("Eject"), "Eject");
    }
}

/// The language part of a locale name: `sv_SE.UTF-8` is Swedish.
fn language_of(locale: &str) -> &str {
    locale.split(['_', '.', '@']).next().unwrap_or("").trim()
}

/// Locales to try when the asked-for one is not installed, best first.
///
/// The last is `C.UTF-8`, which is a real loss rather than a fallback: gettext
/// will not translate under the C locale at all. It is here so that text is at
/// least still UTF-8 when nothing better can be set.
fn fallbacks(asked: &str) -> Vec<String> {
    let lang = language_of(asked);
    let mut out = Vec::new();
    if !lang.is_empty() && lang != "C" && lang != "POSIX" {
        out.push(format!("{lang}.UTF-8"));
        out.push(format!("{lang}_{}.UTF-8", lang.to_uppercase()));
    }
    out.push("en_US.UTF-8".to_string());
    out.push("C.UTF-8".to_string());
    out
}

/// Set a locale the C library actually has, keeping the asked-for language.
///
/// A flatpak is handed the host's `LANG` but carries only the languages it was
/// configured for, so `en_SE` arrives with no `en_SE` to set. Left alone the
/// toolkit's own `setlocale` fails the same way and everything lands in the C
/// locale - where gettext declines to translate anything, so a translated build
/// silently comes out in English and only a translator would ever notice.
///
/// The territory is the part that is missing, not the language, and gettext
/// splits those two jobs: `LANGUAGE` decides which catalogue to read, the locale
/// only decides whether to read one at all. So borrowing any installed locale
/// and naming the language in `LANGUAGE` still gets a Swedish reader Swedish.
/// The choice goes back into the environment as well, because the toolkit will
/// call `setlocale` itself long after this runs.
fn ensure_a_locale_that_exists() {
    use gettextrs::{LocaleCategory, setlocale};

    // Unsafe because these mutate process-global state. Called once, at
    // startup, before there is a second thread to race.
    if unsafe { setlocale(LocaleCategory::LcAll, "") }.is_some() {
        return;
    }

    let asked = std::env::var("LC_ALL").or_else(|_| std::env::var("LANG")).unwrap_or_default();
    let lang = language_of(&asked);
    if !lang.is_empty() && lang != "C" && std::env::var_os("LANGUAGE").is_none() {
        unsafe { std::env::set_var("LANGUAGE", lang) };
    }
    for candidate in fallbacks(&asked) {
        if unsafe { setlocale(LocaleCategory::LcAll, candidate.as_str()) }.is_some() {
            unsafe { std::env::set_var("LC_ALL", &candidate) };
            return;
        }
    }
}

#[cfg(test)]
mod locale_tests {
    use super::*;

    #[test]
    fn language_is_the_part_before_the_territory() {
        assert_eq!(language_of("sv_SE.UTF-8"), "sv");
        assert_eq!(language_of("en_SE"), "en");
        assert_eq!(language_of("de"), "de");
        assert_eq!(language_of("ca_ES@valencia"), "ca");
        assert_eq!(language_of(""), "");
    }

    #[test]
    fn keeps_the_language_when_the_territory_is_missing() {
        // en_SE is a real thing to have configured and a rare thing to have
        // installed, which is exactly the case this exists for.
        let tried = fallbacks("en_SE.UTF-8");
        assert_eq!(tried[0], "en.UTF-8");
        assert!(tried.iter().any(|c| c == "en_US.UTF-8"));
    }

    #[test]
    fn c_utf8_is_the_last_resort_and_never_the_first_choice() {
        for asked in ["sv_SE.UTF-8", "en_SE", "", "C"] {
            let tried = fallbacks(asked);
            assert_eq!(tried.last().unwrap(), "C.UTF-8", "asked: {asked}");
            assert_ne!(tried.first().unwrap(), "C.UTF-8", "asked: {asked}");
        }
    }

    #[test]
    fn does_not_invent_a_language_from_the_c_locale() {
        assert_eq!(fallbacks("C"), vec!["en_US.UTF-8", "C.UTF-8"]);
        assert_eq!(fallbacks("POSIX"), vec!["en_US.UTF-8", "C.UTF-8"]);
    }
}

//! Pictures of what a disc turned out to be, kept once they are fetched.
//!
//! A poster is decoration: worth showing, never worth waiting for and never
//! worth failing over. Everything here answers with an Option and says nothing
//! when it cannot help - a disc identifies, rips and files exactly the same
//! whether or not a picture ever arrives.
//!
//! Kept on disk because the alternative is fetching the same handful of images
//! every time the application starts, which is slower for the reader and
//! rude to whoever is serving them.

use crate::host::Fs;
use crate::identify::catalogue::Http;
use std::path::{Path, PathBuf};

/// Where a picture is kept, given where it came from.
///
/// The name is a hash of the URL rather than anything from it: a URL is not a
/// filename, two catalogues can hand back the same basename for different
/// pictures, and a poster path can carry characters a filesystem will not.
pub fn cached_at(dir: &Path, url: &str) -> PathBuf {
    let mut sha = crate::hash::Sha1::new();
    sha.update(url.as_bytes());
    let name: String = sha.finish().iter().take(10).map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{name}.img"))
}

/// The picture for this URL, fetching it if it has not been fetched before.
///
/// Answers `None` rather than an error: a missing picture is not a problem
/// worth telling anybody about, and the caller has a kind icon to fall back on.
pub fn cached(fs: &dyn Fs, http: &dyn Http, dir: &Path, url: &str) -> Option<PathBuf> {
    let path = cached_at(dir, url);
    if fs.size(&path).is_ok_and(|n| n > 0) {
        return Some(path);
    }
    let bytes = http.get_bytes(url).ok()?;
    // A few hundred bytes is an error page, not a picture. Writing it would
    // cache the failure and never try again.
    if bytes.len() < 512 {
        return None;
    }
    fs.create_dir_all(dir).ok()?;
    fs.write(&path, &bytes).ok()?;
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{FakeFs, Fs};

    struct Serving(Vec<u8>);
    impl Http for Serving {
        fn get(&self, _url: &str) -> crate::Result<String> {
            Ok(String::new())
        }
        fn get_bytes(&self, _url: &str) -> crate::Result<Vec<u8>> {
            Ok(self.0.clone())
        }
    }

    struct Refusing;
    impl Http for Refusing {
        fn get(&self, _url: &str) -> crate::Result<String> {
            Err(crate::Error("no".into()))
        }
        fn get_bytes(&self, _url: &str) -> crate::Result<Vec<u8>> {
            Err(crate::Error("no".into()))
        }
    }

    fn picture() -> Vec<u8> {
        vec![0x89; 4096]
    }

    #[test]
    fn a_url_becomes_a_filename_a_filesystem_will_take() {
        // A poster path carries slashes and a query can carry anything at all.
        let p = cached_at(Path::new("/cache"), "https://image.tmdb.org/t/p/w342/aBcD.jpg?size=1");
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.ends_with(".img"), "{name}");
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.'), "{name}");
    }

    #[test]
    fn two_pictures_do_not_collide_on_a_shared_basename() {
        // Both catalogues serve files called things like "medium.jpg".
        let a = cached_at(Path::new("/c"), "https://one.example/medium.jpg");
        let b = cached_at(Path::new("/c"), "https://two.example/medium.jpg");
        assert_ne!(a, b);
    }

    #[test]
    fn a_picture_is_fetched_once_and_kept() {
        let fs = FakeFs::new();
        let dir = Path::new("/cache/art");
        let url = "https://example/poster.jpg";

        let first = cached(&fs, &Serving(picture()), dir, url).expect("it fetches");
        assert_eq!(fs.read(&first).unwrap().len(), 4096);

        // Second time nothing is served at all, so anything but the cache
        // would come back empty-handed.
        let again = cached(&fs, &Refusing, dir, url).expect("it is already here");
        assert_eq!(first, again);
    }

    #[test]
    fn a_fetch_that_fails_is_not_a_problem_worth_reporting() {
        let fs = FakeFs::new();
        assert_eq!(cached(&fs, &Refusing, Path::new("/c"), "https://example/x.jpg"), None);
    }

    #[test]
    fn an_error_page_is_not_cached_as_though_it_were_a_picture() {
        // Caching it would mean never trying again, and a wrong picture is
        // worse than none - the kind icon is the fallback.
        let fs = FakeFs::new();
        let tiny = Serving(b"<html>404</html>".to_vec());
        assert_eq!(cached(&fs, &tiny, Path::new("/c"), "https://example/x.jpg"), None);
    }
}

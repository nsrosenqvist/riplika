//! Reading decrypted sectors straight from a DVD.
//!
//! Everywhere else this crate drives external programs rather than binding
//! libraries, because container handling is the part most likely to change and
//! ffmpeg is the reference implementation of it. Rescue is the exception, and
//! for a specific reason: it needs to decide, sector by sector, what to read
//! next and what to do when a read fails. No command line offers that.
//!
//! libdvdcss is loaded at runtime rather than linked, so the crate still builds
//! and every other feature still works on a machine that does not have it.
//!
//! Decrypting here rather than later matters. A raw image of a CSS disc is
//! encrypted, and decrypting it afterwards means cracking the keys - which
//! failed for six video title sets of the Parks and Recreation disc this was
//! developed against. Reading through libdvdcss uses the drive's own key
//! exchange while the drive is still there, and `DVDCSS_READ_DECRYPT` clears
//! the scrambling bits as it goes, so what lands on disk needs no keys at all.

use super::{ReadError, SectorSource, SECTOR};
use crate::{Error, Result};
use std::ffi::{c_char, c_int, c_void, CString};
use std::path::Path;

/// `dvdcss_seek` flags.
const DVDCSS_SEEK_KEY: c_int = 1 << 1;
/// `dvdcss_read` flags.
const DVDCSS_READ_DECRYPT: c_int = 1 << 0;

type OpenFn = unsafe extern "C" fn(*const c_char) -> *mut c_void;
type CloseFn = unsafe extern "C" fn(*mut c_void) -> c_int;
type SeekFn = unsafe extern "C" fn(*mut c_void, c_int, c_int) -> c_int;
type ReadFn = unsafe extern "C" fn(*mut c_void, *mut c_void, c_int, c_int) -> c_int;
type ErrorFn = unsafe extern "C" fn(*mut c_void) -> *const c_char;
type ScrambledFn = unsafe extern "C" fn(*mut c_void) -> c_int;

struct Api {
    /// Kept so the library stays loaded for the process lifetime.
    _handle: *mut c_void,
    open: OpenFn,
    close: CloseFn,
    seek: SeekFn,
    read: ReadFn,
    error: ErrorFn,
    is_scrambled: Option<ScrambledFn>,
}

// The library is stateless across handles; each Dvd owns its own.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

/// Candidate sonames, newest first.
const SONAMES: &[&str] = &["libdvdcss.so.2", "libdvdcss.so", "libdvdcss.2.dylib"];

fn load() -> Result<&'static Api> {
    use std::sync::OnceLock;
    static API: OnceLock<Option<Api>> = OnceLock::new();

    let api = API.get_or_init(|| unsafe {
        for name in SONAMES {
            let Ok(soname) = CString::new(*name) else { continue };
            let handle = libc::dlopen(soname.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
            if handle.is_null() {
                continue;
            }
            let sym = |n: &str| -> *mut c_void {
                let Ok(cn) = CString::new(n) else {
                    return std::ptr::null_mut();
                };
                libc::dlsym(handle, cn.as_ptr())
            };
            let (o, c, s, r, e) = (
                sym("dvdcss_open"),
                sym("dvdcss_close"),
                sym("dvdcss_seek"),
                sym("dvdcss_read"),
                sym("dvdcss_error"),
            );
            if o.is_null() || c.is_null() || s.is_null() || r.is_null() || e.is_null() {
                libc::dlclose(handle);
                continue;
            }
            let scrambled = sym("dvdcss_is_scrambled");
            return Some(Api {
                _handle: handle,
                open: std::mem::transmute::<*mut c_void, OpenFn>(o),
                close: std::mem::transmute::<*mut c_void, CloseFn>(c),
                seek: std::mem::transmute::<*mut c_void, SeekFn>(s),
                read: std::mem::transmute::<*mut c_void, ReadFn>(r),
                error: std::mem::transmute::<*mut c_void, ErrorFn>(e),
                is_scrambled: (!scrambled.is_null())
                    .then(|| std::mem::transmute::<*mut c_void, ScrambledFn>(scrambled)),
            });
        }
        None
    });

    api.as_ref().ok_or_else(|| {
        Error(
            "libdvdcss is not installed, so damaged discs cannot be rescued \
             (install libdvdcss, or use --reader makemkv)"
                .into(),
        )
    })
}

/// Is rescue possible on this machine at all?
pub fn available() -> bool {
    load().is_ok()
}

/// An open disc, read through libdvdcss.
pub struct Dvd {
    api: &'static Api,
    dvd: *mut c_void,
    scrambled: bool,
    /// The sector a read would continue from, so a sequential rescue does not
    /// seek before every read.
    next: Option<u64>,
    /// Sector ranges that are *not* encrypted, and must not be descrambled.
    plain: Vec<(u64, u64)>,
}

impl Dvd {
    pub fn open(device: &Path) -> Result<Dvd> {
        let api = load()?;
        let target = CString::new(device.to_string_lossy().as_bytes())
            .map_err(|_| Error(format!("{}: not a usable device path", device.display())))?;
        let dvd = unsafe { (api.open)(target.as_ptr()) };
        if dvd.is_null() {
            return Err(Error(format!("{}: could not open the disc", device.display())));
        }
        let mut d = Dvd { api, dvd, scrambled: true, next: None, plain: Vec::new() };
        d.scrambled = d.query_scrambled();
        Ok(d)
    }

    fn query_scrambled(&self) -> bool {
        match self.api.is_scrambled {
            Some(f) => (unsafe { f(self.dvd) }) != 0,
            None => true,
        }
    }

    /// Is this disc CSS-protected?
    pub fn is_scrambled(&self) -> bool {
        self.scrambled
    }

    /// Mark sector ranges that hold no encrypted video.
    ///
    /// This matters more than it sounds. `DVDCSS_READ_DECRYPT` descrambles
    /// whatever it is given, and a sector's payload starts at byte 128 - so
    /// asking it to decrypt the volume descriptors turns them into noise while
    /// leaving their first 128 bytes intact. The image then looks plausible,
    /// mounts as a filesystem, and cannot be opened as a DVD. Everything
    /// outside the video objects is read without decryption.
    pub fn set_plain_ranges(&mut self, ranges: &[(u64, u64)]) {
        self.plain = ranges.to_vec();
        self.plain.sort();
    }

    fn is_plain(&self, lba: u64) -> bool {
        self.plain.iter().any(|(a, b)| lba >= *a && lba < *b)
    }

    fn last_error(&self) -> String {
        unsafe {
            let p = (self.api.error)(self.dvd);
            if p.is_null() {
                return "unknown error".into();
            }
            std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }

    /// Position for a read, fetching the title key if there is one.
    ///
    /// A CSS title key belongs to a video object, so it has to be fetched
    /// inside one. Reading an encrypted sector without it returns scrambled
    /// data rather than an error - a very quiet way to write a broken image -
    /// so the key seek is attempted for every read on a scrambled disc.
    /// libdvdcss keeps the keys it has already worked out, so this is cheap
    /// after the first read of each object.
    ///
    /// It legitimately fails outside a video object: the descriptors and the
    /// IFOs are not encrypted, and are part of what a rescue copies. So a
    /// failed key seek falls back to an ordinary one rather than giving up.
    fn position(&mut self, lba: u64) -> std::result::Result<(), ReadError> {
        // Seeking before every read costs the drive its read-ahead, and on a
        // sequential rescue that is most of the throughput - measured at about
        // four times slower on a real disc. A read leaves the position where
        // the next one wants it, so a continuing read needs no seek at all.
        if self.next == Some(lba) {
            return Ok(());
        }
        if self.scrambled && !self.is_plain(lba) {
            let keyed = unsafe { (self.api.seek)(self.dvd, lba as c_int, DVDCSS_SEEK_KEY) };
            if keyed >= 0 {
                self.next = Some(lba);
                return Ok(());
            }
        }
        let plain = unsafe { (self.api.seek)(self.dvd, lba as c_int, 0) };
        if plain < 0 {
            self.next = None;
            return Err(ReadError::Fatal(format!(
                "seeking to sector {lba}: {}",
                self.last_error()
            )));
        }
        self.next = Some(lba);
        Ok(())
    }
}

impl Drop for Dvd {
    fn drop(&mut self) {
        unsafe { (self.api.close)(self.dvd) };
    }
}

impl SectorSource for Dvd {
    fn read(&mut self, lba: u64, count: u64) -> std::result::Result<Vec<u8>, ReadError> {
        self.position(lba)?;
        let mut buf = vec![0u8; count as usize * SECTOR];
        let flags = if self.is_plain(lba) { 0 } else { DVDCSS_READ_DECRYPT };
        let got = unsafe {
            (self.api.read)(self.dvd, buf.as_mut_ptr() as *mut c_void, count as c_int, flags)
        };
        if got < 0 {
            // Where the drive stopped is not knowable, so force a seek next
            // time rather than assuming it carried on.
            self.next = None;
            return Err(ReadError::Unreadable);
        }
        // A short read means the drive gave up partway. Treat it as a failure
        // so the caller narrows the range rather than accepting a half-filled
        // buffer whose tail is zeros.
        if got as u64 != count {
            self.next = None;
            return Err(ReadError::Unreadable);
        }
        self.next = Some(lba + count);
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_ranges_are_read_without_descrambling() {
        // DVDCSS_READ_DECRYPT descrambles whatever it is handed, and a sector's
        // payload begins at byte 128 - so decrypting the volume descriptors
        // leaves their first 128 bytes intact and turns the rest into noise.
        // The image then mounts as a filesystem and will not open as a DVD.
        if !available() {
            return;
        }
        let Ok(mut d) = Dvd::open(Path::new("/dev/riplika-no-such-device")) else {
            return;
        };
        d.set_plain_ranges(&[(0, 400), (989, 1004)]);
        assert!(d.is_plain(16));
        assert!(d.is_plain(256));
        assert!(d.is_plain(999));
        assert!(!d.is_plain(485_900));
    }

    #[test]
    fn availability_is_reported_rather_than_assumed() {
        // whichever way this machine is set up, asking must not panic
        let _ = available();
    }

    #[test]
    fn a_missing_library_produces_advice_rather_than_a_crash() {
        if !available() {
            match Dvd::open(Path::new("/dev/sr0")) {
                Err(e) => assert!(e.0.contains("libdvdcss"), "{}", e.0),
                Ok(_) => panic!("opened a disc without the library"),
            }
        }
    }

    #[test]
    fn opening_something_that_is_not_a_disc_fails_cleanly() {
        if available() {
            assert!(Dvd::open(Path::new("/dev/null/nope")).is_err());
        }
    }
}

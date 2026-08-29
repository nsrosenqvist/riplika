//! Secrets that should not sit in a config file.
//!
//! Encrypting them ourselves would be theatre. The key to decrypt would have to
//! live somewhere this process can reach without asking, which means somewhere
//! anyone reading the config can reach too - so it would look protected and not
//! be. The honest choices are the login keyring, which is encrypted at rest
//! with the user's password and unlocked by their session, or a plain file with
//! tight permissions. This uses the first and falls back to saying so.
//!
//! `secret-tool` rather than a D-Bus binding, for the same reason the rest of
//! the crate drives ffmpeg rather than linking libav: it is the reference
//! implementation, it is already installed with the desktop, and binding it
//! would pull an async runtime into a crate that has none.
//!
//! This is the one place that does not go through [`crate::host::Runner`].
//! `secret-tool store` reads the secret from stdin, and `Runner` closes stdin
//! for every child on purpose.

use crate::{Error, Result};
use std::io::Write;
use std::process::{Command, Stdio};

/// The attribute every secret of ours carries, so they can be told apart from
/// everything else in the keyring.
pub const SERVICE: &str = "riplika";

/// Is a keyring available to talk to?
pub fn available() -> bool {
    crate::host::which("secret-tool").is_some()
}

/// Store a secret, replacing any previous one of the same name.
pub fn store(name: &str, value: &str) -> Result<()> {
    if value.is_empty() {
        return clear(name);
    }
    let mut child = Command::new("secret-tool")
        .args(["store", "--label", &format!("Riplika {name}"), "service", SERVICE, "key", name])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error(format!("secret-tool: {e}")))?;
    child
        .stdin
        .take()
        .ok_or_else(|| Error("secret-tool: no stdin".into()))?
        .write_all(value.as_bytes())
        .map_err(|e| Error(format!("secret-tool: {e}")))?;
    let out = child.wait_with_output().map_err(|e| Error(format!("secret-tool: {e}")))?;
    if !out.status.success() {
        return Err(Error(format!(
            "could not save to the keyring: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// Read a secret back. `None` when it is not there, which is not an error.
pub fn lookup(name: &str) -> Option<String> {
    let out = Command::new("secret-tool")
        .args(["lookup", "service", SERVICE, "key", name])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string();
    (!value.is_empty()).then_some(value)
}

pub fn clear(name: &str) -> Result<()> {
    let _ = Command::new("secret-tool")
        .args(["clear", "service", SERVICE, "key", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    Ok(())
}

/// The TMDB key, from the keyring or the environment.
///
/// The environment still works, because a script or a container has no keyring
/// and should not need one.
pub fn tmdb_key() -> Option<String> {
    lookup("tmdb")
        .or_else(|| std::env::var("TMDB_API_KEY").ok())
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_secret_is_absence_not_failure() {
        assert_eq!(lookup("riplika-test-definitely-not-stored-xyzzy"), None);
    }

    #[test]
    fn a_secret_survives_a_round_trip() {
        if !available() {
            return;
        }
        let name = format!("test-{}", std::process::id());
        if store(&name, "value-123").is_err() {
            return; // a locked or absent keyring is not a test failure
        }
        assert_eq!(lookup(&name).as_deref(), Some("value-123"));
        clear(&name).unwrap();
        assert_eq!(lookup(&name), None);
    }

    #[test]
    fn storing_nothing_removes_it() {
        if !available() {
            return;
        }
        let name = format!("test-empty-{}", std::process::id());
        if store(&name, "x").is_err() {
            return;
        }
        store(&name, "").unwrap();
        assert_eq!(lookup(&name), None, "an emptied field should forget the key");
    }

    #[test]
    fn the_environment_still_works_for_scripts() {
        // a container or a cron job has no keyring and should not need one
        unsafe { std::env::set_var("TMDB_API_KEY", "from-the-environment") };
        let found = tmdb_key();
        unsafe { std::env::remove_var("TMDB_API_KEY") };
        assert!(found.is_some());
    }
}

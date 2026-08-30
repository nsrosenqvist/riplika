//! The boundary between the pipeline and the programs it drives.
//!
//! Everything that leaves this process goes through [`Command`] and [`Runner`].
//! That buys two things. A test can substitute [`FakeRunner`] and assert on the
//! exact argv a planner produced, which is the only way to catch an argument
//! that is subtly wrong but not an error - a dropped `-map`, a `-disposition`
//! index one too high. And every invocation gets the same handling of the
//! things that are easy to forget, chief among them stdin.
//!
//! stdin is not a detail. ffmpeg and HandBrake both read it looking for
//! keypresses, and inside a loop that reads a list they will happily consume
//! the rest of that list. The shell version of this pipeline processed one
//! episode out of eight and reported success. `Runner` closes stdin for every
//! child, so that cannot happen again.

use crate::{Error, Result};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// A program and its arguments, built but not yet run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
    /// Environment overrides for the child.
    ///
    /// Set on the command rather than on the process because these are
    /// per-invocation decisions - `DVDCSS_METHOD` matters for reading a disc
    /// and nothing else - and a process-wide setenv is not thread-safe.
    pub env: Vec<(String, String)>,
}

impl Command {
    pub fn new(program: impl Into<String>) -> Self {
        Command { program: program.into(), args: Vec::new(), env: Vec::new() }
    }

    /// Set an environment variable for this invocation only.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }

    pub fn args<I, S>(mut self, it: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(it.into_iter().map(Into::into));
        self
    }

    pub fn path(self, p: &Path) -> Self {
        self.arg(p.to_string_lossy().to_string())
    }

    /// Push `a` only when `cond` holds - the common shape in a planner.
    pub fn arg_if(self, cond: bool, a: impl Into<String>) -> Self {
        if cond { self.arg(a) } else { self }
    }

    /// True when `needle` appears as a whole argument.
    pub fn has(&self, needle: &str) -> bool {
        self.args.iter().any(|a| a == needle)
    }

    /// The argument following `flag`, if any.
    pub fn value_of(&self, flag: &str) -> Option<&str> {
        let i = self.args.iter().position(|a| a == flag)?;
        self.args.get(i + 1).map(String::as_str)
    }

    /// Every value given for a repeated flag, e.g. all `-map` targets.
    pub fn values_of(&self, flag: &str) -> Vec<&str> {
        let mut out = Vec::new();
        for (i, a) in self.args.iter().enumerate() {
            if a == flag
                && let Some(v) = self.args.get(i + 1)
            {
                out.push(v.as_str());
            }
        }
        out
    }

    /// Shell-ish rendering, for logs and for showing the user what ran.
    pub fn display(&self) -> String {
        let quote = |s: &String| {
            if s.is_empty() || s.contains([' ', '\'', '"', '*', '?', '$']) {
                format!("'{}'", s.replace('\'', r"'\''"))
            } else {
                s.clone()
            }
        };
        self.env
            .iter()
            .map(|(k, v)| format!("{k}={}", quote(v)))
            .chain(std::iter::once(quote(&self.program)))
            .chain(self.args.iter().map(quote))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.status == 0
    }

    /// The last non-empty stderr line, which is where these tools put the
    /// actual complaint.
    pub fn last_error(&self) -> &str {
        self.stderr.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("no output")
    }
}

/// A shared flag the user can raise to stop a running job.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() { Err(Error("cancelled".into())) } else { Ok(()) }
    }
}

/// Runs commands. The only way out of the process.
pub trait Runner: Send + Sync {
    /// Run to completion, capturing output.
    fn run(&self, cmd: &Command) -> Result<Output>;

    /// Run while feeding merged output lines to `on_line`, for progress.
    ///
    /// The default implementation just runs and replays the output, which is
    /// enough for fakes and for tools with nothing useful to say as they go.
    fn stream(&self, cmd: &Command, on_line: &mut dyn FnMut(&str)) -> Result<Output> {
        let out = self.run(cmd)?;
        for line in out.stdout.lines().chain(out.stderr.lines()) {
            on_line(line);
        }
        Ok(out)
    }

    /// Has the user asked for this to stop?
    ///
    /// A cancelled command fails like any other, and a caller that treats a
    /// failure as damage will faithfully record every remaining item as broken
    /// and carry on trying. Asking lets it tell the two apart.
    fn cancelled(&self) -> bool {
        false
    }

    /// Run, and turn a non-zero exit into an error.
    fn require(&self, cmd: &Command) -> Result<Output> {
        let out = self.run(cmd)?;
        if !out.ok() {
            return Err(Error(format!(
                "{} failed ({}): {}",
                cmd.program,
                out.status,
                out.last_error()
            )));
        }
        Ok(out)
    }
}

/// Actually spawns processes.
#[derive(Debug, Default, Clone)]
pub struct RealRunner {
    pub cancel: Cancel,
}

impl RealRunner {
    pub fn new(cancel: Cancel) -> Self {
        RealRunner { cancel }
    }

    fn build(&self, cmd: &Command) -> StdCommand {
        let mut c = StdCommand::new(&cmd.program);
        c.args(&cmd.args);
        for (k, v) in &cmd.env {
            c.env(k, v);
        }
        // Never let a child eat our stdin: see the module comment.
        c.stdin(Stdio::null());
        c
    }
}

impl Runner for RealRunner {
    fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    fn run(&self, cmd: &Command) -> Result<Output> {
        self.cancel.check()?;
        let out = self.build(cmd).output().map_err(|e| Error(format!("{}: {e}", cmd.program)))?;
        Ok(Output {
            status: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn stream(&self, cmd: &Command, on_line: &mut dyn FnMut(&str)) -> Result<Output> {
        self.cancel.check()?;
        let mut child = self
            .build(cmd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error(format!("{}: {e}", cmd.program)))?;

        // stderr on a thread: ffmpeg writes progress there while MakeMKV writes
        // it to stdout, and a full pipe on either one deadlocks the child.
        let stderr = child.stderr.take();
        let collected = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&collected);
        let pump = stderr.map(|e| {
            std::thread::spawn(move || {
                for line in BufReader::new(e).lines().map_while(std::result::Result::ok) {
                    let mut s = sink.lock().unwrap();
                    s.push_str(&line);
                    s.push('\n');
                }
            })
        });

        let mut stdout = String::new();
        if let Some(o) = child.stdout.take() {
            for line in BufReader::new(o).lines().map_while(std::result::Result::ok) {
                if self.cancel.is_cancelled() {
                    let _ = child.kill();
                    break;
                }
                on_line(&line);
                stdout.push_str(&line);
                stdout.push('\n');
            }
        }
        let status = child.wait().map_err(|e| Error(format!("{}: {e}", cmd.program)))?;
        if let Some(p) = pump {
            let _ = p.join();
        }
        let stderr = collected.lock().unwrap().clone();
        for line in stderr.lines() {
            on_line(line);
        }
        self.cancel.check()?;
        Ok(Output { status: status.code().unwrap_or(-1), stdout, stderr })
    }
}

/// Records what it was asked to run and replays canned answers.
///
/// Matching is by substring against the rendered command line, longest pattern
/// first, so a test can pin down one specific ffprobe call without having to
/// describe every other call the code makes.
#[derive(Debug, Default)]
pub struct FakeRunner {
    responses: Mutex<Vec<(String, Output)>>,
    calls: Mutex<Vec<Command>>,
    default: Mutex<Output>,
}

impl FakeRunner {
    pub fn new() -> Self {
        FakeRunner {
            responses: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            default: Mutex::new(Output { status: 0, stdout: String::new(), stderr: String::new() }),
        }
    }

    /// Answer any command whose rendering contains `pattern` with `stdout`.
    pub fn on(self, pattern: &str, stdout: &str) -> Self {
        self.responses.lock().unwrap().push((
            pattern.to_string(),
            Output { status: 0, stdout: stdout.to_string(), stderr: String::new() },
        ));
        self
    }

    /// Answer matching commands with a failure.
    pub fn fail(self, pattern: &str, stderr: &str) -> Self {
        self.responses.lock().unwrap().push((
            pattern.to_string(),
            Output { status: 1, stdout: String::new(), stderr: stderr.to_string() },
        ));
        self
    }

    /// Every command run so far, in order.
    pub fn calls(&self) -> Vec<Command> {
        self.calls.lock().unwrap().clone()
    }

    /// The commands that invoked `program`.
    pub fn calls_to(&self, program: &str) -> Vec<Command> {
        self.calls().into_iter().filter(|c| c.program == program).collect()
    }

    /// The single command matching `pattern`, panicking unless there is exactly
    /// one - an assertion in its own right.
    pub fn only_call(&self, pattern: &str) -> Command {
        let hits: Vec<Command> =
            self.calls().into_iter().filter(|c| c.display().contains(pattern)).collect();
        assert_eq!(
            hits.len(),
            1,
            "expected exactly one command matching {pattern:?}, got {}:\n{}",
            hits.len(),
            self.calls().iter().map(|c| c.display()).collect::<Vec<_>>().join("\n")
        );
        hits.into_iter().next().unwrap()
    }
}

impl Runner for FakeRunner {
    fn run(&self, cmd: &Command) -> Result<Output> {
        self.calls.lock().unwrap().push(cmd.clone());
        let line = cmd.display();
        let responses = self.responses.lock().unwrap();
        let mut best: Option<&(String, Output)> = None;
        for r in responses.iter() {
            if line.contains(&r.0) && best.is_none_or(|b| r.0.len() > b.0.len()) {
                best = Some(r);
            }
        }
        Ok(best.map(|(_, o)| o.clone()).unwrap_or_else(|| self.default.lock().unwrap().clone()))
    }
}

/// Look for an executable along a `PATH`-style list.
///
/// Split out from [`which`] so it can be tested: the interesting cases are a
/// name that exists but is not executable, and an empty entry meaning the
/// current directory, and neither is comfortable to arrange for real.
pub fn search_path(
    path_var: &str,
    program: &str,
    is_executable: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    // An absolute or relative path is used as given, not searched for.
    if program.contains('/') {
        let p = PathBuf::from(program);
        return is_executable(&p).then_some(p);
    }
    for dir in path_var.split(':') {
        // POSIX says an empty entry means the current directory
        let dir = if dir.is_empty() { "." } else { dir };
        let candidate = Path::new(dir).join(program);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Is this program installed?
///
/// Used to decide whether an option can be offered at all: a MakeMKV fallback
/// that is switched on but not installed is a promise the application cannot
/// keep, and it would only be discovered forty minutes into a disc.
pub fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    search_path(&path, program, &|p| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(p)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            p.is_file()
        }
    })
}

/// A filesystem, so planners and the job runner can be tested without touching
/// disk. Only the handful of operations the pipeline actually needs.
pub trait Fs: Send + Sync {
    fn exists(&self, p: &Path) -> bool;
    fn create_dir_all(&self, p: &Path) -> Result<()>;
    fn read(&self, p: &Path) -> Result<Vec<u8>>;
    fn write(&self, p: &Path, data: &[u8]) -> Result<()>;
    /// Read part of a file. A produced episode is gigabytes, so anything that
    /// wants a header cannot be made to hold the whole thing to get it.
    fn read_range(&self, p: &Path, offset: u64, len: usize) -> Result<Vec<u8>>;
    /// Overwrite bytes in place, changing nothing else and no length.
    fn write_at(&self, p: &Path, offset: u64, data: &[u8]) -> Result<()>;
    /// Add to the end, leaving every existing byte where it was.
    fn append(&self, p: &Path, data: &[u8]) -> Result<()>;
    fn remove_file(&self, p: &Path) -> Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> Result<()>;
    fn size(&self, p: &Path) -> Result<u64>;
    fn list(&self, p: &Path) -> Result<Vec<PathBuf>>;
}

#[derive(Debug, Default, Clone)]
pub struct RealFs;

impl Fs for RealFs {
    fn exists(&self, p: &Path) -> bool {
        p.exists()
    }
    fn create_dir_all(&self, p: &Path) -> Result<()> {
        std::fs::create_dir_all(p).map_err(|e| Error(format!("{}: {e}", p.display())))
    }
    fn read(&self, p: &Path) -> Result<Vec<u8>> {
        std::fs::read(p).map_err(|e| Error(format!("{}: {e}", p.display())))
    }
    fn write(&self, p: &Path, data: &[u8]) -> Result<()> {
        std::fs::write(p, data).map_err(|e| Error(format!("{}: {e}", p.display())))
    }
    fn read_range(&self, p: &Path, offset: u64, len: usize) -> Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(p).map_err(|e| Error(format!("{}: {e}", p.display())))?;
        f.seek(SeekFrom::Start(offset)).map_err(|e| Error(format!("{}: {e}", p.display())))?;
        let mut buf = vec![0u8; len];
        // A short read is the end of the file, not a failure: a caller asking
        // for a header does not know how much is there.
        let mut got = 0;
        while got < len {
            match f.read(&mut buf[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) => return Err(Error(format!("{}: {e}", p.display()))),
            }
        }
        buf.truncate(got);
        Ok(buf)
    }
    fn write_at(&self, p: &Path, offset: u64, data: &[u8]) -> Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(p)
            .map_err(|e| Error(format!("{}: {e}", p.display())))?;
        f.seek(SeekFrom::Start(offset)).map_err(|e| Error(format!("{}: {e}", p.display())))?;
        f.write_all(data).map_err(|e| Error(format!("{}: {e}", p.display())))
    }
    fn append(&self, p: &Path, data: &[u8]) -> Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(p)
            .map_err(|e| Error(format!("{}: {e}", p.display())))?;
        f.write_all(data).map_err(|e| Error(format!("{}: {e}", p.display())))
    }
    fn remove_file(&self, p: &Path) -> Result<()> {
        match std::fs::remove_file(p) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error(format!("{}: {e}", p.display()))),
        }
    }
    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        std::fs::rename(from, to)
            .map_err(|e| Error(format!("{} -> {}: {e}", from.display(), to.display())))
    }
    fn size(&self, p: &Path) -> Result<u64> {
        std::fs::metadata(p).map(|m| m.len()).map_err(|e| Error(format!("{}: {e}", p.display())))
    }
    fn list(&self, p: &Path) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(p).map_err(|e| Error(format!("{}: {e}", p.display())))? {
            out.push(e.map_err(|e| Error(e.to_string()))?.path());
        }
        out.sort();
        Ok(out)
    }
}

/// An in-memory filesystem for tests.
#[derive(Debug, Default)]
pub struct FakeFs {
    files: Mutex<HashMap<PathBuf, Vec<u8>>>,
    dirs: Mutex<Vec<PathBuf>>,
}

impl FakeFs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_file(self, p: impl Into<PathBuf>, data: &str) -> Self {
        self.files.lock().unwrap().insert(p.into(), data.as_bytes().to_vec());
        self
    }

    pub fn created_dirs(&self) -> Vec<PathBuf> {
        self.dirs.lock().unwrap().clone()
    }

    pub fn files(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self.files.lock().unwrap().keys().cloned().collect();
        v.sort();
        v
    }
}

impl Fs for FakeFs {
    fn exists(&self, p: &Path) -> bool {
        self.files.lock().unwrap().contains_key(p)
            || self.dirs.lock().unwrap().iter().any(|d| d == p)
    }
    fn create_dir_all(&self, p: &Path) -> Result<()> {
        self.dirs.lock().unwrap().push(p.to_path_buf());
        Ok(())
    }
    fn read(&self, p: &Path) -> Result<Vec<u8>> {
        self.files
            .lock()
            .unwrap()
            .get(p)
            .cloned()
            .ok_or_else(|| Error(format!("{}: not found", p.display())))
    }
    fn write(&self, p: &Path, data: &[u8]) -> Result<()> {
        self.files.lock().unwrap().insert(p.to_path_buf(), data.to_vec());
        Ok(())
    }
    fn read_range(&self, p: &Path, offset: u64, len: usize) -> Result<Vec<u8>> {
        let files = self.files.lock().unwrap();
        let all = files.get(p).ok_or_else(|| Error(format!("{}: not found", p.display())))?;
        let start = (offset as usize).min(all.len());
        let end = start.saturating_add(len).min(all.len());
        Ok(all[start..end].to_vec())
    }
    fn write_at(&self, p: &Path, offset: u64, data: &[u8]) -> Result<()> {
        let mut files = self.files.lock().unwrap();
        let all = files.get_mut(p).ok_or_else(|| Error(format!("{}: not found", p.display())))?;
        let start = offset as usize;
        if start + data.len() > all.len() {
            return Err(Error(format!("{}: write past the end", p.display())));
        }
        all[start..start + data.len()].copy_from_slice(data);
        Ok(())
    }
    fn append(&self, p: &Path, data: &[u8]) -> Result<()> {
        let mut files = self.files.lock().unwrap();
        let all = files.get_mut(p).ok_or_else(|| Error(format!("{}: not found", p.display())))?;
        all.extend_from_slice(data);
        Ok(())
    }
    fn remove_file(&self, p: &Path) -> Result<()> {
        self.files.lock().unwrap().remove(p);
        Ok(())
    }
    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        let mut f = self.files.lock().unwrap();
        let data = f.remove(from).unwrap_or_default();
        f.insert(to.to_path_buf(), data);
        Ok(())
    }
    fn size(&self, p: &Path) -> Result<u64> {
        Ok(self.read(p)?.len() as u64)
    }
    fn list(&self, p: &Path) -> Result<Vec<PathBuf>> {
        let f = self.files.lock().unwrap();
        let mut v: Vec<PathBuf> = f.keys().filter(|k| k.parent() == Some(p)).cloned().collect();
        v.sort();
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_quotes_only_what_needs_it() {
        let c = Command::new("ffmpeg").args(["-i", "/tmp/a b.mkv", "-map", "0:s:0"]);
        assert_eq!(c.display(), "ffmpeg -i '/tmp/a b.mkv' -map 0:s:0");
    }

    #[test]
    fn the_environment_shows_up_in_the_rendering_and_reaches_the_child() {
        let c = Command::new("ffprobe").env("DVDCSS_METHOD", "key").arg("-i");
        assert_eq!(c.display(), "DVDCSS_METHOD=key ffprobe -i");
        // and it really is passed on, not just displayed
        let out = RealRunner::default()
            .run(
                &Command::new("sh")
                    .args(["-c", "printf %s \"$RIPLIKA_TEST\""])
                    .env("RIPLIKA_TEST", "yes"),
            )
            .unwrap();
        assert_eq!(out.stdout, "yes");
    }

    #[test]
    fn repeated_flags_are_all_recoverable() {
        // the assertion style the transcode tests rely on
        let c = Command::new("ffmpeg").args(["-map", "0:v:0", "-map", "0:a:1", "-map", "0:s:0"]);
        assert_eq!(c.values_of("-map"), vec!["0:v:0", "0:a:1", "0:s:0"]);
        assert_eq!(c.value_of("-map"), Some("0:v:0"));
    }

    #[test]
    fn fake_runner_prefers_the_more_specific_pattern() {
        let r = FakeRunner::new()
            .on("ffprobe", "generic")
            .on("ffprobe -v error -select_streams a", "audio");
        let out =
            r.run(&Command::new("ffprobe").args(["-v", "error", "-select_streams", "a"])).unwrap();
        assert_eq!(out.stdout, "audio");
        let out =
            r.run(&Command::new("ffprobe").args(["-v", "error", "-select_streams", "v"])).unwrap();
        assert_eq!(out.stdout, "generic");
    }

    #[test]
    fn fake_runner_records_calls_in_order() {
        let r = FakeRunner::new();
        r.run(&Command::new("a")).unwrap();
        r.run(&Command::new("b")).unwrap();
        assert_eq!(r.calls().iter().map(|c| c.program.clone()).collect::<Vec<_>>(), vec!["a", "b"]);
    }

    #[test]
    fn require_turns_a_failure_into_an_error_with_the_last_line() {
        let r = FakeRunner::new().fail("ffmpeg", "something\nInvalid argument\n\n");
        let e = r.require(&Command::new("ffmpeg")).unwrap_err();
        assert!(e.0.contains("Invalid argument"), "{}", e.0);
    }

    #[test]
    fn cancelling_stops_the_next_command() {
        let c = Cancel::new();
        let r = RealRunner::new(c.clone());
        c.cancel();
        assert!(r.run(&Command::new("true")).is_err());
    }

    #[test]
    fn a_program_is_found_along_the_path() {
        let exists = |p: &Path| p == Path::new("/usr/bin/makemkvcon");
        assert_eq!(
            search_path("/bin:/usr/bin", "makemkvcon", &exists),
            Some(PathBuf::from("/usr/bin/makemkvcon"))
        );
        assert_eq!(search_path("/bin", "makemkvcon", &exists), None);
    }

    #[test]
    fn an_empty_path_entry_means_the_current_directory() {
        let exists = |p: &Path| p == Path::new("./tool");
        assert_eq!(search_path("/bin::/usr/bin", "tool", &exists), Some(PathBuf::from("./tool")));
    }

    #[test]
    fn a_program_given_as_a_path_is_not_searched_for() {
        let exists = |p: &Path| p == Path::new("/opt/makemkv/bin/makemkvcon");
        assert_eq!(
            search_path("/bin", "/opt/makemkv/bin/makemkvcon", &exists),
            Some(PathBuf::from("/opt/makemkv/bin/makemkvcon"))
        );
        assert_eq!(search_path("/bin", "/nope", &exists), None);
    }

    #[test]
    fn the_first_match_along_the_path_wins() {
        let exists = |_: &Path| true;
        assert_eq!(search_path("/first:/second", "x", &exists), Some(PathBuf::from("/first/x")));
    }

    #[test]
    fn a_real_lookup_finds_something_that_is_certainly_installed() {
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-real-program-xyzzy").is_none());
    }

    #[test]
    fn real_runner_gives_children_no_stdin() {
        // `cat` with an inherited stdin would block forever here
        let r = RealRunner::default();
        let out = r.run(&Command::new("cat")).unwrap();
        assert!(out.ok());
        assert_eq!(out.stdout, "");
    }
}

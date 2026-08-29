# Working on Riplika

Riplika turns a disc into a tagged, subtitled library. Four stages: **rip → identify → transcode → subtitles**.

This file is what you need before changing anything. The reference material is in [docs/](docs/); this is about how the code is built and why.

## Layout

| crate | what it is |
|---|---|
| `riplika-core` | the whole pipeline as a library |
| `riplika-cli` | `riplika`, the terminal front end |
| `riplika-gui` | `riplika-gui`, a GTK4/libadwaita window |

```
core/src/
  host.rs        Command, Runner, Fs — the only way out of the process
  model.rs       shared vocabulary: discs, titles, tracks, roles, settings
  lang.rs        ISO 639 matching and language preference
  media.rs       ffprobe, as one JSON call
  naming.rs      what a file is called and where it goes
  prefs.rs       settings, and the XDG directories
  secret.rs      the login keyring
  rip/           MakeMKV and the free DVD reader; ISO/IFO parsing
  rescue/        ddrescue for damaged discs; the libdvdcss binding
  identify/      volume label, disc structure, catalogues
  transcode/     what to measure, and the ffmpeg command to build
  subs/          bitmap subtitles to text
  job.rs         the stages in order, reporting as it goes
```

## Two rules

Both come from bugs that shipped in the shell scripts this replaced. Breaking either is how the same bugs come back.

**1. Deciding is separate from doing.** Nothing that talks to ffmpeg or MakeMKV also decides what to ask them. A planner turns state into an argv vector; a runner executes it. So a test can assert on the exact arguments with no disc in the drive — which is the only way to catch an argument that is wrong but not *invalid*: a missing `-map` that silently drops the subtitle track, a `-disposition` index one too high.

**2. The outside world is behind a trait.** `Runner`, `Prober`, `Ripper`, `Catalogue`, `Http` and `Fs` all have fakes. The whole pipeline runs end to end in milliseconds with no hardware, no ffmpeg and no network. A disc with two episodes and a play-all is a few lines of test data.

New code that shells out, reads a file, or makes a request goes through one of those traits. If it cannot, say why in a comment — `rescue/dvdcss.rs` and `secret.rs` both do, and both have a real reason.

## The failure that matters

Almost every bug in this project's history has the same shape: **it produced a result that looked correct.** Not a crash, not an error — a file that plays, a scan that succeeds, a disc that appears to have no episodes on it.

- `-map 0` carried a `bin_data` stream through and duplicated it every pass
- `-c copy` with no subtitle `-map` wrote files with no subtitles in them
- ffmpeg read stdin inside a loop and ate the rest of the list: one episode of eight processed, reported as success
- `fieldmatch,decimate` applied blind threw away one real frame in five
- writing straight to the destination left a truncated file that the next run counted as a finished episode
- perceptual hashing read raw frames through a UTF-8 conversion, so no extended cut ever matched
- an empty language list meant both "no preference" and "none", so unticking every language kept every language

So: **prefer failing loudly over carrying on**, and when something cannot be verified, say so rather than assuming. `ScanHealth`, `is_short`, the duration check after a rip and the `.part` rename all exist for this reason.

## Tests

~390, all offline, all fast. `cargo test` is the whole story; there is no integration suite and nothing needs a disc.

**Name a test after the rule it protects**, not the function it calls: `a_play_all_is_decomposed_into_its_episodes`, not `test_decompose`. When a test exists because something went wrong, say what went wrong in a comment — most of them do, and that is the record of why the code is shaped as it is.

**Tests must not touch the real drive.** Several did, and passed or failed depending on what was in it. Use a device path that cannot exist (`/dev/riplika-no-such-device`) so the code falls back to its no-hardware path.

**Do not read process-global state in a testable function.** Environment variables are shared, so a test that sets `XDG_DATA_HOME` races every other test — which it duly did. Take the value as an argument and read the environment at the edge; `prefs::xdg_dir` is the pattern.

## Strings people read

Every one goes through `tr()` in `crates/gui/src/i18n.rs`, so `xgettext` can find it and a translator can change it without touching Rust. After adding or changing one, run `./po/build.sh` - it refreshes the template and merges every catalogue, so existing translations survive.

Wrap prose, not identifiers. Icon names, CSS classes, widget names and device paths are not read as language and must stay bare, which is why the marking was done by hand rather than by a regex over every string literal.

English is a catalogue like any other. It looks redundant, and it is what proves the machinery works: without it, a broken catalogue path is invisible because English keeps working anyway.

## Conventions

Comments explain **why**, and are worth writing when the reason is not recoverable from the code — a constant chosen from a measurement, an ordering that matters, a fallback that exists because of a specific disc. Do not paraphrase the code.

Commit messages say what changed and why it was wrong before. They are the project's memory.

## Things that will bite you

- **`pkill -f riplika`** matches the shell running it. Use `pkill -x`.
- **The GUI runs jobs on worker threads inside itself**, so there is no separate process to look for, and restarting it kills a running rip.
- **An optical drive can wedge in `D` state**, uninterruptible; not even `kill -9` reaches it until the read returns. Ejecting clears it.
- **A DVD is ISO 9660 *and* UDF.** libdvdread reads the UDF. Copying only the ISO structures yields an image that mounts and will not open as a DVD.
- **`DVDCSS_READ_DECRYPT` descrambles whatever it is given**, and a sector's payload starts at byte 128 — so decrypting the volume descriptors leaves their first 128 bytes intact and turns the rest into noise.
- **DVD title numbering is not contiguous.** One disc here has content at titles 2–19 and again at 39–58. Read the count from the disc, never infer it.
- **MakeMKV reports chapter *counts*; the free reader reports chapter *durations*.** Only the durations can decompose a play-all, so anything that depends on them must handle their absence.

## Verifying against real hardware

There is usually a disc in `/dev/sr0`. Useful, but:

- a full scan probes every title and takes minutes — run it in the background
- **do not truncate the output through `tail`**. Doing that once produced a confident and completely wrong diagnosis, because extras sort last and the episodes were above the cut.
- `~/Videos` is a real library. Write to a scratch directory.

# Working on Riplika

Riplika reads a disc and produces library files. Three pipelines share one drive page, and what the disc turns out to be decides which runs.

- `riplika-core` is the whole pipeline as a library
- `riplika-cli` is `riplika`
- `riplika-gui` is `riplika-gui`, GTK4 and libadwaita

Reference material lives in [docs/](docs/). Hardware behaviour measured here, which is the part you cannot get from the code, is in [docs/discs.md](docs/discs.md#hardware-notes). Cutting a release is [RELEASE.md](RELEASE.md).

## Commands

```sh
./check.sh                              # everything CI runs, in CI's order
./po/build.sh                           # after adding or changing a UI string
packaging/regenerate-cargo-sources.sh   # after Cargo.lock changes
./release.sh 1.0.0 [notes.txt]          # see RELEASE.md
```

`./check.sh` stops at the first failure. Read its exit status, not its output: `cargo clippy -- -D warnings` prints a denied lint as `error`, so grepping for `warning:` reports success while CI fails on the same tree.

The toolchain is pinned in `rust-toolchain.toml`. Bump it as its own commit and fix what the new lints find.

## Architecture

**Deciding is separate from doing.** Nothing that talks to ffmpeg or MakeMKV also decides what to ask it. A planner turns state into an argv vector and a runner executes it, so a test can assert on exact arguments with no disc in the drive. That is what catches an argument that is wrong but not invalid, like a missing `-map` that drops the subtitle track.

**The outside world is behind a trait.** `Runner`, `Prober`, `Ripper`, `Catalogue`, `Http` and `Fs` all have fakes, and the pipeline runs end to end in milliseconds with no hardware and no network. New code that shells out, reads a file or makes a request goes through one of them. If it cannot, say why in a comment, as `rescue/dvdcss.rs` and `secret.rs` do.

**Each kind of disc gets a sibling, not a wider trait.** `MusicCatalogue` sits beside `Catalogue` and `AudioTarget` beside `Format`, because searching by title and returning episodes by season are questions a CD cannot answer.

## What goes wrong here

Almost every bug in this project produced a result that looked correct. Not a crash and not an error, but a file that plays, a scan that succeeds, a disc that appears to have no episodes on it. A `-c copy` with no subtitle `-map` wrote files with no subtitles; writing straight to the destination left a truncated file the next run counted as finished.

So prefer failing loudly over carrying on, and when something cannot be verified, say so instead of assuming.

## Tests

Offline and fast. `cargo test` is the whole story, and the suite passes with nothing on `PATH` but `sh` and `cat` and with no `HOME`.

- Name a test after the rule it protects, not the function it calls
- When a test exists because something broke, say what broke in a comment
- Never touch the real drive. Use `/dev/riplika-no-such-device` so the code takes its no-hardware path
- Do not read process-global state in a testable function. Environment variables are shared and a test that sets one races every other test; take the value as an argument, as `prefs::xdg_dir` does

## Strings people read

Every one goes through `tr()` in `crates/gui/src/i18n.rs`, then `./po/build.sh`.

- **Pass literals at the call site.** `xgettext` reads the source, not the program, so a string arriving through a variable is invisible to it and drops out of the template while the code still compiles
- Wrap prose, not identifiers. Icon names, CSS classes and device paths stay bare
- More than one value in a sentence means `tr_args` with numbered placeholders, since word order is what differs between languages. Counts inside such a sentence come from `tr_n`
- Warnings built in core and the job log stay English, so a log stays greppable

## Conventions

Comments explain why, and are worth writing when the reason is not recoverable from the code. Do not paraphrase the code.

Commit messages say what changed and why it was wrong before.

## Gotchas

- **A FakeRunner accepts commands real ffmpeg refuses.** Anything about how ffmpeg reads a command needs a real run. `riplika process` over two short files takes seconds
- **`pkill -f riplika` matches the shell running it.** Use `pkill -x`
- **The GUI runs jobs on threads inside itself**, so restarting it kills a running rip
- **A disc is only identified when every track matches.** One boundary cut wrong leaves the first file perfect and shifts everything after it

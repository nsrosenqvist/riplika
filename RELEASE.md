# Cutting a release

## Making the tag

```sh
./release.sh 1.0.0 [notes.txt]
git push && git push origin v1.0.0
```

`release.sh` writes the version into `Cargo.toml` and reconciles `Cargo.lock`, points the AppStream screenshot URLs at the tag it is about to make, adds a release entry to the AppStream metadata, runs `./check.sh`, then commits and tags.

The checks run before the tag rather than after it, so a failure means nothing was tagged.

The tag has to point at a tree that already says which version it is. Writing the version in the workflow instead would build the right number and leave the tagged commit saying the old one, so anyone checking out `v1.0.0` would build something calling itself `0.2.0`. The workflow only verifies that the tree and the tag agree, and fails naming both numbers when they do not.

Pushing is left to you, because everything up to that point is local and undoable and the push is what tells GitHub to build and announce a release.

### Release notes

The optional second argument names a file of notes, one line per bullet, which becomes the entry a software centre shows under this version. Without it the entry carries the version and date alone, and the script prints what has landed since the last tag so there is something to write from.

They are not generated from commit subjects. A changelog assembled that way tells somebody deciding whether to update that the README stopped being hard-wrapped, and choosing what matters is editorial. The GitHub release body is separate and is written by `chronikl` in the workflow.

### Pre-releases

A suffix after the patch number makes it a pre-release, and nothing else distinguishes them. `v1.0.0-rc.1` is marked as one on GitHub; `v1.0.0` is not.

## What the tag builds

`.github/workflows/release.yml` runs on `v*` and does four things in parallel where it can.

| | |
|---|---|
| **Checks** | calls `ci.yml`, since a tag is not a push to a branch and nothing would otherwise have checked it |
| **Binaries** | `riplika` and `riplika-gui` for x86_64 Linux, stripped, with the README, the licence and a locale tree beside them |
| **Flatpak** | the same build CI does, exported and bundled into one installable file |
| **Publish** | checksums every artifact, has `chronikl` write the release body, and creates the release |

Attached to the tag: the binary tarball, the `.flatpak` bundle, a Flathub manifest, `cargo-sources.json`, and `SHA256SUMS` covering all of them.

Installing the bundle needs nothing but the GNOME runtime:

```sh
flatpak install ./riplika-1.0.0.flatpak
```

That runtime comes from Flathub, so a machine with no remotes configured has to add Flathub once first.

## Submitting to Flathub

Push the tag first. The AppStream screenshots are served from this repository at that tag, and Flathub's linter fetches them, so a submission before the tag exists fails on four missing images.

The manifest in `packaging/` builds from the working tree, which Flathub does not allow. `packaging/flathub-manifest.py` takes that manifest and swaps the source for a git one pinned to the tag, with the commit alongside it because a tag can be moved. The release attaches the result, so the submission is that file plus the `cargo-sources.json` next to it.

The application id is `com.nsrosenqvist.Riplika`, which Flathub allows on the basis that `nsrosenqvist.com` is reachable over HTTPS and belongs to the author.

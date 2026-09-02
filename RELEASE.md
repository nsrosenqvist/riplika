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

## Where it is published

Riplika is distributed from its own Flatpak remote rather than from Flathub. Flathub's [requirements](https://docs.flathub.org/docs/for-app-authors/requirements) say that "applications containing AI-generated or AI-assisted code, documentation, or any other content are not allowed", and that the prohibition covers the submission as well as the application - manifest, metadata, patches, build scripts and the pull request itself. This is written with an AI assistant and says so in every commit trailer, so the answer is no, and pretending otherwise by rewriting the history that says so would be lying to reviewers rather than complying with them.

The wording was made absolute deliberately. It used to turn on quality and on whether there was meaningful human review; the rewording in May 2026 dropped both conditions. Exceptions "may be granted for mature, well-maintained projects", with no published criteria and no process, and the one public case of somebody asking was turned down first on development history rather than on the AI. That is a door worth knocking on again after a year of releases and answered issues, not now.

Nothing else about the build changes. The manifest, the linter run, the AppStream metadata and the screenshots all still have to be right, because a software centre reads them from the remote exactly as it would from Flathub. `packaging/flathub-manifest.py` and the manifest it produces are still built and still attached to the release: it is the only description of the build that does not depend on this working tree, and it is what any redistributor - Flathub included, one day - would need.

### The remote

The remote is an OSTree repository served as static files over HTTPS. There is no server: `flatpak build-update-repo` writes a directory, that directory is uploaded, and `flatpak install` fetches paths out of it.

It is stored in a Cloudflare R2 bucket published at `dl.nsrosenqvist.com`, not in a Cloudflare Pages project. One release fits in Pages comfortably - measured at 50 MB across 214 files with the largest at 6.7 MiB, against caps of 20,000 files and 25 MiB each - so the reason is not size today. It is that a Pages deployment is a whole immutable site: the repository only ever grows, every release would re-upload all of the history along with the new commit, and there would be no way to put the objects up before the summary that names them. R2 is S3-compatible and incremental, so `aws s3 sync` uploads the few thousand objects that are new, in an order this chooses, with a cache header per object. It charges nothing for egress and the whole thing sits inside the free tier.

Two things sit at the root:

| | |
|---|---|
| `riplika.flatpakrepo` | the ini file a person adds; it names the repository URL and carries the signing key inline |
| `repo/` | the OSTree repository itself - `config`, `summary`, `summary.sig`, `objects/`, `refs/`, `deltas/` |

Adding it is one command, and installing is the next:

```sh
flatpak remote-add --if-not-exists riplika https://dl.nsrosenqvist.com/riplika.flatpakrepo
flatpak install riplika com.nsrosenqvist.Riplika
```

The GNOME runtime still comes from Flathub, which is where `org.gnome.Platform` lives, so a machine with no remotes configured adds that one too. Only the application is served from here.

### Signing

The repository is signed with a GPG key made for this and used for nothing else. A remote served over HTTPS is not thereby trustworthy: the summary and the objects are what flatpak verifies, and `--no-gpg-verify` is not something to ask people to type.

The public key is exported, base64-encoded onto one line, and written into `riplika.flatpakrepo` as `GPGKey=`, which is how it reaches everyone who adds the remote. Changing it after that means everyone re-adds the remote, so it is a key to keep.

The secret key is base64-encoded into the `FLATPAK_GPG_KEY` repository secret. It has no passphrase, because a passphrase stored in the secret beside it protects against nothing.

### What publishing does

The `Flatpak` job in `.github/workflows/release.yml` builds the repository already. Publishing continues from there rather than downloading a 240 MB artifact into a second job.

1. **Fetch what is published.** The new commit goes onto the same ref every release did, and a repository built from scratch each time would drop the history that `flatpak update` walks.
2. **Pull the new build into it**, sign it, and regenerate the summary with `flatpak build-update-repo --generate-static-deltas --prune --prune-depth=2`. The deltas are what make an update download a diff instead of the whole thing; the prune keeps two releases of history and lets the rest go.
3. **Upload the objects first and the summary last.** A client that reads a summary naming objects that are not there yet gets an install that fails halfway. Deleting what pruning removed happens last of all, after nothing refers to it.
4. **Cache accordingly.** Objects are content-addressed and never change, so they go up with a year of `max-age` and `immutable`. `summary` and `summary.sig` change every release and are the whole point of the fetch, so they go up as `no-cache`. Getting this backwards is the failure where a release is published and nobody sees it for hours.

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

It is stored in a Cloudflare R2 bucket published at `flatpak.nsrosenqvist.com`, not in a Cloudflare Pages project. One release fits in Pages comfortably - measured at 50 MB across 214 files with the largest at 6.7 MiB, against caps of 20,000 files and 25 MiB each - so the reason is not size today. It is that a Pages deployment is a whole immutable site: the repository only ever grows, every release would re-upload all of the history along with the new commit, and there would be no way to put the objects up before the summary that names them. R2 is S3-compatible and incremental, so `aws s3 sync` uploads the few thousand objects that are new, in an order this chooses, with a cache header per object. It charges nothing for egress and the whole thing sits inside the free tier.

Two things sit at the root:

| | |
|---|---|
| `nsrosenqvist.flatpakrepo` | the ini file a person adds; it names the repository URL and carries the signing key inline |
| `repo/` | the OSTree repository itself - `config`, `summary`, `summary.sig`, `objects/`, `refs/`, `deltas/` |

It is one remote for everything published this way, not one per application. An OSTree repository holds any number of refs and one summary over all of them - that is what Flathub is - so a second application is another ref in the same repository rather than another hostname, another key, another cache rule and another thing for somebody to add. Which is why nothing here is called Riplika: the bucket is `flatpak`, the file is `nsrosenqvist.flatpakrepo`, and the remote calls itself a person rather than a program. A remote named after the first application to need one is a name that stops being true and cannot be changed, because it is already in everybody's `flatpak remotes`.

Adding it is one command, and installing is the next:

```sh
flatpak remote-add --if-not-exists nsrosenqvist https://flatpak.nsrosenqvist.com/nsrosenqvist.flatpakrepo
flatpak install nsrosenqvist com.nsrosenqvist.Riplika
```

The GNOME runtime still comes from Flathub, which is where `org.gnome.Platform` lives, so a machine with no remotes configured adds that one too. Only the applications are served from here.

Sharing one repository has one edge worth knowing. Each project's release workflow fetches the repository, adds its commit, and writes the summary back, so two projects tagging a release within the same few minutes can have the second write a summary that predates the first. Nothing is lost - the objects are all there - but one of the two is invisible until something publishes again, and re-running the job fixes it. This is not worth designing around at two applications; it is worth recognising rather than debugging.

### Signing

The repository is signed with a GPG key made for this and used for nothing else. A remote served over HTTPS is not thereby trustworthy: the summary and the objects are what flatpak verifies, and `--no-gpg-verify` is not something to ask people to type.

The public key is exported, base64-encoded onto one line, and written into `nsrosenqvist.flatpakrepo` as `GPGKey=`, which is how it reaches everyone who adds the remote. Changing it after that means everyone re-adds the remote, so it is a key to keep.

The secret key is base64-encoded into the `FLATPAK_GPG_KEY` repository secret. It has no passphrase, because a passphrase stored in the secret beside it protects against nothing.

### Setting it up, once

Nine steps, all of them undoable except the fourth, and the whole thing sits inside Cloudflare's free tier. `nsrosenqvist.com` has to be a zone on Cloudflare already, because that is what a custom domain on a bucket needs.

The blocks below are `sh`, which is what the workflow runs. In fish, `KEYID=...` is `set KEYID ...` and `export NAME=...` is `set -x NAME ...`; nothing else in them differs.

**1. Make the bucket.** Cloudflare dashboard, R2, *Create bucket*. Call it `flatpak`, not `riplika` - it holds the remote, and the remote holds whatever gets published to it. Pick a location hint near where most of it will be fetched from. Nothing else on the page matters.

**2. Give it the hostname.** The bucket's *Settings*, then *Public access*, then *Custom Domains*, then *Connect Domain*, and enter `flatpak.nsrosenqvist.com`. Cloudflare writes the DNS record itself and the certificate follows a minute later. The object key becomes the path, so `repo/summary` in the bucket is `https://flatpak.nsrosenqvist.com/repo/summary` on the web, which is what the URL in the `.flatpakrepo` file is pointing at.

Do not enable the `r2.dev` development URL. It is rate-limited and it is a second address for the same files, which is a second address people can end up with in a remote that then behaves differently.

**3. Make a token for the workflow.** R2's *API* menu, *Manage API tokens*, *Create token*. Permission is *Object Read & Write*, scoped to this one bucket - not to the account. It is shown once and gives three things: an access key id, a secret access key, and an S3 endpoint that looks like `https://<account-id>.r2.cloudflarestorage.com`.

**4. Make a signing key.** Not your own key. This one signs one repository, lives in a GitHub secret, and is worth nothing else:

```sh
gpg --batch --passphrase '' --quick-generate-key \
    "Niklas Rosenqvist <flatpak@nsrosenqvist.com>" default default never
KEYID=$(gpg --list-secret-keys --with-colons | awk -F: '/^fpr:/ {print $10; exit}')
```

It signs the remote rather than any one application on it, so it is named after the remote. The identity is what everybody sees in `flatpak remotes`, and it is baked into every copy of the `.flatpakrepo` file, so it is not a thing to reword later.

Keep a copy of the secret key somewhere you will still have it in three years. Losing it means everybody who added the remote has to remove it and add it again, and there is nothing you can publish that would fix it for them, because the thing they would need to trust the fix is the key you lost.

**5. Hand it to the workflow.**

```sh
gpg --export-secret-keys "$KEYID" | base64 -w0 | gh secret set FLATPAK_GPG_KEY
gh secret set R2_ACCESS_KEY_ID
gh secret set R2_SECRET_ACCESS_KEY
gh secret set R2_ENDPOINT      # https://<account-id>.r2.cloudflarestorage.com
gh secret set R2_BUCKET        # flatpak
gh variable set REPO_BASE_URL --body https://flatpak.nsrosenqvist.com
```

`REPO_BASE_URL` is a variable rather than a secret because it is the address printed in the install instructions, and a secret it cannot print is a secret that gets hard-coded somewhere else instead.

**6. Tell the cache what changes.** R2 through a custom domain is served by the CDN, which honours the `Cache-Control` the upload sets, and the upload sets `no-cache` on the summary for exactly this reason. Add a cache rule anyway - *Rules*, *Cache Rules*, matching `http.host eq "flatpak.nsrosenqvist.com" and starts_with(http.request.uri.path, "/repo/summary")`, action *Bypass cache*. It costs nothing and it is the difference between a release being visible in a minute and being visible when a cache somewhere decides it is.

**7. Publish once by hand**, so that the first release is not also the first test:

It is the same sequence the workflow runs, and worth keeping that way: a first publish that did something else would not be a test of anything. Credentials come from `aws configure --profile r2`, which also holds the endpoint, so nothing below has to name it.

```sh
# Built into a repository of its own, then the one ref that gets published
# pulled across into another. Not the whole build: it also holds the debug
# symbols, which are their own extension and are not served from here.
rm -rf repo published
flatpak run org.flatpak.Builder --user --force-clean --repo=repo build packaging/com.nsrosenqvist.Riplika.yml
ostree --repo=published init --mode=archive-z2
ostree --repo=published pull-local repo app/com.nsrosenqvist.Riplika/x86_64/master
flatpak build-sign published com.nsrosenqvist.Riplika --gpg-sign=$KEYID
flatpak build-update-repo published --generate-static-deltas --prune --prune-depth=2 --gpg-sign=$KEYID
./packaging/flatpakrepo.sh https://flatpak.nsrosenqvist.com $KEYID > nsrosenqvist.flatpakrepo

# The same cache headers the workflow uses, from the first upload: an object
# that goes up without one is cached on whatever the CDN decides, and the
# summary is the one file where that decision is wrong. Objects first and the
# summary last, for the same reason the workflow does it in that order.
aws --profile r2 s3 sync published/objects/ s3://flatpak/repo/objects/ \
  --cache-control 'public, max-age=31536000, immutable'
aws --profile r2 s3 sync published/deltas/ s3://flatpak/repo/deltas/ \
  --cache-control 'public, max-age=31536000, immutable'
aws --profile r2 s3 sync published/ s3://flatpak/repo/ --cache-control 'no-cache' \
  --exclude 'objects/*' --exclude 'deltas/*' --exclude 'summary*' \
  --exclude 'tmp/*' --exclude '.lock'
for f in published/summary*; do
  aws --profile r2 s3 cp "$f" "s3://flatpak/repo/$(basename "$f")" --cache-control 'no-cache'
done
aws --profile r2 s3 cp nsrosenqvist.flatpakrepo s3://flatpak/nsrosenqvist.flatpakrepo \
  --cache-control 'no-cache' --content-type 'application/vnd.flatpak.repo'
```

`aws` is `aws-cli-v2` here. The bucket name appears in two places - `s3://` above and the `R2_BUCKET` secret - and nowhere else, so a bucket called something other than `flatpak` costs those two lines and nothing more.

**8. Add it the way a stranger would**, on a machine that has never seen this working tree:

```sh
flatpak remote-add --if-not-exists nsrosenqvist https://flatpak.nsrosenqvist.com/nsrosenqvist.flatpakrepo
flatpak install nsrosenqvist com.nsrosenqvist.Riplika
```

If it installs without `--no-gpg-verify` then the signature, the summary, the objects and the key in the `.flatpakrepo` all agree, which is the only test of this that means anything.

Without a spare machine, this asks the same question for the price of the application alone. It takes the key out of the published file rather than from the keyring, so a key that never made it into that file fails here rather than on somebody else's computer:

```sh
curl -sS https://flatpak.nsrosenqvist.com/nsrosenqvist.flatpakrepo \
  | grep '^GPGKey=' | cut -d= -f2- | base64 -d > pub.gpg
ostree --repo=r init --mode=archive-z2
ostree --repo=r remote add --set=gpg-verify=true --gpg-import=pub.gpg \
  riplika https://flatpak.nsrosenqvist.com/repo/
ostree --repo=r pull riplika app/com.nsrosenqvist.Riplika/x86_64/master
ostree --repo=r show riplika:app/com.nsrosenqvist.Riplika/x86_64/master
```

"Good signature from" in the last line is the answer. `FLATPAK_USER_DIR` is not a way to do this: flatpak honours it, but `--user` on the same command line does not see the remote that `remote-add` just put there, and the install fails with "No remote refs found" as though the remote were broken.

**9. Tag a release** and watch the `Flatpak` job. From here on the workflow does steps 7 and 8's upload for you, and the only thing that changes by hand is the key, which should be never.

### What publishing does

The `Flatpak` job in `.github/workflows/release.yml` builds the repository already. Publishing continues from there rather than downloading a 240 MB artifact into a second job.

1. **Fetch what is published.** The new commit goes onto the same ref every release did, and a repository built from scratch each time would drop the history that `flatpak update` walks.
2. **Pull the new build into it**, sign it, and regenerate the summary with `flatpak build-update-repo --generate-static-deltas --prune --prune-depth=2`. The deltas are what make an update download a diff instead of the whole thing; the prune keeps two releases of history and lets the rest go. Only this application's ref is pulled across: the build repository carries an appstream branch listing one application, and copying that over would replace the remote's catalogue of everything with a catalogue of this. `build-update-repo` writes that branch itself, from whatever is actually in the repository.
3. **Upload the objects first and the summary last.** A client that reads a summary naming objects that are not there yet gets an install that fails halfway. Deleting what pruning removed happens last of all, after nothing refers to it.
4. **Cache accordingly.** Objects are content-addressed and never change, so they go up with a year of `max-age` and `immutable`. `summary` and `summary.sig` change every release and are the whole point of the fetch, so they go up as `no-cache`. Getting this backwards is the failure where a release is published and nobody sees it for hours.

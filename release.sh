#!/bin/sh
# Cut a release: set the version, commit it, and tag that commit.
#
# The tag has to point at a tree that already says which version it is.
# Writing the version in the release workflow instead would build the right
# number and leave the tagged commit saying the old one, so anybody checking
# out the tag would build something that calls itself 0.2.0 forever. One
# commit, holding the version, with the tag on it, is the whole idea.
#
# Pushing is left to you. Everything up to here is local and undoable; the
# push is what tells GitHub to build and announce a release, and that is worth
# typing on purpose.
set -e
cd "$(dirname "$0")"

VERSION="${1:-}"
case "$VERSION" in
  '')
    echo "usage: ./release.sh <version>       e.g. 0.3.0, or 0.3.0-rc.1" >&2
    exit 2
    ;;
  v*)
    echo "give the version without the leading v: ${VERSION#v}" >&2
    exit 2
    ;;
esac

# The same shape the release workflow triggers on. A tag it will not match is
# a tag that quietly does nothing, which is worse than being told now.
if ! printf '%s' "$VERSION" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+([-.+][0-9A-Za-z.-]+)?$'; then
  echo "not a version the release workflow will act on: $VERSION" >&2
  echo "it triggers on v<major>.<minor>.<patch>, optionally with a suffix" >&2
  exit 2
fi

TAG="v$VERSION"
[ -z "$(git status --porcelain)" ] || { echo "working tree is not clean" >&2; exit 1; }
git rev-parse -q --verify "refs/tags/$TAG" >/dev/null &&
  { echo "$TAG already exists" >&2; exit 1; }

case "$VERSION" in
  *-*) echo "== $TAG (a pre-release; GitHub will mark it as one)" ;;
  *)   echo "== $TAG" ;;
esac

# The first `version = ` is [workspace.package]'s, which every crate inherits.
sed -i "0,/^version = .*/s//version = \"$VERSION\"/" Cargo.toml
# Cargo.lock names the workspace crates too, and the flatpak builds offline.
cargo update --workspace --offline

# The screenshots a software centre shows are fetched from this repository by
# URL, and the URL has to name a tag: a branch moves, and what Flathub keeps
# is whatever it fetched at the time. Pointing them at the tag being made here
# is the only way they cannot fall behind it.
META=data/com.nsrosenqvist.Riplika.metainfo.xml
sed -i "s|/riplika/v[0-9][^/]*/data/screenshots/|/riplika/$TAG/data/screenshots/|g" "$META"

# A releases tag is required to pass validation, and a release nobody wrote
# down is a release the software centre says nothing about.
#
# The version and the date are the mechanical part and are filled in here. The
# notes are not: a changelog built from commit subjects would tell somebody
# deciding whether to update that the README stopped being hard-wrapped. Pass a
# file of one line per bullet to say something worth reading, or leave it out
# and the entry carries the version alone.
python3 - "$META" "$VERSION" "$(date -I)" "${2:-}" <<'PYTHON'
import html, sys
meta, version, today, notes = sys.argv[1:5]
text = open(meta).read()
if f'version="{version}"' in text:
    raise SystemExit(0)

lines = []
if notes:
    lines = [l.strip() for l in open(notes) if l.strip()]

if lines:
    items = "\n".join(f"          <li>{html.escape(l)}</li>" for l in lines)
    entry = (
        f'    <release version="{version}" date="{today}">\n'
        f"      <description>\n        <ul>\n{items}\n        </ul>\n      </description>\n"
        f"    </release>"
    )
else:
    entry = f'    <release version="{version}" date="{today}"/>'

open(meta, "w").write(text.replace("  <releases>", f"  <releases>\n{entry}", 1))
PYTHON
appstreamcli validate --no-net "$META" >/dev/null

echo "== checking before tagging, not after"
./check.sh

git add Cargo.toml Cargo.lock "$META"
git commit -m "Riplika $VERSION"
git tag -a "$TAG" -m "Riplika $VERSION"

echo
echo "Tagged $TAG. Nothing has left this machine yet."
echo "  git push && git push origin $TAG"

if [ -z "${2:-}" ]; then
  echo
  echo "No release notes were given, so the AppStream entry carries the version"
  echo "alone. What has landed since the last tag, if you want to write some:"
  echo
  git log --format='  %s' "$(git describe --tags --abbrev=0 "$TAG^" 2>/dev/null || echo "$TAG")..$TAG" 2>/dev/null | head -30
fi

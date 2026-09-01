#!/usr/bin/env python3
"""Write the Flathub manifest for a tag.

Flathub builds from publicly reachable sources, and the manifest here builds
from the working tree so that a local build tests what is in front of you.
That is the only difference between the two, so rather than keep a second
manifest and let it drift, this takes the canonical one and swaps that stanza
for a git source pinned to the tag being released.

    ./packaging/flathub-manifest.py v0.3.0 > com.nsrosenqvist.Riplika.yml

The result goes in the Flathub submission alongside cargo-sources.json, which
it refers to by name and which Flathub needs a copy of.
"""

import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
MANIFEST = HERE / "com.nsrosenqvist.Riplika.yml"
URL = "https://github.com/nsrosenqvist/riplika.git"

LOCAL = """    sources:
      - type: dir
        path: ..
"""


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    tag = sys.argv[1]

    try:
        commit = subprocess.run(
            ["git", "rev-list", "-n", "1", tag],
            cwd=HERE.parent,
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
    except subprocess.CalledProcessError:
        print(f"no such tag: {tag}", file=sys.stderr)
        return 1

    text = MANIFEST.read_text()
    if LOCAL not in text:
        print(
            "the working-tree source stanza is not where this expected it; "
            "the manifest has changed shape and so must this",
            file=sys.stderr,
        )
        return 1

    # The commit as well as the tag: a tag can be moved, and Flathub builds
    # what it was told to build rather than whatever the tag points at today.
    remote = (
        "    sources:\n"
        "      - type: git\n"
        f"        url: {URL}\n"
        f"        tag: {tag}\n"
        f"        commit: {commit}\n"
    )
    sys.stdout.write(text.replace(LOCAL, remote))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Print the release body for one version, from the AppStream metadata.

The notes shown by a software centre and the notes shown on the release page
are the same notes, and they are already written by hand for every version that
has any - `release.sh` puts them there. This reads them back out as markdown.

It exists because the release stopped being made when the thing that writes a
nicer body could not run. GitHub Models went into its retirement brownout and
answered 410, chronikl failed, and a release that was built, checked, signed
and published had nothing to announce it. A generator is worth having and is
not worth blocking on.

    packaging/release-notes.py data/com.nsrosenqvist.Riplika.metainfo.xml 1.0.0
"""

import sys
import xml.etree.ElementTree as ET

meta, version = sys.argv[1:3]
root = ET.parse(meta).getroot()

for release in root.iter("release"):
    if release.get("version") != version:
        continue
    items = [
        " ".join((li.text or "").split())
        for li in release.iter("li")
    ]
    if items:
        print("\n".join(f"- {i}" for i in items))
    else:
        # A version with no notes of its own still gets a body, because an
        # empty one reads as a release nobody bothered with.
        print(f"Riplika {version}.")
    break
else:
    print(f"Riplika {version}.")

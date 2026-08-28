#!/usr/bin/env python3
"""Pick which audio or subtitle tracks to keep, by language.

Prints the type-relative stream indices to keep, one per line, in the order the
languages were requested - so the first language listed ends up first in the
output, and becomes the default track.

Accepts full names or ISO 639 codes, and matches both the bibliographic and
terminological variants (ger/deu, fre/fra, ice/isl) since discs and tools
disagree about which to write.

Usage: langmap.py <file> <a|s> <languages>
"""
import subprocess, sys

# name -> every code that might appear in a stream tag for that language
ALIASES = {
    "english": ["eng", "en"],
    "swedish": ["swe", "sv"],
    "finnish": ["fin", "fi"],
    "icelandic": ["isl", "ice", "is"],
    "norwegian": ["nor", "nb", "nn", "no"],
    "danish": ["dan", "da"],
    "german": ["deu", "ger", "de"],
    "french": ["fra", "fre", "fr"],
    "spanish": ["spa", "es"],
    "italian": ["ita", "it"],
    "dutch": ["nld", "dut", "nl"],
    "portuguese": ["por", "pt"],
    "polish": ["pol", "pl"],
    "russian": ["rus", "ru"],
    "japanese": ["jpn", "ja"],
    "korean": ["kor", "ko"],
    "chinese": ["zho", "chi", "zh"],
    "undetermined": ["und", ""],
}
# let a code be given directly, in either variant
for _name, _codes in list(ALIASES.items()):
    for _c in _codes:
        ALIASES.setdefault(_c, _codes)


def wanted_codes(spec):
    out = []
    for token in spec.replace(";", ",").split(","):
        t = token.strip().lower()
        if not t:
            continue
        out.append(ALIASES.get(t, [t]))
    return out


def stream_languages(path, kind):
    r = subprocess.run(
        ["ffprobe", "-v", "error", "-select_streams", kind,
         "-show_entries", "stream_tags=language", "-of", "csv=p=0", path],
        capture_output=True, text=True)
    langs = []
    for line in r.stdout.splitlines():
        langs.append(line.strip().strip(",").lower())
    return langs


def main():
    path, kind, spec = sys.argv[1], sys.argv[2], sys.argv[3]
    langs = stream_languages(path, kind)
    if not langs:
        return 0

    keep, seen = [], set()
    for codes in wanted_codes(spec):
        for i, l in enumerate(langs):
            if i not in seen and l in codes:
                keep.append(i)
                seen.add(i)

    if not keep:
        # A file with no audio is broken, so fall back to keeping everything;
        # missing subtitles are survivable, so honour the filter and keep none.
        if kind == "a":
            print(f"langmap: no audio matches {spec!r}, keeping all",
                  file=sys.stderr)
            keep = list(range(len(langs)))
        else:
            print(f"langmap: no subtitles match {spec!r}, dropping them all",
                  file=sys.stderr)

    for i in keep:
        print(i)
    return 0


if __name__ == "__main__":
    sys.exit(main())

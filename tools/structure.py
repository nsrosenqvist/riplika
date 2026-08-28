#!/usr/bin/env python3
"""Work out what a ripped disc actually contains.

MakeMKV gives you a pile of titles with meaningless names. This sorts them into
episodes, extended cuts, play-alls and extras using only the disc's own
structure - no network, no guessing:

  play-all      a title whose duration is the sum of others, with chapter marks
                landing on their boundaries
  episode       a title inside a play-all, or one that looks like an episode and
                matches nothing else
  extended cut  a title that duplicates an episode's content but is longer
  extra         everything else

Ordering comes from the play-alls, not from MakeMKV's title numbers, which are
not reliably in broadcast order.

Usage: structure.py <dir> [--min-episode 900] [--max-episode 2700]
"""
import argparse, json, os, subprocess, sys, tempfile
from collections import defaultdict


def probe(path):
    def q(args):
        r = subprocess.run(["ffprobe", "-v", "error", *args, "-of", "csv=p=0", path],
                           capture_output=True, text=True)
        return r.stdout.strip()
    dur = float(q(["-show_entries", "format=duration"]) or 0)
    chaps = []
    out = q(["-show_entries", "chapter=start_time,end_time"])
    for line in out.splitlines():
        try:
            a, b = (float(x) for x in line.split(",")[:2])
            chaps.append(round(b - a, 2))
        except ValueError:
            pass
    return dur, chaps


def dhash_stream(path, fps=1, size=16):
    """Coarse per-second frame hashes, for spotting duplicate content."""
    with tempfile.NamedTemporaryFile(suffix=".raw", delete=False) as t:
        tmp = t.name
    subprocess.run(["ffmpeg", "-v", "error", "-i", path, "-vf",
                    f"fps={fps},scale={size}:{size},format=gray",
                    "-f", "rawvideo", "-y", tmp], check=False)
    data = open(tmp, "rb").read()
    os.unlink(tmp)
    n = size * size
    out = []
    for i in range(0, len(data) - n + 1, n):
        f = data[i:i + n]
        m = sum(f) / n
        out.append(int("".join("1" if v > m else "0" for v in f), 2))
    return out


def similarity(a, b):
    if not a or not b:
        return 0.0
    hits = sum(1 for h in a if min(bin(h ^ x).count("1") for x in b) <= 8)
    return hits / len(a)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("directory")
    ap.add_argument("--min-episode", type=float, default=900)
    ap.add_argument("--max-episode", type=float, default=2700)
    ap.add_argument("--json", action="store_true")
    a = ap.parse_args()

    files = sorted(f for f in os.listdir(a.directory) if f.endswith((".mkv", ".mp4")))
    info = {}
    for f in files:
        d, c = probe(os.path.join(a.directory, f))
        info[f] = {"duration": d, "chapters": c}

    # A play-all repeats other titles back to back, so its chapter list is
    # their chapter lists concatenated. Do not gate this on duration: a
    # two-episode play-all is only 43 minutes, well inside the range a single
    # extended episode can occupy.
    parts = {f: v for f, v in info.items()
             if a.min_episode <= v["duration"] <= a.max_episode and v["chapters"]}
    playalls = {}
    for f, v in info.items():
        if not v["chapters"]:
            continue
        seq, rest = [], list(v["chapters"])
        while rest:
            for g, pv in parts.items():
                if g == f:
                    continue  # a title trivially decomposes into itself
                n = len(pv["chapters"])
                if n and rest[:n] == pv["chapters"]:
                    seq.append(g)
                    rest = rest[n:]
                    break
            else:
                break
        # needs at least two distinct parts, and must not just be itself
        if len(seq) >= 2 and not rest and f not in seq:
            playalls[f] = seq

    # Order play-alls by DVD title number, which follows the disc layout;
    # ordering by duration would put a five-episode run before the two-episode
    # premiere that precedes it on the disc.
    def tnum(name):
        import re as _re
        m = _re.search(r"_t(\d+)", name)
        return int(m.group(1)) if m else 9999

    ordered, seen = [], set()
    for pa in sorted(playalls, key=tnum):
        for g in playalls[pa]:
            if g not in seen:
                ordered.append(g)
                seen.add(g)

    # episode-shaped titles that no play-all claims
    loose = [f for f, v in info.items()
             if a.min_episode <= v["duration"] <= a.max_episode
             and f not in seen and f not in playalls]

    # duplicates: an unclaimed title that repeats one that is claimed
    hashes = {}
    extended, extras = {}, []
    for f in loose:
        hashes.setdefault(f, dhash_stream(os.path.join(a.directory, f)))
        best, score = None, 0.0
        for g in ordered:
            hashes.setdefault(g, dhash_stream(os.path.join(a.directory, g)))
            s = similarity(hashes[f], hashes[g])
            if s > score:
                best, score = g, s
        if best and score >= 0.15:
            extended[f] = (best, score)
        else:
            extras.append(f)

    result = {
        "play_alls": playalls,
        "episodes_in_order": ordered,
        "extended_cuts": {k: {"of": v[0], "similarity": round(v[1], 3)}
                          for k, v in extended.items()},
        "unclaimed_episode_length": extras,
        "durations": {f: round(v["duration"], 1) for f, v in info.items()},
    }
    if a.json:
        print(json.dumps(result, indent=2))
        return

    print(f"{len(files)} titles\n")
    print(f"play-alls ({len(playalls)}):")
    for p, seq in playalls.items():
        print(f"  {p}  {info[p]['duration']/60:.1f}min = {len(seq)} titles")
    print(f"\nepisodes in play-all order ({len(ordered)}):")
    for i, f in enumerate(ordered, 1):
        print(f"  {i:2d}. {f:16} {info[f]['duration']/60:6.2f} min")
    if extended:
        print(f"\nextended cuts ({len(extended)}):")
        for f, (g, s) in extended.items():
            print(f"  {f:16} duplicates {g} ({s:.0%})  "
                  f"{info[f]['duration']/60:.2f} vs {info[g]['duration']/60:.2f} min")
    if extras:
        print(f"\nepisode-length but matching nothing ({len(extras)}) - "
              f"check these by eye, they may be episodes outside the play-all "
              f"or something else entirely:")
        for f in extras:
            print(f"  {f:16} {info[f]['duration']/60:6.2f} min "
                  f"chapters={len(info[f]['chapters'])}")


if __name__ == "__main__":
    main()

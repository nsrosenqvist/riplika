#!/usr/bin/env python3
"""Check that the catalogues match the source, without needing xgettext.

`po/build.sh` regenerates the template, but that needs an xgettext new enough
to know Rust - newer than what the distributions CI runs on carry. This asks
the two questions that actually matter, and asks them of the source directly.

The failure this exists for: four plural forms were collected into a table, so
they reached `tr_n` through a variable. xgettext reads the source rather than
the program, could not see through the variable, and dropped all four - while
the code still compiled and ran, in English, indistinguishably from correct.
That is why an unextractable call is an error here and not a warning: it is
the shape of the bug, and it is invisible everywhere else.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# A Rust string literal, escapes included. `\s*` spans newlines, because these
# calls are often wrapped across several lines.
STRING = r'"((?:[^"\\]|\\.)*)"'
CALL = re.compile(r'\b(tr|tr_n)\(\s*')
LITERAL_FIRST = re.compile(r'\b(tr|tr_n)\(\s*' + STRING)
TR_N_BOTH = re.compile(r'\btr_n\(\s*' + STRING + r'\s*,\s*' + STRING)


def unescape(s):
    return s.replace('\\"', '"').replace('\\n', '\n').replace('\\t', '\t').replace('\\\\', '\\')


def without_comment_lines(text):
    """Drop whole-line comments, keeping line numbering intact.

    Only whole-line ones: a trailing `//` cannot be stripped safely without
    parsing, since `"https://..."` is not a comment.
    """
    return '\n'.join('' if l.lstrip().startswith('//') else l for l in text.splitlines())


def scan():
    wanted, unextractable = set(), []
    for rel in (ROOT / 'po' / 'POTFILES.in').read_text().split():
        text = without_comment_lines((ROOT / rel).read_text())
        for m in TR_N_BOTH.finditer(text):
            wanted.add(unescape(m.group(1)))
            wanted.add(unescape(m.group(2)))
        for m in LITERAL_FIRST.finditer(text):
            wanted.add(unescape(m.group(2)))
        for m in CALL.finditer(text):
            if not LITERAL_FIRST.match(text, m.start()):
                line = text.count('\n', 0, m.start()) + 1
                snippet = text.splitlines()[line - 1].strip()
                unextractable.append(f'{rel}:{line}: {snippet}')
    return wanted, unextractable


def in_template():
    """msgid and msgid_plural values, continuation lines joined."""
    found, current = set(), None
    for line in (ROOT / 'po' / 'riplika.pot').read_text().splitlines():
        line = line.strip()
        m = re.match(r'^(msgid|msgid_plural)\s+' + STRING + r'$', line)
        if m:
            if current is not None:
                found.add(unescape(''.join(current)))
            current = [m.group(2)]
            continue
        m = re.match('^' + STRING + r'$', line)
        if m and current is not None:
            current.append(m.group(1))
            continue
        if current is not None:
            found.add(unescape(''.join(current)))
            current = None
    if current is not None:
        found.add(unescape(''.join(current)))
    found.discard('')
    return found


def main():
    wanted, unextractable = scan()
    have = in_template()
    missing = sorted(wanted - have)
    bad = False

    if unextractable:
        bad = True
        print(f'{len(unextractable)} call(s) xgettext cannot read, so the string')
        print('will never reach a translator:')
        for line in unextractable:
            print(f'  {line}')
        print('\nPass the string literally at the call site, not through a variable.')

    if missing:
        bad = True
        print(f'\n{len(missing)} string(s) a person reads are not in po/riplika.pot:')
        for s in missing:
            print(f'  {s!r}')
        print('\nRun ./po/build.sh.')

    if not bad:
        print(f'all {len(wanted)} source strings are in the template, all extractable')
    return 1 if bad else 0


if __name__ == '__main__':
    sys.exit(main())

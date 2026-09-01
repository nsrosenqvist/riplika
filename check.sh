#!/bin/sh
# Everything CI runs, in the order it runs it.
#
# Exists because checking by eye went wrong: `cargo clippy -- -D warnings`
# reports a denied lint as "error", and a check that counted lines beginning
# "warning:" therefore counted none and said all was well. CI disagreed. Exit
# status is the only thing that knows.
set -e
cd "$(dirname "$0")"

echo "== formatting"
cargo fmt --check

echo "== lints"
cargo clippy --workspace --all-targets -- -D warnings

echo "== tests"
cargo test --workspace --all-targets
cargo test --workspace --doc

echo "== rows"
# An AdwPreferencesRow parses its title as Pango markup, so a file name with an
# ampersand in it draws a row with no title and looks like a lost episode. The
# builders in crates/gui/src/rows.rs turn that off; a row built any other way
# would silently get it back.
if grep -rn 'adw::[A-Za-z]*Row::builder()' crates/gui/src --include='*.rs' \
    | grep -v '^crates/gui/src/rows.rs:'; then
  echo "build rows with crates/gui/src/rows.rs, which turns Pango markup off" >&2
  exit 1
fi
echo "all rows are built with markup off"

echo "== translations"
./po/check.py
while read -r lang; do
  [ -n "$lang" ] || continue
  msgfmt --check "po/$lang.po" -o /dev/null
  msgfmt --statistics "po/$lang.po" -o /dev/null 2> /tmp/riplika-po-stats
  cat /tmp/riplika-po-stats
  if grep -qE 'untranslated|fuzzy' /tmp/riplika-po-stats; then
    echo "po/$lang.po is incomplete - it is generated, so run ./po/build.sh" >&2
    exit 1
  fi
done < po/LINGUAS

echo
echo "all green"

#!/bin/bash
# Write the .flatpakrepo file people add to install Riplika.
#
# One ini file that names the repository and carries the signing key inline, so
# adding the remote is one command with one URL in it and nothing to import by
# hand. It is generated from the key that is about to sign the repository
# rather than kept in the tree beside it: a committed copy would be a second
# place the key lives, and the failure it causes - a remote everybody has added
# that no longer verifies - is one nobody can fix from their end.
#
#     packaging/flatpakrepo.sh https://dl.nsrosenqvist.com > riplika.flatpakrepo
#
# Reads the public half of whatever key GNUPGHOME holds.
set -eu

BASE="${1:?usage: flatpakrepo.sh https://host [keyid]}"
BASE="${BASE%/}"
KEY="${2:-}"

# One line, no wrapping: the value is read to the end of the line, and gpg's
# own 64-column armour would end the field after the first of them.
GPG=$(gpg --export ${KEY:+"$KEY"} | base64 -w0)
if [ -z "$GPG" ]; then
  echo "flatpakrepo.sh: no public key to export" >&2
  exit 1
fi

cat <<EOF
[Flatpak Repo]
Title=Riplika
Url=$BASE/repo/
Homepage=https://github.com/nsrosenqvist/riplika
Comment=Rip discs into a library
Description=Films, television, music CDs and game discs, read and filed the way a media library expects them.
Icon=$BASE/riplika.svg
GPGKey=$GPG
EOF

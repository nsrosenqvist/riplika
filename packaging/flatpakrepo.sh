#!/bin/bash
# Write the .flatpakrepo file people add to install from this remote.
#
# One ini file that names the repository and carries the signing key inline, so
# adding the remote is one command with one URL in it and nothing to import by
# hand. It is generated from the key that is about to sign the repository
# rather than kept in the tree beside it: a committed copy would be a second
# place the key lives, and the failure it causes - a remote everybody has added
# that no longer verifies - is one nobody can fix from their end.
#
#     packaging/flatpakrepo.sh https://flatpak.nsrosenqvist.com > nsrosenqvist.flatpakrepo
#
# It describes the remote, not Riplika. One OSTree repository can hold any
# number of applications, which is what Flathub is, and this one is expected to
# hold more than this one eventually - so what somebody adds is a person's
# remote that Riplika happens to be in, and adding it again for the next
# application is not something they should have to do.
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

# No Icon. There is no icon for the remote itself, and pointing it at one
# application's icon would be wrong the moment there are two. A software centre
# falls back to a generic source icon, which is the truth.
cat <<EOF
[Flatpak Repo]
Title=Niklas Rosenqvist
Url=$BASE/repo/
Homepage=https://github.com/nsrosenqvist
Comment=Applications by Niklas Rosenqvist
Description=A small remote for applications that are not on Flathub.
GPGKey=$GPG
EOF

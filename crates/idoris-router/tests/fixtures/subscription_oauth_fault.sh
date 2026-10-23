#!/bin/sh
set -eu
printf 'attempt\n' >> "${PATH%%:*}/calls"
cat >/dev/null
printf 'fixture fault\n' >&2
exit 23

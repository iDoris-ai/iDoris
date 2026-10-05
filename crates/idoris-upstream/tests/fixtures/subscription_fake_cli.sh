#!/bin/sh
set -eu
mode="${IDORIS_FAKE_CLI_MODE:-echo}"
marker="${IDORIS_FAKE_CLI_MARKER:-}"
payload="${IDORIS_FAKE_CLI_OUTPUT:-fake-cli-output}"
write_marker() {
  [ -n "$marker" ] || return 0
  printf '%s\n' "$1" >> "$marker"
}
output_file=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "-o" ]; then output_file="$arg"; break; fi
  prev="$arg"
done
case "$mode" in
  echo) cat ;;
  stderr) printf '%s\n' "$payload" >&2; cat >/dev/null ;;
  split-output)
    printf '123456'
    printf 'abcdef' >&2
    cat >/dev/null
    ;;
  nonzero) cat >/dev/null; exit 23 ;;
  empty) cat >/dev/null ;;
  output-file)
    cat >/dev/null
    [ -n "$output_file" ] || exit 24
    printf '%s' "$payload" > "$output_file"
    ;;
  pid-marker)
    write_marker "parent:$$:$(ps -o pgid= -p $$ | tr -d ' ')"
    cat >/dev/null
    ;;
  grandchild)
    /bin/sh -c '
      marker="$1"
      trap "exit 0" TERM INT
      [ -z "$marker" ] || printf "child:%s:%s\n" "$$" "$(ps -o pgid= -p $$ | tr -d " ")" >> "$marker"
      while :; do sleep 1; done
    ' sh "$marker" &
    child=$!
    write_marker "parent:$$:$(ps -o pgid= -p $$ | tr -d ' ')"
    trap 'wait "$child" 2>/dev/null || true; exit 0' TERM INT
    wait "$child"
    ;;
  ignore-term)
    trap '' TERM
    write_marker "parent:$$:$(ps -o pgid= -p $$ | tr -d ' ')"
    while :; do sleep 1; done
    ;;
  hold-pipe)
    /bin/sh -c 'trap "exit 0" TERM INT; while :; do sleep 1; done' &
    child=$!
    write_marker "parent:$$:$(ps -o pgid= -p $$ | tr -d ' ')"
    write_marker "child:$child:$(ps -o pgid= -p "$child" | tr -d ' ')"
    printf '%s\n' "$payload"
    exit 0
    ;;
  limit-hang)
    write_marker "parent:$$:$(ps -o pgid= -p $$ | tr -d ' ')"
    printf '12345678901234567890'
    while :; do sleep 1; done
    ;;
  hang)
    write_marker "parent:$$:$(ps -o pgid= -p $$ | tr -d ' ')"
    while :; do sleep 1; done
    ;;
  *) printf 'unknown fake CLI mode\n' >&2; exit 64 ;;
esac

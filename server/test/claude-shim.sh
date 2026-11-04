#!/bin/sh
# Tests server/claude-shim.sh against a fake femmed and a fake real claude, in a temp home: no
# daemon, no account. The pane-on-a-terminal cases run under `script` (a pseudo-terminal) when it
# is installed, and say so when it is not.
#   sh server/test/claude-shim.sh
set -u
src=$(cd "$(dirname "$0")/.." && pwd)/claude-shim.sh
T=$(mktemp -d "${TMPDIR:-/tmp}/claude-shim-test.XXXXXX") || exit 1
trap 'rm -rf "$T"' EXIT
trap 'exit 1' HUP INT TERM
mkdir -p "$T/home/.local/bin" "$T/bin" "$T/real"
cp "$src" "$T/home/.local/bin/claude"
shim=$T/home/.local/bin/claude
cat > "$T/bin/femmed" <<'FAKE'
#!/bin/sh
# Fake femmed: prints its arguments and exits with $FAKE_OPEN_EXIT (default 0).
echo femmed "$@"
exit "${FAKE_OPEN_EXIT:-0}"
FAKE
cat > "$T/real/claude" <<'FAKE'
#!/bin/sh
echo "real claude" "$@"
FAKE
chmod +x "$T/bin/femmed" "$T/real/claude"

fails=0
check() { # name, want, got
  if [ "$2" = "$3" ]; then echo "ok   $1"; else echo "FAIL $1: want [$2] got [$3]"; fails=$((fails + 1)); fi
}
# A clean environment: only the fakes and the usual tools on PATH, plus what each case adds.
run() { env -i HOME="$T/home" PATH="$T/home/.local/bin:$T/bin:$T/real:/usr/bin:/bin" "$@"; }

check "outside a pane the real claude runs" "real claude" "$(run /bin/sh "$shim" </dev/null)"
check "arguments in a pane run the real claude" "real claude -p hi" \
  "$(run FEMME_PANE=1 /bin/sh "$shim" -p hi </dev/null)"
check "a bare claude without a terminal runs the real claude" "real claude" \
  "$(run FEMME_PANE=1 /bin/sh "$shim" </dev/null)"
check "no claude anywhere is an error" "claude: not found on PATH" \
  "$(env -i HOME="$T/home" PATH="$T/home/.local/bin:$T/bin:/usr/bin:/bin" /bin/sh "$shim" 2>&1 </dev/null)"

if command -v script >/dev/null; then
  check "a bare claude in a pane on a terminal opens a session" "femmed open" \
    "$(run FEMME_PANE=1 script -qec "/bin/sh $shim" /dev/null | tr -d '\r')"
  check "a refused session falls back to the real claude here" \
    "femmed open
claude: no daemon session, starting claude here
real claude" \
    "$(run FEMME_PANE=1 FAKE_OPEN_EXIT=1 script -qec "/bin/sh $shim 2>&1" /dev/null | tr -d '\r')"
else
  echo "skip terminal cases: no script(1) here"
fi

echo "$fails failed"
[ "$fails" -eq 0 ]

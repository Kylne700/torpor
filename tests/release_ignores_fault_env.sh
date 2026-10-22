#!/bin/sh
# The plain release build (no `fault-inject` feature) must ignore every TORPOR_TEST_* and TORPOR_FAULT variable.
# Usage, from the torpor/ folder:
#   sh tests/release_ignores_fault_env.sh [path-to-binary]
# With no argument it builds `cargo build --release` and checks target/release/torpor. CI and the release workflow pass
# the binary they ship (built plain, in its own target dir), so the check runs on exactly that file.
# Note: `cargo test --release` builds the same target/release/torpor WITH fault-inject, which is why the argument exists.
set -eu
cd "$(dirname "$0")/.."
if [ $# -ge 1 ]; then
  BIN=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
else
  cargo build --release --quiet
  BIN="$PWD/target/release/torpor"
fi
[ -x "$BIN" ] || { echo "FAIL: $BIN is not an executable file"; exit 1; }
T="$(mktemp -d "${TMPDIR:-/tmp}/torpor-relcheck.XXXXXX")"
trap 'rm -rf "$T"' EXIT
mkdir -p "$T/proj" "$T/hm"
echo hello > "$T/proj/a.txt"
unset CLAUDE_CONFIG_DIR CLAUDECODE TORPOR_OFF TORPOR_FAULT
export HOME="$T/hm" XDG_CONFIG_HOME="$T/hm/.config" TORPOR_HOME="$T/store" TORPOR_KEY_FILE="$T/hm/.config/torpor/key" CLAUDE_PROJECT_DIR="$T/proj" CLAUDE_CODE_MANAGED_SETTINGS_PATH="$T/managed"
fail() { echo "FAIL: $1"; exit 1; }
hook() { # hook <event> <extra json fields, with a leading comma or empty>
  printf '%s' '{"session_id":"s1","cwd":"'"$T"'/proj","hook_event_name":"'"$1"'","prompt_id":"p1","permission_mode":"default"'"$2"'}' | "$BIN" hook "$1"
}

# A panic, a 10 s sleep and an empty disk: a build with fault-inject would print nothing, take 5 s or skip the capture.
export TORPOR_TEST_PANIC=1 TORPOR_TEST_SLEEP_MS=10000 TORPOR_TEST_FREE_BYTES=0 TORPOR_TEST_NO_GC=1
start=$(date +%s) # whole seconds: BSD date (macOS) has no %N
code=0
out=$(hook UserPromptSubmit ',"prompt":"hi"' 2>"$T/err") || code=$?
secs=$(( $(date +%s) - start ))
[ "$code" = 0 ] || fail "exit code $code"
[ -z "$out" ] || fail "stdout not empty: $out"
[ ! -s "$T/err" ] || fail "stderr not empty"
[ "$secs" -lt 5 ] || fail "took $secs s: TORPOR_TEST_SLEEP_MS (10 s) was honoured"
if [ -f "$T/store/faults.log" ] && grep -Eq 'panic|timeout|disk' "$T/store/faults.log"; then fail "a fault was injected: $(cat "$T/store/faults.log")"; fi
unset TORPOR_TEST_PANIC TORPOR_TEST_SLEEP_MS TORPOR_TEST_FREE_BYTES
"$BIN" status --json | grep -q '"points":[1-9]' || fail "the capture did not run (TORPOR_TEST_FREE_BYTES honoured?)"

# A second state, saved with a kill-at-the-first-capture-step request that a plain build must ignore.
echo changed > "$T/proj/a.txt"
code=0
TORPOR_FAULT=capture:1 hook Stop ',"stop_hook_active":false,"last_assistant_message":"done"' >/dev/null 2>"$T/err" || code=$?
[ "$code" = 0 ] || fail "Stop exited $code with TORPOR_FAULT=capture:1: the variable was honoured"
"$BIN" status --json | grep -q '"points":[2-9]' || fail "the second save point is missing"

# Going back must really restore the first state, with a kill-at-the-first-restore-step request that a plain build must ignore.
code=0
TORPOR_FAULT=restore:1 "$BIN" back 1 >"$T/out" 2>"$T/err" || code=$?
[ "$code" = 0 ] || fail "back exited $code with TORPOR_FAULT=restore:1: $(cat "$T/err")"
[ "$(cat "$T/proj/a.txt")" = hello ] || fail "back did not bring a.txt back (it says: $(cat "$T/proj/a.txt"))"
# `torpor runner install` has one source for its download. A plain build carries no code that reads the registry, hash or browser overrides
# of the tests, nor the random-number and port overrides (TORPOR_TEST_PORT also matches TORPOR_TEST_PORTS): the strings are not in the file. Without --yes
# and without a terminal it asks, says so and downloads nothing.
for v in TORPOR_TEST_REGISTRY TORPOR_TEST_INTEGRITY TORPOR_TEST_NO_BROWSER TORPOR_TEST_NO_RANDOM TORPOR_TEST_PORT; do
  if grep -aq "$v" "$BIN"; then fail "$v is in this build: it can be read"; fi
done
if command -v node >/dev/null 2>&1; then
  code=0
  TORPOR_TEST_REGISTRY=http://127.0.0.1:1 "$BIN" runner install </dev/null >"$T/out" 2>"$T/err" || code=$?
  [ "$code" = 3 ] || fail "runner install without --yes exited $code, not 3: $(cat "$T/err")"
  grep -q 'npm registry' "$T/out" || fail "the question does not name the npm registry: $(cat "$T/out")"
  grep -q '127.0.0.1' "$T/out" && fail "the question names 127.0.0.1: $(cat "$T/out")"
  [ ! -e "$T/store/runner" ] || fail "runner install wrote into the runner folder before an answer"
fi
echo "ok: $BIN ignores TORPOR_TEST_PANIC, TORPOR_TEST_SLEEP_MS, TORPOR_TEST_FREE_BYTES, TORPOR_FAULT=capture and TORPOR_FAULT=restore, and carries no override for the runner install registry, the random numbers or the app port (hook took ${secs}s)"

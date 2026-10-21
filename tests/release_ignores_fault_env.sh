#!/bin/sh
# The plain release build (no `fault-inject` feature) must ignore every TORPOR_TEST_* variable.
# Run from the torpor/ folder: sh tests/release_ignores_fault_env.sh
# Note: `cargo test --release` builds the same target/release/torpor WITH fault-inject, so this script builds first.
set -eu
cd "$(dirname "$0")/.."
cargo build --release --quiet
BIN="$PWD/target/release/torpor"
T="$(mktemp -d "${TMPDIR:-/tmp}/torpor-relcheck.XXXXXX")"
trap 'rm -rf "$T"' EXIT
mkdir -p "$T/proj" "$T/home"
echo hello > "$T/proj/a.txt"
unset CLAUDE_CONFIG_DIR TORPOR_OFF TORPOR_FAULT
export HOME="$T/home" XDG_CONFIG_HOME="$T/home/.config" TORPOR_HOME="$T/store" TORPOR_KEY_FILE="$T/home/.config/torpor/key" CLAUDE_PROJECT_DIR="$T/proj"
# A panic, a 10 s sleep and an empty disk: a build with fault-inject would print nothing, take 5 s or skip the capture.
export TORPOR_TEST_PANIC=1 TORPOR_TEST_SLEEP_MS=10000 TORPOR_TEST_FREE_BYTES=0 TORPOR_TEST_NO_GC=1
P='{"session_id":"s1","cwd":"'"$T"'/proj","hook_event_name":"UserPromptSubmit","prompt":"hi","prompt_id":"p1"}'
start=$(date +%s%N)
out=$(printf '%s' "$P" | "$BIN" hook UserPromptSubmit 2>"$T/err")
code=$?
ms=$(( ($(date +%s%N) - start) / 1000000 ))
fail() { echo "FAIL: $1"; exit 1; }
[ "$code" = 0 ] || fail "exit code $code"
[ -z "$out" ] || fail "stdout not empty: $out"
[ ! -s "$T/err" ] || fail "stderr not empty"
[ "$ms" -lt 2000 ] || fail "took $ms ms: TORPOR_TEST_SLEEP_MS was honoured"
if [ -f "$T/store/faults.log" ] && grep -Eq 'panic|timeout|disk' "$T/store/faults.log"; then fail "a fault was injected: $(cat "$T/store/faults.log")"; fi
unset TORPOR_TEST_PANIC TORPOR_TEST_SLEEP_MS TORPOR_TEST_FREE_BYTES
"$BIN" status --json | grep -q '"points":[1-9]' || fail "the capture did not run (TORPOR_TEST_FREE_BYTES honoured?)"
echo "ok: plain release build ignores TORPOR_TEST_PANIC, TORPOR_TEST_SLEEP_MS and TORPOR_TEST_FREE_BYTES (hook took $ms ms)"

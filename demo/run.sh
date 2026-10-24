#!/bin/sh
# Offline demo: drives the real torpor binary with real hook JSON in a throwaway folder, HOME and store.
# Needs node. Usage: demo/run.sh   (TORPOR_BIN=path to use another binary). Never touches your own HOME.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${TORPOR_BIN:-}
if [ -z "$BIN" ]; then
  for c in "$ROOT/target/release/torpor" "$ROOT/target/debug/torpor" "$ROOT/bin/torpor"; do [ -x "$c" ] && BIN=$c && break; done
fi
[ -x "$BIN" ] || { echo "no torpor binary: run cargo build first, or set TORPOR_BIN" >&2; exit 2; }
command -v node >/dev/null || { echo "the demo needs node" >&2; exit 2; }
W=$(mktemp -d "${TMPDIR:-/tmp}/torpor-demo.XXXXXX")
trap 'rm -rf "$W"' EXIT INT TERM
P=$W/proj; mkdir -p "$P" "$W/hm"
cp "$ROOT/tests/fixtures/app/server.js" "$P/server.js"
SRV=$(cat "$P/server.js"); rm "$P/server.js"
export HOME=$W/hm XDG_CONFIG_HOME=$W/hm/.config TORPOR_HOME=$W/store TORPOR_KEY_FILE=$W/hm/key CLAUDE_PROJECT_DIR=$P
export CLAUDE_CODE_MANAGED_SETTINGS_PATH=$W/hm/managed
unset CLAUDECODE CLAUDE_CONFIG_DIR TORPOR_OFF TORPOR_FAULT
cd "$P" || exit 2
SID=demo-session-0000-4000-8000-000000000001; TR=$W/transcript.jsonl; N=0; PID=; PMODE=default

say() { printf '\n== %s\n' "$*"; }
# hook EVENT [extra json members, starting with a comma]; prints the answer (the part the person or the AI sees)
hook() {
  ev=$1; extra=${2:-}; pid=; [ "$ev" != SessionStart ] && pid=",\"prompt_id\":\"$PID\""
  printf '{"session_id":"%s","transcript_path":"%s","cwd":"%s","hook_event_name":"%s","permission_mode":"%s"%s%s}' \
    "$SID" "$TR" "$P" "$ev" "$PMODE" "$pid" "$extra" | "$BIN" hook "$ev"
}
jstr() { printf '%s' "$1" | node -e 'process.stdout.write(JSON.stringify(require("fs").readFileSync(0,"utf8")))'; }
prompt() {
  N=$((N+1)); PID=prompt-$N
  printf '{"type":"user","uuid":"u-%s","promptId":"%s","turnOrigin":"human","promptSource":"typed","isSidechain":false}\n' "$N" "$PID" >> "$TR"
  say "turn $N, you say: $1"; hook UserPromptSubmit ",\"prompt\":$(jstr "$1")"
}
stop() { hook Stop ',"stop_hook_active":false,"last_assistant_message":"done"'; }
T=0
# edit_tool Write|Edit FILE CONTENT: the AI's edit tool (Torpor sees it before and after)
edit_tool() {
  T=$((T+1)); inp="{\"file_path\":\"$P/$2\"}"
  hook PreToolUse ",\"tool_name\":\"$1\",\"tool_input\":$inp,\"tool_use_id\":\"t$T\""
  printf '%s\n' "$3" > "$P/$2"
  hook PostToolUse ",\"tool_name\":\"$1\",\"tool_input\":$inp,\"tool_use_id\":\"t$T\",\"tool_response\":{\"filePath\":\"$P/$2\",\"success\":true}"
}
# shell CMD: the AI's Bash tool
shell() {
  T=$((T+1)); inp="{\"command\":$(jstr "$1")}"
  hook PreToolUse ",\"tool_name\":\"Bash\",\"tool_input\":$inp,\"tool_use_id\":\"t$T\""
  sh -c "$1" >/dev/null 2>&1
  hook PostToolUse ",\"tool_name\":\"Bash\",\"tool_input\":$inp,\"tool_use_id\":\"t$T\",\"tool_response\":{\"stdout\":\"\",\"stderr\":\"\",\"interrupted\":false}"
}
# ai ARGS... < stdin: the AI runs a torpor command in its Bash tool (CLAUDECODE is set, so the AI-only limits apply)
ai() {
  spec=$(cat); T=$((T+1))
  hook PreToolUse ",\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"torpor $*\"},\"tool_use_id\":\"t$T\""
  printf '%s' "$spec" | CLAUDECODE=1 "$BIN" "$@"; echo "[exit $?]"
}
you() { echo "\$ torpor $*"; "$BIN" "$@"; echo "[exit $?]"; }

SPEC='{"v":1,"name":"login works","why":"people can sign in","start":{"cmd":"node server.js --port {port}","ready":"/login","timeout_s":30},"env":{},"steps":[
 {"http":{"method":"POST","path":"/api/login","json":{"email":"kim@example.com"}},"expect":{"status":200},"why":"sign-in is accepted"},
 {"http":{"path":"/api/me"},"expect":{"status":200,"json_has":["email"]},"why":"after sign-in the account data shows"}]}'

say "session start"; hook SessionStart ',"source":"startup"'
prompt "make a login page"
BAD=$(printf '%s' "$SRV" | sed 's/const BROKEN = false;/const BROKEN = true;/')
edit_tool Write server.js "$BAD"; edit_tool Write notes.txt "v1"
stop
prompt "login does not work, fix it"
edit_tool Edit server.js "$SRV"
stop
prompt "great, sign-in is accepted now. save a check for it"
say "the AI saves a check (a promise): it fails before the fix and passes after"
printf '%s' "$SPEC" | ai promise add -
stop
prompt "tidy up the code"
edit_tool Edit notes.txt "v2"
shell "sed -i 's/const BROKEN = false;/const BROKEN = true;/' server.js"
stop
prompt "add a readme"
edit_tool Write README.md "# demo"
stop

say "you rewind in Claude Code: it restores the file the edit tool changed (notes.txt), not the shell change"
printf 'v1\n' > "$P/notes.txt"
prompt "carry on"
stop

say "which turn broke the login?"
printf '%s' "$SPEC" | ai find-break --check -
say "you undo turn 4 (the break): the result is checked first, and passes"
you undo 4
say "you undo turn 2 (the fix): the check protects it, so the undo is refused"
you undo 2
say "log"; you log -n 6

say "a blocking check stops the AI once"
you promise block 1
prompt "speed up the server"
shell "sed -i 's/const BROKEN = false;/const BROKEN = true;/' server.js"
say "the AI tries to finish:"; stop
say "and again (Torpor blocks only once for the same change):"; stop
echo; echo "demo done"

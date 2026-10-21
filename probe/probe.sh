#!/bin/sh
# Torpor day-1 probe (spec section 11). Throwaway, synthetic runs only, never shipped.
# Saves the stdin of every hook call to $PROBE_OUT/<ns>-<event>.json plus side files.
ev="$1"
out="${PROBE_OUT:-/tmp/claude-1000/torpor-probe/out-default}"
tok="${PROBE_TOKEN:-none}"
mkdir -p "$out" 2>/dev/null
n="$(date +%s%N)-$$"
f="$out/$n-$ev"
cat > "$f.json"
printf 'argv0=%s args=%s plugin_root=%s project_dir=%s cwd=%s\n' "$0" "$*" "${CLAUDE_PLUGIN_ROOT-UNSET}" "${CLAUDE_PROJECT_DIR-UNSET}" "$(pwd)" > "$f.info"
env | cut -d= -f1 | sort > "$f.envnames"
date +%s.%N > "$f.time"
base="${ev%-shell}"
case "$base" in
  SessionStart)
    # P5: a detached helper that must outlive the session (checked against SessionEnd time).
    setsid sh -c "sleep 45; date +%s.%N > '$out/detached-ok-$n'" </dev/null >/dev/null 2>&1 &
    ;;
  UserPromptSubmit)
    # P8: additionalContext notice (different token family than the Stop line).
    case "$ev" in
      *-shell) ;;
      *) printf '{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"TORPOR-NOTICE-%s: probe notice, ignore it."}}' "$tok" ;;
    esac
    ;;
  Stop)
    case "$ev" in
      *-shell) printf '{"systemMessage":"TORPOR-PROBE-S%s"}' "$tok" ;;
      *) printf '{"systemMessage":"TORPOR-PROBE-E%s"}' "$tok" ;;
    esac
    ;;
  PreToolUse)
    body="$(cat "$f.json")"
    case "$body" in
      *'"echo probe-ask"'*)
        printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask","permissionDecisionReason":"Torpor probe: ask for echo probe-ask"}}' ;;
      *'"echo probe-deny"'*)
        printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"Torpor probe: deny for echo probe-deny"}}' ;;
    esac
    ;;
esac
exit 0

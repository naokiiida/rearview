#!/usr/bin/env bash
# Per-feature end-to-end checks of the Antigravity (agy) provider against a real
# agy, driven through cmux (`cmux send`, `cmux send-key`, `cmux read-screen`).
#
# Needs: cmux running, agy logged in, sqlite3. Uses a scratch workspace and a
# scratch project directory; it renames only the session it creates.
# Usage: scripts/agy-cmux-e2e.sh [feature ...]   (default: all)
#   features: title rename fork model
set -euo pipefail
export CMUX_QUIET=1

DATA="${ANTIGRAVITY_DATA:-$HOME/.gemini/antigravity-cli}"
PROJECT="$(mktemp -d /tmp/rearview-agy-e2e.XXXXXX)"
WS=""
pass=0 fail=0

step() { printf '\n== %s\n' "$*"; }
ok() { pass=$((pass + 1)); printf 'ok   %s\n' "$*"; }
bad() { fail=$((fail + 1)); printf 'FAIL %s\n' "$*"; }
check() { # check "<name>" <command...>
  local name=$1; shift
  if "$@" >/dev/null 2>&1; then ok "$name"; else bad "$name"; fi
}

send() { cmux send --workspace "$WS" -- "$1" >/dev/null; }
key() { cmux send-key --workspace "$WS" -- "$1" >/dev/null; }
screen() { cmux read-screen --workspace "$WS" --lines "${1:-40}"; }
line() { send "$1"; sleep 1; key enter; }
wait_for() { # wait_for <seconds> <pattern>
  local deadline=$((SECONDS + $1))
  while ((SECONDS < deadline)); do
    screen 60 | rg -q -- "$2" && return 0
    sleep 1
  done
  return 1
}
wait_for_file() { # wait_for_file <file>
  for _ in $(seq 15); do
    test -s "$1" && return 0
    sleep 1
  done
  return 1
}
latest_session() { ls -t "$DATA/brain" | head -1; }
summary_title() { sqlite3 "$DATA/conversation_summaries.db" \
  "select title from conversation_summaries where conversation_id='$1'"; }

cleanup() {
  [[ -n "$WS" ]] && cmux workspace close --workspace "$WS" >/dev/null 2>&1 || true
  rm -rf "$PROJECT"
}
trap cleanup EXIT

start_agy() {
  WS=$(cmux workspace create --name rearview-agy-e2e --cwd "$PROJECT" | awk '{print $2}')
  sleep 2
  line "agy --dangerously-skip-permissions -i 'reply with the single word pong'"
  wait_for 20 "trust this folder" && key enter
  wait_for 90 "pong|Hi" || { bad "agy answered the prompt"; return 1; }
  sleep 2
  SESSION=$(latest_session)
}

features=("$@")
[[ ${#features[@]} -eq 0 ]] && features=(title rename fork model rearview)
want() { printf '%s\n' "${features[@]}" | rg -qx "$1"; }

step "launch agy"
start_agy
echo "session $SESSION"
check "session directory has a transcript" wait_for_file "$DATA/brain/$SESSION/.system_generated/logs/transcript.jsonl"

if want title; then
  step "title: agy records one in conversation_summaries"
  for _ in $(seq 20); do [[ -n "$(summary_title "$SESSION")" ]] && break; sleep 1; done
  [[ -n "$(summary_title "$SESSION")" ]] && ok "generated title: $(summary_title "$SESSION")" || bad "generated title"
fi

if want rename; then
  step "rename: /rename writes conversation_summaries.title"
  line "/rename Rearview E2E Title"
  wait_for 10 "renamed to: Rearview E2E Title" && ok "agy confirmed rename" || bad "agy confirmed rename"
  sleep 1
  [[ "$(summary_title "$SESSION")" == "Rearview E2E Title" ]] && ok "title stored in summaries" || bad "title stored in summaries"
fi

if want fork; then
  step "fork: only exists as /fork inside a running session"
  before=$(latest_session)
  line "/fork"
  wait_for 10 "Forked conversation" && ok "/fork inside a session works" || bad "/fork inside a session"
  sleep 1
  after=$(latest_session)
  [[ "$before" != "$after" ]] && ok "fork made a new session $after" || bad "fork made a new session"
  line "say hi in one word"
  wait_for 20 "Hi|pong" || true
  check "fork has its own transcript" wait_for_file "$DATA/brain/$after/.system_generated/logs/transcript.jsonl"
fi

if want model; then
  step "model: the per-conversation database names it"
  db="$DATA/conversations/$SESSION.db"
  model=$(python3 -c "
import sqlite3, re
try:
    db = sqlite3.connect('$db')
    for (data,) in db.execute('SELECT data FROM gen_metadata'):
        m = re.search(r'(claude|gemini|gpt)-[a-zA-Z0-9.-]+', data.decode('latin1'))
        if m:
            print(m.group(0))
            break
except Exception:
    pass
")
  if [[ -n "$model" ]]; then
    ok "model: $model"
  else
    bad "model name in gen_metadata"
  fi
fi

if want rearview; then
  step "rearview: loads and reads both agy sessions"
  cargo run --quiet --bin rearview -- agent sessions >/dev/null
  check "rearview reads root session" cargo run --quiet --bin rearview -- agent read "$SESSION"
  if [[ -n "${after:-}" ]]; then
    check "rearview reads forked session" cargo run --quiet --bin rearview -- agent read "$after"
  fi
fi

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[[ $fail -eq 0 ]]

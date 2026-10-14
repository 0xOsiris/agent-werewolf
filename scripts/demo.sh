#!/usr/bin/env bash
# Starts a server and every roster agent, each on a different model, that find it over mDNS.
set -euo pipefail

cd "$(dirname "$0")/.."
# shellcheck source=scripts/roster.sh
source scripts/roster.sh

if [ -z "${OPENROUTER_API_KEY:-}" ]; then
  echo "OPENROUTER_API_KEY is not set; export it first (get one at https://openrouter.ai/keys)" >&2
  exit 1
fi

cleanup() {
  local pids
  pids=$(jobs -p)
  if [ -n "$pids" ]; then
    # shellcheck disable=SC2086 # word splitting is intended: one pid per word
    kill $pids 2>/dev/null || true
    wait 2>/dev/null || true
  fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM

cargo build --release -p werewolf-server -p werewolf-agent
BIN=target/release

TMP_ROOT="${TMPDIR:-/tmp}"
SERVER_LOG=$(mktemp "${TMP_ROOT%/}/werewolf-server.XXXXXX")
"$BIN/werewolf-server" --auto-start "${#PLAYERS[@]}" --wolves "${WOLVES:-2}" >"$SERVER_LOG" 2>&1 &
server_pid=$!

spectate=""
for _ in $(seq 100); do
  spectate=$(grep -m 1 '^spectate: ' "$SERVER_LOG" || true)
  [ -n "$spectate" ] && break
  if ! kill -0 "$server_pid" 2>/dev/null; then
    echo "server exited during startup:" >&2
    cat "$SERVER_LOG" >&2
    exit 1
  fi
  sleep 0.1
done
if [ -z "$spectate" ]; then
  echo "server printed no spectate URL within 10s; see $SERVER_LOG" >&2
  exit 1
fi
echo
echo "  $spectate"
echo "  server log: $SERVER_LOG"
echo

agent_pids=()
for entry in "${PLAYERS[@]}"; do
  IFS='|' read -r name model persona <<<"$entry"
  "$BIN/werewolf-agent" --name "$name" --model "$model" --persona "$persona" &
  agent_pids+=("$!")
done

status=0
for pid in "${agent_pids[@]}"; do
  wait "$pid" || status=1
done
exit "$status"

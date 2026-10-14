#!/usr/bin/env bash
# Plays one fast game with six --mock agents against a local server, then prints its summary.
set -euo pipefail

cd "$(dirname "$0")/.."
PORT="${PORT:-7777}"
# Mock games go to a scratch dir so they don't mix with real benchmark data in ./games.
TMP_ROOT="${TMPDIR:-/tmp}"
LOG_DIR="${LOG_DIR:-$(mktemp -d "${TMP_ROOT%/}/werewolf-mock.XXXXXX")}"

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

"$BIN/werewolf-server" --port "$PORT" --auto-start 6 --pace-ms 200 --turn-timeout-secs 10 \
  --log-dir "$LOG_DIR" --no-mdns &
server_pid=$!

agent_pids=()
for i in 1 2 3 4 5 6; do
  "$BIN/werewolf-agent" --mock --seed "$i" --name "mock-$i" --server "http://127.0.0.1:$PORT" &
  agent_pids+=("$!")
done

status=0
for pid in "${agent_pids[@]}"; do
  wait "$pid" || status=1
done

# The summary is written as the game ends; give it a moment before stopping the server.
summary=""
for _ in $(seq 50); do
  summary=$(find "$LOG_DIR" -name '*-summary.json' | head -n 1)
  [ -n "$summary" ] && break
  sleep 0.1
done
kill "$server_pid" 2>/dev/null || true
wait "$server_pid" 2>/dev/null || true

if [ -z "$summary" ]; then
  echo "no summary file found in $LOG_DIR" >&2
  exit 1
fi
echo "summary: $summary"
cat "$summary"
exit "$status"

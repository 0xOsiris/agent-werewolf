#!/usr/bin/env bash
# Starts a server plus AI players and opens the game so you can join from your browser or phone.
# Usage: scripts/play.sh            (9 AIs, 2 wolves, starts when 1 human joins)
#        AIS=4 HUMANS=3 WOLVES=1 scripts/play.sh
#        HUMANS=0 scripts/play.sh   (AIs only; just watch)
set -euo pipefail

cd "$(dirname "$0")/.."
# shellcheck source=scripts/roster.sh
source scripts/roster.sh

AIS="${AIS:-9}"
HUMANS="${HUMANS:-1}"
WOLVES="${WOLVES:-2}"
PORT="${PORT:-7777}"
total=$((AIS + HUMANS))

if ((AIS < 1 || AIS > ${#PLAYERS[@]})); then
  echo "AIS must be between 1 and ${#PLAYERS[@]}" >&2
  exit 1
fi
if ((HUMANS < 0 || total < 5 || total > 12)); then
  echo "need HUMANS >= 0 and AIS + HUMANS between 5 and 12 (got $AIS + $HUMANS)" >&2
  exit 1
fi
# Sheriff and Cupid are on by default; the server refuses to start a game too small for them.
need=$((2 * WOLVES + 1 > WOLVES + 5 ? 2 * WOLVES + 1 : WOLVES + 5))
if ((total < need)); then
  echo "warning: $WOLVES wolves + sheriff + Cupid need $need players (have $total)." >&2
  echo "         Turn off sheriff or Cupid in the lobby settings, or set WOLVES=1." >&2
fi
mock=0
if [ -z "${OPENROUTER_API_KEY:-}" ]; then
  mock=1
  echo "OPENROUTER_API_KEY is not set, so the AIs will be random mock bots." >&2
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

cargo build --release -q -p werewolf-server -p werewolf-agent
BIN=target/release
local_url="http://127.0.0.1:$PORT"

TMP_ROOT="${TMPDIR:-/tmp}"
SERVER_LOG=$(mktemp "${TMP_ROOT%/}/werewolf-server.XXXXXX")
# Auto-start fires on every join, so after a Reset the next game starts once everyone is back.
"$BIN/werewolf-server" --port "$PORT" --auto-start "$total" --wolves "$WOLVES" >"$SERVER_LOG" 2>&1 &
server_pid=$!

spectate=""
for _ in $(seq 100); do
  spectate=$(sed -n 's/^spectate: \([^ ]*\).*/\1/p' "$SERVER_LOG" | head -n 1)
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
phone_url=$(curl -fsS --max-time 5 "$local_url/v1/info" | sed -n 's/.*"play_url":"\([^"]*\)".*/\1/p')

# Every AI draws a fresh character (name and persona) from the whole roster each game.
CAST=$(mktemp "${TMP_ROOT%/}/werewolf-cast.XXXXXX")
for entry in "${PLAYERS[@]}"; do
  IFS='|' read -r name _ persona <<<"$entry"
  printf '%s|%s\n' "$name" "$persona"
done >"$CAST"

for entry in "${PLAYERS[@]:0:AIS}"; do
  IFS='|' read -r _ model _ <<<"$entry"
  if ((mock)); then
    "$BIN/werewolf-agent" --mock --cast "$CAST" --server "$local_url" --games 0 &
  else
    "$BIN/werewolf-agent" --model "$model" --cast "$CAST" --server "$local_url" --games 0 &
  fi
done

cat <<EOF

  Watch (projector): $spectate
  Join (this laptop): $local_url/play
  Join (phone):       ${phone_url:-scan the QR code on the watch page}

  The game starts when $HUMANS human(s) join. For another round, click Reset on the
  watch page, then Play again on /play. Ctrl-C stops everything.
  Server log: $SERVER_LOG

EOF

if command -v open >/dev/null; then
  open "$spectate"
  ((HUMANS > 0)) && open "$local_url/play"
fi
wait "$server_pid"

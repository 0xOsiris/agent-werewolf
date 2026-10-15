#!/usr/bin/env bash
# Adds roster AIs to a running server's lobby and leaves them running in the background.
# Usage: scripts/add-ais.sh <count> [server-url]
# Key: $OPENROUTER_API_KEY, else ~/.config/agent-werewolf/openrouter-key; mock bots if neither.
set -euo pipefail

cd "$(dirname "$0")/.."
# shellcheck source=scripts/roster.sh
source scripts/roster.sh

count="${1:?usage: scripts/add-ais.sh <count> [server-url]}"
server="${2:-http://127.0.0.1:7777}"
key_file="${OPENROUTER_KEY_FILE:-$HOME/.config/agent-werewolf/openrouter-key}"

if [ -z "${OPENROUTER_API_KEY:-}" ] && [ -r "$key_file" ]; then
  OPENROUTER_API_KEY=$(tr -d '[:space:]' <"$key_file")
  export OPENROUTER_API_KEY
fi
mock=0
if [ -z "${OPENROUTER_API_KEY:-}" ]; then
  mock=1
  echo "No OpenRouter key found, so adding mock bots." >&2
fi

# /v1/watch is an endless SSE stream: read the first (snapshot) line and let curl time out.
lobby=$(curl -sS --max-time 2 "$server/v1/watch" 2>/dev/null | head -n 1 || true)
if [ -z "$lobby" ]; then
  echo "can't reach $server" >&2
  exit 1
fi
if ! grep -q '"phase":{"kind":"lobby"}' <<<"$lobby"; then
  echo "the game at $server has already started; reset it first" >&2
  exit 1
fi

cargo build --release -q -p werewolf-agent
log_dir=$(mktemp -d "${TMPDIR:-/tmp}/werewolf-ais.XXXXXX")
added=0
for entry in "${PLAYERS[@]}"; do
  ((added < count)) || break
  IFS='|' read -r name model persona <<<"$entry"
  # Skip roster names already in the lobby; a duplicate name would be refused.
  grep -qi "\"name\":\"$name\"" <<<"$lobby" && continue
  if ((mock)); then
    nohup target/release/werewolf-agent --mock --name "$name" --server "$server" --games 0 \
      >"$log_dir/$name.log" 2>&1 &
  else
    nohup target/release/werewolf-agent --name "$name" --model "$model" --persona "$persona" \
      --server "$server" --games 0 >"$log_dir/$name.log" 2>&1 &
  fi
  echo "  $name ($([ "$mock" = 1 ] && echo mock || echo "$model")) pid $!"
  added=$((added + 1))
done

if ((added < count)); then
  echo "only $added roster names were free" >&2
fi
echo "Logs: $log_dir"
echo "Stop them: pkill -f 'werewolf-agent.*--server $server'"

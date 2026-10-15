#!/usr/bin/env bash
# Plays one staked game end to end on a local anvil chain: 7 mock bots (one forces its moves
# on-chain), a challenge that makes the server prove, then settlement and a token audit.
# Usage: scripts/staked-game.sh    (needs anvil, forge, cast, jq and zk/target/release/werewolf-prove)
set -euo pipefail

cd "$(dirname "$0")/.."
ANVIL_PORT="${ANVIL_PORT:-18546}"
PORT="${PORT:-17778}"
RPC="http://127.0.0.1:$ANVIL_PORT"
SERVER="http://127.0.0.1:$PORT"
PROVER="${PROVER:-zk/target/release/werewolf-prove}"
STAKE=100
MINT=1000000
BOTS=7
# Anvil's well-known public dev keys: account 0 operates, 1-7 play, 8 challenges.
KEYS=(
  0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
  0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d
  0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a
  0x7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6
  0x47e179ec197488593b187f80a00eb0da91f1b9d0b13f8733639f19c30a34926a
  0x8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba
  0x92db14e403b83dfe3df233f83dfa3a0d7096f21ca9b0d6d6b8d88b2b4ec1564e
  0x4bbbf85ce3377467afe5d46f804f221813b2bb87f24d81f60f1fcdbf7cbf4356
  0xdbda1821b80551c9d65939329250298aa3472ba22feea921c0cf5d620ea67b97
)
# GameView words in `gameInfo`'s return data.
STATUS=10
WINNERS=17
PROVEN=22
PROOF_DEADLINE=20
SETTLED=5

for tool in anvil forge cast jq; do
  command -v "$tool" >/dev/null || {
    echo "$tool is required" >&2
    exit 1
  }
done
# Another chain or server on these ports would silently take our bots' stakes.
for port in "$ANVIL_PORT" "$PORT"; do
  if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
    echo "port $port is in use; set ANVIL_PORT / PORT" >&2
    exit 1
  fi
done
if [ ! -x "$PROVER" ]; then
  echo "$PROVER is missing; build it first (see zk/README.md) or set PROVER" >&2
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

# Writes to the original stderr, so it works inside wait_for's redirections too.
exec 4>&2
fail() {
  echo "FAIL: $*" >&4
  echo "logs: $WORK" >&4
  exit 1
}

# Retries "$@" every second until it succeeds or $1 seconds pass.
wait_for() {
  local secs=$1
  shift
  for _ in $(seq "$secs"); do
    "$@" >/dev/null 2>&1 && return 0
    sleep 1
  done
  return 1
}

WORK=$(mktemp -d "${TMPDIR:-/tmp}/werewolf-staked.XXXXXX")
umask 077
for i in "${!KEYS[@]}"; do
  printf '%s\n' "${KEYS[$i]}" >"$WORK/key$i"
done

cargo build --release -q -p werewolf-server -p werewolf-agent
BIN=target/release

anvil --port "$ANVIL_PORT" >"$WORK/anvil.log" 2>&1 &
wait_for 20 cast chain-id --rpc-url "$RPC" || fail "anvil did not start"

(cd contracts && forge script script/Deploy.s.sol --rpc-url "$RPC" --private-key "${KEYS[0]}" \
  --broadcast) >"$WORK/deploy.log" 2>&1 || fail "deploy failed; see $WORK/deploy.log"
deployed=$(grep -o '{"chain_id".*}' "$WORK/deploy.log" | tail -n 1)
chain_id=$(jq -r .chain_id <<<"$deployed")
escrow=$(jq -r .escrow <<<"$deployed")
token=$(jq -r .token <<<"$deployed")

lower() { tr '[:upper:]' '[:lower:]'; }
addrs=()
for i in "${!KEYS[@]}"; do
  addrs+=("$(cast wallet address --private-key "${KEYS[$i]}" | lower)")
  cast send -q "$token" "mint(address,uint256)" "${addrs[$i]}" "$MINT" \
    --private-key "${KEYS[0]}" --rpc-url "$RPC"
done

"$BIN/werewolf-server" --bind 127.0.0.1 --port "$PORT" --no-mdns --log-dir "$WORK/games" --public-url "$SERVER" \
  --auto-start "$BOTS" --max-players "$BOTS" --pace-ms 50 --turn-timeout-secs 8 \
  --discussion-secs 8 --wolf-chat-secs 5 --quiet-secs 3 --ai-chat-cooldown-secs 2 \
  --ai-chat-jitter-secs 1 --stake "$STAKE" --chain-id "$chain_id" --rpc-url "$RPC" \
  --public-rpc-url "$RPC" \
  --escrow "$escrow" --token "$token" --operator-key-file "$WORK/key0" \
  --prover-bin "$PROVER" --prover-mode mock --settle-poll-secs 1 >"$WORK/server.log" 2>&1 &
server_pid=$!

has_stake() {
  kill -0 "$server_pid" || fail "server exited; see $WORK/server.log"
  curl -fsS --max-time 2 "$SERVER/v1/info" | jq -e .stake >/dev/null
}
wait_for 60 has_stake || fail "server created no staked game; see $WORK/server.log"
info=$(curl -fsS --max-time 2 "$SERVER/v1/info")
[ "$(jq -r .stake.escrow <<<"$info" | lower)" = "$(lower <<<"$escrow")" ] ||
  fail "the server on port $PORT is not ours"
game_id=$(jq -r .stake.game_id <<<"$info")
echo "game $game_id on escrow $escrow; logs in $WORK"

# Word $1 of the game's on-chain GameView, in decimal.
field() {
  local raw
  raw=$(cast call "$escrow" "gameInfo(uint256)" "$game_id" --rpc-url "$RPC")
  cast to-dec "0x${raw:$((2 + 64 * $1)):64}"
}
status_at_least() { (($(field "$STATUS") >= $1)); }
balance() { cast call "$token" "balanceOf(address)(uint256)" "$1" --rpc-url "$RPC" | cut -d' ' -f1; }
claimable() { cast call "$escrow" "claimable(uint256,address)(uint256)" "$game_id" "$1" --rpc-url "$RPC" | cut -d' ' -f1; }

bot_pids=()
for i in $(seq "$BOTS"); do
  force=""
  if ((i == BOTS)); then force=--force-moves; fi
  "$BIN/werewolf-agent" --mock --seed "$i" --name "Bot$i" --server "$SERVER" --games 1 \
    --wallet-key-file "$WORK/key$i" ${force:+"$force"} >"$WORK/bot$i.log" 2>&1 &
  bot_pids+=("$!")
done

wait_for 900 status_at_least 3 || fail "no proposal within 15 minutes"
echo "proposed; challenging from ${addrs[8]}"
cast send -q "$token" "approve(address,uint256)" "$escrow" "$STAKE" --private-key "${KEYS[8]}" --rpc-url "$RPC"
cast send -q "$escrow" "challenge(uint256)" "$game_id" --private-key "${KEYS[8]}" --rpc-url "$RPC"
proven() { (($(field "$PROVEN") == 1)); }
wait_for 300 proven || fail "server did not prove within 5 minutes"

now=$(cast block latest --field timestamp --rpc-url "$RPC")
cast rpc evm_increaseTime $(($(field "$PROOF_DEADLINE") - now + 1)) --rpc-url "$RPC" >/dev/null
cast rpc evm_mine --rpc-url "$RPC" >/dev/null
wait_for 120 status_at_least "$SETTLED" || fail "game did not finalize after the proof window"
(($(field "$STATUS") == SETTLED)) || fail "game ended in status $(field "$STATUS"), not Settled"

for pid in "${bot_pids[@]}"; do
  for _ in $(seq 120); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 1
  done
  kill -0 "$pid" 2>/dev/null && fail "a bot did not exit within 2 minutes of settlement"
  wait "$pid" || fail "a bot exited with an error"
done
operator_claimed() { (($(claimable "${addrs[0]}") == 0)); }
wait_for 60 operator_claimed || fail "operator did not claim its bond"

# Seats are on-chain join order; each winner gets pot / winners, and the lowest also the dust.
read -r -a seats <<<"$(cast call "$escrow" "seats(uint256)(address[],bytes32[])" "$game_id" \
  --rpc-url "$RPC" | head -n 1 | tr -d '[],' | lower)"
winners=$(field "$WINNERS")
pot=$((STAKE * ${#seats[@]}))
count=0
for s in "${!seats[@]}"; do
  if (((winners >> s) & 1)); then count=$((count + 1)); fi
done
((count > 0)) || fail "no winners"
dust=$((pot % count))
total=0
for i in "${!addrs[@]}"; do
  got=$(balance "${addrs[$i]}")
  total=$((total + got))
  case $i in
  0) want=$((MINT + STAKE)) ;; # operator: its bond back plus the losing challenge bond
  8) want=$((MINT - STAKE)) ;;
  *) want=$MINT ;;
  esac
  for s in "${!seats[@]}"; do
    if [ "${seats[$s]}" = "${addrs[$i]}" ]; then
      want=$((MINT - STAKE))
      if (((winners >> s) & 1)); then
        want=$((want + pot / count + dust))
        dust=0
      fi
    fi
  done
  ((got == want)) || fail "account $i holds $got, expected $want"
done
left=$(balance "$escrow")
((total + left == MINT * ${#KEYS[@]})) || fail "tokens don't add up: $total + escrow $left"
((left == 0)) || fail "escrow still holds $left"

if grep -l ERROR "$WORK"/bot*.log; then fail "a bot logged an error"; fi
# Bot $BOTS answers target turns only on-chain; every other bot only over HTTP.
force_seat=-1
for s in "${!seats[@]}"; do
  if [ "${seats[$s]}" = "${addrs[$BOTS]}" ]; then force_seat=$s; fi
done
answers=$(curl -fsS --max-time 5 "$SERVER/v1/transcript?game_id=$game_id" |
  jq -c --argjson seat "$force_seat" '[.steps[] | select(.type == "round") | .answers[]] |
    {mine: map(select(.seat == $seat) | .source) | group_by(.) | map({(.[0]): length}) | add,
     others_forced: map(select(.seat != $seat and .source == "forced")) | length}')
forced=$(jq '.mine.forced // 0' <<<"$answers")
(($(jq '.others_forced' <<<"$answers") == 0)) || fail "bots without --force-moves have forced answers: $answers"
(($(jq '.mine.signed // 0' <<<"$answers") == 0)) || fail "bot $BOTS sent a target move over HTTP: $answers"
if (($(jq '.mine.timeout // 0' <<<"$answers") > 0)); then
  fail "a forced move by bot $BOTS was not applied: $answers"
fi
if ((forced > 0)); then
  forced_note="$forced forced move(s) by bot $BOTS in the transcript"
else
  forced_note="bot $BOTS had no target turn, so nothing was forced"
fi

echo "PASS: game $game_id Settled after a challenge and proof; $count winner(s) paid, $forced_note."

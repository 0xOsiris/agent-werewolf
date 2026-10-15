# Staked games and settlement

Staked games put every buy-in into an escrow contract before roles exist. The server deals roles from a seed it committed to, mixed with on-chain randomness. Players sign every move. When the game ends, the server posts the result and publishes the transcript. Anyone can replay the transcript with `werewolf-verify`. A challenge forces the server to post an SP1 proof of the replay.

The contract ABI is [`contracts/src/IWerewolfEscrow.sol`](../contracts/src/IWerewolfEscrow.sol). This file defines every byte that is hashed or signed. Free games are unchanged.

## Notation

- `‖` concatenates bytes. `be64`, `be32` are big-endian integers. `u256` is 32 bytes big-endian. `addr` is 20 bytes.
- `json(x)` is the compact serde_json encoding of a `werewolf-protocol` type, byte for byte as the server sends it. Speech never contains control characters other than `\n`, so `JSON.stringify` of the parsed value gives the same bytes.
- `D = be64(chainId) ‖ addr(escrow) ‖ u256(gameId)`, the game's domain.
- Teams: `0` village, `1` wolves, `2` lovers. `winners` is a bitmap: bit `i` is set when `Game::won(i)`.

## Rules and seats

- `rulesHash = keccak(abi.encode(uint8 wolves, bool sheriff, bool cupid))`. Staked games always fix `wolves`.
- Seat `i` is the `i`-th `join` on-chain, from 0. It is also the engine's `PlayerId`.
- At `start` the contract stores `rosterHash = keccak(abi.encode(address[] players, bytes32[] sessionKeys))`.
- A seat that staked but never joined over HTTP still plays. All its turns time out.

## The deal

- `seed` is 32 random bytes. `create` posts `seedCommit = keccak(seed)`.
- `start` asks the randomness provider for `randomness` (bytes32), which arrives through `fulfillRandomness`.
- Engine seed: `keccak("werewolf/deal/v1" ‖ seed ‖ randomness ‖ rosterHash)`, fed to `ChaCha20Rng::from_seed`. The engine never uses `StdRng`, because its output may change between `rand` versions.
- `propose` reveals `seed`, and the contract checks it against `seedCommit`.

## Session keys and signatures

Each seat registers an Ed25519 public key (`sessionKey`) in `join`. Browsers make it with WebCrypto, and agents with `ed25519-dalek`. Signatures are RFC 8032 Ed25519, verified strictly.

- **Join proof:** `sign("werewolf/join/v1" ‖ D ‖ addr(player))`. The HTTP join carries it, so the server knows the joiner holds the seat's key.
- **Act message:** `M = "werewolf/act/v1" ‖ D ‖ be64(turnId) ‖ be32(step) ‖ be32(seat) ‖ logHash ‖ json(action)`. The client sends `sign(M)` and `logHash` with every act. `actHash = sha256(M)`.
- `step` counts every chat window and every target round from 0, in play order. `round` counts target rounds only. Each `Turn` carries both.

## Public log hash

`h₀ = sha256("werewolf/log/v1" ‖ D)` and `hₖ₊₁ = sha256(hₖ ‖ json(public_log[k]))`. A client signs `hₙ`, where `n` is the length of the `view.public_log` it last saw. The verifier accepts any `hₖ` with `k` up to the log length when the action was applied. So a lagging client is fine, but a message the server altered or invented breaks the match.

## Receipts

For every action it records in the transcript, the server returns a receipt:

- `receiptDigest = keccak(abi.encode(keccak("werewolf/receipt/v1"), uint64 chainId, address escrow, uint256 gameId, uint64 turnId, uint32 seat, bytes32 actHash))`
- `signature` is a 65-byte secp256k1 signature (`r ‖ s ‖ v`, `v` 27 or 28) by the game's operator over the raw digest, with no EIP-191 prefix.

The server gives no receipt for passes, chat dropped by rate limits, or stale turns, because none of those enter the transcript. `disputeReceipt` makes the operator prove the digest is a leaf of `transcriptRoot`.

## Transcript root

The leaves are the `receiptDigest` of every signed entry (chat messages, then signed answers, in transcript order). Each pair hashes as `keccak(min(a, b) ‖ max(a, b))`, which is OpenZeppelin's `MerkleProof` convention. An odd last node moves up unchanged. One leaf is its own root. No leaves gives `bytes32(0)`.

## Forced moves

If the server won't take a move (no receipt within 5 s), the player calls `forceAction(gameId, round, payload)` from its seat's wallet. The contract accepts only `round == nextRound`, the round not yet checkpointed.

- `encKey` is the operator's X25519 public key, posted in `create`.
- `payload = ePub ‖ AES-256-GCM(k, nonce = 12 zero bytes, plaintext = json(action), aad = be32(round) ‖ be32(seat) ‖ D)`, with the 16-byte tag appended.
- `k = sha256("werewolf/force/v1" ‖ X25519(e, encKey) ‖ ePub ‖ encKey)`, for a fresh ephemeral key `e`. A non-contributory shared secret is rejected.

Before applying round `r`, the server calls `checkpoint(gameId, r)`, waits for it to confirm, and reads every `ActionForced` for round `r` logged before it. A seat that forced a move and has no signed answer in round `r` must get the forced move as its answer. If a seat forced more than once, the last one counts. A payload that doesn't decrypt is ignored.

## Op log

The contract keeps `opLogHash`, starting at `bytes32(0)` at `start`:

- `forceAction`: `opLogHash = keccak(abi.encode(opLogHash, uint8(1), uint32 seat, uint32 round, keccak(payload)))`
- `checkpoint`: `opLogHash = keccak(abi.encode(opLogHash, uint8(2), uint32(0), uint32 round, bytes32(0)))`

`checkpoint` rounds must go 0, 1, 2 and so on. Both functions stop working after `propose`.

## Transcript

`GET /v1/transcript?game_id=N` serves it after the game ends. That URL is the `transcriptUri` in `propose`. It is also saved as `<log-dir>/staked/<chain>-<escrow>-<game>.transcript.json`.

```json
{
  "version": 1,
  "chain_id": 31337, "escrow": "0x…", "game_id": "1",
  "rules": { "wolves": 2, "sheriff": true, "cupid": true },
  "seats": [{ "address": "0x…", "session_key": "0x…" }],
  "names": ["Sherlock", "osiris"],
  "seed": "0x…", "randomness": "0x…", "enc_key": "0x…",
  "op_log": [
    { "kind": "force", "seat": 3, "round": 2, "payload": "0x…" },
    { "kind": "checkpoint", "round": 0 }
  ],
  "steps": [
    { "type": "chat", "kind": "wolf_chat", "messages": [
      { "seat": 1, "turn_id": 7, "text": "…", "log_hash": "0x…", "signature": "0x…" } ] },
    { "type": "round", "round": 0, "answers": [
      { "seat": 2, "source": "signed", "turn_id": 9, "action": { "type": "target", "target": 4 }, "log_hash": "0x…", "signature": "0x…" },
      { "seat": 4, "source": "forced", "op_index": 0, "action": { "type": "target", "target": 1 } },
      { "seat": 5, "source": "timeout" } ] }
  ],
  "result": { "winner_team": 1, "winners": "0x…", "transcript_root": "0x…" }
}
```

## Replay rules

`werewolf-verify` and the SP1 program run the same function. It fails on the first broken rule.

1. Build the engine from `rules`, the seat count and the derived seed. Names don't affect play.
2. Loop until the game is over. If `game.chat()` is `Some(kind)`, the next step must be a chat step of that kind: call `speak` for each message in order, then `close_chat`. Otherwise the next step must be a round. Its answers must match `game.turns()` one for one, in that order. Then call `submit`.
3. Every signed entry must carry a valid signature by its seat's session key over `M`, with this step's index. Its `log_hash` must be a valid prefix hash (see above). `(seat, turn_id)` must be unique across the transcript.
4. A forced answer must point at a `force` entry in `op_log` for that seat and round. That entry must be the seat's last force for the round, and it must decrypt to the same action. The forced-move rule above must hold for every round. With no decryption key (`werewolf-verify` run by a player), the decrypted actions are trusted.
5. `op_log` must hash to `opLogHash`. Its checkpoints must be exactly the rounds played, in order.
6. The transcript must end when the game ends. The output is `PublicValues` (see the ABI), with the winners from `won`, and the root over every signed entry.

## Contract lifecycle

`Open → Started → Proposed → (Challenged) → Settled`. Any game can also end `Void` or `Cancelled`.

| Call | Who | Effect |
|---|---|---|
| `create` | operator | Pulls a bond of `stake × maxSeats`. Needs `joinBy < playBy`. |
| `join` | player | Pulls `stake` and takes the next seat. One seat per address, before `joinBy`. |
| `start` | operator | Needs `minSeats`. Stores `rosterHash` and requests randomness. |
| `checkpoint` / `forceAction` | operator / seat | Extends the op log. |
| `propose` | operator | Before `playBy`. Reveals the seed. Starts the challenge window. |
| `challenge` | anyone | Bond of `stake`. The operator must `prove` before `proofDeadline`. |
| `disputeReceipt` | anyone | Bond of `stake / 4`. The operator must `answerDispute` with a Merkle proof within the dispute window. |
| `prove` | anyone | Verifies the SP1 proof. A result that differs from the proposal replaces it. |
| `finalize` | anyone | Moves the game to its end state once its deadline passes. |
| `claim` | anyone owed | Pays out. Nothing is ever pushed. |

What each end state pays:

- **Settled:** each winner gets `pot / winners`, and the lowest winning seat also gets the dust. The operator gets its bond back, plus the bonds of any challenge or dispute it beat. If a proof overturned the proposal, the operator's bond goes to the challenger, or is split across the seats when nobody challenged.
- **Void** (no proposal by `playBy`, no proof in time, or an unanswered dispute): every seat gets its stake back plus an equal share of the operator's bond. Challengers and disputers get their bonds back.
- **Cancelled** (still open after `joinBy`): seats and the operator get their deposits back.

A game only settles after its dispute window closes, even once proven: at `challengeEnds` if unchallenged, or at `proofDeadline` if challenged. Without that, the operator could prove right after `propose` and settle before anyone disputes a dropped action. `prove` and `answerDispute` revert after their deadlines, and `challenge` reverts once proven. Dispute ids count from 0 per game.

`setPaused` blocks only `create` and `join`. The deployer sets the windows: challenge, proof and dispute.

## Running a staked server

- Flags: `--stake <base units> --chain-id --rpc-url --escrow --token --operator-key-file`. Optional: `--public-rpc-url`, the join and play windows, `--prover-bin` and `--prover-mode`.
- `/v1/info.stake.rpc_url` comes only from `--public-rpc-url`, because `--rpc-url` may carry a provider API key.
- The server creates the next on-chain game on reset. A lobby left past `joinBy` is cancelled and replaced.
- Rules (wolves, sheriff, Cupid) lock once the game exists on-chain. Staked speech is rejected (422), never rewritten, because the signed text is replayed verbatim.
- After each game a background task proves challenges with `werewolf-prove`, answers receipt disputes, finalizes, and claims the operator's bond. It does not resume after a restart. The seed and encryption secret stay in `<log-dir>/staked/` (mode 0600), so a challenge can still be proven by hand.
- `/readyz` fails when the RPC is unreachable or the game couldn't be created. `/healthz` is liveness only.
- `scripts/staked-game.sh` plays a full staked game on a local anvil chain, including a challenge, a mock proof and the payouts.

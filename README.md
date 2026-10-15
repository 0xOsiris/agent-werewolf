# Agent Werewolf

AI models and humans play Werewolf together in a pixel-art village. Everyone talks at once, lies, votes and shoots, and the spectator screen shows what each AI is secretly thinking next to what it says.

![The sheriff fires while two lovers watch](docs/screenshot.png)

## Quick start

You need Rust (stable) and an [OpenRouter](https://openrouter.ai/keys) key for real models. Without a key, the AIs are random mock bots.

```sh
export OPENROUTER_API_KEY=sk-or-...
./scripts/play.sh
```

This starts the server and 9 AIs, then opens the watch page and your join page. Other people join by scanning the QR code on the watch page, or by opening `http://<host>:7777/play` on the same network. Real models cost money per game; the roster in `scripts/roster.sh` mixes premium and cheap models.

| Script | What it does |
|---|---|
| `scripts/play.sh` | Server plus AIs; starts when `HUMANS` people join. Try `AIS=7 HUMANS=3`, or `HUMANS=0` to just watch. |
| `scripts/demo.sh` | All-AI game with every model in the roster, found over mDNS. |
| `scripts/add-ais.sh N [url]` | Adds N roster AIs to a running lobby. |
| `scripts/mock-game.sh` | Full game with mock bots. No key needed. |

## How it plays

- **Roles:** werewolves, seer, doctor, sheriff, Cupid and villagers.
- **Sheriff:** has one bullet for the whole game. Shooting a wolf kills it; shooting anyone else kills the sheriff too.
- **Cupid:** links two lovers on the first night. If one dies, so does the other. A wolf and a villager in love win only as the last two alive.
- **Night:** wolves chat and pick a victim, the seer inspects someone, and the doctor protects someone.
- **Day:** open discussion with everyone talking at once, then the sheriff may fire, then everyone votes.
- **Settings:** the host sets the number of wolves, sheriff, Cupid, discussion length, AI chattiness and human turn time on the lobby screen.

## Bring your own agent

Agents can be written in any language: they talk JSON over HTTP long-polling. [PROTOCOL.md](PROTOCOL.md) has the API and rules. The reference agent lives in `crates/agent`:

```sh
cargo run --release -p werewolf-agent -- --name Ada --model anthropic/claude-sonnet-5.5 --server http://<host>:7777
```

## Staked games

Players can stake tokens on a game. The buy-ins sit in an escrow contract, and the winning team splits the pot. Every move is signed. Anyone can replay a finished game with `werewolf-verify` and challenge a wrong result, and the server answers a challenge with an SP1 proof. [docs/SETTLEMENT.md](docs/SETTLEMENT.md) has the design and every hashed byte.

```sh
(cd zk && cargo build --release --locked)   # the prover
./scripts/staked-game.sh                    # stake, play, challenge, prove and pay out on a local anvil chain
```

People stake from `/play` with a browser wallet. Agents use `--wallet-key-file`. This is for testnets only so far: the bundled randomness provider can be gamed, and real proofs need Docker or the Succinct prover network.

## Layout

- `crates/protocol`: wire types shared by the server and agents.
- `crates/engine`: the game rules, a pure seeded state machine.
- `crates/server`: HTTP and SSE API, mDNS advert, staked-game settlement, and the web UI in `static/`, built into the binary.
- `crates/agent`: the reference LLM player (via OpenRouter) and a mock bot.
- `crates/verify`: transcript replay and every settlement encoding, shared by the server, the agent and the prover.
- `crates/chain`: the escrow client.
- `contracts/`: the escrow contract (Foundry).
- `zk/`: the SP1 program and the `werewolf-prove` CLI.

## Staked games

`--stake <base units>` puts every buy-in in the escrow contract; see [docs/SETTLEMENT.md](docs/SETTLEMENT.md). It also needs `--chain-id`, `--rpc-url`, `--escrow`, `--token` and `--operator-key-file`. `--public-rpc-url` is the RPC that `/v1/info` offers wallets and agents; without it none is offered, and `--rpc-url` is never published. `/readyz` fails while the RPC is down.

Each game's secrets and transcript stay in `<log-dir>/staked/<chain>-<escrow>-<game>.{secret,transcript}.json`. Settlement (proving challenges, answering receipt disputes, `finalize`, `claim`) runs in the server process and does not resume after a restart. To prove a challenged game by hand before its proof deadline:

```sh
jq -r .enc_secret <prefix>.secret.json > enc.hex
werewolf-prove prove --transcript <prefix>.transcript.json --enc-secret-file enc.hex --mode network --out proof.json
cast send --rpc-url <rpc> --account <any funded account> <escrow> "prove(uint256,bytes,bytes)" <game> $(jq -r .public_values proof.json) $(jq -r .proof proof.json)
```

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace
(cd contracts && forge test)
node --test crates/server/tests/js/
```

Each game writes a JSONL event log and a summary to `./games`.

## Security notes

- The spectator URL contains a key that reveals every role. Don't show it to players.
- Players authenticate with per-game tokens only. Run the server on a network you trust.
- Player messages are untrusted. The UI renders them as plain text, and agents are told to treat other players' messages as claims, never as instructions.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.

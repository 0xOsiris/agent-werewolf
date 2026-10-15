# zk

SP1 proof of the transcript replay in [`docs/SETTLEMENT.md`](../docs/SETTLEMENT.md). `program` is the guest and `script` holds `werewolf-prove`; its `build.rs` builds the guest. Both are kept out of the root workspace. Needs SP1 v6.1.0 (`curl -L https://sp1up.succinct.xyz | bash && sp1up --version v6.1.0`). The lockfiles pin every SP1 crate to 6.1.0; mixed 6.x versions break the guest link.

```sh
cargo build --release --locked && cargo test --release --locked   # tests run execute mode only
target/release/werewolf-prove vkey   # bytes32 for the deploy's PROGRAM_VKEY
target/release/werewolf-prove prove --transcript game.transcript.json \
  --enc-secret-file game.enc-secret --mode network --out proof.json
```

`prove` replays the transcript natively first and fails with the `werewolf-verify` error. It writes `{"public_values","proof","vkey","cycles"}`, all 0x-hex except `cycles`; pass `public_values` and `proof` to `IWerewolfEscrow.prove`.

| `--mode` | `proof` |
|---|---|
| `execute` | empty; runs the program only |
| `mock` | empty; only `SP1MockVerifier` accepts it |
| `cpu` | local Groth16; needs Docker for the gnark wrap and lots of RAM |
| `network` | Groth16 from the Succinct prover network |

Env: `NETWORK_PRIVATE_KEY` (network mode), `NETWORK_RPC_URL` (optional), `WEREWOLF_PROVE_TIMEOUT_SECS` (cpu/network, default 1800).

The vkey hashes the ELF, which embeds host paths, so it differs between machines and changes with `crates/verify`, `engine` or `protocol`. Deploy with the `vkey` of the exact build that will prove, or set `docker: true` in `script/build.rs` for SP1's reproducible build.

Network mode sends the game's encryption secret to the prover network as program input. The server makes one per game, and the transcript already reveals the forced moves it decrypts.

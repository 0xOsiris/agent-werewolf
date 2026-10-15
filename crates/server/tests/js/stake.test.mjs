// Checks static/js/stake.js against crates/verify/tests/vectors.json and `cast` outputs.
// Run: node --test crates/server/tests/js/

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

import {
  actMessage, actionJson, bytesToHex, calldata, cleanText, decodeAddress, decodeGameInfo, decodeSeats,
  decodeString, decodeUint, domain, ecrecoverInput, forcePayload, formatUnits, hexToBytes, joinMessage,
  logHash, parseStake, publicKeyBytes, receiptMatches, sha256, sign,
} from "../../static/js/stake.js";
import { Wallet, walletMessage, loadItem, saveItem } from "../../static/js/wallet.js";

const V = JSON.parse(readFileSync(new URL("../../../verify/tests/vectors.json", import.meta.url), "utf8"));
const subtle = globalThis.crypto.subtle;

const terms = (over = {}) => ({
  chain_id: V.domain.chain_id,
  escrow: V.domain.escrow,
  game_id: V.domain.game_id,
  token: "0x00000000000000000000000000000000000000c2",
  stake: "1000000000000000000",
  operator: "0x00000000000000000000000000000000000000b1",
  enc_key: V.force.enc_key,
  rpc_url: "http://127.0.0.1:8545",
  min_seats: 5,
  max_seats: 8,
  join_by: 1760000000,
  ...over,
});
const S = parseStake(terms());

// PKCS#8 wrappers for raw 32-byte secrets (RFC 8410).
const pkcs8 = (oid, secret) => hexToBytes("0x302e020100300506032b65" + oid + "04220420" + secret.slice(2));
const ed25519 = async (secret, pub) => ({
  privateKey: await subtle.importKey("pkcs8", pkcs8("70", secret), { name: "Ed25519" }, false, ["sign"]),
  publicKey: await subtle.importKey("raw", hexToBytes(pub), { name: "Ed25519" }, true, ["verify"]),
});

test("domain bytes", () => {
  assert.equal(bytesToHex(domain(S)), V.domain.bytes);
});

for (const v of V.acts) {
  test("act message and signature: " + v.name, async () => {
    assert.equal(actionJson(v.action), v.action_json);
    // Field order must not depend on the caller's object.
    assert.equal(actionJson(Object.fromEntries(Object.entries(v.action).reverse())), v.action_json);
    const m = actMessage(S, { turnId: v.turn_id, step: v.step, seat: v.seat, logHash: hexToBytes(v.log_hash), action: v.action });
    assert.equal(bytesToHex(m), v.message);
    assert.equal(bytesToHex(await sha256(m)), v.act_hash);
    const key = await ed25519(v.secret_key, v.session_key);
    assert.equal(bytesToHex(await publicKeyBytes(key)), v.session_key);
    const sig = await sign(key.privateKey, m);
    assert.equal(bytesToHex(sig), v.signature);
    assert.ok(await subtle.verify({ name: "Ed25519" }, key.publicKey, sig, m));
  });
}

test("join message and signature", async () => {
  const v = V.join;
  const m = joinMessage(S, v.player);
  assert.equal(bytesToHex(m), v.message);
  const key = await ed25519(v.secret_key, v.session_key);
  assert.equal(bytesToHex(await sign(key.privateKey, m)), v.signature);
});

test("log hash chain, including non-ASCII text and \\n", async () => {
  const { events, event_json, hashes } = V.log;
  events.forEach((ev, k) => assert.equal(JSON.stringify(ev), event_json[k]));
  for (let k = 0; k <= events.length; k++) {
    assert.equal(bytesToHex(await logHash(S, events.slice(0, k))), hashes[k], "h" + k);
  }
  // A parsed copy (what fetch().json() gives) hashes the same.
  assert.equal(bytesToHex(await logHash(S, JSON.parse(JSON.stringify(events)))), hashes[3]);
  // The speak vector signs h3 of this log.
  assert.equal(hashes[3], V.acts[0].log_hash);
});

test("force payload with a fixed ephemeral key", async () => {
  const f = V.force;
  const ephemeral = {
    privateKey: await subtle.importKey("pkcs8", pkcs8("6e", f.ephemeral_secret), { name: "X25519" }, false, ["deriveBits"]),
    publicKey: await subtle.importKey("raw", hexToBytes(f.ephemeral_public), { name: "X25519" }, true, []),
  };
  const payload = await forcePayload(S, { round: f.round, seat: f.seat, action: f.action, ephemeral });
  assert.equal(bytesToHex(payload), f.payload);
});

test("force payload round-trips to the operator key", async () => {
  const f = V.force;
  const action = { type: "speak", text: "Grüße\n🐺" };
  const payload = await forcePayload(S, { round: 7, seat: 2, action });
  const ePub = payload.subarray(0, 32);
  const op = await subtle.importKey("pkcs8", pkcs8("6e", f.enc_secret), { name: "X25519" }, false, ["deriveBits"]);
  const peer = await subtle.importKey("raw", ePub, { name: "X25519" }, false, []);
  const shared = new Uint8Array(await subtle.deriveBits({ name: "X25519", public: peer }, op, 256));
  const k = await sha256(new Uint8Array([...new TextEncoder().encode("werewolf/force/v1"), ...shared, ...ePub, ...hexToBytes(f.enc_key)]));
  const aes = await subtle.importKey("raw", k, { name: "AES-GCM" }, false, ["decrypt"]);
  const aad = new Uint8Array([0, 0, 0, 7, 0, 0, 0, 2, ...domain(S)]);
  const plain = await subtle.decrypt({ name: "AES-GCM", iv: new Uint8Array(12), additionalData: aad }, aes, payload.subarray(32));
  assert.equal(new TextDecoder().decode(plain), actionJson(action));
});

test("a zero (non-contributory) operator key is refused", async () => {
  const bad = parseStake(terms({ enc_key: "0x" + "00".repeat(32) }));
  await assert.rejects(forcePayload(bad, { round: 0, seat: 0, action: { type: "target", target: 1 } }));
});

// Expected values are `cast calldata` outputs.
test("calldata matches cast", () => {
  const escrow = V.domain.escrow;
  const player = V.join.player;
  assert.equal(calldata.approve(escrow, 10n ** 18n),
    "0x095ea7b30000000000000000000000005fbdb2315678afecb367f032d93f642f64180aa30000000000000000000000000000000000000000000000000de0b6b3a7640000");
  assert.equal(calldata.allowance(player, escrow),
    "0xdd62ed3e00000000000000000000000000000000000000000000000000000000000000a90000000000000000000000005fbdb2315678afecb367f032d93f642f64180aa3");
  assert.equal(calldata.balanceOf(player), "0x70a0823100000000000000000000000000000000000000000000000000000000000000a9");
  assert.equal(calldata.decimals(), "0x313ce567");
  assert.equal(calldata.symbol(), "0x95d89b41");
  assert.equal(calldata.join(1n, V.join.session_key),
    "0x9a478b490000000000000000000000000000000000000000000000000000000000000001fd1724385aa0c75b64fb78cd602fa1d991fdebf76b13c58ed702eac835e9f618");
  assert.equal(calldata.forceAction(1n, 2, hexToBytes(V.force.payload)),
    "0xd2b411ca000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000000004cff2ee45601ec1b67310c7790404585ae697331eee1c1f8cf2419731c1fff3e6b50f9cbb91c256d091e17b4c587f67e0e42df0272f624a631e4113bf58c0fe4d73709fbfd5283b1ffa25ca7520000000000000000000000000000000000000000");
  assert.equal(calldata.claim(1n), "0x379607f50000000000000000000000000000000000000000000000000000000000000001");
  assert.equal(calldata.claimable(1n, player),
    "0xa0c7f71c000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000a9");
  assert.equal(calldata.gameInfo(1n), "0xa6f816680000000000000000000000000000000000000000000000000000000000000001");
  assert.equal(calldata.seats(1n), "0x3b33c7020000000000000000000000000000000000000000000000000000000000000001");
  assert.equal(calldata.receiptDigest(1n, 7, 1, V.acts[0].act_hash),
    "0x8443c45f00000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000007000000000000000000000000000000000000000000000000000000000000000199fa91dc1253dbdff03d32c46438b26d921dda880be3482c8e4cc1a7a095dfb4");
  assert.throws(() => calldata.forceAction(1n, 2 ** 32, new Uint8Array(0)), RangeError);
});

// `cast abi-encode` of a GameView with status Started, 6 seats taken, nextRound 3.
const GAME_VIEW = "0x00000000000000000000000000000000000000000000000000000000000000b100000000000000000000000000000000000000000000000000000000000000c20000000000000000000000000000000000000000000000000de0b6b3a7640000000000000000000000000000000000000000000000000000000000000000000500000000000000000000000000000000000000000000000000000000000000080000000000000000000000000000000000000000000000000000000068e778000000000000000000000000000000000000000000000000000000000068e78610df92d62175d67607cc8aab61268e34e39c3407c7a4b2b2b172f05931f43d4473b569321de72d0af89c2fb48a484de3fc9343f31600ae1f3e13d633cb48cbf8167b0d47d93427f8311160781c7c733fd89f88970aef490d8aa0ee19a4cb8a1b1400000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000006fcfb52392ce131208c9ef7e0d02ed30d30d6560947ad415ffc3fdec2632826d52222222222222222222222222222222222222222222222222222222222222222fe8aca07bfd6fb73a3d601a8054f07e629ea0a0b07580558b95c5c9de883cfd6000000000000000000000000000000000000000000000000000000000000000300000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";

test("decodes return data from cast", () => {
  assert.deepEqual(decodeGameInfo(GAME_VIEW), { stake: 10n ** 18n, maxSeats: 8, status: 2, seatCount: 6, nextRound: 3 });
  assert.throws(() => decodeGameInfo(GAME_VIEW.slice(0, -64)));
  const seats = decodeSeats("0x000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000a0000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000a100000000000000000000000000000000000000000000000000000000000000028a88e3dd7409f195fd52db2d3cba5d72ca6709bf1d94121bf3748801b40f6f5c8139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394");
  assert.deepEqual(seats, {
    players: V.roster.seats.slice(0, 2).map((s) => s.address),
    keys: V.roster.seats.slice(0, 2).map((s) => s.session_key),
  });
  assert.throws(() => decodeSeats("0x" + "00".repeat(31) + "40"), /short|offset/);
  assert.equal(decodeString("0x000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000045553444300000000000000000000000000000000000000000000000000000000"), "USDC");
  assert.equal(decodeUint("0x0000000000000000000000000000000000000000000000000000000000000006"), 6n);
  assert.throws(() => decodeUint("0x"));
  assert.equal(decodeAddress("0x"), null);
  assert.equal(decodeAddress("0x000000000000000000000000f39fd6e51aad88f6f4ce6ab8827279cfffb92266"), "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266");
});

test("ecrecover input is digest, v, r, s", () => {
  const r = "11".repeat(32);
  const s = "22".repeat(32);
  const digest = V.acts[0].receipt_digest;
  const want = digest + "00".repeat(31) + "1c" + r + s;
  assert.equal(ecrecoverInput(digest, "0x" + r + s + "1c"), want);
  assert.equal(ecrecoverInput(digest, "0x" + r + s + "01"), want);
});

test("receipts must name the move", () => {
  const v = V.acts[0];
  const good = { turn_id: v.turn_id, seat: v.seat, act_hash: v.act_hash, digest: v.receipt_digest, signature: "0x" + "ab".repeat(65) };
  const want = { turnId: v.turn_id, seat: v.seat, actHash: hexToBytes(v.act_hash) };
  assert.ok(receiptMatches(good, want));
  assert.ok(!receiptMatches({ ...good, seat: 2 }, want));
  assert.ok(!receiptMatches({ ...good, act_hash: V.acts[1].act_hash }, want));
  assert.ok(!receiptMatches({ ...good, signature: "0x12" }, want));
  assert.ok(!receiptMatches(undefined, want));
});

test("cleanText mirrors sanitize::text", () => {
  assert.equal(cleanText(" hi\r\nthere\u0007  "), "hi\nthere");
  assert.equal(cleanText("‮evil‏"), "evil");
  assert.equal(cleanText("ééééé", 3), "ééé");
  assert.equal(cleanText("ab   cd", 4), "ab");
  // U+FEFF is not Unicode White_Space, so Rust keeps it where String.trim() would drop it.
  assert.equal(cleanText("﻿hi"), "﻿hi");
  assert.equal(cleanText("　 hi  "), "hi");
  assert.equal(cleanText("🐺".repeat(601)), "🐺".repeat(600));
  assert.equal(cleanText("  \n "), "");
});

test("parseStake refuses bad terms", () => {
  assert.equal(S.gameId, 1n);
  assert.equal(S.escrow, V.domain.escrow);
  for (const bad of [
    { chain_id: "31337" }, { escrow: "0x1234" }, { enc_key: V.force.enc_key.slice(0, 64) }, { game_id: "-1" },
    { stake: "1e18" }, { min_seats: 9 }, { join_by: 1.5 }, { operator: null },
  ]) assert.throws(() => parseStake(terms(bad)), /bad stake/, JSON.stringify(bad));
  assert.equal(parseStake(terms({ rpc_url: "javascript:alert(1)" })).rpcUrl, null);
});

test("formatUnits", () => {
  assert.equal(formatUnits(1500000n, 6), "1.5");
  assert.equal(formatUnits(5n, 6), "0.000005");
  assert.equal(formatUnits(10n ** 18n, 18), "1");
  assert.equal(formatUnits(42n, null), "42");
});

// ------------------------------------------------------------ wallet glue with a fake EIP-1193 provider

function fakeProvider(handlers) {
  const calls = [];
  return {
    calls,
    async request({ method, params }) {
      calls.push(method);
      const h = handlers[method];
      if (!h) throw Object.assign(new Error("unsupported " + method), { code: 4200 });
      return h(params);
    },
  };
}

test("switchChain adds the chain when the wallet doesn't know it", async () => {
  let chain = "0x1";
  const p = fakeProvider({
    eth_chainId: () => chain,
    wallet_switchEthereumChain: () => {
      throw Object.assign(new Error("Unrecognized chain ID"), { code: 4902 });
    },
    wallet_addEthereumChain: ([c]) => {
      assert.equal(c.chainId, "0x7a69");
      assert.deepEqual(c.rpcUrls, ["http://127.0.0.1:8545/"]);
      chain = c.chainId;
      return null;
    },
  });
  await new Wallet(p, S).switchChain();
  assert.deepEqual(p.calls, ["eth_chainId", "wallet_switchEthereumChain", "wallet_addEthereumChain", "eth_chainId"]);
});

test("an unknown chain without a public RPC asks the player to add it", async () => {
  const p = fakeProvider({
    eth_chainId: () => "0x1",
    wallet_switchEthereumChain: () => {
      throw Object.assign(new Error("Unrecognized chain ID"), { code: 4902 });
    },
  });
  const noRpc = parseStake(terms({ rpc_url: null }));
  assert.equal(noRpc.rpcUrl, null);
  await assert.rejects(new Wallet(p, noRpc).switchChain(), /doesn't know chain 31337/);
  assert.ok(!p.calls.includes("wallet_addEthereumChain"));
});

test("a rejected wallet request reads as cancelled", async () => {
  const p = fakeProvider({
    eth_requestAccounts: () => {
      throw Object.assign(new Error("User rejected the request."), { code: 4001 });
    },
  });
  const err = await new Wallet(p, S).connect().catch((e) => e);
  assert.equal(walletMessage(err), "Cancelled in wallet");
});

test("mined fails clearly on revert and on timeout", async () => {
  const hash = "0x" + "aa".repeat(32);
  const reverted = fakeProvider({ eth_getTransactionReceipt: () => ({ status: "0x0" }) });
  await assert.rejects(new Wallet(reverted, S).mined(hash, { pollMs: 1 }), /reverted/);
  const pending = fakeProvider({ eth_getTransactionReceipt: () => null });
  await assert.rejects(new Wallet(pending, S).mined(hash, { timeoutMs: 20, pollMs: 5 }), /Not confirmed/);
  let n = 0;
  const flaky = fakeProvider({
    eth_getTransactionReceipt: () => {
      if (++n < 3) throw new Error("rate limited");
      return { status: "0x1" };
    },
  });
  assert.equal((await new Wallet(flaky, S).mined(hash, { pollMs: 1 })).status, "0x1");
});

test("storage falls back to memory without IndexedDB", async () => {
  assert.equal(await saveItem("keys", "k", { a: 1 }), false);
  assert.deepEqual(await loadItem("keys", "k"), { a: 1 });
  assert.equal(await loadItem("keys", "missing"), undefined);
});

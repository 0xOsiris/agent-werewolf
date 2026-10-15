// Staked-game logic with no DOM: bytes, ABI calldata, signed messages, the log hash and forced
// moves. docs/SETTLEMENT.md defines every byte; crates/server/tests/js/stake.test.mjs checks them.

const subtle = globalThis.crypto.subtle;
const utf8 = new TextEncoder();
const ADDR = /^0x[0-9a-fA-F]{40}$/;
const B32 = /^0x[0-9a-fA-F]{64}$/;
const SIG65 = /^0x[0-9a-fA-F]{130}$/;
const DEC = /^[0-9]{1,78}$/;

/** 4-byte selectors from `cast sig`; the test checks each against `cast calldata`. */
export const SELECTOR = {
  approve: "095ea7b3", // approve(address,uint256)
  allowance: "dd62ed3e", // allowance(address,address)
  balanceOf: "70a08231", // balanceOf(address)
  decimals: "313ce567", // decimals()
  symbol: "95d89b41", // symbol()
  join: "9a478b49", // join(uint256,bytes32)
  forceAction: "d2b411ca", // forceAction(uint256,uint32,bytes)
  claim: "379607f5", // claim(uint256)
  claimable: "a0c7f71c", // claimable(uint256,address)
  gameInfo: "a6f81668", // gameInfo(uint256)
  seats: "3b33c702", // seats(uint256)
  receiptDigest: "8443c45f", // receiptDigest(uint256,uint64,uint32,bytes32)
};

/** `IWerewolfEscrow.Status`, by value. */
export const STATUS = ["None", "Open", "Started", "Proposed", "Challenged", "Settled", "Void", "Cancelled"];
/** The ecrecover precompile, used to check receipt signatures with a plain eth_call. */
export const ECRECOVER = "0x0000000000000000000000000000000000000001";

// ------------------------------------------------------------ bytes

export function hexToBytes(hex, len) {
  if (typeof hex !== "string" || !/^0x([0-9a-fA-F]{2})*$/.test(hex)) throw new Error("not 0x hex");
  const out = new Uint8Array((hex.length - 2) / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(2 + 2 * i, 4 + 2 * i), 16);
  if (len !== undefined && out.length !== len) throw new Error("expected " + len + " bytes, got " + out.length);
  return out;
}

export const bytesToHex = (bytes) => "0x" + Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");

export function concat(...parts) {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

/** Big-endian `value` in `size` bytes; throws if it doesn't fit in `bits`. */
function uint(value, size, bits = size * 8) {
  let v = BigInt(value);
  if (v < 0n || v >= 1n << BigInt(bits)) throw new RangeError(value + " does not fit in uint" + bits);
  const out = new Uint8Array(size);
  for (let i = size - 1; i >= 0; i--) {
    out[i] = Number(v & 0xffn);
    v >>= 8n;
  }
  return out;
}

export const be32 = (v) => uint(v, 4);
export const be64 = (v) => uint(v, 8);
export const sha256 = async (bytes) => new Uint8Array(await subtle.digest("SHA-256", bytes));

// ------------------------------------------------------------ stake terms

/** Validates `/v1/info.stake` (untrusted) into the fields the page uses. Throws on anything odd. */
export function parseStake(raw) {
  const need = (ok, field) => {
    if (!ok) throw new Error("bad stake." + field);
  };
  need(raw && typeof raw === "object", "terms");
  need(Number.isSafeInteger(raw.chain_id) && raw.chain_id > 0, "chain_id");
  for (const f of ["escrow", "token", "operator"]) need(ADDR.test(raw[f]), f);
  need(B32.test(raw.enc_key), "enc_key");
  need(DEC.test(raw.game_id), "game_id");
  need(DEC.test(raw.stake), "stake");
  need(Number.isInteger(raw.min_seats) && Number.isInteger(raw.max_seats)
    && raw.min_seats > 0 && raw.min_seats <= raw.max_seats && raw.max_seats <= 255, "seats");
  need(Number.isSafeInteger(raw.join_by) && raw.join_by >= 0, "join_by");
  let rpcUrl = null;
  if (typeof raw.rpc_url === "string") {
    try {
      const url = new URL(raw.rpc_url);
      if (url.protocol === "http:" || url.protocol === "https:") rpcUrl = url.href;
    } catch (err) {
      rpcUrl = null;
    }
  }
  return {
    chainId: raw.chain_id,
    escrow: raw.escrow.toLowerCase(),
    gameId: BigInt(raw.game_id),
    token: raw.token.toLowerCase(),
    stake: BigInt(raw.stake),
    operator: raw.operator.toLowerCase(),
    encKey: raw.enc_key.toLowerCase(),
    rpcUrl,
    minSeats: raw.min_seats,
    maxSeats: raw.max_seats,
    joinBy: raw.join_by,
  };
}

/** One game on one escrow on one chain; keys stored session keys and receipts. */
export const gameKey = (s) => s.chainId + ":" + s.escrow + ":" + s.gameId;

// ------------------------------------------------------------ signed messages

/** `D = be64(chainId) ‖ addr(escrow) ‖ u256(gameId)` */
export const domain = (s) => concat(be64(s.chainId), hexToBytes(s.escrow, 20), uint(s.gameId, 32));

/** `"werewolf/join/v1" ‖ D ‖ addr(player)` */
export const joinMessage = (s, player) => concat(utf8.encode("werewolf/join/v1"), domain(s), hexToBytes(player, 20));

/** `json(action)` with serde's field order; also the exact body the page sends. */
export function actionJson(action) {
  if (action && action.type === "speak" && typeof action.text === "string") {
    return JSON.stringify({ type: "speak", text: action.text });
  }
  if (action && action.type === "target" && (action.target === null || Number.isInteger(action.target))) {
    return JSON.stringify({ type: "target", target: action.target });
  }
  throw new Error("unknown action");
}

/** `M = "werewolf/act/v1" ‖ D ‖ be64(turnId) ‖ be32(step) ‖ be32(seat) ‖ logHash ‖ json(action)` */
export function actMessage(s, { turnId, step, seat, logHash, action }) {
  if (logHash.length !== 32) throw new Error("log hash must be 32 bytes");
  return concat(
    utf8.encode("werewolf/act/v1"), domain(s), be64(turnId), be32(step), be32(seat), logHash,
    utf8.encode(actionJson(action)),
  );
}

/** `hₙ` over `events`: `h₀ = sha256("werewolf/log/v1" ‖ D)`, `hₖ₊₁ = sha256(hₖ ‖ json(event k))`. */
export async function logHash(s, events) {
  if (!Array.isArray(events)) throw new Error("public log is not a list");
  let h = await sha256(concat(utf8.encode("werewolf/log/v1"), domain(s)));
  for (const ev of events) h = await sha256(concat(h, utf8.encode(JSON.stringify(ev))));
  return h;
}

/** Mirrors the server's `sanitize::text`; it refuses (422) signed text this would change. */
export function cleanText(input, max = 600) {
  const kept = [...input]
    .filter((c) => (c === "\n" || !/\p{Cc}/u.test(c)) && !/[‪-‮⁦-⁩‎‏]/u.test(c))
    .join("");
  const trimmed = kept.replace(/^\p{White_Space}+|\p{White_Space}+$/gu, "");
  return [...trimmed].slice(0, max).join("").replace(/\p{White_Space}+$/u, "");
}

// ------------------------------------------------------------ keys

/** A fresh Ed25519 session key; the private half can never leave WebCrypto. */
export const generateSessionKey = () => subtle.generateKey({ name: "Ed25519" }, false, ["sign", "verify"]);
export const publicKeyBytes = async (pair) => new Uint8Array(await subtle.exportKey("raw", pair.publicKey));
export const sign = async (privateKey, message) => new Uint8Array(await subtle.sign({ name: "Ed25519" }, privateKey, message));

/**
 * `payload = ePub ‖ AES-256-GCM(k, 12 zero bytes, json(action), be32(round) ‖ be32(seat) ‖ D)`, with
 * `k = sha256("werewolf/force/v1" ‖ X25519(e, encKey) ‖ ePub ‖ encKey)`. `ephemeral` is for tests only.
 */
export async function forcePayload(s, { round, seat, action, ephemeral }) {
  const e = ephemeral || (await subtle.generateKey({ name: "X25519" }, false, ["deriveBits"]));
  const ePub = new Uint8Array(await subtle.exportKey("raw", e.publicKey));
  const encKey = hexToBytes(s.encKey, 32);
  const peer = await subtle.importKey("raw", encKey, { name: "X25519" }, false, []);
  const shared = new Uint8Array(await subtle.deriveBits({ name: "X25519", public: peer }, e.privateKey, 256));
  if (shared.every((b) => b === 0)) throw new Error("the operator's encryption key is invalid");
  const k = await sha256(concat(utf8.encode("werewolf/force/v1"), shared, ePub, encKey));
  const aes = await subtle.importKey("raw", k, { name: "AES-GCM" }, false, ["encrypt"]);
  const sealed = await subtle.encrypt(
    { name: "AES-GCM", iv: new Uint8Array(12), additionalData: concat(be32(round), be32(seat), domain(s)), tagLength: 128 },
    aes,
    utf8.encode(actionJson(action)),
  );
  return concat(ePub, new Uint8Array(sealed));
}

// ------------------------------------------------------------ receipts

/** Local checks on an untrusted receipt: it must name this move. */
export function receiptMatches(r, { turnId, seat, actHash }) {
  return !!r && typeof r === "object" && r.turn_id === turnId && r.seat === seat
    && B32.test(r.act_hash) && r.act_hash.toLowerCase() === bytesToHex(actHash)
    && B32.test(r.digest) && SIG65.test(r.signature);
}

/** ecrecover input `digest ‖ u256(v) ‖ r ‖ s` for a 65-byte `r ‖ s ‖ v` signature. */
export function ecrecoverInput(digest, signature) {
  const sig = hexToBytes(signature, 65);
  const v = sig[64] < 27 ? sig[64] + 27 : sig[64];
  return bytesToHex(concat(hexToBytes(digest, 32), uint(v, 32), sig.subarray(0, 64)));
}

// ------------------------------------------------------------ ABI

const word = (v, bits = 256) => uint(v, 32, bits);
const addrWord = (a) => concat(new Uint8Array(12), hexToBytes(a, 20));
const encode = (selector, ...words) => bytesToHex(concat(hexToBytes("0x" + selector), ...words));

export const calldata = {
  approve: (spender, amount) => encode(SELECTOR.approve, addrWord(spender), word(amount)),
  allowance: (owner, spender) => encode(SELECTOR.allowance, addrWord(owner), addrWord(spender)),
  balanceOf: (owner) => encode(SELECTOR.balanceOf, addrWord(owner)),
  decimals: () => encode(SELECTOR.decimals),
  symbol: () => encode(SELECTOR.symbol),
  join: (gameId, sessionKey) => encode(SELECTOR.join, word(gameId), hexToBytes(sessionKey, 32)),
  forceAction: (gameId, round, payload) => encode(
    SELECTOR.forceAction, word(gameId), word(round, 32), word(96), word(payload.length), payload,
    new Uint8Array((32 - (payload.length % 32)) % 32),
  ),
  claim: (gameId) => encode(SELECTOR.claim, word(gameId)),
  claimable: (gameId, account) => encode(SELECTOR.claimable, word(gameId), addrWord(account)),
  gameInfo: (gameId) => encode(SELECTOR.gameInfo, word(gameId)),
  seats: (gameId) => encode(SELECTOR.seats, word(gameId)),
  receiptDigest: (gameId, turnId, seat, actHash) => encode(
    SELECTOR.receiptDigest, word(gameId), word(turnId, 64), word(seat, 32), hexToBytes(actHash, 32),
  ),
};

/** Reader over untrusted return data; every access is bounds-checked. */
function words(hex) {
  const b = hexToBytes(hex);
  const at = (offset) => {
    if (!Number.isSafeInteger(offset) || offset < 0 || offset + 32 > b.length) throw new Error("short return data");
    return b.subarray(offset, offset + 32);
  };
  const num = (offset) => BigInt(bytesToHex(at(offset)));
  const small = (offset) => {
    const n = num(offset);
    if (n > BigInt(b.length)) throw new Error("bad offset in return data");
    return Number(n);
  };
  return { b, at, num, small };
}

export const decodeUint = (hex) => words(hex).num(0);

/** Last 20 bytes of word 0, or null for an empty or zero result (ecrecover's failure). */
export function decodeAddress(hex) {
  if (hex === "0x") return null;
  const a = bytesToHex(words(hex).at(0).subarray(12));
  return /^0x0{40}$/.test(a) ? null : a;
}

/** `GameView` is all static fields, so word `i` is field `i`. */
export function decodeGameInfo(hex) {
  const w = words(hex);
  w.at(23 * 32);
  return {
    stake: w.num(2 * 32),
    maxSeats: Number(w.num(4 * 32)),
    status: Number(w.num(10 * 32)),
    seatCount: Number(w.num(11 * 32)),
    nextRound: Number(w.num(15 * 32)),
  };
}

/** `seats()` returns `(address[] players, bytes32[] sessionKeys)`. */
export function decodeSeats(hex) {
  const w = words(hex);
  const list = (head, item) => {
    const offset = w.small(head);
    const n = w.small(offset);
    return Array.from({ length: n }, (_, i) => item(w.at(offset + 32 + 32 * i)));
  };
  return {
    players: list(0, (x) => bytesToHex(x.subarray(12))),
    keys: list(32, (x) => bytesToHex(x)),
  };
}

/** An ABI `string` return, for ERC-20 `symbol()`. */
export function decodeString(hex) {
  const w = words(hex);
  const offset = w.small(0);
  const n = w.small(offset);
  if (offset + 32 + n > w.b.length) throw new Error("short return data");
  return new TextDecoder().decode(w.b.subarray(offset + 32, offset + 32 + n));
}

// ------------------------------------------------------------ display

export function formatUnits(amount, decimals) {
  if (!Number.isInteger(decimals) || decimals <= 0) return amount.toString();
  const digits = amount.toString().padStart(decimals + 1, "0");
  const whole = digits.slice(0, -decimals);
  const frac = digits.slice(-decimals).replace(/0+$/, "");
  return frac ? whole + "." + frac : whole;
}

/** "20 USDC", or "20000000 base units" before the token's decimals are known. */
export function amountText(amount, token) {
  if (!token || !Number.isInteger(token.decimals)) return amount.toString() + " base units";
  return formatUnits(amount, token.decimals) + " " + (token.symbol || "tokens");
}

export const shortAddr = (a) => a.slice(0, 6) + "…" + a.slice(-4);
export const localTime = (unix) => new Date(unix * 1000).toLocaleString([], { dateStyle: "medium", timeStyle: "short" });

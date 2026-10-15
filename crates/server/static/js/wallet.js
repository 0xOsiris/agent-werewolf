// Browser glue for staked games: EIP-1193 wallets, chain reads and IndexedDB. No DOM.

import { ECRECOVER, calldata, decodeAddress, decodeGameInfo, decodeString, decodeUint, ecrecoverInput } from "./stake.js";

const READ_MS = 15000;
// Wallet prompts wait for a person; bound them anyway so a lost popup can't hang the flow.
const PROMPT_MS = 5 * 60 * 1000;
const MINE_MS = 3 * 60 * 1000;
const ADDR = /^0x[0-9a-fA-F]{40}$/;
export const NO_WALLET = "Open this page in your wallet app's browser (MetaMask, Coinbase Wallet, Rabby).";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function withTimeout(promise, ms, what) {
  let timer;
  const late = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(what + " got no answer within " + Math.round(ms / 1000) + " s")), ms);
  });
  return Promise.race([promise, late]).finally(() => clearTimeout(timer));
}

/** EIP-1193 4001, plus the shapes some wallets use instead. */
export function isCancelled(err) {
  return !!err && (err.code === 4001 || err.code === "ACTION_REJECTED"
    || /user (rejected|denied|cancel)/i.test(String(err.message || "")));
}

/** What to show for a failed wallet or chain call. */
export function walletMessage(err) {
  if (isCancelled(err)) return "Cancelled in wallet";
  const msg = String((err && (err.message || (err.data && err.data.message))) || err);
  return msg.length > 200 ? msg.slice(0, 200) + "…" : msg;
}

const unknownChain = (err) => !!err && (err.code === 4902
  || (err.data && err.data.originalError && err.data.originalError.code === 4902)
  || /unrecognized chain|chain .*not (been )?added/i.test(String(err.message || "")));

/** Injected wallets: EIP-6963 announcements, else `window.ethereum`. Returns a getter. */
export function discoverWallets(onChange) {
  const found = new Map();
  window.addEventListener("eip6963:announceProvider", (e) => {
    const d = e.detail;
    if (!d || !d.info || !d.provider || typeof d.provider.request !== "function") return;
    found.set(String(d.info.uuid), { name: String(d.info.name || "").slice(0, 32), provider: d.provider });
    onChange();
  });
  window.dispatchEvent(new Event("eip6963:requestProvider"));
  return () => {
    if (found.size) return [...found.values()];
    const eth = window.ethereum;
    return eth && typeof eth.request === "function" ? [{ name: "", provider: eth }] : [];
  };
}

/** A read-only EIP-1193 provider over HTTP JSON-RPC, for display-only reads via `stake.rpc_url`. */
export function httpProvider(url) {
  let id = 0;
  return {
    async request({ method, params }) {
      const res = await fetch(url, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ jsonrpc: "2.0", id: ++id, method, params }),
        signal: AbortSignal.timeout(READ_MS),
      });
      const body = await res.json().catch(() => null);
      if (!res.ok || !body || body.error) {
        throw new Error("RPC " + method + " failed: " + ((body && body.error && body.error.message) || "HTTP " + res.status));
      }
      return body.result;
    },
  };
}

/** One EIP-1193 provider used for the game in `stake`. */
export class Wallet {
  constructor(provider, stake) {
    this.provider = provider;
    this.stake = stake;
    this.address = null;
  }

  request(method, params, ms = READ_MS) {
    return withTimeout(Promise.resolve().then(() => this.provider.request({ method, params })), ms, method);
  }

  /** Asks for the account (no prompt once granted) and moves the wallet to the game's chain. */
  async connect() {
    this.useAccounts(await this.request("eth_requestAccounts", [], PROMPT_MS));
    if (!this.address) throw new Error("The wallet shared no account");
    await this.switchChain();
    return this.address;
  }

  /** Picks up an already granted account without a prompt. */
  async resume() {
    this.useAccounts(await this.request("eth_accounts", []));
    return this.address;
  }

  useAccounts(accounts) {
    const a = Array.isArray(accounts) ? accounts[0] : null;
    this.address = typeof a === "string" && ADDR.test(a) ? a.toLowerCase() : null;
  }

  async onChain() {
    const id = await this.request("eth_chainId", []);
    return typeof id === "string" && /^0x[0-9a-fA-F]+$/.test(id) && BigInt(id) === BigInt(this.stake.chainId);
  }

  async switchChain() {
    if (await this.onChain()) return;
    const chainId = "0x" + this.stake.chainId.toString(16);
    try {
      await this.request("wallet_switchEthereumChain", [{ chainId }], PROMPT_MS);
    } catch (err) {
      if (!unknownChain(err)) throw err;
      if (!this.stake.rpcUrl) {
        throw new Error("Your wallet doesn't know chain " + this.stake.chainId + ". Add it in your wallet, then try again.");
      }
      await this.request("wallet_addEthereumChain", [{
        chainId,
        chainName: "Agent Werewolf (chain " + this.stake.chainId + ")",
        rpcUrls: [this.stake.rpcUrl],
        nativeCurrency: { name: "Ether", symbol: "ETH", decimals: 18 },
      }], PROMPT_MS);
    }
    if (!(await this.onChain())) throw new Error("Switch your wallet to chain " + this.stake.chainId + " and try again.");
  }

  async call(to, data) {
    const out = await this.request("eth_call", [{ to, data }, "latest"]);
    if (typeof out !== "string") throw new Error("eth_call returned no data");
    return out;
  }

  /** Sends a transaction and waits for it to succeed. `onSent(hash)` fires once the wallet signs. */
  async transact(to, data, onSent = () => {}) {
    if (!this.address) throw new Error("Connect your wallet first.");
    await this.switchChain();
    const hash = await this.request("eth_sendTransaction", [{ from: this.address, to, data }], PROMPT_MS);
    if (typeof hash !== "string" || !/^0x[0-9a-fA-F]{64}$/.test(hash)) throw new Error("The wallet returned no transaction hash");
    onSent(hash);
    return this.mined(hash);
  }

  async mined(hash, { timeoutMs = MINE_MS, pollMs = 1500 } = {}) {
    const end = Date.now() + timeoutMs;
    let lastError = null;
    for (;;) {
      try {
        const r = await this.request("eth_getTransactionReceipt", [hash]);
        if (r && r.status !== undefined && r.status !== null) {
          if (BigInt(r.status) !== 1n) throw Object.assign(new Error("Transaction reverted on-chain: " + hash), { final: true });
          return r;
        }
      } catch (err) {
        if (err.final) throw err;
        // An RPC hiccup while polling isn't fatal; keep the reason for the timeout message.
        lastError = err;
      }
      if (Date.now() + pollMs > end) {
        throw new Error("Not confirmed after " + Math.round(timeoutMs / 1000) + " s (" + hash + ")"
          + (lastError ? ": " + walletMessage(lastError) : ". Check your wallet."));
      }
      await sleep(pollMs);
    }
  }

  /** Seats taken, token decimals and symbol. Decimals and symbol are optional in ERC-20, so null/"" when absent. */
  async terms() {
    const s = this.stake;
    const [info, decimals, symbol] = await Promise.all([
      this.call(s.escrow, calldata.gameInfo(s.gameId)).then(decodeGameInfo),
      this.call(s.token, calldata.decimals()).then(decodeUint).then((d) => (d <= 36n ? Number(d) : null), () => null),
      this.call(s.token, calldata.symbol()).then(decodeString).then((t) => t.replace(/[^\p{L}\p{N}._-]/gu, "").slice(0, 12), () => ""),
    ]);
    return { seatCount: info.seatCount, decimals, symbol };
  }

  /** True when the escrow computes the same digest and the operator signed it. */
  async receiptSigned(r) {
    const s = this.stake;
    const digest = await this.call(s.escrow, calldata.receiptDigest(s.gameId, r.turn_id, r.seat, r.act_hash));
    if (digest.toLowerCase() !== r.digest.toLowerCase()) return false;
    return decodeAddress(await this.call(ECRECOVER, ecrecoverInput(r.digest, r.signature))) === s.operator;
  }
}

// ------------------------------------------------------------ storage

// IndexedDB keeps CryptoKeys (non-extractable ones too) across reloads; memory is the fallback.
const memory = new Map();
let db = null;

function openDb() {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open("werewolf-stake", 1);
    req.onupgradeneeded = () => {
      req.result.createObjectStore("keys");
      req.result.createObjectStore("receipts");
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
    req.onblocked = () => reject(new Error("IndexedDB blocked"));
  });
}

async function inStore(store, mode, op) {
  db = db || openDb();
  const handle = await db;
  return new Promise((resolve, reject) => {
    const tx = handle.transaction(store, mode);
    const req = op(tx.objectStore(store));
    tx.oncomplete = () => resolve(req.result);
    tx.onerror = tx.onabort = () => reject(tx.error);
  });
}

export async function loadItem(store, key) {
  try {
    const value = await inStore(store, "readonly", (s) => s.get(key));
    if (value !== undefined) return value;
  } catch (err) {
    console.warn("IndexedDB read failed; using memory", err && err.name);
  }
  return memory.get(store + "|" + key);
}

/** Saves to memory and IndexedDB; false when only memory has it (lost on reload). */
export async function saveItem(store, key, value) {
  memory.set(store + "|" + key, value);
  try {
    await inStore(store, "readwrite", (s) => s.put(value, key));
    return true;
  } catch (err) {
    console.warn("IndexedDB write failed; kept in memory only", err && err.name);
    return false;
  }
}

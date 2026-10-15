// Human player page: join, watch the village from your seat, and answer turns via long-poll.

import { Game, connectStream, ROLE_LABEL, ROLE_GOAL, rulesLine } from "./events.js";
import { Feed, el } from "./feed.js";
import { Scene, clock } from "./scene.js";
import { iconCanvas } from "./sprites.js";
import {
  STATUS, actMessage, amountText, bytesToHex, calldata, cleanText, decodeGameInfo, decodeSeats, decodeUint,
  forcePayload, gameKey, generateSessionKey, joinMessage, localTime, logHash, parseStake, publicKeyBytes,
  receiptMatches, sha256, shortAddr, sign,
} from "./stake.js";
import { NO_WALLET, Wallet, discoverWallets, httpProvider, loadItem, saveItem, walletMessage } from "./wallet.js";

const $ = (id) => document.getElementById(id);
const SESSION_KEY = "werewolf.session";
const NAME_KEY = "werewolf.name";
const MAX_TEXT = 600;
// Open chat: speak turns repeat during a window, with a cooldown and a per-window cap.
const CHAT_KINDS = new Set(["day_speak", "wolf_chat"]);
const CHAT_MAX = 6;
const CHAT_COOLDOWN_MS = 8000;
const PRIVATE = new Set(["wolf_spoke", "wolf_voted", "inspected", "protected", "linked", "in_love"]);
const TURN_TEXT = {
  day_vote: ["Vote to eliminate", "Tap a player in the village or pick from the list."],
  wolf_kill: ["Pick tonight's victim (wolves only)", "Your pack's votes decide who dies."],
  seer_inspect: ["Choose someone to inspect", "You will learn whether they are a werewolf."],
  doctor_protect: ["Choose someone to protect", "If the wolves pick them tonight, they survive."],
  sheriff_shoot: ["Sheriff: shoot or hold fire", "Hit a wolf and only they die. Hit anyone else and you die too."],
  cupid_link: ["Choose the first lover", "You can pick yourself. If one lover dies, so does the other."],
};

// ------------------------------------------------------------ storage

function store(key, value) {
  try {
    if (value === null) localStorage.removeItem(key);
    else localStorage.setItem(key, JSON.stringify(value));
  } catch (err) {
    console.warn("localStorage unavailable", err);
  }
}

function load(key) {
  try {
    const raw = localStorage.getItem(key);
    return raw ? JSON.parse(raw) : null;
  } catch (err) {
    console.warn("localStorage unavailable", err);
    return null;
  }
}

function validSession(s) {
  return s && typeof s.token === "string" && Number.isInteger(s.player_id) && typeof s.name === "string" ? s : null;
}

// ------------------------------------------------------------ state

let session = validSession(load(SESSION_KEY));
let stream = null;
let pollGen = 0;
let turn = null;
let selected = null;
let busy = false;
let rejoining = false;
let overShown = false;
let noticeTimer = null;
let wakeRejoin = null;
let chatTurn = null;
let chatSent = 0;
let chatLastSent = 0;
let chatBuzzed = false;
let chatBusy = false;

const game = new Game();
const you = () => (session ? session.player_id : null);
const scene = new Scene($("scene"), game, { you, onPick: (id) => pick(id) });
new Feed($("feed"), game, { filter: (ev) => !PRIVATE.has(ev.type), you, emptyText: "The village is quiet…" });
new Feed($("private-feed"), game, { filter: (ev) => PRIVATE.has(ev.type), you, emptyText: "Nothing secret yet." });
new Feed($("chat-log"), game, {
  filter: (ev) => ev.type === "spoke" || ev.type === "wolf_spoke" || ev.type === "phase_started",
  you,
  emptyText: "No messages yet.",
});
$("brand").prepend(iconCanvas("werewolf", 3));

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const backoff = (n) => Math.min(15000, 1000 * 2 ** Math.min(n, 4)) * (0.75 + Math.random() * 0.5);

function setConn(text, bad) {
  $("conn").textContent = text;
  $("conn").classList.toggle("bad", !!bad);
}

function notice(text) {
  $("notice").textContent = text;
  $("notice-card").hidden = false;
  clearTimeout(noticeTimer);
  noticeTimer = setTimeout(() => ($("notice-card").hidden = true), 3500);
}

function watch(token) {
  if (stream) stream.close();
  const url = token ? "/v1/watch?token=" + encodeURIComponent(token) : "/v1/watch";
  stream = connectStream(url, game, {
    onStatus: (t, bad) => setConn(bad ? t : token ? "connected" : "watching", bad),
    // A refused token stream usually means the server restarted; confirm via poll.
    onClosed: () => {
      if (token) checkToken(token);
    },
  });
}

async function checkToken(token) {
  try {
    const res = await fetch("/v1/poll?wait=0", {
      headers: { authorization: "Bearer " + token },
      signal: AbortSignal.timeout(10000),
    });
    if (res.status === 401 && session && session.token === token) forget("The game was reset. Join again!");
  } catch (err) {
    console.warn("token check failed", err);
  }
}

// ------------------------------------------------------------ rendering

function render() {
  const s = game.state;
  const me = session ? game.player(session.player_id) : null;
  const role = session ? s.roles[session.player_id] : null;
  $("join-card").hidden = !!session || rejoining;

  const card = $("role-card");
  card.hidden = !role;
  if (role) {
    card.className = "card role-card role-" + role;
    if (card.dataset.role !== role) {
      card.dataset.role = role;
      $("role-icon").replaceChildren(iconCanvas(role, 5));
    }
    $("role-name").textContent = role === "cupid" ? "You are Cupid" : "You are the " + (ROLE_LABEL[role] || role);
    $("role-goal").textContent = ROLE_GOAL[role] || "";
    const pack = role === "werewolf"
      ? Object.keys(s.roles).map(Number).filter((id) => id !== session.player_id && s.roles[id] === "werewolf")
      : [];
    $("role-pack").textContent = pack.length
      ? "Your pack: " + pack.map((id) => game.nameOf(id) + (game.player(id)?.alive === false ? " (dead)" : "")).join(", ")
      : role === "werewolf" ? "You hunt alone." : "";
  }
  const partner = lovePartner();
  $("role-love").textContent = partner === null ? ""
    : "You're in love with " + game.nameOf(partner) + (s.roles[partner] ? " (" + (ROLE_LABEL[s.roles[partner]] || s.roles[partner]) + ")" : "")
      + ". If one of you dies, so does the other.";
  $("private-card").hidden = !role || ((role === "villager" || role === "sheriff") && partner === null);
  const rules = rulesLine(s.settings);
  $("role-rules").textContent = rules ? "Rules: " + rules : "";
  $("status-rules").textContent = rules && s.phase.kind === "lobby" ? "Rules: " + rules : "";

  let status = "";
  if (rejoining) status = "Waiting for the next lobby to open…";
  else if (session && s.phase.kind === "lobby") {
    status = "You're in, " + session.name + "! Waiting for the host to start… (" + s.players.length + " joined)";
  } else if (me && !me.alive && s.phase.kind !== "over") status = "You were eliminated. You can keep watching the game.";
  else if (session && s.phase.kind === "over") status = "Game over.";
  $("status-text").textContent = status;
  $("status-card").hidden = !status;
  $("status-again").hidden = !(session && s.phase.kind === "over") || !!stake;
  $("status-card").classList.toggle("banner-dead", !!(me && !me.alive));

  if (!session && !rejoining) {
    const inGame = s.phase.kind !== "lobby";
    const hint = $("join-hint");
    if ($("join-error").textContent && inGame) hint.textContent = "Game in progress, you can watch until the next lobby.";
    else if (!inGame) hint.textContent = s.players.length ? s.players.length + " in the lobby. Join them!" : "";
    else hint.textContent = "A game is running. You can watch and join when the next lobby opens.";
  }
  if (s.phase.kind === "over" && session && !overShown) showGameOver(s.winner);
  if (overShown) renderRoles();
  renderChat();
  renderStake();
}

game.on((ev) => {
  if (ev.type === "chat_window") {
    if (ev.open) {
      chatSent = 0;
      chatLastSent = 0;
      chatBuzzed = false;
    } else {
      dropChatTurn();
    }
  }
  if ((ev.type === "reset" || (ev.type === "snapshot" && game.state.phase.kind === "lobby")) && wakeRejoin) wakeRejoin();
  // A reset invalidates every token; offer the join form with the name kept.
  if (ev.type === "reset" && stake) refreshInfo();
  if (ev.type === "reset" && session && !rejoining) {
    $("name").value = session.name;
    forget("A new lobby is open. Join again!");
    return;
  }
  if (ev.type === "snapshot" && game.state.phase.kind === "lobby" && !session && $("join-error").textContent) {
    $("join-error").textContent = "";
  }
  render();
});

// ------------------------------------------------------------ joining

async function postJoin(name, stakeJoin) {
  const res = await fetch("/v1/join", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ name, model: null, human: true, ...(stakeJoin && { stake: stakeJoin }) }),
    signal: AbortSignal.timeout(10000),
  });
  const body = await res.json().catch(() => ({}));
  return { status: res.status, body };
}

/** My partner's id if I'm one of the lovers, else null. */
function lovePartner() {
  if (!session) return null;
  const me = session.player_id;
  const pair = game.lovers(me);
  if (!pair || !pair.includes(me)) return null;
  return pair[0] === me ? pair[1] : pair[0];
}

function startSession(s) {
  session = s;
  store(SESSION_KEY, s);
  store(NAME_KEY, s.name);
  overShown = false;
  $("gameover").hidden = true;
  $("join-error").textContent = "";
  watch(s.token);
  pollGen++;
  pollLoop(pollGen);
  render();
}

function forget(message) {
  const had = !!session;
  session = null;
  sessionKey = null;
  receipts = [];
  dropForce();
  store(SESSION_KEY, null);
  pollGen++;
  hideTurn();
  $("join-error").textContent = message || "";
  if (had) watch(null);
  render();
}

$("join-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const name = $("name").value.trim();
  if (!name) return;
  $("join-btn").disabled = true;
  $("join-error").textContent = "";
  try {
    await refreshInfo();
    if (stakeProblem) $("join-error").textContent = stakeProblem;
    else if (stake) await joinStaked(name);
    else {
      const { status, body } = await postJoin(name);
      if (status === 200 && Number.isInteger(body.player_id) && typeof body.token === "string") {
        startSession({ token: body.token, player_id: body.player_id, name });
      } else {
        $("join-error").textContent = (body && body.error) || "Join failed (HTTP " + status + ")";
      }
    }
  } catch (err) {
    $("join-error").textContent = "Cannot reach the server: " + err.message;
  } finally {
    $("join-btn").disabled = false;
    render();
  }
});

/** "Play again": re-join under the same name once the lobby reopens. */
async function rejoin() {
  const name = (session && session.name) || load(NAME_KEY) || "";
  $("gameover").hidden = true;
  if (session) forget("");
  if (!name) return;
  rejoining = true;
  render();
  for (let attempt = 0; rejoining; attempt++) {
    let res;
    try {
      res = await postJoin(name);
    } catch (err) {
      setConn("cannot reach the server, retrying…", true);
      res = null;
    }
    if (res && res.status === 200 && Number.isInteger(res.body.player_id)) {
      rejoining = false;
      startSession({ token: res.body.token, player_id: res.body.player_id, name });
      return;
    }
    if (res && res.status === 409 && game.state.phase.kind === "lobby" && attempt > 0) {
      // The lobby is open but refused us (name taken or full): let the person choose.
      rejoining = false;
      $("name").value = name;
      $("join-error").textContent = res.body.error || "Could not join";
      render();
      return;
    }
    if (res && res.status !== 409) {
      rejoining = false;
      $("join-error").textContent = res.body.error || "Join failed (HTTP " + res.status + ")";
      render();
      return;
    }
    await new Promise((r) => {
      wakeRejoin = r;
      setTimeout(r, backoff(attempt));
    });
    wakeRejoin = null;
  }
}

$("again").addEventListener("click", () => rejoin());
$("status-again").addEventListener("click", () => rejoin());
$("go-close").addEventListener("click", () => ($("gameover").hidden = true));

// ------------------------------------------------------------ turns

async function pollLoop(gen) {
  let failures = 0;
  while (gen === pollGen && session) {
    const token = session.token;
    let res;
    try {
      res = await fetch("/v1/poll?wait=25", {
        headers: { authorization: "Bearer " + token },
        signal: AbortSignal.timeout(35000),
      });
    } catch (err) {
      if (gen !== pollGen) return;
      failures++;
      setConn("connection problem, retrying…", true);
      await sleep(backoff(failures));
      continue;
    }
    if (gen !== pollGen) return;
    if (res.status === 401) {
      forget("The game was reset. Join again!");
      return;
    }
    const body = await res.json().catch(() => null);
    if (!res.ok || !body) {
      failures++;
      setConn("server error " + res.status + ", retrying…", true);
      await sleep(backoff(failures));
      continue;
    }
    failures = 0;
    if (body.status === "turn" && CHAT_KINDS.has(body.kind)) {
      hideTurn();
      showChatTurn(body);
      await new Promise((r) => {
        chatTurn.settle = r;
        setTimeout(r, body.deadline_ms + 1500);
      });
    } else if (body.status === "turn") {
      dropChatTurn();
      if (force && Number.isInteger(body.round) && body.round > force.round) dropForce();
      if (!turn || turn.turn_id !== body.turn_id) showTurn(body);
      else turn.deadline = performance.now() + body.deadline_ms;
      await new Promise((r) => {
        turn.settle = r;
        setTimeout(r, body.deadline_ms + 1500);
      });
    } else if (body.status === "game_over") {
      hideTurn();
      dropChatTurn();
      dropForce();
      showGameOver(body.winner, body.view);
      return;
    } else {
      hideTurn();
      dropChatTurn();
    }
  }
}

function showTurn(t) {
  turn = { ...t, deadline: performance.now() + t.deadline_ms, settle: () => {} };
  selected = null;
  let [title, help] = TURN_TEXT[t.kind] || [t.kind, ""];
  const players = t.view && t.view.players ? t.view.players : game.state.players;
  if (t.kind === "cupid_link") {
    // The second link turn offers every living player but the first pick.
    const first = players.find((p) => p.alive && !t.candidates.includes(p.id));
    if (first) {
      title = "Choose the second lover";
      help = "First lover: " + (first.id === you() ? "you" : first.name) + ". Who do they fall for?";
    }
  }
  $("act-title").textContent = title;
  $("act-help").textContent = help;
  $("act-error").textContent = "";
  const names = new Map(players.map((p) => [p.id, p]));
  const buttons = t.candidates.map((id) => {
    const p = names.get(id);
    const b = el("button");
    b.type = "button";
    b.dataset.id = String(id);
    b.appendChild(document.createTextNode(p ? p.name : "#" + id));
    b.appendChild(el("span", "c-model", !p ? "" : id === you() ? "you" : p.human ? "human" : p.model || "agent"));
    b.addEventListener("click", () => pick(id));
    return b;
  });
  $("act-cands").replaceChildren(...buttons);
  const shoot = t.kind === "sheriff_shoot";
  $("action").classList.toggle("shoot", shoot);
  $("action").classList.toggle("cupid", t.kind === "cupid_link");
  $("act-abstain").textContent = shoot ? "Hold fire" : t.kind === "wolf_kill" ? "Spare everyone"
    : t.kind === "cupid_link" ? "No lovers" : "Abstain";
  scene.setPicking(t.candidates, null);
  updateSelection();
  $("action").hidden = false;
  syncSpacer();
  buzz();
  tick();
}

function buzz() {
  try {
    // Browsers refuse (and log) vibration before the first tap.
    const tapped = !navigator.userActivation || navigator.userActivation.hasBeenActive;
    if (navigator.vibrate && tapped) navigator.vibrate([120, 60, 120]);
  } catch (err) {
    console.warn("vibrate failed", err);
  }
}

function syncSpacer() {
  document.body.classList.toggle("has-action", !$("action").hidden || !$("chat").hidden);
}

// ------------------------------------------------------------ open chat

function showChatTurn(t) {
  if (!chatTurn || chatTurn.turn_id !== t.turn_id) {
    chatTurn = { ...t, deadline: performance.now() + t.deadline_ms, settle: () => {} };
    $("chat-error").textContent = "";
    if (!chatBuzzed) {
      chatBuzzed = true;
      buzz();
    }
  } else {
    chatTurn.deadline = performance.now() + t.deadline_ms;
  }
  renderChat();
}

/** Forgets the pending chat turn (window closed, stale, or answered) and lets the poll loop go on. */
function dropChatTurn() {
  const current = chatTurn;
  chatTurn = null;
  if (current) current.settle();
  renderChat();
}

function renderChat() {
  const s = game.state;
  const me = session ? game.player(session.player_id) : null;
  const open = !!(me && me.alive && (chatTurn || s.chat));
  const box = $("chat");
  if (box.hidden === open) {
    box.hidden = !open;
    syncSpacer();
    if (open) $("chat-log").scrollTop = $("chat-log").scrollHeight;
  }
  if (!open) return;
  const now = performance.now();
  const kind = s.chat ? s.chat.kind : chatTurn.kind;
  const endsAt = s.chat ? s.chat.endsAt : chatTurn.deadline;
  const wolf = kind === "wolf_chat";
  $("chat-title").textContent = wolf ? "Wolf chat" : "Open discussion";
  $("chat-time").textContent = clock(endsAt - now) + " left";
  box.classList.toggle("wolf", wolf);
  $("chat-text").placeholder = wolf ? "Plot with your pack…" : "Say something to the village…";
  const left = Math.max(0, CHAT_MAX - chatSent);
  $("chat-left").textContent = left + " of " + CHAT_MAX + " left";
  let status = "";
  if (left === 0) status = "You've used all " + CHAT_MAX + " messages this round.";
  else if (!chatTurn) {
    const wait = chatLastSent + CHAT_COOLDOWN_MS - now;
    status = wait > 0 ? "You can talk again in " + Math.ceil(wait / 1000) + "s" : "Waiting for your turn…";
  }
  $("chat-status").textContent = status;
  const n = [...$("chat-text").value].length;
  $("chat-counter").textContent = n + " / " + MAX_TEXT;
  $("chat-send").disabled = !chatTurn || chatBusy || left === 0 || n === 0 || n > MAX_TEXT;
}

async function sendChat() {
  const text = stake ? cleanText($("chat-text").value) : $("chat-text").value.trim();
  if (!chatTurn || chatBusy || !session || !text) return;
  const current = chatTurn;
  chatBusy = true;
  $("chat-error").textContent = "";
  renderChat();
  try {
    const action = { type: "speak", text };
    const signed = await signTurn(current, action);
    const res = await fetch("/v1/act", {
      method: "POST",
      headers: { authorization: "Bearer " + session.token, "content-type": "application/json" },
      body: JSON.stringify({ turn_id: current.turn_id, action, ...(signed && signed.fields) }),
      signal: AbortSignal.timeout(10000),
    });
    if (res.ok) {
      if (signed) keepReceipt(await res.json().catch(() => null), current, signed);
      chatSent++;
      chatLastSent = performance.now();
      $("chat-text").value = "";
      if (chatTurn === current) dropChatTurn();
      return;
    }
    const body = await res.json().catch(() => ({}));
    if (res.status === 401) forget("The game was reset. Join again!");
    // 409: the window closed (or the turn moved on); drop it quietly and keep the draft.
    else if (res.status === 409) {
      if (chatTurn === current) dropChatTurn();
    } else $("chat-error").textContent = (res.status === 422 ? "Not allowed: " : "Error: ") + ((body && body.error) || "HTTP " + res.status);
  } catch (err) {
    $("chat-error").textContent = "Could not send (" + err.message + "). Try again.";
  } finally {
    chatBusy = false;
    renderChat();
  }
}

$("chat-text").addEventListener("input", renderChat);
// Enter sends; Shift+Enter adds a newline. Skip while an IME is composing so Enter can confirm a character.
$("chat-text").addEventListener("keydown", (e) => {
  if (e.key !== "Enter" || e.shiftKey || e.isComposing) return;
  e.preventDefault();
  sendChat();
});
$("chat-send").addEventListener("click", () => sendChat());

function hideTurn() {
  if (turn) turn.settle();
  turn = null;
  selected = null;
  $("action").hidden = true;
  syncSpacer();
  scene.setPicking(null, null);
}

function pick(id) {
  if (!turn || !turn.candidates.includes(id)) return;
  selected = id;
  updateSelection();
}

function updateSelection() {
  for (const b of $("act-cands").children) b.classList.toggle("selected", Number(b.dataset.id) === selected);
  $("act-confirm").disabled = selected === null || busy;
  const verb = { sheriff_shoot: "Fire at", cupid_link: "Pick" }[turn && turn.kind] || "Confirm";
  $("act-confirm").textContent = selected === null ? verb.replace(" at", "")
    : verb + " " + game.nameOf(selected);
  if (turn) scene.setPicking(turn.candidates, selected);
}

function tick() {
  renderChat();
  if (!turn) return;
  const left = Math.max(0, turn.deadline - performance.now());
  const total = Math.max(turn.deadline_ms, 1);
  $("act-time").textContent = left > 0 ? Math.ceil(left / 1000) + "s left" : "time's up";
  $("act-countdown").firstElementChild.style.width = (100 * left) / total + "%";
  $("act-countdown").classList.toggle("urgent", left < 10000);
}
setInterval(tick, 250);

async function act(action) {
  if (!turn || busy || !session) return;
  const current = turn;
  busy = true;
  setButtons();
  $("act-error").textContent = "";
  try {
    const signed = await signTurn(current, action);
    if (signed) armForce(current, action);
    const res = await fetch("/v1/act", {
      method: "POST",
      headers: { authorization: "Bearer " + session.token, "content-type": "application/json" },
      body: JSON.stringify({ turn_id: current.turn_id, action, ...(signed && signed.fields) }),
      signal: AbortSignal.timeout(10000),
    });
    if (res.ok) {
      if (signed) {
        keepReceipt(await res.json().catch(() => null), current, signed).then((ok) => ok && settleForce(current.turn_id));
      }
      hideTurn();
      notice("Choice locked in.");
      return;
    }
    const body = await res.json().catch(() => ({}));
    const msg = (body && body.error) || "HTTP " + res.status;
    if (res.status === 401) {
      forget("The game was reset. Join again!");
    } else if (res.status === 409) {
      // Stale turn: poll again; the panel stays only if the server still has this turn pending.
      $("act-error").textContent = "That turn is over: " + msg;
      notice("That turn already ended.");
      current.settle();
    } else {
      // A 422 judges the move itself, so fix it rather than force it.
      if (res.status === 422) settleForce(current.turn_id);
      $("act-error").textContent = (res.status === 422 ? "Not allowed: " : "Error: ") + msg;
    }
  } catch (err) {
    $("act-error").textContent = "Could not send (" + err.message + "). Try again.";
  } finally {
    busy = false;
    setButtons();
  }
}

function setButtons() {
  $("act-abstain").disabled = busy;
  updateSelection();
}
$("act-confirm").addEventListener("click", () => {
  if (selected !== null) act({ type: "target", target: selected });
});
$("act-abstain").addEventListener("click", () => act({ type: "target", target: null }));

// ------------------------------------------------------------ game over

const teamOf = (role) => (role === "werewolf" ? "wolves" : "village");

function showGameOver(winner, view) {
  if (!winner) return;
  overShown = true;
  const role = (view && view.role) || (session && game.state.roles[session.player_id]);
  const partner = lovePartner();
  const partnerRole = partner === null ? null
    : game.state.roles[partner] || (view && view.players.find((p) => p.id === partner)?.revealed_role);
  // Lovers from opposite teams only ever win together, as "lovers".
  const torn = !!(role && partnerRole && teamOf(role) !== teamOf(partnerRole));
  const won = !role ? null : winner === "lovers" ? partner !== null : teamOf(role) === winner && !torn;
  const res = $("go-result");
  res.textContent = won === null ? "Game over" : won ? "You won!" : "You lost";
  res.className = "result" + (won === null ? "" : won ? " win" : " lose");
  // The poll's game_over can beat the stream's; then the view says who is still standing.
  const standing = view ? view.players.filter((p) => p.alive).map((p) => p.id) : [];
  const pair = game.lovers(you()) || (winner === "lovers" && standing.length === 2 ? standing : null);
  let text = { village: "The village wins!", wolves: "The werewolves win!", lovers: "The lovers win!" }[winner] || "Game over";
  if (winner === "lovers" && pair) text += " " + game.nameOf(pair[0]) + " ♥ " + game.nameOf(pair[1]);
  else if (torn && teamOf(role) === winner) text += " Your love crossed sides, so your team's win isn't yours.";
  $("go-winner").textContent = text;
  $("again").hidden = !!stake;
  $("again-hint").textContent = stake ? "Close this to see your payout under Staked game." : "";
  renderRoles(view);
  $("gameover").hidden = false;
  startPayout();
}

function renderRoles(view) {
  const roles = { ...game.state.roles };
  if (view) for (const p of view.players) if (p.revealed_role) roles[p.id] = p.revealed_role;
  const lovers = game.lovers(you());
  const items = game.state.players.map((p) => {
    const li = el("li");
    const who = el("span", null, p.name + (p.id === you() ? " (you)" : ""));
    if (lovers && lovers.includes(p.id)) who.appendChild(el("span", "lover-mark", " ♥"));
    li.appendChild(who);
    const r = roles[p.id];
    li.appendChild(el("span", r ? "r-" + r : "", r ? ROLE_LABEL[r] || r : "?"));
    return li;
  });
  $("go-roles").replaceChildren(...items);
}

// ------------------------------------------------------------ staked games (docs/SETTLEMENT.md)

const FORCE_AFTER_MS = 5000;
const PAYOUT_EVERY_MS = 10000;
const PAYOUT_FOR_MS = 2 * 60 * 60 * 1000;
const STEPS = [["approve", "Approve the buy-in"], ["seat", "Stake and take a seat"], ["http", "Join the game"]];
const FINAL = new Set(["Settled", "Void", "Cancelled"]);

let stake = null; // validated /v1/info.stake; null in free games
let stakeProblem = "";
let stakeAlert = "";
let tokenInfo = null; // { seatCount, decimals, symbol } read from the chain, for display only
let wallet = null;
let walletStamp = "";
let sessionKey = null;
let keyNote = "";
let receipts = [];
let steps = {};
let force = null; // { round, turnId, action, timer, shown, busy, sent, status, error }
let payout = null; // { stake, address, until, owed, paid, timer, busy, text, error }

const wallets = discoverWallets(() => queueMicrotask(render));
const keyId = (s, address) => gameKey(s) + ":" + address;
const money = (v) => amountText(v, tokenInfo);
// Render runs on every game event; rewriting unchanged alert text would re-announce it.
const setText = (id, text) => {
  if ($(id).textContent !== text) $(id).textContent = text;
};
/** The game a wallet works for: the open one, or the one still paying out. */
const walletGame = () => stake || (payout && payout.stake);

/** Reads /v1/info; a staked game sets `stake`. Leaves everything as is when the server is unreachable. */
async function refreshInfo() {
  let info;
  try {
    const res = await fetch("/v1/info", { signal: AbortSignal.timeout(10000) });
    if (!res.ok) throw new Error("HTTP " + res.status);
    info = await res.json();
  } catch (err) {
    console.warn("cannot load /v1/info", err);
    return;
  }
  let next = null;
  stakeProblem = "";
  if (info && info.stake) {
    try {
      next = parseStake(info.stake);
    } catch (err) {
      stakeProblem = "This server's stake terms are invalid (" + err.message + ").";
    }
  }
  if (!next || !stake || gameKey(next) !== gameKey(stake)) {
    tokenInfo = null;
    steps = {};
  }
  stake = next;
  if (stake && wallet) wallet.stake = stake;
  if (stake) loadToken();
  render();
}
const infoReady = refreshInfo();

/** Seats taken and the token's decimals and symbol, via `rpc_url` or else the wallet. */
async function loadToken() {
  const s = stake;
  if (!s) return;
  const readers = [s.rpcUrl && new Wallet(httpProvider(s.rpcUrl), s), wallet].filter(Boolean);
  for (const reader of readers) {
    try {
      if (reader === wallet && !(await wallet.onChain())) continue;
      const t = await reader.terms();
      if (s === stake) tokenInfo = t;
      break;
    } catch (err) {
      console.warn("cannot read the game from the chain", walletMessage(err));
    }
  }
  render();
}
setInterval(() => {
  if (stake && !session && document.visibilityState === "visible") loadToken();
}, 15000);

function adopt(w) {
  wallet = w;
  if (typeof w.provider.on === "function") {
    w.provider.on("accountsChanged", (accounts) => {
      if (wallet !== w) return;
      w.useAccounts(accounts);
      render();
    });
  }
  loadToken();
  if (payout) {
    clearTimeout(payout.timer);
    checkPayout(payout);
  }
}

async function connectWallet(provider) {
  stakeAlert = "";
  try {
    const w = new Wallet(provider, walletGame());
    await w.connect();
    adopt(w);
  } catch (err) {
    stakeAlert = walletMessage(err);
  }
  render();
}

/** The connected wallet on the game's chain; `address` (the seat's owner) must match when given. */
async function ensureWallet(address) {
  if (!walletGame()) throw new Error("No staked game");
  if (!wallet) {
    const list = wallets();
    if (list.length !== 1) throw new Error(list.length ? "Pick a wallet above first." : NO_WALLET);
    const w = new Wallet(list[0].provider, walletGame());
    await w.connect();
    adopt(w);
  } else {
    await wallet.connect();
  }
  if (address && wallet.address !== address) {
    throw new Error("Switch your wallet to " + shortAddr(address) + ", the account that holds your seat.");
  }
  return wallet;
}

/** After a reload, picks up the seat's wallet without a prompt if it is still connected. */
async function resumeWallet(address) {
  for (const { provider } of wallets()) {
    const w = new Wallet(provider, stake);
    try {
      if ((await w.resume()) === address) return adopt(w);
    } catch (err) {
      console.warn("wallet resume failed", walletMessage(err));
    }
  }
}

/** This seat's session key from IndexedDB, or a new one. */
async function sessionKeyFor(address) {
  const id = keyId(stake, address);
  const stored = await loadItem("keys", id);
  if (stored && stored.privateKey && stored.publicKey) return stored;
  const pair = await generateSessionKey();
  if (!(await saveItem("keys", id, pair))) {
    keyNote = "This browser can't store your session key: reloading the page loses your seat's signing key.";
  }
  return pair;
}

async function joinStaked(name) {
  const s = stake;
  let step = null;
  const mark = (id, state, detail) => {
    step = id;
    steps[id] = { state, detail };
    renderStake();
  };
  steps = {};
  stakeAlert = "";
  try {
    const w = await ensureWallet();
    const me = w.address;
    mark("approve", "busy", "Checking the escrow…");
    const keys = await sessionKeyFor(me);
    const pub = bytesToHex(await publicKeyBytes(keys));
    const seats = decodeSeats(await w.call(s.escrow, calldata.seats(s.gameId)));
    const seat = seats.players.indexOf(me);
    if (seat >= 0 && seats.keys[seat] !== pub) {
      throw new Error("This wallet already holds seat " + seat + " with another session key. Join from the browser you staked in.");
    }
    if (seat >= 0) {
      mark("approve", "done", "Not needed");
      mark("seat", "done", "Seat " + seat + " is already yours");
    } else {
      if (Date.now() / 1000 > s.joinBy) throw new Error("Joining closed at " + localTime(s.joinBy) + ".");
      if (seats.players.length >= s.maxSeats) throw new Error("All " + s.maxSeats + " seats are taken.");
      const balance = decodeUint(await w.call(s.token, calldata.balanceOf(me)));
      if (balance < s.stake) throw new Error("You have " + money(balance) + ", but the buy-in is " + money(s.stake) + ".");
      if (decodeUint(await w.call(s.token, calldata.allowance(me, s.escrow))) < s.stake) {
        mark("approve", "busy", "Confirm in your wallet…");
        await w.transact(s.token, calldata.approve(s.escrow, s.stake), () => mark("approve", "busy", "Waiting for confirmation…"));
        mark("approve", "done", "Approved " + money(s.stake));
      } else {
        mark("approve", "done", "Already approved");
      }
      mark("seat", "busy", "Confirm in your wallet…");
      await w.transact(s.escrow, calldata.join(s.gameId, pub), () => mark("seat", "busy", "Waiting for confirmation…"));
      mark("seat", "done", "Staked " + money(s.stake));
    }
    mark("http", "busy", "Joining…");
    const signature = bytesToHex(await sign(keys.privateKey, joinMessage(s, me)));
    const { status, body } = await postJoin(name, { address: me, session_key: pub, signature }).catch((err) => {
      throw new Error("Cannot reach the server: " + err.message);
    });
    if (status !== 200 || !Number.isInteger(body.player_id) || typeof body.token !== "string") {
      throw new Error((body && body.error) || "Join failed (HTTP " + status + ")");
    }
    mark("http", "done", "You're in");
    sessionKey = keys;
    startSession({ token: body.token, player_id: body.player_id, name, address: me });
  } catch (err) {
    if (step) steps[step] = { state: "fail", detail: "" };
    stakeAlert = walletMessage(err);
    render();
  }
}

/** Staked games: `log_hash` and `signature` for this answer, and the act hash its receipt must name. */
async function signTurn(t, action) {
  await infoReady;
  if (!stake) return null;
  if (!session.address) throw new Error("This seat has no wallet on record; join again.");
  sessionKey = sessionKey || (await loadItem("keys", keyId(stake, session.address))) || null;
  if (!sessionKey) throw new Error("This browser lost your session key, so it can't sign moves");
  if (!Number.isInteger(t.step) || !t.view) throw new Error("The server sent a turn without a step");
  const hash = await logHash(stake, t.view.public_log);
  const message = actMessage(stake, { turnId: t.turn_id, step: t.step, seat: session.player_id, logHash: hash, action });
  return {
    fields: { log_hash: bytesToHex(hash), signature: bytesToHex(await sign(sessionKey.privateKey, message)) },
    actHash: await sha256(message),
  };
}

/** Checks and keeps the receipt for a signed answer; false when it's missing or doesn't hold up. */
async function keepReceipt(body, t, signed) {
  const r = body && body.receipt;
  if (!session) return false;
  try {
    if (!receiptMatches(r, { turnId: t.turn_id, seat: session.player_id, actHash: signed.actHash })) {
      if (r) stakeAlert = "The server's receipt for turn " + t.turn_id + " doesn't match your move.";
      return false;
    }
    // The escrow recomputes the digest and ecrecover checks the operator; skipped on another chain.
    if (wallet && (await wallet.onChain()) && !(await wallet.receiptSigned(r))) {
      stakeAlert = "The operator's receipt for turn " + t.turn_id + " has a bad signature.";
      return false;
    }
    receipts.push(r);
    saveItem("receipts", gameKey(stake) + ":" + r.turn_id + ":" + r.act_hash, r);
    return true;
  } catch (err) {
    console.warn("could not check a receipt", walletMessage(err));
    return false;
  } finally {
    render();
  }
}

function armForce(t, action) {
  dropForce();
  if (!Number.isInteger(t.round)) return;
  const f = { round: t.round, turnId: t.turn_id, action, shown: false, busy: false, sent: false, status: "", error: "" };
  f.timer = setTimeout(() => offerForce(f), FORCE_AFTER_MS);
  force = f;
}

function settleForce(turnId) {
  if (force && force.turnId === turnId && !force.sent) dropForce();
}

function dropForce() {
  if (!force || force.busy) return;
  clearTimeout(force.timer);
  force = null;
  renderForce();
}

async function roundOpen(round) {
  const g = decodeGameInfo(await wallet.call(stake.escrow, calldata.gameInfo(stake.gameId)));
  return STATUS[g.status] === "Started" && g.nextRound === round;
}

async function offerForce(f) {
  let open = true;
  try {
    if (wallet && (await wallet.onChain())) open = await roundOpen(f.round);
  } catch (err) {
    // The click checks again and shows the error; offering costs nothing.
    console.warn("cannot check the round", walletMessage(err));
  }
  if (force !== f) return;
  if (!open) return dropForce();
  f.shown = true;
  renderForce();
  $("force-card").scrollIntoView({ block: "nearest", behavior: "smooth" });
  buzz();
}

async function sendForce() {
  const f = force;
  if (!f || f.busy || f.sent || !session) return;
  f.busy = true;
  f.error = "";
  f.status = "Checking the round…";
  renderForce();
  try {
    const w = await ensureWallet(session.address);
    if (!(await roundOpen(f.round))) throw new Error("This round has closed, so the contract won't take the move.");
    const payload = await forcePayload(stake, { round: f.round, seat: session.player_id, action: f.action });
    f.status = "Confirm in your wallet…";
    renderForce();
    await w.transact(stake.escrow, calldata.forceAction(stake.gameId, f.round, payload), () => {
      f.status = "Waiting for confirmation…";
      renderForce();
    });
    f.sent = true;
    f.status = "Your move is on-chain. The server must use it for this round.";
  } catch (err) {
    f.status = "";
    f.error = walletMessage(err);
  } finally {
    f.busy = false;
    renderForce();
  }
}
$("force-send").addEventListener("click", () => sendForce());

function startPayout() {
  if (!stake || !session || !session.address || payout) return;
  payout = { stake, address: session.address, until: Date.now() + PAYOUT_FOR_MS, owed: 0n, paid: 0n, busy: false, text: "Checking your payout…", error: "" };
  checkPayout(payout);
}

async function checkPayout(p) {
  if (payout !== p || p.busy || p.paid) return;
  const s = p.stake;
  let again = true;
  try {
    if (!wallet) p.text = "Connect your wallet to see your payout.";
    else if (!(await wallet.onChain())) p.text = "Switch your wallet to chain " + s.chainId + " to see your payout.";
    else {
      const [g, owed] = await Promise.all([
        wallet.call(s.escrow, calldata.gameInfo(s.gameId)).then(decodeGameInfo),
        wallet.call(s.escrow, calldata.claimable(s.gameId, p.address)).then(decodeUint),
      ]);
      const status = STATUS[g.status] || "Unknown";
      p.owed = owed;
      if (owed > 0n) {
        p.text = "You can claim " + money(owed) + ".";
        again = false;
      } else if (FINAL.has(status)) {
        p.text = "Game " + status.toLowerCase() + ". Nothing left to claim for this seat.";
        again = false;
      } else {
        p.text = "Waiting for the result to settle on-chain (" + status.toLowerCase() + "). Checking every 10 s.";
      }
    }
    p.error = "";
  } catch (err) {
    p.error = "Can't read your payout (" + walletMessage(err) + "). Retrying.";
  }
  if (again && Date.now() + PAYOUT_EVERY_MS > p.until) {
    again = false;
    p.text = "Nothing to claim after 2 hours of checking. Reload the page to check again.";
  }
  if (again) p.timer = setTimeout(() => checkPayout(p), PAYOUT_EVERY_MS);
  renderPayout();
}

async function claim() {
  const p = payout;
  if (!p || p.busy || p.owed <= 0n) return;
  p.busy = true;
  p.error = "";
  renderPayout();
  try {
    const w = await ensureWallet(p.address);
    p.text = "Confirm in your wallet…";
    renderPayout();
    await w.transact(p.stake.escrow, calldata.claim(p.stake.gameId), () => {
      p.text = "Waiting for confirmation…";
      renderPayout();
    });
    p.paid = p.owed;
    p.owed = 0n;
    p.text = "Paid out " + money(p.paid) + " to " + shortAddr(p.address) + ".";
  } catch (err) {
    p.text = "You can claim " + money(p.owed) + ".";
    p.error = walletMessage(err);
  } finally {
    p.busy = false;
    renderPayout();
  }
}
$("claim").addEventListener("click", () => claim());

function renderStake() {
  $("join-btn").textContent = stake ? "Stake & join" : "Join game";
  const card = $("stake-card");
  card.hidden = !stake && !stakeProblem && !payout;
  if (card.hidden) return;
  const joining = !!stake && !session;
  const rows = [];
  if (joining) {
    const closed = Date.now() / 1000 > stake.joinBy;
    const units = tokenInfo && Number.isInteger(tokenInfo.decimals) ? " (" + stake.stake + " base units)" : "";
    rows.push(["Buy-in", money(stake.stake) + units]);
    rows.push(["Seats", (tokenInfo ? tokenInfo.seatCount + " of " + stake.maxSeats + " taken" : "up to " + stake.maxSeats)
      + ", " + stake.minSeats + " to start"]);
    rows.push(["Join by", localTime(stake.joinBy) + (closed ? " (closed)" : "")]);
  }
  $("stake-terms").replaceChildren(...rows.flatMap(([k, v]) => [el("dt", null, k), el("dd", null, v)]));
  $("stake-terms").hidden = rows.length === 0;
  renderWallet();
  const shown = joining && Object.keys(steps).length > 0;
  $("stake-steps").replaceChildren(...(shown ? STEPS : []).map(([id, label]) => {
    const st = steps[id] || { state: "todo", detail: "" };
    return el("li", "step-" + st.state, label + (st.detail ? ": " + st.detail : ""));
  }));
  setText("stake-error", stakeProblem || stakeAlert);
  const notes = [game.state.error && "The staked game stopped: " + game.state.error, keyNote];
  if (stake && session) notes.push("Operator receipts for your moves: " + receipts.length + ".");
  setText("stake-note", notes.filter(Boolean).join(" "));
  renderForce();
  renderPayout();
}

function renderWallet() {
  const list = wallets();
  const addr = wallet && wallet.address;
  const seat = session && session.address;
  const stamp = [addr, seat, !!walletGame(), ...list.map((w) => w.name)].join("|");
  if (stamp === walletStamp) return;
  walletStamp = stamp;
  let nodes;
  if (addr) {
    const text = "Wallet " + shortAddr(addr) + (!seat ? "" : seat === addr ? " · seat " + session.player_id
      : " · your seat is " + shortAddr(seat));
    nodes = [el("span", "wallet-addr", text)];
  } else if (!walletGame()) {
    nodes = [];
  } else if (!list.length) {
    nodes = [el("span", "hint", NO_WALLET)];
  } else {
    nodes = list.map(({ name, provider }) => {
      const b = el("button", "secondary", list.length > 1 && name ? "Connect " + name : "Connect wallet");
      b.type = "button";
      b.addEventListener("click", () => connectWallet(provider));
      return b;
    });
  }
  $("wallet-row").replaceChildren(...nodes);
}

function renderForce() {
  const f = force && force.shown ? force : null;
  $("force-card").hidden = !f;
  if (!f) return;
  setText("force-text", f.status
    || "The server hasn't confirmed your move. You can send it to the escrow contract yourself; your wallet pays the gas.");
  $("force-send").hidden = f.sent;
  $("force-send").disabled = f.busy;
  setText("force-error", f.error);
}

function renderPayout() {
  const p = payout;
  $("payout").hidden = !p;
  if (!p) return;
  setText("payout-text", p.text);
  $("claim").hidden = !(p.owed > 0n);
  $("claim").disabled = p.busy;
  setText("claim", "Claim " + money(p.owed));
  setText("payout-error", p.error);
}

// ------------------------------------------------------------ boot

$("name").value = load(NAME_KEY) || "";
if (session) startSession(session);
else watch(null);
render();
infoReady.then(() => {
  if (stake && session && session.address) resumeWallet(session.address);
});

// Human player page: join, watch the village from your seat, and answer turns via long-poll.

import { Game, connectStream, ROLE_LABEL, ROLE_GOAL } from "./events.js";
import { Feed, el } from "./feed.js";
import { Scene } from "./scene.js";
import { iconCanvas } from "./sprites.js";

const $ = (id) => document.getElementById(id);
const SESSION_KEY = "werewolf.session";
const NAME_KEY = "werewolf.name";
const MAX_TEXT = 600;
const PRIVATE = new Set(["wolf_spoke", "wolf_voted", "inspected", "protected"]);
const TURN_TEXT = {
  day_speak: ["Your turn to speak", "Everyone hears this. Accuse, defend, or bluff."],
  wolf_chat: ["Whisper to your pack", "Only werewolves see this."],
  day_vote: ["Vote to eliminate", "Tap a player in the village or pick from the list."],
  wolf_kill: ["Pick tonight's victim (wolves only)", "Your pack's votes decide who dies."],
  seer_inspect: ["Choose someone to inspect", "You will learn whether they are a werewolf."],
  doctor_protect: ["Choose someone to protect", "If the wolves pick them tonight, they survive."],
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

const game = new Game();
const you = () => (session ? session.player_id : null);
const scene = new Scene($("scene"), game, { you, onPick: (id) => pick(id) });
new Feed($("feed"), game, { filter: (ev) => !PRIVATE.has(ev.type), you, emptyText: "The village is quiet…" });
new Feed($("private-feed"), game, { filter: (ev) => PRIVATE.has(ev.type), you, emptyText: "Nothing secret yet." });
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
    $("role-name").textContent = "You are the " + (ROLE_LABEL[role] || role);
    $("role-goal").textContent = ROLE_GOAL[role] || "";
    const pack = role === "werewolf"
      ? Object.keys(s.roles).map(Number).filter((id) => id !== session.player_id && s.roles[id] === "werewolf")
      : [];
    $("role-pack").textContent = pack.length
      ? "Your pack: " + pack.map((id) => game.nameOf(id) + (game.player(id)?.alive === false ? " (dead)" : "")).join(", ")
      : role === "werewolf" ? "You hunt alone." : "";
  }
  $("private-card").hidden = !role || role === "villager";

  let status = "";
  if (rejoining) status = "Waiting for the next lobby to open…";
  else if (session && s.phase.kind === "lobby") {
    status = "You're in, " + session.name + "! Waiting for the host to start… (" + s.players.length + " joined)";
  } else if (me && !me.alive && s.phase.kind !== "over") status = "You were eliminated. You can keep watching the game.";
  else if (session && s.phase.kind === "over") status = "Game over.";
  $("status-text").textContent = status;
  $("status-card").hidden = !status;
  $("status-again").hidden = !(session && s.phase.kind === "over");
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
}

game.on((ev) => {
  if ((ev.type === "reset" || (ev.type === "snapshot" && game.state.phase.kind === "lobby")) && wakeRejoin) wakeRejoin();
  // A reset invalidates every token; offer the join form with the name kept.
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

async function postJoin(name) {
  const res = await fetch("/v1/join", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ name, model: null, human: true }),
    signal: AbortSignal.timeout(10000),
  });
  const body = await res.json().catch(() => ({}));
  return { status: res.status, body };
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
    const { status, body } = await postJoin(name);
    if (status === 200 && Number.isInteger(body.player_id) && typeof body.token === "string") {
      startSession({ token: body.token, player_id: body.player_id, name });
    } else {
      $("join-error").textContent = (body && body.error) || "Join failed (HTTP " + status + ")";
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
    if (body.status === "turn") {
      if (!turn || turn.turn_id !== body.turn_id) showTurn(body);
      else turn.deadline = performance.now() + body.deadline_ms;
      await new Promise((r) => {
        turn.settle = r;
        setTimeout(r, body.deadline_ms + 1500);
      });
    } else if (body.status === "game_over") {
      hideTurn();
      showGameOver(body.winner, body.view);
      return;
    } else {
      hideTurn();
    }
  }
}

function showTurn(t) {
  turn = { ...t, deadline: performance.now() + t.deadline_ms, settle: () => {} };
  selected = null;
  const [title, help] = TURN_TEXT[t.kind] || [t.kind, ""];
  $("act-title").textContent = title;
  $("act-help").textContent = help;
  $("act-error").textContent = "";
  const speaks = t.kind === "day_speak" || t.kind === "wolf_chat";
  $("act-speak").hidden = !speaks;
  $("act-target").hidden = speaks;
  if (speaks) {
    $("act-text").value = "";
    $("act-text").placeholder = t.kind === "wolf_chat" ? "Plot with your pack…" : "Say something to the village…";
    updateCounter();
    scene.setPicking(null, null);
  } else {
    const names = new Map((t.view && t.view.players ? t.view.players : game.state.players).map((p) => [p.id, p]));
    const buttons = t.candidates.map((id) => {
      const p = names.get(id);
      const b = el("button");
      b.type = "button";
      b.dataset.id = String(id);
      b.appendChild(document.createTextNode(p ? p.name : "#" + id));
      b.appendChild(el("span", "c-model", p ? (p.human ? "human" : p.model || "agent") : ""));
      b.addEventListener("click", () => pick(id));
      return b;
    });
    $("act-cands").replaceChildren(...buttons);
    $("act-abstain").textContent = t.kind === "wolf_kill" ? "Spare everyone" : "Abstain";
    scene.setPicking(t.candidates, null);
  }
  updateSelection();
  $("action").hidden = false;
  document.body.classList.add("has-action");
  if (speaks && matchMedia("(min-width: 900px)").matches) $("act-text").focus();
  try {
    // Browsers refuse (and log) vibration before the first tap.
    const tapped = !navigator.userActivation || navigator.userActivation.hasBeenActive;
    if (navigator.vibrate && tapped) navigator.vibrate([120, 60, 120]);
  } catch (err) {
    console.warn("vibrate failed", err);
  }
  tick();
}

function hideTurn() {
  if (turn) turn.settle();
  turn = null;
  selected = null;
  $("action").hidden = true;
  document.body.classList.remove("has-action");
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
  $("act-confirm").textContent = selected === null ? "Confirm" : "Confirm " + game.nameOf(selected);
  if (turn) scene.setPicking(turn.candidates, selected);
}

function updateCounter() {
  const n = [...$("act-text").value].length;
  $("act-counter").textContent = n + " / " + MAX_TEXT;
  $("act-send").disabled = busy || n === 0 || n > MAX_TEXT;
}

function tick() {
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
    const res = await fetch("/v1/act", {
      method: "POST",
      headers: { authorization: "Bearer " + session.token, "content-type": "application/json" },
      body: JSON.stringify({ turn_id: current.turn_id, action }),
      signal: AbortSignal.timeout(10000),
    });
    if (res.ok) {
      hideTurn();
      notice(action.type === "speak" ? (action.text ? "Said it." : "You stayed silent.") : "Choice locked in.");
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
  for (const id of ["act-silent", "act-abstain"]) $(id).disabled = busy;
  updateCounter();
  updateSelection();
}

$("act-text").addEventListener("input", updateCounter);
$("act-text").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) $("act-send").click();
});
$("act-send").addEventListener("click", () => act({ type: "speak", text: $("act-text").value.trim() }));
$("act-silent").addEventListener("click", () => act({ type: "speak", text: "" }));
$("act-confirm").addEventListener("click", () => {
  if (selected !== null) act({ type: "target", target: selected });
});
$("act-abstain").addEventListener("click", () => act({ type: "target", target: null }));

// ------------------------------------------------------------ game over

function showGameOver(winner, view) {
  if (!winner) return;
  overShown = true;
  const role = (view && view.role) || (session && game.state.roles[session.player_id]);
  const team = role === "werewolf" ? "wolves" : "village";
  const won = role ? team === winner : null;
  const res = $("go-result");
  res.textContent = won === null ? "Game over" : won ? "You won!" : "You lost";
  res.className = "result" + (won === null ? "" : won ? " win" : " lose");
  $("go-winner").textContent = winner === "village" ? "The village wins!" : "The werewolves win!";
  $("again-hint").textContent = "";
  renderRoles(view);
  $("gameover").hidden = false;
}

function renderRoles(view) {
  const roles = { ...game.state.roles };
  if (view) for (const p of view.players) if (p.revealed_role) roles[p.id] = p.revealed_role;
  const items = game.state.players.map((p) => {
    const li = el("li");
    li.appendChild(el("span", null, p.name + (p.id === you() ? " (you)" : "")));
    const r = roles[p.id];
    li.appendChild(el("span", r ? "r-" + r : "", r ? ROLE_LABEL[r] || r : "?"));
    return li;
  });
  $("go-roles").replaceChildren(...items);
}

// ------------------------------------------------------------ boot

$("name").value = load(NAME_KEY) || "";
if (session) startSession(session);
else watch(null);
render();

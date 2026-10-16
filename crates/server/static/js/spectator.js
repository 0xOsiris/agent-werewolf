// Projector page: all roles, private events, and reasoning via /spectate/events?key=...
// Without a key it falls back to the public audience stream and hides the admin controls.

import { Game, connectStream, phaseTitle, rulesLine } from "./events.js";
import { Feed, el } from "./feed.js";
import { Scene } from "./scene.js";
import { iconCanvas } from "./sprites.js";

const $ = (id) => document.getElementById(id);
const key = new URLSearchParams(location.search).get("key") || "";
const game = new Game();
new Feed($("feed"), game, { thoughts: true });
new Scene($("scene"), game, { chatBanner: true });
$("brand").prepend(iconCanvas("werewolf", 4));

function setStatus(text, bad) {
  const s = $("status");
  s.textContent = text;
  s.classList.toggle("bad", !!bad);
}

function render() {
  const s = game.state;
  const ph = s.phase.kind;
  $("phase").textContent = phaseTitle(s.phase, s.winner);
  document.body.classList.toggle("day", ph === "day" || ph === "lobby" || (ph === "over" && (s.winner === "village" || s.winner === "lovers")));
  const alive = s.players.filter((p) => p.alive).length;
  const humans = s.players.filter((p) => p.human).length;
  const rules = rulesLine(s.settings);
  const count = ph === "lobby"
    ? s.players.length + " joined" + (humans ? " · " + humans + " human" + (humans === 1 ? "" : "s") : "")
    : alive + " of " + s.players.length + " alive";
  $("subtitle").textContent = count + (rules ? " · " + rules : "");
  $("start").disabled = ph !== "lobby";
  $("reset").disabled = ph !== "lobby" && ph !== "over";
  const lobby = ph === "lobby";
  $("lobby").hidden = !lobby;
  $("feed").hidden = lobby;
  renderSettings();
}

// ------------------------------------------------------------ host settings (lobby, key only)

const PRESETS = [
  { label: "Werewolves", options: [["Auto", { wolves: null }], ["1", { wolves: 1 }], ["2", { wolves: 2 }], ["3", { wolves: 3 }]] },
  { label: "Sheriff", options: [["On", { sheriff: true }], ["Off", { sheriff: false }]] },
  { label: "Cupid", options: [["On", { cupid: true }], ["Off", { cupid: false }]] },
  { label: "Jester", options: [["On", { jester: true }], ["Off", { jester: false }]] },
  { label: "Discussion", options: [["1 min", { discussion_secs: 60 }], ["2 min", { discussion_secs: 120 }], ["3 min", { discussion_secs: 180 }]] },
  {
    label: "AI chattiness",
    options: [
      ["Quiet", { ai_chat_cooldown_secs: 25, ai_max_chat_messages: 2 }],
      ["Normal", { ai_chat_cooldown_secs: 15, ai_max_chat_messages: 3 }],
      ["Chatty", { ai_chat_cooldown_secs: 8, ai_max_chat_messages: 5 }],
    ],
  },
  { label: "Human turn time", options: [["30s", { human_turn_timeout_secs: 30 }], ["60s", { human_turn_timeout_secs: 60 }], ["90s", { human_turn_timeout_secs: 90 }]] },
];
let saving = false;
let settingsKey = "";

const matches = (settings, patch) => Object.entries(patch).every(([k, v]) => settings[k] === v);

function renderSettings() {
  const s = game.state;
  const box = $("settings");
  box.hidden = !key || !s.settings;
  if (box.hidden) return;
  const editable = s.phase.kind === "lobby" && !saving;
  const stamp = JSON.stringify(s.settings) + editable;
  if (stamp === settingsKey) return;
  settingsKey = stamp;
  // A server without a field would reject it (unknown fields are refused), so hide its row.
  const known = PRESETS.filter((row) => Object.keys(row.options[0][1]).every((k) => k in s.settings));
  const rows = known.map((row) => {
    const line = el("div", "set-row");
    line.appendChild(el("span", "set-label", row.label));
    const group = el("div", "seg");
    let any = false;
    for (const [label, patch] of row.options) {
      const b = el("button", "seg-btn", label);
      b.type = "button";
      const on = matches(s.settings, patch);
      any = any || on;
      b.classList.toggle("on", on);
      b.setAttribute("aria-pressed", String(on));
      b.disabled = !editable;
      b.addEventListener("click", () => saveSettings(patch));
      group.appendChild(b);
    }
    if (!any) group.appendChild(el("span", "seg-custom", "Custom"));
    line.appendChild(group);
    return line;
  });
  $("set-rows").replaceChildren(...rows);
}

let statusTimer = null;

/** `fade` clears the text after a moment; errors stay until the next click. */
function setSettingsStatus(text, bad, fade) {
  $("set-status").textContent = text;
  $("set-status").classList.toggle("bad", !!bad);
  clearTimeout(statusTimer);
  if (fade) statusTimer = setTimeout(() => ($("set-status").textContent = ""), 2500);
}

async function saveSettings(patch) {
  const current = game.state.settings;
  if (!current || saving) return;
  saving = true;
  setSettingsStatus("Saving…");
  renderSettings();
  try {
    const res = await fetch("/admin/settings", {
      method: "PUT",
      headers: { "x-spectator-key": key, "content-type": "application/json" },
      body: JSON.stringify({ ...current, ...patch }),
      signal: AbortSignal.timeout(10000),
    });
    const body = await res.json().catch(() => ({}));
    if (res.ok) {
      // The settings event follows on the stream; apply the reply now so the click feels instant.
      if (body && typeof body === "object" && "sheriff" in body) game.state.settings = body;
      else game.state.settings = { ...current, ...patch };
      setSettingsStatus("Saved", false, true);
    } else {
      setSettingsStatus("Not saved: " + ((body && body.error) || "HTTP " + res.status), true);
    }
  } catch (err) {
    setSettingsStatus("Not saved: " + err.message, true);
  } finally {
    saving = false;
    render();
  }
}

game.on(() => render());

async function admin(path) {
  try {
    const res = await fetch(path, {
      method: "POST",
      headers: { "x-spectator-key": key },
      signal: AbortSignal.timeout(10000),
    });
    if (!res.ok) {
      const body = await res.json().catch(() => ({ error: res.statusText }));
      setStatus(path.replace("/admin/", "") + " failed: " + (body.error || res.status), true);
    }
  } catch (err) {
    setStatus("request failed: " + err.message, true);
  }
}

async function loadInfo() {
  try {
    const res = await fetch("/v1/info", { signal: AbortSignal.timeout(10000) });
    if (!res.ok) throw new Error("HTTP " + res.status);
    const info = await res.json();
    if (typeof info.play_url === "string") $("play-url").textContent = info.play_url;
  } catch (err) {
    console.warn("cannot load /v1/info", err);
    $("play-url").textContent = location.origin + "/play";
  }
}

$("start").addEventListener("click", () => admin("/admin/start"));
$("reset").addEventListener("click", () => admin("/admin/reset"));

render();
loadInfo();
if (key) {
  connectStream("/spectate/events?key=" + encodeURIComponent(key), game, {
    onStatus: setStatus,
    onClosed: () => setStatus("connection refused: check the ?key= in the URL", true),
  });
} else {
  $("controls").querySelectorAll("button").forEach((b) => (b.hidden = true));
  setStatus("audience view (add ?key= for roles and controls)", false);
  connectStream("/v1/watch", game, { onStatus: (t, bad) => setStatus(bad ? t : "audience view · live", bad) });
}

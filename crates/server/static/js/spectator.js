// Projector page: all roles, private events, and reasoning via /spectate/events?key=...
// Without a key it falls back to the public audience stream and hides the admin controls.

import { Game, connectStream, phaseTitle } from "./events.js";
import { Feed } from "./feed.js";
import { Scene } from "./scene.js";
import { iconCanvas } from "./sprites.js";

const $ = (id) => document.getElementById(id);
const key = new URLSearchParams(location.search).get("key") || "";
const game = new Game();
new Feed($("feed"), game, { thoughts: true });
new Scene($("scene"), game);
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
  document.body.classList.toggle("day", ph === "day" || ph === "lobby" || (ph === "over" && s.winner === "village"));
  const alive = s.players.filter((p) => p.alive).length;
  const humans = s.players.filter((p) => p.human).length;
  $("subtitle").textContent = ph === "lobby"
    ? s.players.length + " joined" + (humans ? " · " + humans + " human" + (humans === 1 ? "" : "s") : "")
    : alive + " of " + s.players.length + " alive";
  $("start").disabled = ph !== "lobby";
  $("reset").disabled = ph !== "lobby" && ph !== "over";
  const lobby = ph === "lobby";
  $("lobby").hidden = !lobby;
  $("feed").hidden = lobby;
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

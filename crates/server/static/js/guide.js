// "Roles" button and dialog explaining every role and the rules; mounted on both pages.

import { ROLE_LABEL, ROLE_GOAL } from "./events.js";

const ROLES = [
  ["werewolf", "wolf"], ["villager", "villager"], ["seer", "seer"],
  ["doctor", "doctor"], ["sheriff", "sheriff"], ["cupid", "cupid"], ["jester", "jester"],
];
const RULES = [
  ["Night", "Wolves pick a victim. The seer and doctor act in secret."],
  ["Day", "The night's death is announced. Everyone talks, and the sheriff may shoot at any moment. Then everyone votes. Most votes is out; a tie saves everyone."],
  ["Lovers", "If one Lover dies, so does the other. A wolf and a villager in love win only as the last two alive."],
  ["Winning", "The village wins when every wolf is dead. Wolves win when they equal or outnumber everyone else, or reach day 10. A Jester, if the host adds one, wins alone by getting voted out, which ends the game."],
  ["Death", "Dead players' roles are revealed. Once two are dead, ghosts chat in the Graveyard by day; the living never see it."],
];

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text) e.textContent = text;
  return e;
}

function list(cls, items) {
  const dl = el("dl", cls);
  for (const [term, text, color] of items) {
    const dt = el("dt", null, term);
    if (color) dt.style.color = `var(--${color})`;
    dl.append(dt, el("dd", null, text));
  }
  return dl;
}

function build() {
  const dialog = el("dialog", "guide");
  dialog.setAttribute("aria-labelledby", "guide-title");
  const body = el("div", "guide-body");
  const head = el("div", "guide-head");
  const title = el("h2", null, "Roles");
  title.id = "guide-title";
  const close = el("button", "secondary", "Close");
  close.type = "button";
  close.addEventListener("click", () => dialog.close());
  head.append(title, close);
  body.append(
    head,
    list("guide-roles", ROLES.map(([role, color]) => [ROLE_LABEL[role], ROLE_GOAL[role], color])),
    list("guide-rules", RULES),
  );
  dialog.append(body);
  // Clicks on the backdrop land on the dialog itself; the body covers everything inside.
  dialog.addEventListener("click", (e) => { if (e.target === dialog) dialog.close(); });
  return dialog;
}

const host = document.querySelector(".player-head, .topbar");
if (host) {
  const dialog = build();
  const open = el("button", "secondary guide-open", "Roles");
  open.type = "button";
  open.title = "How each role works";
  open.setAttribute("aria-haspopup", "dialog");
  open.addEventListener("click", () => dialog.showModal());
  host.append(open);
  document.body.append(dialog);
}

// "Leaderboard" button and dialog ranking each AI model, and humans together, by win rate.

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

/** "3 of 5", or a dash when the row never played that side. */
const record = (wins, games) => (games ? wins + " of " + games : "–");

function table(rows, minGames) {
  const t = el("table", "board");
  const head = el("tr");
  for (const h of ["#", "Player", "Games", "Won", "As village", "As wolf"]) head.append(el("th", null, h));
  const thead = el("thead");
  thead.append(head);
  t.append(thead);
  const body = el("tbody");
  rows.forEach((r, i) => {
    const tr = el("tr", (r.human ? "human" : "") + (r.games < minGames ? " few" : ""));
    const name = el("td", "board-name", r.human ? "Humans" : r.name.split("/").pop());
    if (!r.human) name.title = r.name;
    tr.append(
      el("td", "board-rank", String(i + 1)),
      name,
      el("td", null, String(r.games)),
      el("td", "board-rate", Math.round((100 * r.wins) / r.games) + "%"),
      el("td", null, record(r.village_wins, r.village_games)),
      el("td", null, record(r.wolf_wins, r.wolf_games)),
    );
    body.append(tr);
  });
  t.append(body);
  return t;
}

async function load(content) {
  content.replaceChildren(el("p", "board-note", "Loading…"));
  try {
    const res = await fetch("/v1/leaderboard", { signal: AbortSignal.timeout(10000) });
    if (!res.ok) throw new Error("HTTP " + res.status);
    const { min_games: minGames, rows } = await res.json();
    if (!rows.length) {
      content.replaceChildren(el("p", "board-note", "No finished games yet."));
      return;
    }
    const notes = [el("p", "board-note", "Every finished game on this server. Humans count as one row.")];
    if (rows.some((r) => r.games < minGames)) {
      notes.push(el("p", "board-note", "Faded rows have fewer than " + minGames + " games, so they rank last."));
    }
    content.replaceChildren(notes[0], table(rows, minGames), ...notes.slice(1));
  } catch (err) {
    content.replaceChildren(el("p", "error", "Couldn't load the leaderboard (" + err.message + "). Close and try again."));
  }
}

function build() {
  const dialog = el("dialog", "guide leaderboard");
  dialog.setAttribute("aria-labelledby", "board-title");
  const body = el("div", "guide-body");
  const head = el("div", "guide-head");
  const title = el("h2", null, "Leaderboard");
  title.id = "board-title";
  const close = el("button", "secondary", "Close");
  close.type = "button";
  close.addEventListener("click", () => dialog.close());
  head.append(title, close);
  const content = el("div", "board-content");
  body.append(head, content);
  dialog.append(body);
  // Clicks on the backdrop land on the dialog itself; the body covers everything inside.
  dialog.addEventListener("click", (e) => { if (e.target === dialog) dialog.close(); });
  return { dialog, content };
}

const host = document.querySelector(".player-head, .topbar");
if (host) {
  const { dialog, content } = build();
  const open = el("button", "secondary guide-open", "Leaderboard");
  open.type = "button";
  open.title = "How each AI model does across games";
  open.setAttribute("aria-haspopup", "dialog");
  open.addEventListener("click", () => {
    dialog.showModal();
    load(content);
  });
  host.append(open);
  document.body.append(dialog);
}

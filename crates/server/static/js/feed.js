// Scrolling chat log shared by both pages. Untrusted text goes through textContent only.

import { KIND_LABEL, ROLE_LABEL, phaseTitle } from "./events.js";
import { thinkIcon } from "./sprites.js";

export function el(tag, cls, text) {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined && text !== null) node.textContent = String(text);
  return node;
}

/** What a player did when the reasoning arrived with no matching event (silence, abstaining). */
const SILENT_ACTION = {
  day_speak: "Stays silent",
  wolf_chat: "Stays silent",
  day_vote: "Abstains",
  wolf_kill: "Wants to spare everyone",
  seer_inspect: "Checks no one",
  doctor_protect: "Protects no one",
  sheriff_shoot: "Holds fire",
};

const DEATH_HOW = {
  night: " was killed in the night",
  vote: " was eliminated by vote",
  shot: " was shot by the sheriff",
  backfire: " died from their own misfire",
};

/** Card colour and tag for a reasoning-only card, by turn kind. */
const KIND_CONTEXT = {
  wolf_chat: ["wolves", "wolves only"],
  wolf_kill: ["wolves", "wolves only"],
  seer_inspect: ["seer", "private"],
  doctor_protect: ["doctor", "private"],
  sheriff_shoot: ["sheriff", "private"],
};

/**
 * Renders `game` events into `root`. `filter(ev)` can drop events (e.g. the private panel).
 * `emptyText` shows while the log is empty. `thoughts` (spectator) renders each action as a
 * card with the actor's secret role and a "Thinks" row for their reasoning.
 */
export class Feed {
  constructor(root, game, { filter = () => true, emptyText = "Waiting for the game to start…", you = () => null, thoughts = false } = {}) {
    this.root = root;
    this.game = game;
    this.filter = filter;
    this.emptyText = emptyText;
    this.you = you;
    this.thoughts = thoughts;
    this.follow = true;
    root.addEventListener("scroll", () => {
      this.follow = root.scrollHeight - root.scrollTop - root.clientHeight < 120;
    });
    game.on((ev, live) => this.onEvent(ev, live));
    this.clear();
  }

  clear() {
    this.root.replaceChildren(el("div", "feed-empty", this.emptyText));
  }

  append(node, player) {
    const first = this.root.firstElementChild;
    if (first && first.classList.contains("feed-empty")) first.remove();
    node.classList.add("feed-item");
    if (player !== undefined) node.dataset.player = String(player);
    this.root.appendChild(node);
    if (this.follow) this.root.scrollTop = this.root.scrollHeight;
  }

  who(id, tag) {
    const g = this.game;
    const box = el("div", "who");
    box.appendChild(el("span", "who-name", g.nameOf(id)));
    const role = this.thoughts ? g.state.roles[id] : null;
    if (role) box.appendChild(el("span", "role-tag role-" + role, ROLE_LABEL[role] || role));
    const p = g.player(id);
    if (p && p.human) box.appendChild(el("span", "tag tag-human", "human"));
    if (id === this.you()) box.appendChild(el("span", "tag tag-you", "you"));
    if (p && p.model) box.appendChild(el("span", "who-model", p.model));
    if (tag) box.appendChild(el("span", "tag", tag));
    return box;
  }

  bubble(cls, player, text, tag) {
    const box = el("div", "msg " + cls);
    box.appendChild(this.who(player, tag));
    box.appendChild(el("div", "msg-text", text));
    return box;
  }

  line(cls, text, tag) {
    const node = el("div", "line " + cls, text);
    if (tag) node.appendChild(el("span", "tag", tag));
    return node;
  }

  /** Spectator card: header, then a "Says" or "Does" row; reasoning adds a "Thinks" row. */
  card(cls, player, verb, text, tag) {
    const box = el("div", "act " + cls);
    box.appendChild(this.who(player, tag));
    box.appendChild(this.row(verb.toLowerCase(), verb, text));
    box.dataset.thinks = "0";
    return box;
  }

  row(cls, label, text) {
    const row = el("div", "act-row " + cls);
    const lab = el("div", "act-label");
    if (cls === "thinks") lab.appendChild(thinkIcon());
    lab.appendChild(document.createTextNode(label));
    row.append(lab, el("div", "act-text", text));
    return row;
  }

  onEvent(ev, live) {
    if (ev.type === "snapshot_begin" || ev.type === "reset") {
      this.clear();
      this.follow = true;
      return;
    }
    if (ev.type === "snapshot") {
      this.root.scrollTop = this.root.scrollHeight;
      return;
    }
    if (!this.filter(ev)) return;
    if (this.thoughts && this.onAction(ev)) return;
    const g = this.game;
    const name = (id) => g.nameOf(id);
    switch (ev.type) {
      case "phase_started":
        this.append(el("div", "divider " + ev.phase.kind, phaseTitle(ev.phase)));
        break;
      case "spoke":
        this.append(this.bubble("", ev.player, ev.text), ev.player);
        break;
      case "voted":
        this.append(this.line("vote", ev.target === null
          ? name(ev.voter) + " abstained"
          : name(ev.voter) + " voted for " + name(ev.target)), ev.voter);
        break;
      case "shot":
        this.append(this.line("msg sheriff", "Sheriff " + name(ev.sheriff) + " shot " + name(ev.target) + "!"), ev.sheriff);
        break;
      case "died": {
        const how = DEATH_HOW[ev.cause] || " died";
        this.append(el("div", "divider death", name(ev.player) + how + " — " + (ROLE_LABEL[ev.role] || ev.role)));
        break;
      }
      case "peaceful_night":
        this.append(el("div", "divider quiet", "Nobody died last night"));
        break;
      case "no_elimination":
        this.append(el("div", "divider quiet", "No one was eliminated"));
        break;
      case "game_over":
        this.append(el("div", "divider end", ev.winner === "village" ? "Village wins!" : "Werewolves win!"));
        break;
      case "wolf_spoke":
        this.append(this.bubble("wolves", ev.player, ev.text, "wolves only"), ev.player);
        break;
      case "wolf_voted":
        this.append(this.line("msg wolves", ev.target === null
          ? name(ev.voter) + " wants to spare everyone"
          : name(ev.voter) + " wants to kill " + name(ev.target), "wolves only"), ev.voter);
        break;
      case "inspected": {
        const seer = g.holder("seer");
        const who = seer === undefined ? "The seer" : "Seer " + name(seer);
        this.append(this.line("msg seer", who + " inspected " + name(ev.target) + ": "
          + (ev.is_werewolf ? "WEREWOLF" : "not a werewolf"), "private"), seer);
        break;
      }
      case "protected": {
        const doc = g.holder("doctor");
        const who = doc === undefined ? "The doctor" : "Doctor " + name(doc);
        this.append(this.line("msg doctor", who + " protected " + name(ev.target), "private"), doc);
        break;
      }
      case "turn_ended":
        if (ev.timed_out && live) {
          this.append(this.line("timeout", name(ev.player) + " ran out of time (" + (KIND_LABEL[ev.kind] || ev.kind) + ")"));
        }
        break;
    }
  }

  /** Card rendering for actions and reasoning; returns false for events it leaves to onEvent. */
  onAction(ev) {
    const name = (id) => this.game.nameOf(id);
    switch (ev.type) {
      case "spoke":
        this.append(this.card("", ev.player, "Says", ev.text), ev.player);
        return true;
      case "wolf_spoke":
        this.append(this.card("wolves", ev.player, "Says", ev.text, "wolves only"), ev.player);
        return true;
      case "voted":
        this.append(this.card("", ev.voter, "Does",
          ev.target === null ? "Abstains" : "Votes to eliminate " + name(ev.target)), ev.voter);
        return true;
      case "wolf_voted":
        this.append(this.card("wolves", ev.voter, "Does",
          ev.target === null ? "Wants to spare everyone" : "Wants to kill " + name(ev.target), "wolves only"), ev.voter);
        return true;
      case "inspected": {
        const seer = this.game.holder("seer");
        if (seer === undefined) return false;
        this.append(this.card("seer", seer, "Does", "Checks " + name(ev.target) + ": "
          + (ev.is_werewolf ? "WEREWOLF" : "not a werewolf"), "private"), seer);
        return true;
      }
      case "shot":
        this.append(this.card("sheriff", ev.sheriff, "Does", "Shoots " + name(ev.target)), ev.sheriff);
        return true;
      case "protected": {
        const doc = this.game.holder("doctor");
        if (doc === undefined) return false;
        this.append(this.card("doctor", doc, "Does", "Protects " + name(ev.target), "private"), doc);
        return true;
      }
      case "reasoning": {
        const last = this.root.lastElementChild;
        if (last && last.dataset.player === String(ev.player) && last.dataset.thinks === "0") {
          last.appendChild(this.row("thinks", "Thinks", ev.text));
          last.dataset.thinks = "1";
          if (this.follow) this.root.scrollTop = this.root.scrollHeight;
        } else {
          const [cls, tag] = KIND_CONTEXT[ev.kind] || ["", undefined];
          const box = this.card(cls, ev.player, "Does", SILENT_ACTION[ev.kind] || KIND_LABEL[ev.kind] || ev.kind, tag);
          box.appendChild(this.row("thinks", "Thinks", ev.text));
          box.dataset.thinks = "1";
          this.append(box, ev.player);
        }
        return true;
      }
    }
    return false;
  }
}

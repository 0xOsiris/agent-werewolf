// Shared SSE → state reducer. Both pages feed `snapshot` and live events into a Game,
// which updates `game.state` and tells listeners about each event (with `live` false on replay).

export const ROLE_LABEL = {
  werewolf: "Werewolf", seer: "Seer", doctor: "Doctor", sheriff: "Sheriff", cupid: "Cupid", villager: "Villager",
};
export const KIND_LABEL = {
  day_speak: "day speech", day_vote: "day vote", wolf_chat: "wolf chat",
  wolf_kill: "wolf kill", seer_inspect: "seer inspect", doctor_protect: "doctor protect",
  sheriff_shoot: "sheriff shot", cupid_link: "cupid's arrow", dead_chat: "graveyard",
};
export const ROLE_GOAL = {
  werewolf: "Each night, pick a victim with your pack. By day, blend in. You win when wolves equal or outnumber everyone else.",
  seer: "Each night, inspect one player to learn if they are a werewolf. Help the village vote the wolves out.",
  doctor: "Each night, protect one player from the wolves (not the same player two nights in a row).",
  sheriff: "You have one bullet. Each day, before the vote, you may fire it. Hit a wolf and only they die; hit anyone else and you die too. Firing reveals you.",
  cupid: "On the first night, pick two players to fall in love (you may pick yourself). If one lover dies, the other dies of a broken heart.",
  villager: "Find the werewolves and vote them out before they eat the village.",
};

/** Compact read-only rules, e.g. "2 wolves · sheriff · 2 min talk". */
export function rulesLine(s) {
  if (!s) return "";
  const wolves = s.wolves == null ? "auto wolves" : s.wolves + (s.wolves === 1 ? " wolf" : " wolves");
  const secs = Number(s.discussion_secs) || 0;
  const talk = secs % 60 === 0 ? secs / 60 + " min talk" : Math.round(secs) + "s talk";
  return [wolves, s.sheriff && "sheriff", s.cupid && "cupid", talk].filter(Boolean).join(" · ");
}

export function emptyState() {
  return {
    phase: { kind: "lobby" },
    winner: null,
    players: [],
    roles: {},
    pending: {},
    tally: {},
    votes: [],
    wolfVotes: [],
    // Open chat window: { kind: "day_speak" | "wolf_chat", endsAt: performance.now() ms } or null.
    chat: null,
    // Open dead chat, alongside the day's chat; only dead players and spectators see it.
    deadChat: null,
    // Host settings (see /admin/settings), or null before the first snapshot.
    settings: null,
    // Sheriff who fired last, so the shot's deaths can name them.
    lastShot: null,
    // Cupid's pair [a, b] when this viewer saw the link (spectator, Cupid).
    lovers: null,
    // This viewer's own partner when they are a lover: { partner, partner_role }.
    inLove: null,
  };
}

export function phaseTitle(phase, winner) {
  if (phase.kind === "night") return "Night " + phase.n;
  if (phase.kind === "day") return "Day " + phase.n;
  if (phase.kind === "over") {
    if (winner === "village") return "Village wins!";
    if (winner === "wolves") return "Werewolves win!";
    if (winner === "lovers") return "Lovers win!";
    return "Game over";
  }
  return "Lobby";
}

export class Game {
  constructor() {
    this.state = emptyState();
    this.listeners = [];
  }

  /** `fn(event, live)`; `event.type` is the inner event type (spoke, died, snapshot, ...). */
  on(fn) {
    this.listeners.push(fn);
  }

  emit(event, live) {
    for (const fn of this.listeners) {
      try {
        fn(event, live);
      } catch (err) {
        console.error("listener failed on", event.type, err);
      }
    }
  }

  player(id) {
    return this.state.players.find((p) => p.id === id);
  }

  nameOf(id) {
    const p = this.player(id);
    return p ? p.name : "#" + id;
  }

  /** The id holding `role`, if this viewer knows it. */
  holder(role) {
    const id = Object.keys(this.state.roles).find((k) => this.state.roles[k] === role);
    return id === undefined ? undefined : Number(id);
  }

  /**
   * The Lovers this viewer knows, as [a, b]: Cupid's link, `me` and my partner, or the last
   * two standing after a Lovers win. Null otherwise.
   */
  lovers(me) {
    const s = this.state;
    if (s.lovers) return s.lovers;
    if (s.inLove && me !== null) return [me, s.inLove.partner];
    if (s.winner === "lovers") {
      const alive = s.players.filter((p) => p.alive).map((p) => p.id);
      if (alive.length === 2) return alive;
    }
    return null;
  }

  /** Applies one SpectatorEvent JSON object from the server. */
  handle(ev) {
    const s = this.state;
    switch (ev.type) {
      case "snapshot": {
        this.state = emptyState();
        this.state.players = (ev.players || []).map((p) => ({ ...p }));
        this.state.settings = ev.settings || null;
        for (const r of ev.roles || []) this.state.roles[r.player] = r.role;
        this.emit({ type: "snapshot_begin" }, false);
        for (const item of ev.events || []) this.handleFeed(item, false);
        this.state.phase = ev.phase;
        this.state.winner = ev.winner ?? null;
        const now = performance.now();
        if (ev.chat) this.state.chat = { kind: ev.chat.kind, endsAt: now + ev.chat.ends_in_ms };
        if (ev.dead_chat) this.state.deadChat = { kind: ev.dead_chat.kind, endsAt: now + ev.dead_chat.ends_in_ms };
        for (const t of ev.pending || []) {
          this.state.pending[t.player] = { kind: t.kind, deadline: now + t.deadline_ms };
        }
        this.emit({ type: "snapshot" }, false);
        return;
      }
      case "player_joined":
        if (!this.player(ev.player.id)) s.players.push({ ...ev.player });
        this.emit(ev, true);
        return;
      case "roles":
        for (const r of ev.roles) s.roles[r.player] = r.role;
        this.emit(ev, true);
        return;
      case "turn_started":
        s.pending[ev.player] = { kind: ev.kind, deadline: performance.now() + ev.deadline_ms };
        this.emit(ev, true);
        return;
      case "turn_ended":
        delete s.pending[ev.player];
        this.emit(ev, true);
        return;
      case "chat_window": {
        const room = ev.open ? { kind: ev.kind, endsAt: performance.now() + ev.ends_in_ms } : null;
        if (ev.kind === "dead_chat") s.deadChat = room;
        else s.chat = room;
        this.emit(ev, true);
        return;
      }
      case "settings":
        s.settings = ev.settings;
        this.emit(ev, true);
        return;
      case "reset": {
        const settings = s.settings;
        this.state = emptyState();
        this.state.settings = settings;
        this.emit(ev, true);
        return;
      }
      default:
        this.handleFeed(ev, true);
    }
  }

  handleFeed(ev, live) {
    if (ev.type === "public") this.applyPublic(ev.event, live);
    else if (ev.type === "private") this.applyPrivate(ev.event, live);
    else if (ev.type === "reasoning") this.emit(ev, live);
  }

  applyPublic(ev, live) {
    const s = this.state;
    switch (ev.type) {
      case "phase_started":
        s.phase = ev.phase;
        s.chat = null;
        s.deadChat = null;
        s.tally = {};
        s.votes = [];
        s.wolfVotes = [];
        break;
      case "voted":
        s.votes.push({ voter: ev.voter, target: ev.target });
        if (ev.target !== null) s.tally[ev.target] = (s.tally[ev.target] || 0) + 1;
        break;
      case "shot": {
        const p = this.player(ev.sheriff);
        if (p) p.revealed_role = "sheriff";
        s.roles[ev.sheriff] = "sheriff";
        s.lastShot = { sheriff: ev.sheriff, target: ev.target };
        break;
      }
      case "died": {
        const p = this.player(ev.player);
        if (p) {
          p.alive = false;
          p.revealed_role = ev.role;
        }
        s.roles[ev.player] = ev.role;
        delete s.pending[ev.player];
        break;
      }
      case "game_over":
        s.phase = { kind: "over" };
        s.winner = ev.winner;
        s.pending = {};
        s.chat = null;
        s.deadChat = null;
        break;
    }
    this.emit(ev, live);
  }

  applyPrivate(ev, live) {
    const s = this.state;
    switch (ev.type) {
      case "wolf_voted":
        s.wolfVotes.push({ voter: ev.voter, target: ev.target });
        break;
      case "inspected":
        if (ev.is_werewolf) s.roles[ev.target] = s.roles[ev.target] || "werewolf";
        break;
      case "linked":
        s.lovers = [ev.a, ev.b];
        break;
      case "in_love":
        // Spectators get one per lover; only a lover's own stream makes this "mine".
        if (!s.lovers) s.inLove = { partner: ev.partner, partner_role: ev.partner_role };
        s.roles[ev.partner] = s.roles[ev.partner] || ev.partner_role;
        break;
    }
    this.emit(ev, live);
  }
}

/**
 * Opens an EventSource and feeds parsed events to `game`. Reports status via `onStatus(text, bad)`.
 * Returns a handle with `close()`. A refused connection (closed by the browser) calls `onClosed`.
 */
export function connectStream(url, game, { onStatus = () => {}, onClosed = () => {} } = {}) {
  let source = null;
  let stopped = false;
  let retry = 0;
  let timer = null;

  const open = () => {
    if (stopped) return;
    source = new EventSource(url);
    source.onopen = () => {
      retry = 0;
      onStatus("live", false);
    };
    source.onmessage = (msg) => {
      let ev;
      try {
        ev = JSON.parse(msg.data);
      } catch (err) {
        console.error("unparseable event", err);
        return;
      }
      game.handle(ev);
    };
    source.onerror = () => {
      if (source.readyState === EventSource.CLOSED) {
        // The browser gives up on non-200 responses; retry with bounded backoff and jitter.
        source.close();
        onStatus("connection lost, retrying…", true);
        onClosed();
        const delay = Math.min(15000, 1000 * 2 ** retry) * (0.75 + Math.random() * 0.5);
        retry = Math.min(retry + 1, 6);
        timer = setTimeout(open, delay);
      } else {
        onStatus("reconnecting…", true);
      }
    };
  };
  open();
  return {
    close() {
      stopped = true;
      clearTimeout(timer);
      if (source) source.close();
    },
  };
}

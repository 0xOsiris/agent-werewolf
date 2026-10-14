// Shared SSE → state reducer. Both pages feed `snapshot` and live events into a Game,
// which updates `game.state` and tells listeners about each event (with `live` false on replay).

export const ROLE_LABEL = { werewolf: "Werewolf", seer: "Seer", doctor: "Doctor", villager: "Villager" };
export const KIND_LABEL = {
  day_speak: "day speech", day_vote: "day vote", wolf_chat: "wolf chat",
  wolf_kill: "wolf kill", seer_inspect: "seer inspect", doctor_protect: "doctor protect",
};
export const ROLE_GOAL = {
  werewolf: "Each night, pick a victim with your pack. By day, blend in. You win when wolves equal or outnumber everyone else.",
  seer: "Each night, inspect one player to learn if they are a werewolf. Help the village vote the wolves out.",
  doctor: "Each night, protect one player from the wolves (not the same player two nights in a row).",
  villager: "Find the werewolves and vote them out before they eat the village.",
};

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
  };
}

export function phaseTitle(phase, winner) {
  if (phase.kind === "night") return "Night " + phase.n;
  if (phase.kind === "day") return "Day " + phase.n;
  if (phase.kind === "over") {
    if (winner === "village") return "Village wins!";
    if (winner === "wolves") return "Werewolves win!";
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

  /** Applies one SpectatorEvent JSON object from the server. */
  handle(ev) {
    const s = this.state;
    switch (ev.type) {
      case "snapshot": {
        this.state = emptyState();
        this.state.players = (ev.players || []).map((p) => ({ ...p }));
        for (const r of ev.roles || []) this.state.roles[r.player] = r.role;
        this.emit({ type: "snapshot_begin" }, false);
        for (const item of ev.events || []) this.handleFeed(item, false);
        this.state.phase = ev.phase;
        this.state.winner = ev.winner ?? null;
        const now = performance.now();
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
      case "reset":
        this.state = emptyState();
        this.emit(ev, true);
        return;
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
        s.tally = {};
        s.votes = [];
        s.wolfVotes = [];
        break;
      case "voted":
        s.votes.push({ voter: ev.voter, target: ev.target });
        if (ev.target !== null) s.tally[ev.target] = (s.tally[ev.target] || 0) + 1;
        break;
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

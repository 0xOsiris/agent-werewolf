// 2D village renderer shared by both pages. Draws at native pixel resolution (W x H "virtual
// pixels") into a small canvas that CSS scales up crisply; text lives in DOM overlays.

import { playerSprites, SPRITES, ICONS, MINI, pixelText, rng, shade } from "./sprites.js";
import { phaseTitle, ROLE_LABEL } from "./events.js";
import { el } from "./feed.js";

const clamp = (v, lo, hi) => Math.max(lo, Math.min(hi, v));
const BAYER = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];
const SKY_DAY = ["#4fb6e6", "#68c4ec", "#86d2f0", "#a9e0f3"];
const SKY_NIGHT = ["#090d29", "#10173b", "#18214c", "#232d5e"];
const DARK = "#1a1c2c";

function canvas(w, h) {
  const c = document.createElement("canvas");
  c.width = w;
  c.height = h;
  return c;
}

/** Adds a 1px dark outline around every opaque pixel of `src`. */
function outlined(src, color = DARK) {
  const w = src.width;
  const h = src.height;
  const data = src.getContext("2d").getImageData(0, 0, w, h).data;
  const solid = (x, y) => x >= 0 && y >= 0 && x < w && y < h && data[(y * w + x) * 4 + 3] > 0;
  const out = canvas(w, h);
  const ctx = out.getContext("2d");
  ctx.fillStyle = color;
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      if (!solid(x, y) && (solid(x - 1, y) || solid(x + 1, y) || solid(x, y - 1) || solid(x, y + 1))) {
        ctx.fillRect(x, y, 1, 1);
      }
    }
  }
  ctx.drawImage(src, 0, 0);
  return out;
}

/** Bresenham line drawn with a `w`-pixel square brush. */
function pixLine(ctx, x0, y0, x1, y1, w = 1) {
  x0 = Math.round(x0); y0 = Math.round(y0); x1 = Math.round(x1); y1 = Math.round(y1);
  const dx = Math.abs(x1 - x0);
  const dy = -Math.abs(y1 - y0);
  const sx = x0 < x1 ? 1 : -1;
  const sy = y0 < y1 ? 1 : -1;
  let err = dx + dy;
  const off = Math.floor((w - 1) / 2);
  for (let i = 0; i < 2000; i++) {
    ctx.fillRect(x0 - off, y0 - off, w, w);
    if (x0 === x1 && y0 === y1) break;
    const e2 = 2 * err;
    if (e2 >= dy) { err += dy; x0 += sx; }
    if (e2 <= dx) { err += dx; y0 += sy; }
  }
}

function disc(ctx, cx, cy, r, color) {
  ctx.fillStyle = color;
  for (let y = -r; y <= r; y++) {
    const half = Math.floor(Math.sqrt(r * r - y * y) + 0.3);
    ctx.fillRect(cx - half, cy + y, half * 2 + 1, 1);
  }
}

/** Dithered vertical bands. */
function bands(ctx, colors, y0, y1, w) {
  const step = (y1 - y0) / colors.length;
  for (let y = y0; y < y1; y++) {
    const f = (y - y0) / step;
    const i = Math.floor(f);
    const frac = f - i;
    for (let x = 0; x < w; x++) {
      const next = frac * 16 > BAYER[y & 3][x & 3] + 8 && i + 1 < colors.length;
      ctx.fillStyle = colors[next ? i + 1 : i];
      ctx.fillRect(x, y, 1, 1);
    }
  }
}

function cottage(rand, roof, wall) {
  const w = 24 + Math.floor(rand() * 6);
  const wallH = 12 + Math.floor(rand() * 3);
  const roofH = 11;
  const c = canvas(w + 6, roofH + wallH + 6);
  const ctx = c.getContext("2d");
  const ox = 3;
  const base = c.height - 1;
  const wallTop = base - wallH + 1;
  ctx.fillStyle = wall;
  ctx.fillRect(ox, wallTop, w, wallH);
  ctx.fillStyle = shade(wall, 0.84);
  ctx.fillRect(ox + w - 7, wallTop, 7, wallH);
  ctx.fillStyle = "#6b4a34";
  ctx.fillRect(ox, wallTop, w, 1);
  ctx.fillRect(ox, wallTop, 1, wallH);
  ctx.fillRect(ox + w - 1, wallTop, 1, wallH);
  // Door.
  const dx = ox + 3 + Math.floor(rand() * 3);
  ctx.fillStyle = "#6b3e26";
  ctx.fillRect(dx, base - 7, 6, 8);
  ctx.fillStyle = "#8f5a3a";
  ctx.fillRect(dx + 1, base - 6, 4, 7);
  ctx.fillStyle = "#ffd166";
  ctx.fillRect(dx + 4, base - 3, 1, 1);
  // Window (lit at night by the scene).
  const wx = ox + w - 11;
  const wy = base - 10;
  ctx.fillStyle = "#e8e8e8";
  ctx.fillRect(wx - 1, wy - 1, 7, 7);
  ctx.fillStyle = "#9fd8ef";
  ctx.fillRect(wx, wy, 5, 5);
  ctx.fillStyle = "#e8e8e8";
  ctx.fillRect(wx + 2, wy, 1, 5);
  ctx.fillRect(wx, wy + 2, 5, 1);
  // Chimney then roof over it.
  const roofTop = wallTop - roofH;
  ctx.fillStyle = "#8b4a3a";
  ctx.fillRect(ox + w - 9, roofTop + 1, 4, 7);
  ctx.fillStyle = "#5d6482";
  ctx.fillRect(ox + w - 10, roofTop, 6, 2);
  const cx = ox + w / 2;
  for (let r = 0; r < roofH; r++) {
    const half = Math.round((w / 2 + 3) * (r + 1) / roofH);
    ctx.fillStyle = r % 3 === 2 ? shade(roof, 0.78) : roof;
    ctx.fillRect(Math.round(cx - half), roofTop + r, half * 2, 1);
  }
  ctx.fillStyle = shade(roof, 1.3);
  for (let r = 0; r < roofH; r++) {
    const half = Math.round((w / 2 + 3) * (r + 1) / roofH);
    ctx.fillRect(Math.round(cx - half), roofTop + r, 1, 1);
  }
  return { img: outlined(c), window: { x: wx, y: wy }, chimney: { x: ox + w - 7, y: roofTop - 1 } };
}

function tree(rand, pine) {
  const c = canvas(24, 34);
  const ctx = c.getContext("2d");
  ctx.fillStyle = "#6b3e26";
  ctx.fillRect(10, 22, 4, 11);
  ctx.fillStyle = "#4a2a1a";
  ctx.fillRect(13, 22, 1, 11);
  if (pine) {
    for (let tier = 0; tier < 3; tier++) {
      const top = 2 + tier * 7;
      for (let r = 0; r < 10; r++) {
        const half = Math.round(2 + r * (0.55 + tier * 0.12));
        for (let x = -half; x <= half; x++) {
          const t = x / half;
          ctx.fillStyle = t < -0.35 ? "#4a9a5a" : t > 0.45 ? "#1f5c3a" : "#2f7a4a";
          if (rand() < 0.06) ctx.fillStyle = "#265c42";
          ctx.fillRect(12 + x, top + r, 1, 1);
        }
      }
    }
  } else {
    const lumps = [[12, 11, 9], [7, 15, 6], [17, 15, 6], [12, 6, 6]];
    for (let y = 0; y < 24; y++) {
      for (let x = 0; x < 24; x++) {
        let inside = false;
        for (const [lx, ly, lr] of lumps) if ((x - lx) ** 2 + (y - ly) ** 2 <= lr * lr) inside = true;
        if (!inside) continue;
        const light = (-(x - 12) * 0.6 - (y - 11) * 0.8) / 10 + (rand() - 0.5) * 0.35;
        ctx.fillStyle = light > 0.38 ? "#7bd65a" : light > 0 ? "#4fa64a" : light > -0.45 ? "#3e8948" : "#265c42";
        ctx.fillRect(x, y, 1, 1);
      }
    }
  }
  return outlined(c);
}

function moonSprite() {
  const c = canvas(15, 15);
  const ctx = c.getContext("2d");
  disc(ctx, 7, 7, 6, "#f5f0c8");
  ctx.globalCompositeOperation = "destination-out";
  disc(ctx, 10, 5, 5, "#000");
  ctx.globalCompositeOperation = "source-over";
  ctx.fillStyle = "#d8d2a6";
  ctx.fillRect(3, 8, 1, 1);
  ctx.fillRect(4, 10, 2, 1);
  return c;
}

function sunSprite() {
  const c = canvas(21, 21);
  const ctx = c.getContext("2d");
  ctx.fillStyle = "#ffe066";
  for (const [x, y] of [[10, 0], [10, 1], [10, 19], [10, 20], [0, 10], [1, 10], [19, 10], [20, 10],
    [3, 3], [4, 4], [16, 16], [17, 17], [17, 3], [16, 4], [3, 17], [4, 16]]) ctx.fillRect(x, y, 1, 1);
  disc(ctx, 10, 10, 6, "#ffd23f");
  disc(ctx, 9, 9, 3, "#fff0a0");
  return c;
}

function cloudSprite() {
  const c = canvas(30, 12);
  const ctx = c.getContext("2d");
  const puffs = [[8, 7, 4], [14, 5, 5], [21, 7, 4], [25, 8, 3], [4, 8, 3]];
  for (const [x, y, r] of puffs) disc(ctx, x, y, r, "#ffffff");
  ctx.fillStyle = "#dbe8f5";
  ctx.fillRect(2, 10, 26, 1);
  return c;
}

const MOON = moonSprite();
const SUN = sunSprite();

export class Scene {
  /**
   * `root`: sized container. `game`: events.Game. Options: `you()` returns the viewer's id,
   * `onPick(id)` makes candidate sprites tappable.
   */
  constructor(root, game, { you = () => null, onPick = null, chatBanner = false } = {}) {
    this.root = root;
    this.game = game;
    this.you = you;
    this.onPick = onPick;
    this.stage = el("div", "stage");
    this.canvas = document.createElement("canvas");
    this.canvas.className = "stage-canvas";
    this.overlay = el("div", "stage-overlay");
    this.toasts = el("div", "stage-toasts");
    this.chatBar = el("div", "chat-banner");
    this.chatBar.hidden = true;
    this.showChat = chatBanner;
    this.top = el("div", "stage-top");
    this.top.append(this.chatBar, this.toasts);
    this.stage.append(this.canvas, this.overlay, this.top);
    root.appendChild(this.stage);
    this.ctx = this.canvas.getContext("2d");
    this.actors = new Map();
    this.effects = [];
    this.particles = [];
    // Vote arrows outlive their phase: they stay until the result is announced, then fade.
    this.arrows = { day: null, wolf: null };
    this.z = 1;
    this.night = 0;
    this.W = 0;
    this.H = 0;
    this.vp = 1;
    this.candidates = null;
    this.selected = null;
    this.banner = { text: "Lobby", at: -1e9 };
    this.overAt = null;
    this.last = performance.now();
    this.sparkAcc = 0;
    game.on((ev, live) => this.onEvent(ev, live));
    new ResizeObserver(() => this.resize()).observe(root);
    this.resize();
    const loop = (t) => {
      try {
        this.frame(t);
      } catch (err) {
        console.error("scene frame failed", err);
      }
      requestAnimationFrame(loop);
    };
    requestAnimationFrame(loop);
  }

  // ------------------------------------------------------------ layout

  resize() {
    const cw = this.root.clientWidth;
    const ch = this.root.clientHeight;
    if (!cw || !ch) return;
    // Big stages keep chunky sprites; small ones get more virtual pixels so 12 seats fit.
    const H = ch >= 600 ? 176 : Math.round(clamp(ch / 1.8, 150, 200));
    const vp = ch / H;
    const W = clamp(Math.floor(cw / vp), 150, 520);
    if (W === this.W && H === this.H && Math.abs(vp - this.vp) < 0.01) return;
    this.W = W;
    this.H = H;
    this.vp = vp;
    this.canvas.width = W;
    this.canvas.height = H;
    this.stage.style.width = W * vp + "px";
    this.stage.style.height = H * vp + "px";
    this.stage.style.setProperty("--vp", String(vp));
    this.horizon = Math.round(H * 0.3);
    // Seat ellipse fills the stage: back row on the horizon, front-row labels just inside the
    // bottom edge, flank labels (max 34 virtual px wide, see .nametag) just inside the sides.
    const font = clamp(3.3 * vp, 10, 20);
    const labelH = (font * 1.1 * 1.72) / vp;
    const bottom = H - 2 - labelH;
    this.cy = (this.horizon + bottom) / 2;
    this.ry = (bottom - this.horizon) / 2;
    this.rx = W / 2 - 17;
    this.fx = Math.round(W / 2);
    this.fy = Math.round(this.cy - 2);
    this.arc = [0];
    for (let k = 1, prev = this.ellipse(0); k <= 720; k++) {
      const pt = this.ellipse(k / 720);
      this.arc.push(this.arc[k - 1] + Math.hypot(pt.x - prev.x, pt.y - prev.y));
      prev = pt;
    }
    // Small banner glyphs are ~4.5% of the stage height; toasts sit just below it.
    this.bannerScale = clamp(Math.round(Math.max(14, H * vp * 0.045) / (7 * vp)), 1, 3);
    this.top.style.top = (8 + 10 * this.bannerScale) * vp + "px";
    this.buildBackground();
    this.layout(true);
  }

  /** Point on the seat ellipse; `f` runs 0..1 clockwise from the top. */
  ellipse(f) {
    const a = -Math.PI / 2 + 2 * Math.PI * f;
    return { x: this.fx + this.rx * Math.cos(a), y: this.cy + this.ry * Math.sin(a) };
  }

  /** Seats sit at equal distances along the ellipse, so the flanks don't stack up. */
  seat(i, n) {
    const total = this.arc[this.arc.length - 1];
    const want = ((i + 0.5) / Math.max(n, 1)) * total;
    let lo = 0;
    let hi = this.arc.length - 1;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (this.arc[mid] < want) lo = mid + 1;
      else hi = mid;
    }
    return this.ellipse(lo / (this.arc.length - 1));
  }

  layout(snap) {
    const players = this.game.state.players;
    players.forEach((p, i) => {
      const a = this.actors.get(p.id);
      if (!a) return;
      const s = this.seat(i, players.length);
      a.tx = s.x;
      a.ty = s.y;
      if (snap) {
        a.x = s.x;
        a.y = s.y;
      }
    });
  }

  buildBackground() {
    const { W, H, horizon } = this;
    const rand = rng(7 + W);
    this.skyDay = canvas(W, horizon + 12);
    bands(this.skyDay.getContext("2d"), SKY_DAY, 0, horizon + 12, W);
    this.skyNight = canvas(W, horizon + 12);
    bands(this.skyNight.getContext("2d"), SKY_NIGHT, 0, horizon + 12, W);
    this.stars = [];
    for (let i = 0; i < Math.round(W / 4); i++) {
      this.stars.push({ x: Math.floor(rand() * W), y: Math.floor(rand() * (horizon - 4)), p: rand() * 6.28, big: rand() < 0.12 });
    }
    this.clouds = [0, 1, 2].map((i) => ({ img: cloudSprite(), x: rand() * W, y: 6 + i * 9 + rand() * 6, v: 0.002 + rand() * 0.003 }));

    const g = canvas(W, H);
    const ctx = g.getContext("2d");
    // Far hills.
    for (const [col, amp, off, freq] of [["#8fd17a", 6, -6, 0.045], ["#6dbb5a", 4, -1, 0.07]]) {
      ctx.fillStyle = col;
      for (let x = 0; x < W; x++) {
        const top = Math.round(horizon + off - amp * (0.5 + 0.5 * Math.sin(x * freq + off)));
        ctx.fillRect(x, top, 1, H - top);
      }
    }
    bands(ctx, ["#62b14f", "#58a647", "#4f9a43", "#47903e"], horizon + 4, H, W);
    // Clearing around the fire.
    const crx = this.rx * 0.6;
    const cry = this.ry * 0.62;
    for (let y = Math.floor(this.fy - cry - 2); y < this.fy + cry + 6; y++) {
      for (let x = Math.floor(this.fx - crx - 2); x < this.fx + crx + 2; x++) {
        const d = ((x - this.fx) / crx) ** 2 + ((y - this.fy - 2) / cry) ** 2;
        const edge = d + (BAYER[y & 3][x & 3] - 7.5) / 40;
        if (edge > 1) continue;
        ctx.fillStyle = edge > 0.82 ? "#9c7a4c" : (x * 7 + y * 13) % 29 === 0 ? "#8a6a40" : "#b48e5a";
        ctx.fillRect(x, y, 1, 1);
      }
    }
    // Tufts and flowers.
    for (let i = 0; i < (W * H) / 90; i++) {
      const x = Math.floor(rand() * W);
      const y = Math.floor(horizon + 6 + rand() * (H - horizon - 6));
      const d = ((x - this.fx) / (this.rx * 0.6)) ** 2 + ((y - this.fy) / (this.ry * 0.62)) ** 2;
      if (d < 1.1) continue;
      const r = rand();
      if (r < 0.75) {
        ctx.fillStyle = rand() < 0.5 ? "#3e7f36" : "#6cc35a";
        ctx.fillRect(x, y, 1, 1);
        ctx.fillRect(x - 1, y - 1, 1, 1);
        ctx.fillRect(x + 1, y - 1, 1, 1);
      } else {
        ctx.fillStyle = ["#ffffff", "#ffe066", "#ff77a8", "#b07cff"][Math.floor(rand() * 4)];
        ctx.fillRect(x, y, 1, 1);
      }
    }
    // Back props: cottages and trees along the horizon, leaving the middle open.
    const props = canvas(W, H);
    const pctx = props.getContext("2d");
    this.windows = [];
    this.chimneys = [];
    const roofs = ["#b0392b", "#4b5bab", "#a0703c", "#68386c"];
    const walls = ["#efe2c0", "#e4cfa0", "#f2e9dc", "#d9c2a0"];
    const slots = [0.06, 0.22, 0.38, 0.62, 0.78, 0.94];
    const items = [];
    slots.forEach((f, i) => {
      const kind = i === 2 || i === 3 ? "tree" : (i % 2 === 0 ? "cottage" : "tree2");
      items.push({ f, kind, i });
    });
    for (const it of items) {
      const x = Math.round(it.f * W);
      if (it.kind === "cottage" || (it.kind === "tree2" && W > 300 && it.i % 3 === 1)) {
        const c = cottage(rand, roofs[it.i % roofs.length], walls[it.i % walls.length]);
        const left = x - Math.round(c.img.width / 2);
        const top = horizon + 10 - c.img.height;
        pctx.drawImage(c.img, left, top);
        this.windows.push({ x: left + c.window.x, y: top + c.window.y });
        this.chimneys.push({ x: left + c.chimney.x, y: top + c.chimney.y });
      } else {
        const t = tree(rand, rand() < 0.5);
        pctx.drawImage(t, x - 12, horizon + 9 - t.height);
      }
    }
    // Fence along the back of the green.
    const fy = horizon + 13;
    pctx.fillStyle = "#a0703c";
    for (let x = 2; x < W; x += 7) {
      if (Math.abs(x - this.fx) < this.rx * 0.5) continue;
      pctx.fillRect(x, fy - 7, 2, 8);
    }
    pctx.fillStyle = "#c08a50";
    for (let x = 0; x < W; x++) {
      if (Math.abs(x - this.fx) < this.rx * 0.5 - 2) continue;
      pctx.fillRect(x, fy - 5, 1, 1);
      pctx.fillRect(x, fy - 2, 1, 1);
    }
    // Bushes in the front corners.
    for (const bx of [8, W - 8]) {
      const b = canvas(18, 12);
      const bctx = b.getContext("2d");
      disc(bctx, 6, 7, 5, "#3e8948");
      disc(bctx, 12, 7, 5, "#3e8948");
      disc(bctx, 9, 5, 4, "#4fa64a");
      bctx.fillStyle = "#e43b44";
      bctx.fillRect(5, 5, 1, 1);
      bctx.fillRect(12, 4, 1, 1);
      bctx.fillRect(9, 8, 1, 1);
      pctx.drawImage(outlined(b), bx - 9, H - 12);
    }
    ctx.drawImage(outlined(props), 0, 0);
    this.ground = g;

    // Night lighting: dithered darkness with a hole around the fire, plus a warm glow.
    const dark = canvas(W, H);
    const glow = canvas(W, H);
    const dctx = dark.getContext("2d");
    const gctx = glow.getContext("2d");
    const R = Math.max(this.rx, this.ry * 2.2) * 1.05;
    for (let y = 0; y < H; y++) {
      for (let x = 0; x < W; x++) {
        const d = Math.hypot((x - this.fx) / 1.25, (y - this.fy + 8) * 1.3) / R;
        const b = BAYER[y & 3][x & 3] / 16;
        const level = Math.floor(clamp(d, 0, 1.2) * 4 + b);
        const sky = y < horizon ? 0.45 : 1;
        dctx.fillStyle = `rgba(8, 10, 38, ${(Math.min(level, 4) * 0.15 + 0.1) * sky})`;
        dctx.fillRect(x, y, 1, 1);
        const gl = Math.floor((1 - clamp(d * 1.6, 0, 1)) * 3 + b);
        if (gl > 0 && y >= horizon) {
          gctx.fillStyle = `rgba(255, 150, 60, ${gl * 0.07})`;
          gctx.fillRect(x, y, 1, 1);
        }
      }
    }
    this.dark = dark;
    this.glow = glow;
  }

  // ------------------------------------------------------------ actors

  sync(spawn) {
    const players = this.game.state.players;
    const ids = new Set(players.map((p) => p.id));
    for (const [id, a] of this.actors) {
      if (!ids.has(id)) {
        a.label.remove();
        a.stack.remove();
        a.hit.remove();
        this.actors.delete(id);
      }
    }
    const now = performance.now();
    players.forEach((p, i) => {
      let a = this.actors.get(p.id);
      // Each lobby reshuffles outfits; older servers send no look, so fall back to the seat.
      const look = p.look ?? p.id;
      if (a && (a.look !== look || a.name !== p.name)) {
        a.look = look;
        a.name = p.name;
        a.sprites = playerSprites(look, p.name);
      }
      if (!a) {
        const s = this.seat(i, players.length);
        a = {
          id: p.id,
          look,
          name: p.name,
          sprites: playerSprites(look, p.name),
          x: s.x, y: s.y, tx: s.x, ty: s.y,
          phase: (p.id * 1.7) % 6.28,
          spawnAt: spawn ? now : -1e9,
          talkUntil: 0,
          deathAt: null,
          deathCause: null,
          form: "human",
          label: el("div", "nametag"),
          stack: el("div", "bubble-stack"),
          hit: el("button", "hit"),
          speech: null,
          thought: null,
          stackSize: null,
        };
        a.hit.type = "button";
        a.hit.addEventListener("click", () => {
          if (this.onPick && this.candidates && this.candidates.includes(a.id)) this.onPick(a.id);
        });
        this.overlay.append(a.label, a.stack, a.hit);
        this.actors.set(p.id, a);
      }
      this.renderLabel(a, p);
    });
    this.layout(false);
  }

  renderLabel(a, p) {
    const role = this.game.state.roles[p.id] || p.revealed_role;
    const key = [p.name, p.model, p.alive, p.human, role, this.you()].join("|");
    if (a.labelKey === key) return;
    a.labelKey = key;
    const name = el("span", "nt-name", p.name);
    const parts = [name];
    if (p.id === this.you()) parts.push(el("span", "nt-you", "you"));
    // "anthropic/claude-sonnet-4.5" -> "claude-sonnet-4.5"; the feed keeps the full label.
    const sub = el("span", "nt-model", p.human ? "human" : (p.model ? p.model.split("/").pop() : "agent"));
    if (p.human) sub.classList.add("is-human");
    parts.push(sub);
    if (!p.alive && role) parts.push(el("span", "nt-role role-" + role, ROLE_LABEL[role] || role));
    a.label.replaceChildren(...parts);
    a.label.classList.toggle("dead", !p.alive);
    a.label.classList.toggle("me", p.id === this.you());
    a.hit.setAttribute("aria-label", "Choose " + p.name);
  }

  // ------------------------------------------------------------ events

  onEvent(ev, live) {
    const now = performance.now();
    const actor = (id) => this.actors.get(id);
    switch (ev.type) {
      case "snapshot_begin":
        return;
      case "snapshot":
        this.clearTransient();
        this.sync(false);
        this.night = this.nightTarget();
        this.arrowsFromState();
        this.banner = { text: this.bannerText(), at: -1e9 };
        this.overAt = this.game.state.phase.kind === "over" ? now - 5000 : null;
        for (const a of this.actors.values()) a.form = this.wantsWolf(a.id) ? "wolf" : "human";
        this.showLoversWin();
        return;
      case "reset":
        for (const a of this.actors.values()) {
          a.label.remove();
          a.stack.remove();
          a.hit.remove();
        }
        this.actors.clear();
        this.clearTransient();
        this.banner = { text: "Lobby", at: now };
        this.overAt = null;
        this.showLoversWin();
        return;
      case "player_joined":
        this.sync(live);
        return;
      case "roles":
        this.sync(false);
        // Game-over roles name the lovers' roles on the win card.
        if (this.loversCard) this.showLoversWin();
        return;
    }
    if (!live) return;
    switch (ev.type) {
      case "phase_started":
        this.banner = { text: this.bannerText(), at: now };
        if (ev.phase.kind === "night") this.fadeArrows("day", 600);
        this.clearBubbles();
        break;
      case "chat_window":
        // The vote follows; clearing the discussion's bubbles shows who is still voting.
        if (!ev.open && ev.kind === "day_speak") this.clearBubbles();
        break;
      case "spoke":
        this.say(ev.player, ev.text, "speech");
        break;
      case "wolf_spoke":
        this.say(ev.player, ev.text, "wolf");
        break;
      case "reasoning":
        this.say(ev.player, ev.text, "thought");
        break;
      case "voted":
        this.arrowSet("day").votes.push({ voter: ev.voter, target: ev.target, born: now });
        break;
      case "wolf_voted":
        this.arrowSet("wolf").votes.push({ voter: ev.voter, target: ev.target, born: now });
        break;
      case "linked": {
        // Cupid's arrow flies from Cupid (if known) to the first lover, then on to the second.
        const cupid = this.game.holder("cupid");
        this.effects.push({ type: "cupid", id: cupid ?? ev.a, a: ev.a, b: ev.b, start: this.afterBanner(now), dur: 1600 });
        break;
      }
      case "in_love": {
        // A lover's own stream has no `linked`: the arrow drops out of the sky onto the pair.
        const me = this.you();
        if (!this.game.state.lovers && me !== null) {
          this.effects.push({ type: "cupid", id: me, sky: true, a: me, b: ev.partner, start: this.afterBanner(now), dur: 1600 });
        }
        break;
      }
      case "shot":
        // Muzzle flash, tracer, hit flash; the target's death follows as its own event.
        this.effects.push({ type: "shot", id: ev.sheriff, target: ev.target, start: now, dur: 900 });
        this.shotLandsAt = now + 650;
        this.sync(false);
        break;
      case "died": {
        const a = actor(ev.player);
        const name = this.game.nameOf(ev.player);
        const role = ROLE_LABEL[ev.role] || ev.role;
        if (a) {
          // The dead stop talking; their bubbles would also hide the death effects.
          this.retireStack(a, now);
          a.deathCause = ev.cause;
          const delay = { vote: 1500, night: 900, shot: 250, backfire: 700, heartbreak: 1300 }[ev.cause] ?? 900;
          a.deathAt = Math.max(now + delay, ev.cause === "shot" ? this.shotLandsAt || 0 : 0);
          if (ev.cause === "night") this.effects.push({ type: "slash", id: ev.player, start: now, dur: 1100 });
          if (ev.cause === "heartbreak") this.effects.push({ type: "heartbreak", id: ev.player, start: now, dur: 2200 });
          if (ev.cause === "backfire") {
            this.effects.push({ type: "misfire", id: ev.player, start: now, dur: 2000 });
            this.puff(Math.round(a.x), Math.round(a.y) - 12, 16, ["#c0cbdc", "#8a8fa6", "#5d6482"]);
          }
        }
        // Arrows keep pointing at the victim through the slash or the fall, then fade.
        if (ev.cause === "night") this.fadeArrows("wolf", 1800);
        else if (ev.cause === "vote") this.fadeArrows("day", 2600);
        this.sync(false);
        if (ev.cause === "shot") {
          const shot = this.game.state.lastShot;
          const sheriff = shot ? this.game.nameOf(shot.sheriff) : "the sheriff";
          this.toast("Sheriff " + sheriff + " shot " + name + " — "
            + (ev.role === "werewolf" ? "Werewolf!" : role + ". The sheriff dies too."), "death");
        } else if (ev.cause === "heartbreak") {
          this.toast(name + " died of a broken heart — " + role, "love");
        } else if (ev.role === "jester" && ev.cause === "vote") {
          this.toast(name + " was voted out — the Jester wins!", "love");
        } else if (ev.cause !== "backfire") {
          const how = ev.cause === "night" ? " was killed in the night" : " was eliminated";
          this.toast(name + how + " — " + role, "death");
        }
        break;
      }
      case "peaceful_night": {
        this.toast("Nobody died last night", "quiet");
        const saved = this.wolfTarget();
        if (saved !== null) this.effects.push({ type: "shield", id: saved, start: now, dur: 2600 });
        this.fadeArrows("wolf", 1800);
        break;
      }
      case "no_elimination":
        this.toast("No one was eliminated", "quiet");
        this.fadeArrows("day", 1500);
        break;
      case "game_over":
        for (const a of this.actors.values()) this.retireStack(a, now);
        this.overAt = now;
        this.banner = { text: this.bannerText(), at: now };
        this.sync(false);
        this.showLoversWin();
        break;
      case "inspected":
        this.effects.push({ type: "inspect", id: ev.target, wolf: ev.is_werewolf, start: now, dur: 3200 });
        break;
      case "protected":
        this.effects.push({ type: "shield", id: ev.target, start: now, dur: 2600 });
        break;
    }
  }

  /** `now`, or when the big phase banner leaves the middle of the stage if it is still up. */
  afterBanner(now) {
    return Math.max(now, this.banner.at + 1700);
  }

  clearBubbles() {
    for (const a of this.actors.values()) {
      if (!a.speech && !a.thought) continue;
      a.speech = null;
      a.thought = null;
      this.renderStack(a);
    }
  }

  clearTransient() {
    this.effects = [];
    this.arrows = { day: null, wolf: null };
    for (const a of this.actors.values()) {
      a.speech = null;
      a.thought = null;
      a.stack.replaceChildren();
      a.stackSize = null;
      a.deathAt = null;
    }
    this.toasts.replaceChildren();
  }

  /** The open arrow set for `kind`, starting a fresh one if the last is fading. */
  arrowSet(kind) {
    const set = this.arrows[kind];
    if (set && set.fadeAt === null) return set;
    return (this.arrows[kind] = { votes: [], fadeAt: null });
  }

  fadeArrows(kind, delay) {
    const set = this.arrows[kind];
    if (set && set.fadeAt === null) set.fadeAt = performance.now() + delay;
  }

  arrowsFromState() {
    const s = this.game.state;
    const set = (votes) => ({ votes: votes.map((v) => ({ ...v, born: -1e9 })), fadeAt: null });
    this.arrows = {
      day: s.phase.kind === "day" && s.votes.length ? set(s.votes) : null,
      wolf: s.phase.kind === "night" && s.wolfVotes.length ? set(s.wolfVotes) : null,
    };
  }

  /** The player most wolves pointed at tonight, or null. */
  wolfTarget() {
    const set = this.arrows.wolf;
    if (!set) return null;
    const counts = new Map();
    for (const v of set.votes) if (v.target !== null) counts.set(v.target, (counts.get(v.target) || 0) + 1);
    let best = null;
    for (const [id, n] of counts) if (best === null || n > counts.get(best)) best = id;
    return best;
  }

  bannerText() {
    const s = this.game.state;
    return phaseTitle(s.phase, s.winner);
  }

  nightTarget() {
    const s = this.game.state;
    if (s.phase.kind === "night") return 1;
    if (s.phase.kind === "over") return s.winner === "wolves" ? 1 : 0;
    return 0;
  }

  wantsWolf(id) {
    const s = this.game.state;
    const p = this.game.player(id);
    if (!p || !p.alive || s.roles[id] !== "werewolf") return false;
    return s.phase.kind === "over" || this.night > 0.5;
  }

  /** Shows a speech, wolf-chat, or thought bubble over a player. */
  say(id, text, kind) {
    const a = this.actors.get(id);
    if (!a) return;
    const now = performance.now();
    const speech = kind !== "thought";
    // Speech stays until newer speech pushes it out (see makeRoom) or the phase changes.
    const until = speech ? Infinity : now + clamp(2600 + text.length * 55, 3500, 11000);
    // One thought on stage at a time. A new speech also clears thoughts, since its own
    // reasoning follows right after it.
    for (const b of this.actors.values()) {
      if (b.thought && !b.thought.leaving && (speech || b !== a)) this.retire(b.thought, now);
    }
    if (speech) {
      const node = el("div", kind === "wolf" ? "speech wolf" : "speech");
      if (kind === "wolf") node.appendChild(el("div", "bubble-tag", "wolves only"));
      node.appendChild(el("div", "bubble-text", text));
      a.speech = { node, until, at: now };
      a.talkUntil = now + clamp(text.length * 45, 1200, 4500);
    } else {
      const role = this.game.state.roles[id];
      const node = el("div", "thought role-" + (role || "unknown"));
      node.appendChild(el("div", "thought-head", this.game.nameOf(id) + " thinks"));
      node.appendChild(el("div", "bubble-text", clip(text, 140)));
      a.thought = { node, until };
    }
    a.stack.style.zIndex = String(++this.z);
    this.renderStack(a);
    this.makeRoom(a, now);
  }

  /**
   * Open chat lets several players talk at once: keep up to three speech bubbles, dropping any
   * older bubble the new one would cover, then the oldest beyond three.
   */
  makeRoom(a, now) {
    a.stack.hidden = false;
    a.stackSize = { w: a.stack.offsetWidth, h: a.stack.offsetHeight };
    const mine = this.stackRect(a);
    const others = [...this.actors.values()]
      .filter((b) => b !== a && b.speech && !b.speech.leaving)
      .sort((p, q) => p.speech.at - q.speech.at);
    const kept = [];
    for (const b of others) {
      const r = this.stackRect(b);
      const hit = r && r.left < mine.right && mine.left < r.right && r.top < mine.bottom && mine.top < r.bottom;
      if (hit) this.retireStack(b, now);
      else kept.push(b);
    }
    for (const b of kept.slice(0, Math.max(0, kept.length - 2))) this.retireStack(b, now);
  }

  retireStack(b, now) {
    if (b.speech) this.retire(b.speech, now);
    if (b.thought) this.retire(b.thought, now);
  }

  /** Where `a`'s bubble stack sits in stage CSS px (bottom-centre anchored above the head). */
  stackRect(a) {
    if (!a.stackSize) return null;
    const vp = this.vp;
    const { w, h } = a.stackSize;
    const cx = clamp(a.x * vp, w / 2 + 4, this.W * vp - w / 2 - 4);
    const bottom = Math.max((a.y - 27) * vp, h + 4);
    return { left: cx - w / 2, right: cx + w / 2, top: bottom - h, bottom, cx };
  }

  retire(bubble, now) {
    bubble.leaving = true;
    bubble.node.classList.add("leaving");
    bubble.until = Math.min(bubble.until, now + 200);
  }

  renderStack(a) {
    const kids = [];
    if (a.thought) kids.push(a.thought.node);
    if (a.speech) kids.push(a.speech.node);
    a.stack.replaceChildren(...kids);
    a.stackSize = null;
  }

  toast(text, cls) {
    const node = el("div", "toast " + cls, text);
    this.toasts.appendChild(node);
    while (this.toasts.children.length > 3) this.toasts.firstChild.remove();
    setTimeout(() => node.classList.add("fade"), 4500);
    setTimeout(() => node.remove(), 5200);
  }

  /** Chat countdown (where enabled), or the day vote's progress on every page. */
  renderChatBar(vote) {
    const chat = this.showChat ? this.game.state.chat : null;
    let text = "";
    if (chat) {
      text = (chat.kind === "wolf_chat" ? "Wolf chat · " : "Open discussion · ")
        + clock(chat.endsAt - performance.now()) + " left";
    } else if (vote) {
      text = "Vote · " + vote.done.size + " of " + vote.total + " voted";
    }
    this.chatBar.hidden = !text;
    if (!text) return;
    if (this.chatBar.textContent !== text) this.chatBar.textContent = text;
    this.chatBar.classList.toggle("wolf", !!chat && chat.kind === "wolf_chat");
  }

  /** Candidate ids to highlight (null clears), plus the currently selected id. */
  setPicking(candidates, selected) {
    this.candidates = candidates;
    this.selected = selected;
  }

  // ------------------------------------------------------------ drawing

  frame(t) {
    if (!this.W) {
      this.resize();
      if (!this.W) return;
    }
    const dt = clamp(t - this.last, 0, 100);
    this.last = t;
    const ctx = this.ctx;
    const { W, H } = this;
    const s = this.game.state;
    ctx.imageSmoothingEnabled = false;
    this.night += (this.nightTarget() - this.night) * Math.min(1, dt / 500);
    const night = this.night;

    // Sky, celestial bodies, clouds.
    ctx.globalAlpha = 1;
    ctx.drawImage(this.skyDay, 0, 0);
    if (night > 0.01) {
      ctx.globalAlpha = night;
      ctx.drawImage(this.skyNight, 0, 0);
      for (const st of this.stars) {
        const tw = 0.45 + 0.55 * Math.sin(t * 0.0021 + st.p * 3);
        ctx.globalAlpha = night * clamp(tw, 0, 1);
        ctx.fillStyle = "#ffffff";
        ctx.fillRect(st.x, st.y, 1, 1);
        if (st.big && tw > 0.6) {
          ctx.fillStyle = "#c7d2ff";
          ctx.fillRect(st.x - 1, st.y, 1, 1);
          ctx.fillRect(st.x + 1, st.y, 1, 1);
          ctx.fillRect(st.x, st.y - 1, 1, 1);
          ctx.fillRect(st.x, st.y + 1, 1, 1);
        }
      }
    }
    ctx.globalAlpha = 1;
    const sunY = Math.round(6 + night * (this.horizon + 10));
    ctx.drawImage(SUN, Math.round(W * 0.82) - 10, sunY);
    const moonY = Math.round(5 + (1 - night) * (this.horizon + 10));
    if (s.phase.kind === "over" && s.winner === "wolves") ctx.filter = "hue-rotate(-50deg) saturate(3)";
    ctx.drawImage(MOON, Math.round(W * 0.16) - 7, moonY);
    ctx.filter = "none";
    if (night < 0.99) {
      ctx.globalAlpha = 1 - night;
      for (const c of this.clouds) {
        c.x += dt * c.v;
        if (c.x > W + 10) c.x = -40;
        ctx.drawImage(c.img, Math.round(c.x), Math.round(c.y));
      }
      ctx.globalAlpha = 1;
    }
    ctx.drawImage(this.ground, 0, 0);

    // Actors, back to front.
    const players = s.players;
    for (const a of this.actors.values()) {
      a.x += (a.tx - a.x) * Math.min(1, dt / 250);
      a.y += (a.ty - a.y) * Math.min(1, dt / 250);
    }
    const order = [...this.actors.values()].sort((p, q) => p.y - q.y);
    for (const a of order) this.drawActor(a, t, this.game.player(a.id));

    // Night falls over everything drawn so far; lights stay bright.
    if (night > 0.01) {
      ctx.globalAlpha = night;
      ctx.drawImage(this.dark, 0, 0);
      const flick = 0.8 + 0.2 * Math.sin(t * 0.013) * Math.sin(t * 0.007);
      ctx.globalAlpha = night * flick;
      ctx.drawImage(this.glow, 0, 0);
      ctx.globalAlpha = night;
      for (const w of this.windows) {
        ctx.fillStyle = "#ffd166";
        ctx.fillRect(w.x, w.y, 5, 5);
        ctx.fillStyle = "#ffb347";
        ctx.fillRect(w.x + 2, w.y, 1, 5);
        ctx.fillRect(w.x, w.y + 2, 5, 1);
      }
      ctx.globalAlpha = 1;
    }
    const fire = SPRITES.fire[Math.floor(t / 110) % SPRITES.fire.length];
    ctx.drawImage(fire, this.fx - 10, this.fy - 18);
    this.updateParticles(dt, t);

    this.drawLovers(t);
    this.drawVotes(t);
    const vote = this.game.voteProgress();
    for (const a of order) this.drawActorHud(a, t, this.game.player(a.id), vote);
    this.drawEffects(t);
    this.drawBanner(t);
    const top = Math.round(this.topAt * this.vp) + "px";
    if (this.top.style.top !== top) this.top.style.top = top;
    this.placeDom(players);
    this.renderChatBar(vote);
  }

  drawActor(a, t, p) {
    if (!p) return;
    const ctx = this.ctx;
    const x = Math.round(a.x);
    let y = Math.round(a.y);
    const now = performance.now();
    // Drop in when joining the lobby.
    const sp = (now - a.spawnAt) / 600;
    if (sp < 1) y -= Math.round((1 - sp) * (1 - sp) * 50);

    const dying = !p.alive && a.deathAt !== null && now < a.deathAt + 1000;
    if (!p.alive && !dying) {
      this.drawShadow(x, y, 8);
      ctx.drawImage(SPRITES.tomb, x - 8, y - 22);
      const rise = a.deathAt ? clamp((now - a.deathAt - 1000) / 1200, 0, 1) : 1;
      ctx.globalAlpha = 0.55 * rise;
      const gy = y - 22 - Math.round(4 + 8 * rise + Math.sin(t * 0.003 + a.phase) * 1.5);
      ctx.drawImage(Math.floor(t / 400 + a.phase) % 2 ? SPRITES.ghost : SPRITES.ghost2, x - 8, gy);
      ctx.globalAlpha = 1;
      return;
    }

    // Candidate ring for the player's own picks.
    if (this.candidates && this.candidates.includes(a.id) && p.alive) {
      const sel = this.selected === a.id;
      const pulse = sel || Math.floor(t / 300) % 2 === 0;
      ctx.fillStyle = sel ? "#ffd23f" : pulse ? "#fff3b0" : "#ffd23f";
      this.ring(x, y, sel ? 10 : 9, 3);
    }
    if (dying && a.deathCause === "vote" && now < a.deathAt) {
      ctx.fillStyle = Math.floor(t / 150) % 2 ? "#ff4d5e" : "#ffd23f";
      this.ring(x, y, 11, 4);
    }

    const want = this.wantsWolf(a.id) ? "wolf" : "human";
    if (want !== a.form && p.alive) {
      a.form = want;
      this.puff(x, y - 11, 14, ["#e8ecff", "#b9c1e6", "#8a8fa6"]);
    }
    const talking = now < a.talkUntil;
    let img;
    if (a.form === "wolf") {
      img = talking && Math.floor(t / 130) % 2 ? a.sprites.wolfTalk
        : (t + a.phase * 500) % 1600 < 220 ? a.sprites.wolfGlow : a.sprites.wolf;
    } else {
      // Sheriffs wear the hat and star wherever their role is known (spectators, or once revealed).
      const sheriff = (this.game.state.roles[a.id] || p.revealed_role) === "sheriff";
      const idle = sheriff ? a.sprites.sheriffIdle : a.sprites.idle;
      if (talking) img = Math.floor(t / 140) % 2 ? (sheriff ? a.sprites.sheriffTalk : a.sprites.talk) : idle;
      else img = (t + a.phase * 997) % 4200 < 150 ? (sheriff ? a.sprites.sheriffBlink : a.sprites.blink) : idle;
    }
    const bob = !dying && Math.sin(t * 0.0035 + a.phase) > 0.35 ? 1 : 0;
    this.drawShadow(x, y, a.form === "wolf" ? 10 : 8);
    if (dying) {
      const k = clamp((now - a.deathAt) / 450, 0, 1);
      if (now < a.deathAt) {
        ctx.drawImage(img, x - 8, y - 22 + bob);
      } else if (k < 1 || now < a.deathAt + 700) {
        if (!a.puffed && k >= 1) {
          a.puffed = true;
          this.puff(x, y - 4, 18, ["#e8ecff", "#b9c1e6", "#8a8fa6"]);
        }
        ctx.save();
        ctx.translate(x, y);
        ctx.rotate((k * Math.PI) / 2);
        ctx.globalAlpha = 1 - clamp((now - a.deathAt - 450) / 250, 0, 1);
        ctx.drawImage(img, -8, -22);
        ctx.restore();
        ctx.globalAlpha = 1;
      }
      return;
    }
    a.puffed = false;
    ctx.drawImage(img, x - 8, y - 22 + bob);
  }

  drawShadow(x, y, half) {
    const ctx = this.ctx;
    ctx.fillStyle = "rgba(20, 30, 20, 0.28)";
    ctx.fillRect(x - half + 2, y + 1, half * 2 - 4, 1);
    ctx.fillRect(x - half, y, half * 2, 1);
    ctx.fillRect(x - half + 2, y - 1, half * 2 - 4, 1);
  }

  /** Pixel ellipse ring around a player's feet in the current fillStyle. */
  ring(x, y, rx, ry) {
    const ctx = this.ctx;
    for (let i = 0; i < 48; i++) {
      const a = (i / 48) * Math.PI * 2;
      ctx.fillRect(Math.round(x + Math.cos(a) * rx), Math.round(y + Math.sin(a) * ry), 1, 1);
    }
  }

  drawActorHud(a, t, p, vote) {
    if (!p) return;
    const ctx = this.ctx;
    const s = this.game.state;
    const x = Math.round(a.x);
    const y = Math.round(a.y);
    const now = performance.now();
    if (now - a.spawnAt < 600) return;
    const role = s.roles[a.id] || p.revealed_role;
    if (!p.alive) return;
    if (role && MINI[role]) {
      const icon = MINI[role];
      ctx.drawImage(icon, x - 6 - icon.width + 1, y - 25);
    }
    if (p.human) ctx.drawImage(SPRITES.heart, x + 7, y - 24);
    if (a.id === this.you()) {
      ctx.drawImage(SPRITES.you, x - 4, y - 32 + (Math.floor(t / 350) % 2));
    }
    const pending = s.pending[a.id];
    if (pending && !a.speech) {
      const bx = x + 2;
      const by = y - 33;
      ctx.drawImage(SPRITES.think, bx, by);
      const k = Math.floor(t / 250) % 4;
      ctx.fillStyle = DARK;
      for (let i = 0; i < 3; i++) if (i < k) ctx.fillRect(bx + 3 + i * 3, by + 3, 2, 2);
    } else if (vote && vote.done.has(a.id)) {
      ctx.drawImage(SPRITES.voted, x + 3, y - 33);
    }
    const votes = s.phase.kind === "day" ? s.tally[a.id] : 0;
    if (votes) {
      const txt = pixelText(String(votes), "#ffffff", "#a22633");
      const w = txt.width + 2;
      const bx = x - Math.round(w / 2);
      const by = y - 36;
      ctx.fillStyle = DARK;
      ctx.fillRect(bx - 1, by - 1, w + 2, 12);
      ctx.fillRect(x - 1, by + 11, 3, 1);
      ctx.fillStyle = "#e0505e";
      ctx.fillRect(bx, by, w, 10);
      ctx.drawImage(txt, bx + 1, by);
    }
  }

  drawVotes(t) {
    const now = performance.now();
    for (const kind of ["day", "wolf"]) {
      const set = this.arrows[kind];
      if (!set) continue;
      let alpha = 1;
      if (set.fadeAt !== null && now > set.fadeAt) {
        alpha = 1 - (now - set.fadeAt) / 1000;
        if (alpha <= 0) {
          this.arrows[kind] = null;
          continue;
        }
      }
      set.votes.forEach((v, i) => {
        const recent = i === set.votes.length - 1;
        this.drawArrow(v, kind === "wolf", alpha * (recent || kind === "wolf" ? 1 : 0.75), now, t);
      });
    }
    this.ctx.globalAlpha = 1;
  }

  /** Voter-to-target arrow; kill arrows are thicker, red, and pulse. */
  drawArrow(v, kill, alpha, now, t) {
    if (v.target === null) return;
    const from = this.actors.get(v.voter);
    const to = this.actors.get(v.target);
    if (!from || !to) return;
    const ctx = this.ctx;
    const k = clamp((now - v.born) / 350, 0, 1);
    const x0 = from.x;
    const y0 = from.y - 12;
    const len = Math.hypot(to.x - x0, to.y - 12 - y0);
    if (len < 1) return;
    const ux = (to.x - x0) / len;
    const uy = (to.y - 12 - y0) / len;
    const end = Math.max(0, len - 9) * k;
    const sx = x0 + ux * 7;
    const sy = y0 + uy * 7;
    const x1 = sx + ux * Math.max(0, end - 7);
    const y1 = sy + uy * Math.max(0, end - 7);
    const w = kill ? 2 : 1;
    const head = (dy, width) => {
      if (k < 1) return;
      for (const ang of [2.5, -2.5]) {
        const hx = x1 + (ux * Math.cos(ang) - uy * Math.sin(ang)) * (kill ? 6 : 4);
        const hy = y1 + (ux * Math.sin(ang) + uy * Math.cos(ang)) * (kill ? 6 : 4);
        pixLine(ctx, x1, y1 + dy, hx, hy + dy, width);
      }
    };
    if (kill) {
      ctx.globalAlpha = alpha * (0.3 + 0.25 * Math.sin(t * 0.008));
      ctx.fillStyle = "#ff4d5e";
      pixLine(ctx, sx, sy, x1, y1, 5);
      head(0, 5);
    }
    ctx.globalAlpha = alpha;
    ctx.fillStyle = DARK;
    pixLine(ctx, sx, sy + 1, x1, y1 + 1, w);
    head(1, w);
    ctx.fillStyle = kill ? "#ff4d5e" : "#ffd23f";
    pixLine(ctx, sx, sy, x1, y1, w);
    head(0, w);
    ctx.globalAlpha = 1;
  }

  drawEffects(t) {
    const ctx = this.ctx;
    const now = performance.now();
    this.effects = this.effects.filter((e) => now - e.start < e.dur);
    for (const e of this.effects) {
      const a = this.actors.get(e.id);
      if (!a) continue;
      const k = (now - e.start) / e.dur;
      const x = Math.round(a.x);
      const y = Math.round(a.y);
      if (e.type === "shot") {
        this.drawShot(e, a, now);
      } else if (e.type === "cupid") {
        this.drawCupidArrow(e, a, now);
      } else if (e.type === "heartbreak") {
        // The heart pops up over the partner, then splits apart.
        const img = SPRITES.brokenHeart;
        const half = Math.floor(img.width / 2);
        const gap = Math.round(clamp((k - 0.25) / 0.5, 0, 1) * 4);
        const top = this.labelTop(a, y - 38 - Math.round(k * 4));
        ctx.globalAlpha = k > 0.8 ? (1 - k) / 0.2 : 1;
        ctx.drawImage(img, 0, 0, half, img.height, x - half - gap, top + gap, half, img.height);
        ctx.drawImage(img, half, 0, img.width - half, img.height, x + gap, top, img.width - half, img.height);
        ctx.globalAlpha = 1;
      } else if (e.type === "misfire") {
        ctx.globalAlpha = k > 0.8 ? (1 - k) / 0.2 : 1;
        const txt = pixelText("MISFIRE!", "#ff8a3d");
        const top = this.labelTop(a, y - 40 - Math.round(k * 6));
        ctx.drawImage(txt, clamp(x - Math.round(txt.width / 2), 1, this.W - txt.width - 1), top);
        ctx.globalAlpha = 1;
      } else if (e.type === "slash") {
        ctx.globalAlpha = k < 0.6 ? 1 : 1 - (k - 0.6) / 0.4;
        for (let i = 0; i < 3; i++) {
          const p = clamp(k * 4 - i * 0.5, 0, 1);
          if (p <= 0) continue;
          const sx = x - 7 + i * 4;
          const sy = y - 24;
          ctx.fillStyle = DARK;
          pixLine(ctx, sx + 1, sy, sx + 1 + 8 * p, sy + 18 * p);
          ctx.fillStyle = i === 1 ? "#ffffff" : "#ff4d5e";
          pixLine(ctx, sx, sy, sx + 8 * p, sy + 18 * p);
        }
        ctx.globalAlpha = 1;
      } else if (e.type === "shield") {
        ctx.globalAlpha = k > 0.8 ? (1 - k) / 0.2 : 1;
        ctx.drawImage(SPRITES.shield, x - 4, y - 36 - Math.round(k * 4));
        if (Math.random() < 0.3) this.particles.push(spark(x + (Math.random() - 0.5) * 16, y - 6 - Math.random() * 16, "#3ddc97"));
        ctx.globalAlpha = 1;
      } else if (e.type === "inspect") {
        ctx.globalAlpha = k > 0.85 ? (1 - k) / 0.15 : 1;
        const top = this.labelTop(a, y - 50);
        const ball = ICONS.seer;
        ctx.drawImage(ball, x - 5, top + 10);
        const txt = pixelText(e.wolf ? "WOLF!" : "NOT WOLF", e.wolf ? "#ff4d5e" : "#3ddc97");
        ctx.drawImage(txt, clamp(x - Math.round(txt.width / 2), 1, this.W - txt.width - 1), top);
        ctx.globalAlpha = 1;
      }
    }
  }

  /** The "Lovers win!" card over the clearing: both lovers' portraits with a heart between. */
  showLoversWin() {
    if (this.loversCard) this.loversCard.remove();
    this.loversCard = null;
    const s = this.game.state;
    const pair = s.phase.kind === "over" && s.winner === "lovers" ? this.game.lovers(this.you()) : null;
    if (!pair) return;
    const card = el("div", "lovers-win");
    const portrait = (id) => {
      const box = el("div", "lw-lover");
      const a = this.actors.get(id);
      const role = s.roles[id];
      if (a) box.appendChild(copyCanvas(role === "werewolf" ? a.sprites.wolf : a.sprites.idle));
      box.appendChild(el("div", "lw-name", this.game.nameOf(id)));
      if (role) box.appendChild(el("div", "nt-role role-" + role, ROLE_LABEL[role] || role));
      return box;
    };
    const heart = el("div", "lw-heart");
    heart.appendChild(copyCanvas(SPRITES.loveHeart));
    card.append(portrait(pair[0]), heart, portrait(pair[1]));
    this.stage.appendChild(card);
    this.loversCard = card;
  }

  /**
   * Top of a ~10px label that wants to sit at `want` above actor `a`: pushed below the phase
   * banner and any toasts so back-row labels stay readable. When even the actor's chest is under
   * the toasts (back row on phones), it goes under their name tag instead.
   */
  labelTop(a, want) {
    const y = Math.round(a.y);
    // `topAt` is just under the banner that is up, the big game-over one included.
    let floor = (this.topAt ?? 8 + 10 * this.bannerScale) - 3;
    if (this.top.offsetHeight && (this.toasts.childElementCount || !this.chatBar.hidden)) {
      floor = Math.max(floor, (this.top.offsetTop + this.top.offsetHeight) / this.vp + 1);
    }
    if (want >= floor) return Math.round(want);
    if (floor <= y - 14) return Math.round(floor);
    return Math.round(Math.min(y + 3 + a.label.offsetHeight / this.vp, this.H - 11));
  }

  /** A pulsing pink dotted link with a heart between living lovers; it fades as one dies. */
  drawLovers(t) {
    const pair = this.game.lovers(this.you());
    if (!pair) return;
    const [a, b] = pair.map((id) => this.actors.get(id));
    const [pa, pb] = pair.map((id) => this.game.player(id));
    if (!a || !b || !pa || !pb) return;
    const now = performance.now();
    // Wait for Cupid's arrow to land before the link appears.
    if (this.effects.some((e) => e.type === "cupid" && now - e.start < 1300)) return;
    let fade = 1;
    if (!pa.alive || !pb.alive) {
      const broken = this.effects.find((e) => e.type === "heartbreak" && pair.includes(e.id));
      if (!broken) return;
      fade = clamp(1 - (now - broken.start) / 900, 0, 1);
      if (fade <= 0) return;
    }
    const ctx = this.ctx;
    const x0 = Math.round(a.x);
    const y0 = Math.round(a.y - 12);
    const x1 = Math.round(b.x);
    const y1 = Math.round(b.y - 12);
    // 2px dashes flow from a to b, over a dark shadow so they read on grass and at night.
    const len = Math.hypot(x1 - x0, y1 - y0);
    const beat = t % 900 < 140;
    ctx.globalAlpha = (beat ? 1 : 0.8) * fade;
    for (let d = (t / 80) % 5; d < len; d += 5) {
      const px = Math.round(x0 + ((x1 - x0) * d) / len);
      const py = Math.round(y0 + ((y1 - y0) * d) / len);
      ctx.fillStyle = DARK;
      ctx.fillRect(px, py + 1, 2, 2);
      ctx.fillStyle = beat ? "#ffb3d1" : "#ff77a8";
      ctx.fillRect(px, py, 2, 2);
    }
    ctx.globalAlpha = fade;
    const heart = beat ? SPRITES.loveHeartBig : SPRITES.loveHeart;
    const bob = Math.round(Math.sin(t * 0.006) * 1.5);
    ctx.drawImage(heart, Math.round((x0 + x1) / 2 - heart.width / 2), Math.round((y0 + y1) / 2 - heart.height / 2) + bob);
    ctx.globalAlpha = 1;
  }

  /**
   * Cupid's arrow: a gold shaft with a heart tip, lobbed from Cupid (or, for a lover who doesn't
   * know Cupid, out of the sky) to the first lover, then on to the second, trailing sparkles.
   */
  drawCupidArrow(e, a, now) {
    const la = this.actors.get(e.a);
    const lb = this.actors.get(e.b);
    if (!la || !lb) return;
    const ms = now - e.start;
    const from = e.sky ? { x: (la.x + lb.x) / 2, y: 14 } : a;
    const legs = [[from, la, 0, 600], [la, lb, 650, 1250]];
    const ctx = this.ctx;
    for (const [src, dst, t0, t1] of legs) {
      if (ms < t0 || ms > t1 + 300) continue;
      const p = clamp((ms - t0) / (t1 - t0), 0, 1);
      const x0 = src.x;
      const y0 = src.y - 14;
      const x1 = dst.x;
      const y1 = dst.y - 12;
      // A gentle arc so the arrow reads as lobbed rather than fired.
      const lift = Math.min(24, Math.hypot(x1 - x0, y1 - y0) * 0.25);
      const at = (q) => ({ x: x0 + (x1 - x0) * q, y: y0 + (y1 - y0) * q - Math.sin(q * Math.PI) * lift });
      if (p < 1) {
        const head = at(p);
        const prev = at(Math.max(0, p - 0.02));
        const d = Math.hypot(head.x - prev.x, head.y - prev.y) || 1;
        const ux = (head.x - prev.x) / d;
        const uy = (head.y - prev.y) / d;
        const tx = head.x - ux * 11;
        const ty = head.y - uy * 11;
        ctx.fillStyle = DARK;
        pixLine(ctx, tx, ty + 1, head.x, head.y + 1, 2);
        ctx.fillStyle = "#ffd23f";
        pixLine(ctx, tx, ty, head.x, head.y, 2);
        ctx.fillStyle = "#ffffff";
        for (const side of [1, -1]) pixLine(ctx, tx, ty, tx - ux * 3 - uy * 3 * side, ty - uy * 3 + ux * 3 * side);
        const tip = SPRITES.loveHeart;
        ctx.drawImage(tip, Math.round(head.x - tip.width / 2), Math.round(head.y - tip.height / 2));
        if (Math.random() < 0.7) this.particles.push(spark(tx, ty, Math.random() < 0.5 ? "#ff77a8" : "#ffe0ee"));
      } else if (!e["hit" + t0]) {
        e["hit" + t0] = true;
        this.puff(Math.round(x1), Math.round(y1), 16, ["#ff77a8", "#ffb3d1", "#ffffff"]);
      }
    }
  }

  /** Sheriff's shot: flash at the gun, a tracer to the target, then a hit burst. */
  drawShot(e, a, now) {
    const b = this.actors.get(e.target);
    if (!b) return;
    const ctx = this.ctx;
    const ms = now - e.start;
    const dir = b.x >= a.x ? 1 : -1;
    const x0 = Math.round(a.x + dir * 8);
    const y0 = Math.round(a.y - 11);
    const x1 = Math.round(b.x);
    const y1 = Math.round(b.y - 12);
    if (ms < 180) {
      const r = ms < 90 ? 4 : 2;
      ctx.fillStyle = "#fff3b0";
      ctx.fillRect(x0 - r, y0, r * 2 + 1, 1);
      ctx.fillRect(x0, y0 - r, 1, r * 2 + 1);
      ctx.fillStyle = "#ffcd3c";
      for (const [dx, dy] of [[-2, -2], [2, -2], [-2, 2], [2, 2]]) ctx.fillRect(x0 + dx, y0 + dy, 1, 1);
      ctx.fillStyle = "#ffffff";
      ctx.fillRect(x0 - 1, y0 - 1, 3, 3);
    }
    if (ms >= 80 && ms < 520) {
      const p = clamp((ms - 80) / 360, 0, 1);
      const tail = Math.max(0, p - 0.3);
      ctx.fillStyle = "#ffcd3c";
      pixLine(ctx, x0 + (x1 - x0) * tail, y0 + (y1 - y0) * tail, x0 + (x1 - x0) * p, y0 + (y1 - y0) * p, 2);
      ctx.fillStyle = "#ffffff";
      pixLine(ctx, x0 + (x1 - x0) * Math.max(tail, p - 0.08), y0 + (y1 - y0) * Math.max(tail, p - 0.08),
        x0 + (x1 - x0) * p, y0 + (y1 - y0) * p);
    }
    if (ms >= 440 && ms < 900) {
      const q = (ms - 440) / 460;
      const r = Math.round(2 + q * 8);
      ctx.globalAlpha = 1 - q;
      ctx.fillStyle = q < 0.3 ? "#ffffff" : "#ff4d5e";
      this.ring(x1, y1, r, r);
      ctx.fillRect(x1 - 1, y1 - 1, 3, 3);
      ctx.globalAlpha = 1;
      if (!e.burst) {
        e.burst = true;
        this.puff(x1, y1, 14, ["#ffffff", "#ff4d5e", "#ffcd3c"]);
      }
    }
  }

  puff(x, y, n, colors) {
    for (let i = 0; i < n; i++) {
      const ang = Math.random() * Math.PI * 2;
      const sp = 0.01 + Math.random() * 0.03;
      this.particles.push({
        x, y, vx: Math.cos(ang) * sp, vy: Math.sin(ang) * sp * 0.6 - 0.008,
        life: 0, max: 500 + Math.random() * 400, color: colors[i % colors.length], size: Math.random() < 0.4 ? 2 : 1,
      });
    }
  }

  updateParticles(dt, t) {
    const ctx = this.ctx;
    const s = this.game.state;
    // Campfire sparks, livelier at night.
    this.sparkAcc += dt * (0.006 + this.night * 0.012);
    while (this.sparkAcc > 1) {
      this.sparkAcc -= 1;
      this.particles.push({
        x: this.fx + (Math.random() - 0.5) * 6, y: this.fy - 12, vx: (Math.random() - 0.5) * 0.01,
        vy: -0.02 - Math.random() * 0.02, life: 0, max: 700 + Math.random() * 900,
        color: Math.random() < 0.5 ? "#ffcd3c" : "#f77622", size: 1, wobble: Math.random() * 6,
      });
    }
    if (Math.random() < dt * 0.004 && this.chimneys.length) {
      const c = this.chimneys[Math.floor(Math.random() * this.chimneys.length)];
      this.particles.push({ x: c.x, y: c.y, vx: 0.006, vy: -0.008, life: 0, max: 2200, color: "#c0cbdc", size: 2, smoke: true });
    }
    const party = s.winner === "village" || s.winner === "lovers";
    if (s.phase.kind === "over" && party && this.overAt && performance.now() - this.overAt < 6000 && Math.random() < dt * 0.05) {
      const colors = s.winner === "lovers" ? ["#ff77a8", "#ffb3d1", "#ff4d6d", "#ffffff"]
        : ["#ffd23f", "#e43b44", "#3ddc97", "#0099db", "#ff77a8"];
      this.particles.push({
        x: Math.random() * this.W, y: -2, vx: (Math.random() - 0.5) * 0.02, vy: 0.03 + Math.random() * 0.03,
        life: 0, max: 5000, color: colors[Math.floor(Math.random() * colors.length)], size: 2,
      });
    }
    this.particles = this.particles.filter((p) => (p.life += dt) < p.max);
    if (this.particles.length > 400) this.particles.splice(0, this.particles.length - 400);
    for (const p of this.particles) {
      p.x += p.vx * dt + (p.wobble !== undefined ? Math.sin(t * 0.01 + p.wobble) * 0.08 : 0);
      p.y += p.vy * dt;
      ctx.globalAlpha = p.smoke ? 0.5 * (1 - p.life / p.max) : clamp(1.3 - p.life / p.max, 0, 1);
      ctx.fillStyle = p.color;
      ctx.fillRect(Math.round(p.x), Math.round(p.y), p.size, p.size);
    }
    ctx.globalAlpha = 1;
  }

  drawBanner(t) {
    const ctx = this.ctx;
    const now = performance.now();
    const s = this.game.state;
    const over = s.phase.kind === "over";
    const text = this.banner.text;
    const winColor = { wolves: "#ff4d5e", lovers: "#ff77a8", jester: "#b55088" }[s.winner] || "#ffd23f";
    const color = over ? winColor : s.phase.kind === "night" ? "#c7d2ff" : "#ffffff";
    const img = pixelText(text, color);
    const fit = (want) => Math.max(1, Math.min(want, Math.floor((this.W - 6) / img.width)));
    const age = now - this.banner.at;
    const small = this.bannerScale;
    // Toasts sit just under whichever banner is up, so they never cover the result.
    this.topAt = 8 + 10 * small;
    if (over) {
      const sc = fit(Math.round(small * 2.5));
      const k = clamp(age / 500, 0, 1);
      const y = Math.round(this.H * 0.05 - (1 - k) * 20);
      this.topAt = Math.max(this.topAt, Math.round(this.H * 0.05) + img.height * sc + 2);
      ctx.globalAlpha = k;
      ctx.drawImage(img, Math.round((this.W - img.width * sc) / 2), y, img.width * sc, img.height * sc);
      ctx.globalAlpha = 1;
      return;
    }
    if (age < 1900) {
      const k = age < 300 ? age / 300 : age > 1500 ? 1 - (age - 1500) / 400 : 1;
      const sc = fit(small * 2);
      ctx.globalAlpha = clamp(k, 0, 1);
      ctx.drawImage(img, Math.round((this.W - img.width * sc) / 2), Math.round(this.H * 0.4 - (img.height * sc) / 2),
        img.width * sc, img.height * sc);
      ctx.globalAlpha = 1;
      return;
    }
    const sc = fit(small);
    ctx.drawImage(img, Math.round((this.W - img.width * sc) / 2), 3, img.width * sc, img.height * sc);
  }

  // ------------------------------------------------------------ DOM overlay

  placeDom(players) {
    const vp = this.vp;
    const now = performance.now();
    for (const p of players) {
      const a = this.actors.get(p.id);
      if (!a) continue;
      this.renderLabel(a, p);
      const x = a.x * vp;
      const y = a.y * vp;
      setPos(a.label, x, y + 2 * vp);
      // Expire bubbles.
      let changed = false;
      if (a.speech && now > a.speech.until) { a.speech = null; changed = true; }
      if (a.thought && now > a.thought.until) { a.thought = null; changed = true; }
      if (changed) this.renderStack(a);
      if (a.speech || a.thought) {
        // Unhide before measuring: a display:none stack measures 0x0.
        a.stack.hidden = false;
        if (!a.stackSize) a.stackSize = { w: a.stack.offsetWidth, h: a.stack.offsetHeight };
        const r = this.stackRect(a);
        setPos(a.stack, r.cx, r.bottom);
        a.stack.style.setProperty("--tail", Math.round(clamp(x - r.left, 14, a.stackSize.w - 14)) + "px");
      } else if (!a.stack.hidden) {
        a.stack.hidden = true;
      }
      const pick = this.candidates && this.candidates.includes(p.id) && p.alive;
      a.hit.hidden = !pick;
      if (pick) {
        a.hit.style.left = (a.x - 10) * vp + "px";
        a.hit.style.top = (a.y - 26) * vp + "px";
        a.hit.style.width = 20 * vp + "px";
        a.hit.style.height = 30 * vp + "px";
        a.hit.classList.toggle("selected", this.selected === p.id);
      }
    }
  }
}

/** "1:42" for a millisecond countdown. */
export function clock(ms) {
  const secs = Math.max(0, Math.ceil(ms / 1000));
  return Math.floor(secs / 60) + ":" + String(secs % 60).padStart(2, "0");
}

/** Shortens to about `max` characters at a word boundary; the feed keeps the full text. */
function clip(text, max) {
  const chars = [...text];
  if (chars.length <= max) return text;
  const cut = chars.slice(0, max).join("");
  const space = cut.lastIndexOf(" ");
  return (space > max * 0.6 ? cut.slice(0, space) : cut).trimEnd() + "…";
}

function setPos(node, x, y) {
  const v = Math.round(x) + "," + Math.round(y);
  if (node.dataset.pos === v) return;
  node.dataset.pos = v;
  node.style.left = Math.round(x) + "px";
  node.style.top = Math.round(y) + "px";
}

/** A same-size copy of a sprite canvas, for DOM use; CSS scales it up crisply. */
function copyCanvas(src) {
  const c = document.createElement("canvas");
  c.width = src.width;
  c.height = src.height;
  c.getContext("2d").drawImage(src, 0, 0);
  c.className = "pixel";
  return c;
}

function spark(x, y, color) {
  return { x, y, vx: 0, vy: -0.015, life: 0, max: 700, color, size: 1 };
}


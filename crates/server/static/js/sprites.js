// Pixel art authored as run-length rows: "3.8s3." means 3 transparent, 8 's', 3 transparent.
// Each sprite is built once into an offscreen canvas at 1 canvas pixel per art pixel.

const CHAR_W = 14;
const CHAR_H = 22;
const OUTLINE = "#1a1c2c";

/** Expands one run-length row ("3.8s3.") into a plain string. */
function rle(row) {
  let out = "";
  let count = "";
  for (const ch of row) {
    if (ch >= "0" && ch <= "9") {
      count += ch;
    } else {
      out += ch.repeat(count ? Number(count) : 1);
      count = "";
    }
  }
  return out;
}

/** A map of {row index: rle row} or an array of rle rows, expanded to `h` rows of width `w`. */
function grid(rows, w, h) {
  const out = [];
  for (let y = 0; y < h; y++) {
    const src = Array.isArray(rows) ? rows[y] : rows[y];
    const line = src === undefined ? ".".repeat(w) : rle(src);
    if (line.length !== w) console.warn("sprite row width", line.length, "!=", w, src);
    out.push(line.padEnd(w, ".").slice(0, w));
  }
  return out;
}

function hexToRgb(hex) {
  const n = parseInt(hex.slice(1), 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

function rgbToHex([r, g, b]) {
  const c = (v) => Math.max(0, Math.min(255, Math.round(v))).toString(16).padStart(2, "0");
  return "#" + c(r) + c(g) + c(b);
}

/** Darkens (k < 1) or lightens (k > 1) a hex colour. */
export function shade(hex, k) {
  const [r, g, b] = hexToRgb(hex);
  if (k <= 1) return rgbToHex([r * k, g * k, b * k * 1.04]);
  return rgbToHex([r + (255 - r) * (k - 1), g + (255 - g) * (k - 1), b + (255 - b) * (k - 1)]);
}

/** Lowercase palette keys get an automatic uppercase shadow unless one is given. */
function withShades(palette) {
  const out = { ...palette };
  for (const [k, v] of Object.entries(palette)) {
    const up = k.toUpperCase();
    if (up !== k && out[up] === undefined) out[up] = shade(v, 0.72);
  }
  return out;
}

function canvas(w, h) {
  const c = document.createElement("canvas");
  c.width = w;
  c.height = h;
  return c;
}

/**
 * Paints layers (arrays of equal-sized strings) with `palette` and adds a 1px outline.
 * Returns a canvas 2px larger than the art in each dimension.
 */
export function buildSprite(layers, palette, outline = OUTLINE) {
  const h = layers[0].length;
  const w = layers[0][0].length;
  const pal = withShades(palette);
  const px = Array.from({ length: h }, () => new Array(w).fill(null));
  for (const layer of layers) {
    for (let y = 0; y < h; y++) {
      for (let x = 0; x < w; x++) {
        const ch = layer[y][x];
        if (ch === "." || ch === undefined) continue;
        if (ch === "_") { px[y][x] = null; continue; }
        const col = pal[ch];
        if (col) px[y][x] = col;
      }
    }
  }
  const c = canvas(w + 2, h + 2);
  const ctx = c.getContext("2d");
  const solid = (x, y) => y >= 0 && y < h && x >= 0 && x < w && px[y][x] !== null;
  if (outline) {
    ctx.fillStyle = outline;
    for (let y = -1; y <= h; y++) {
      for (let x = -1; x <= w; x++) {
        if (solid(x, y)) continue;
        if (solid(x - 1, y) || solid(x + 1, y) || solid(x, y - 1) || solid(x, y + 1)) {
          ctx.fillRect(x + 1, y + 1, 1, 1);
        }
      }
    }
  }
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      if (px[y][x] === null) continue;
      ctx.fillStyle = px[y][x];
      ctx.fillRect(x + 1, y + 1, 1, 1);
    }
  }
  return c;
}

// ---------------------------------------------------------------- villagers

const BODY = grid({
  3: "3.8s3.",
  4: "2.10s2.", 5: "2.10s2.", 6: "2.10s2.", 7: "2.10s2.", 8: "2.10s2.",
  9: "2.sr6srs2.",
  10: "2.10s2.",
  11: "3.8s3.",
  12: "3.3c2s3c3.",
  13: "2.10c2.",
  14: "2.C8cC2.",
  15: "2.C8cC2.",
  16: "2.s8ls2.",
  17: "3.8p3.",
  18: "3.3p2.3p3.",
  19: "4.2p2.2p4.",
  20: "4.2b2.2b4.",
  21: "3.3b2.3b3.",
}, CHAR_W, CHAR_H);

const FACES = {
  idle: grid({ 7: "4.e4.e4.", 8: "4.e4.e4.", 10: "6.2m6." }, CHAR_W, CHAR_H),
  blink: grid({ 8: "4.e4.e4.", 10: "6.2m6." }, CHAR_W, CHAR_H),
  talk: grid({ 7: "4.e4.e4.", 8: "4.e4.e4.", 10: "5.4m5.", 11: "6.2M6." }, CHAR_W, CHAR_H),
};

const HAIR = [
  { 2: "3.8h3.", 3: "2.10h2.", 4: "2.10h2.", 5: "2.2h6.2h2.", 6: "2.h8.h2." }, // short
  { 2: "3.8h3.", 3: "2.10h2.", 4: "2.10h2.", 5: "2.4h4.2h2.", 6: "2.h8.h2." }, // side part
  { 2: "3.8h3.", 3: "2.10h2.", 4: "1.12h1.", 5: "1.4h4.4h1.", 6: "1.3h6.3h1.",
    7: "1.2h8.2h1.", 8: "1.2h8.2h1.", 9: "1.2h8.2h1.", 10: "1.2h8.2h1.", 11: "1.2h8.2h1.",
    12: "1.2h8.2h1.", 13: "1.2h8.2h1." }, // long
  { 1: "3.h.h2.h.h3.", 2: "2.10h2.", 3: "2.10h2.", 4: "2.10h2.", 5: "2.h.h4.h.h2." }, // spiky
  { 3: "3.8h3.", 4: "2.h8.h2." }, // buzz
  { 0: "5.4h5.", 1: "5.4h5.", 2: "3.8h3.", 3: "2.10h2.", 4: "2.10h2.", 5: "2.2h6.2h2." }, // bun
  { 0: "4.6h4.", 1: "2.10h2.", 2: "1.12h1.", 3: "1.12h1.", 4: "1.12h1.", 5: "1.3h6.3h1.",
    6: "1.2h8.2h1.", 7: "1.2h8.2h1." }, // curly
].map((rows) => grid(rows, CHAR_W, CHAR_H));

// `clear`: hair rows above this index are hidden under the accessory. `over`: drawn after the face.
const ACCESSORIES = [
  { name: "none", rows: {} },
  { name: "hat", clear: 3, rows: { 0: "4.6a4.", 1: "4.6a4.", 2: "4.6A4.", 3: "1.12a1." } },
  { name: "beanie", clear: 5, rows: { 0: "6.2b6.", 1: "4.6a4.", 2: "3.8a3.", 3: "2.10a2.", 4: "2.10A2." } },
  { name: "hood", clear: 5, rows: { 1: "4.6a4.", 2: "3.8a3.", 3: "2.10a2.", 4: "1.12a1.",
    5: "1.3a6.3a1.", 6: "1.2a8.2a1.", 7: "1.2a8.2a1.", 8: "1.2a8.2a1.", 9: "1.2a8.2a1.",
    10: "1.2a8.2a1.", 11: "1.2a8.2a1.", 12: "1.3a6.3a1.", 13: "1.2a8.2a1." } },
  { name: "beard", rows: { 9: "2.h8.h2.", 10: "2.3h4.3h2.", 11: "2.10h2.", 12: "3.8h3.", 13: "4.6h4.", 14: "5.4h5." } },
  { name: "glasses", over: true, rows: { 6: "3.3g2.3g3.", 7: "3.g.4g.g3.", 8: "3.g.g2.g.g3.", 9: "3.3g2.3g3." } },
  { name: "bow", rows: { 1: "8.2a.2a1.", 2: "8.2aA2a1.", 3: "8.2a.2a1." } },
  { name: "scarf", rows: { 12: "3.8a3.", 13: "3.8a3.", 14: "4.2a8.", 15: "4.2a8." } },
].map((a) => ({ ...a, grid: grid(a.rows, CHAR_W, CHAR_H) }));

// Sheriff look: wide-brim cowboy hat and a tin star on the chest.
const COWBOY_HAT = grid({ 0: "4.6k4.", 1: "4.6k4.", 2: "k3.6K3.k", 3: "14k" }, CHAR_W, CHAR_H);
const TIN_STAR = grid({ 13: "5.y8.", 14: "4.3y7.", 15: "5.y8." }, CHAR_W, CHAR_H);
const KEEPS_UNDER_HAT = new Set(["none", "beard", "glasses", "scarf"]);

const SKINS = ["#ffd6b0", "#f1c27d", "#d9a066", "#b5774a", "#8d5524", "#ffe2cc"];
const HAIRS = ["#3b2416", "#6b3e26", "#e8c547", "#b0392b", "#2b2b33", "#d4d4e0", "#7b4fa8"];
const SHIRTS = [
  "#e43b44", "#3e8948", "#0099db", "#feae34", "#b55088", "#2ce8f5",
  "#f77622", "#63c74d", "#8b5cf6", "#ffe066", "#4b5bab", "#e8e8e8",
];
const PANTS = ["#3a4466", "#5a3a28", "#262b44", "#5b6070"];
const ACC_COLORS = ["#c0cbdc", "#e4a672", "#ff77a8", "#5a6988", "#68386c", "#ffd166", "#2ce8f5", "#a22633"];
const FURS = ["#6b6d7e", "#7a5a44", "#4a4b59", "#8b8580"];

function hash(n) {
  let x = (n + 0x9e3779b9) | 0;
  x = Math.imul(x ^ (x >>> 16), 0x85ebca6b);
  x = Math.imul(x ^ (x >>> 13), 0xc2b2ae35);
  return (x ^ (x >>> 16)) >>> 0;
}

/** Deterministic look for a player's `look` seed; consecutive seeds get different shirts. */
export function variantFor(id) {
  const h = hash(id);
  return {
    skin: SKINS[(h >>> 3) % SKINS.length],
    hair: HAIRS[(id * 3 + (h % 3)) % HAIRS.length],
    hairStyle: (id * 5 + 2) % HAIR.length,
    shirt: SHIRTS[id % SHIRTS.length],
    pants: PANTS[(h >>> 7) % PANTS.length],
    accessory: ACCESSORIES[(id * 3 + 1) % ACCESSORIES.length],
    accColor: ACC_COLORS[(h >>> 11) % ACC_COLORS.length],
    fur: FURS[(h >>> 5) % FURS.length],
  };
}

const WOLF = {
  0: "2.f8.f2.", 1: "2.2f6.2f2.", 2: "2.fFf4.fFf2.", 3: "2.fF6fFf2.",
  4: "1.12f1.", 5: "1.12f1.", 6: "1.2f2y4f2y2f1.", 7: "1.4f4l4f1.",
  8: "2.2f2l2n2l2f2.", 9: "2.f8lf2.", 10: "3.fw4mwf3.", 11: "4.6f4.",
  12: "2.2f6c2f2.", 13: "1.3f2c2l2c3f1.", 14: "1.2f.2c2l2c.2f1.", 15: "1.2f.6c.2f1.",
  16: "1.2k.cCcCcC.2k1.", 17: "4.6p4.", 18: "4.2p2.2p4.", 19: "4.2f2.2f4.",
  20: "3.3f2.3f3.", 21: "3.k.k2.k.k3.",
};

/** Every frame a player can show, for a player's `look` seed. */
export function playerSprites(id) {
  const v = variantFor(id);
  const acc = v.accessory;
  let hair = HAIR[v.hairStyle];
  if (acc.clear) hair = hair.map((row, y) => (y < acc.clear ? ".".repeat(CHAR_W) : row));
  const pal = {
    s: v.skin, r: shade(v.skin, 0.86), e: "#1a1c2c", m: "#7a3040", M: "#c2405a",
    h: v.hair, c: v.shirt, l: "#5a3a28", L: "#5a3a28", p: v.pants, b: "#3e2731",
    a: v.accColor, A: shade(v.accColor, 0.65), g: "#262b44", G: "#9fd8ef",
  };
  const under = acc.over ? [] : [acc.grid];
  const over = acc.over ? [acc.grid] : [];
  const human = (face) => buildSprite([BODY, hair, ...under, face, ...over], pal);
  const wolfPal = {
    f: v.fur, l: shade(v.fur, 1.45), y: "#ffd23f", Y: "#fff6a0", n: "#1a1c2c", w: "#f4f4f4",
    m: "#6b1020", k: "#e8e8e8", c: v.shirt, p: v.pants,
  };
  const wolfIdle = grid(WOLF, CHAR_W, CHAR_H);
  const wolfGlow = grid({ ...WOLF, 6: "1.2f2Y4f2Y2f1." }, CHAR_W, CHAR_H);
  const wolfTalk = grid({ ...WOLF, 11: "4.f4mf4." }, CHAR_W, CHAR_H);
  const capped = HAIR[v.hairStyle].map((row, y) => (y < 3 ? ".".repeat(CHAR_W) : row));
  const keep = KEEPS_UNDER_HAT.has(acc.name) ? [acc.grid] : [];
  const sheriffPal = { ...pal, k: "#8f5a3a", K: "#3e2731", y: "#ffd23f" };
  const sheriff = (face) => buildSprite([BODY, capped, ...(acc.over ? [] : keep), COWBOY_HAT, TIN_STAR, face,
    ...(acc.over ? keep : [])], sheriffPal);
  return {
    idle: human(FACES.idle),
    blink: human(FACES.blink),
    talk: human(FACES.talk),
    sheriffIdle: sheriff(FACES.idle),
    sheriffBlink: sheriff(FACES.blink),
    sheriffTalk: sheriff(FACES.talk),
    wolf: buildSprite([wolfIdle], wolfPal),
    wolfGlow: buildSprite([wolfGlow], wolfPal),
    wolfTalk: buildSprite([wolfTalk], wolfPal),
  };
}

// ---------------------------------------------------------------- shared sprites

const GHOST = { 4: "4.6w4.", 5: "3.8w3.", 6: "2.10w2.", 7: "2.2w2e2w2e2w2.", 8: "2.2w2e2w2e2w2.",
  9: "2.10w2.", 10: "2.4w2m4w2.", 11: "1.12w1.", 12: "1.12w1.", 13: "2.10w2.", 14: "2.10w2.",
  15: "2.10w2.", 16: "2.w.2w.2w.2w2." };
const GHOST_PAL = { w: "#e8ecff", e: "#3b4170", m: "#3b4170" };

const TOMB = { 10: "4.6g4.", 11: "3.8g3.", 12: "2.4g2G4g2.", 13: "2.2g6G2g2.", 14: "2.4g2G4g2.",
  15: "2.4g2G4g2.", 16: "2.9gG2.", 17: "2.9gG2.", 18: "2.m8gG2.", 19: "2.2m7gG2.", 20: "1.12d1.", 21: "14d" };
const TOMB_PAL = { g: "#9aa1bd", G: "#5d6482", m: "#5a8f3e", d: "#6b4a34" };

export const ICONS = {};
/** 7x7 role badges drawn next to sprites in the scene. */
export const MINI = {};
export const SPRITES = {};

function buildShared() {
  SPRITES.ghost = buildSprite([grid(GHOST, CHAR_W, CHAR_H)], GHOST_PAL, "#6f78a8");
  SPRITES.ghost2 = buildSprite([grid({ ...GHOST, 16: "2.2w.2w.2w.w2." }, CHAR_W, CHAR_H)], GHOST_PAL, "#6f78a8");
  SPRITES.tomb = buildSprite([grid(TOMB, CHAR_W, CHAR_H)], TOMB_PAL);
  SPRITES.heart = buildSprite([grid(["1.2h.2h1.", "hw5h", "7h", "1.5h1.", "2.3h2.", "3.h3."], 7, 6)],
    { h: "#ff4d6d", w: "#ffd0da" });
  SPRITES.loveHeart = buildSprite([grid(["1.2h.2h1.", "hw5h", "7h", "1.5h1.", "2.3h2.", "3.h3."], 7, 6)],
    { h: "#ff77a8", w: "#ffe0ee" }, "#7a1f4a");
  SPRITES.loveHeartBig = buildSprite([grid(["1.3h1.3h1.", "2hw6h", "hw7h", "9h", "1.7h1.", "2.5h2.", "3.3h3.", "4.h4."], 9, 8)],
    { h: "#ff77a8", w: "#ffe0ee" }, "#7a1f4a");
  SPRITES.brokenHeart = buildSprite([grid(["1.2h3.2h1.", "4h.4h", "3h.5h", "1.3h.3h1.", "2.2h.2h2.", "3.h.h3."], 9, 6)],
    { h: "#ff77a8" }, "#7a1f4a");
  SPRITES.you = buildSprite([grid(["7y", "1.5y1.", "2.3y2.", "3.y3."], 7, 4)], { y: "#ffd23f" });
  SPRITES.think = buildSprite([grid(["1.9w1.", "11w", "11w", "11w", "1.9w1.", "2.w8.", "1.w9."], 11, 7)],
    { w: "#ffffff" });
  SPRITES.cloud = buildSprite([grid(["2.3w1.3w2.", "1.9w1.", "11w", "11w", "1.9w1.", "3.5w3.", "1.2w8.", "w10."], 11, 8)],
    { w: "#ffffff" }, "#4a3a7a");
  SPRITES.shield = buildSprite([grid(["7g", "g2Gw2Gg", "g5wg", "g2Gw2Gg", ".gGwGg.", "2.gwg2.", "3.g3."], 7, 7)],
    { g: "#1f9e6a", G: "#3ddc97", w: "#ffffff" });
  MINI.werewolf = buildSprite([grid(["g5.g", "2g3.2g", "7g", "gy3gyg", "7g", ".gLnLg.", "2.3g2."], 7, 7)],
    { g: "#e0505e", y: "#ffd23f", L: "#ffd9dd", n: "#1a1c2c" });
  MINI.seer = buildSprite([grid([".5p.", "pw5p", "pw4pP", "5p2P", ".4pP.", "2.3p2.", ".5b."], 7, 7)],
    { p: "#b07cff", P: "#7a4fd0", w: "#f2e9ff", b: "#a0703c" });
  MINI.doctor = buildSprite([grid(["2.3r2.", "2.3r2.", "7r", "7r", "7r", "2.3r2.", "2.3r2."], 7, 7)],
    { r: "#3ddc97" });
  MINI.sheriff = buildSprite([grid(["3.y3.", "2.3y2.", "7y", ".2yY2y.", ".5y.", ".2y.2y.", "y5.y"], 7, 7)],
    { y: "#ffd23f", Y: "#b8860b" });
  MINI.cupid = buildSprite([grid(["2.2h.2h2.", "1.7h1.", "2a5h2a", "2.5h2.", "3.3h3.", "4.h4."], 9, 6)],
    { h: "#ff77a8", a: "#ffd23f" });
  MINI.villager = buildSprite([grid(["m.m.m", "m.m.m", "5m", "2.w2.", "2.w2.", "2.w2.", "2.w2."], 5, 7)],
    { m: "#d6dae8", w: "#c08040" });
  ICONS.werewolf = buildSprite([grid(["g9.g", "2g7.2g", "11g", "11g", "2gy5gy2g", "11g",
    ".3g3L3g.", "2.2gLnL2g2.", "3.g3Lg3.", "4.3g4."], 11, 10)],
  { g: "#e0505e", y: "#ffd23f", L: "#ffd9dd", n: "#1a1c2c" });
  ICONS.seer = buildSprite([grid(["2.5p2.", ".pw5p.", "pw6pP", "8pP", "7p2P", ".5p2P.", "2.5p2.", ".7b.", "9B"], 9, 9)],
    { p: "#b07cff", P: "#7a4fd0", w: "#f2e9ff", b: "#a0703c", B: "#6b4a2a" });
  ICONS.doctor = buildSprite([grid(["3.3r3.", "3.3r3.", "3.3r3.", "9r", "9r", "9r", "3.3r3.", "3.3r3.", "3.3r3."], 9, 9)],
    { r: "#3ddc97", R: "#1f9e6a" });
  ICONS.sheriff = buildSprite([grid(["5.y5.", "4.3y4.", "4.3y4.", "11y", "1.9y1.", "2.3yY3y2.", "2.7y2.",
    "2.7y2.", "1.3y3.3y1.", "1.2y5.2y1.", "y9.y"], 11, 11)],
  { y: "#ffd23f", Y: "#b8860b" });
  // A bow with a heart-tipped arrow nocked on its string.
  ICONS.cupid = buildSprite([grid(["..bbs........", ".b..s........", ".b..s..hh.hh.", "b...s.hwhhhhh", "b.f.s.hhhhhhh",
    "b.aaaaahhhhh.", "b.f.s...hhh..", "b...s....h...", ".b..s........", ".b..s........", "..bbs........"], 13, 11)],
  { b: "#c08040", s: "#f2e9dc", a: "#ffd23f", f: "#ffffff", h: "#ff77a8", w: "#ffe0ee" });
  ICONS.villager = buildSprite([grid(["m2.m2.m", "m2.m2.m", "m2.m2.m", "7m", "3.w3.", "3.w3.", "3.w3.",
    "3.w3.", "3.w3.", "3.w3.", "3.w3."], 7, 11)],
  { m: "#d6dae8", w: "#c08040" });
}

// ---------------------------------------------------------------- campfire

/** Seeded PRNG (mulberry32). */
export function rng(seed) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** Three flickering campfire frames, 20x22 each, base centred at (10, 20). */
function buildFire() {
  const frames = [];
  for (let f = 0; f < 3; f++) {
    const c = canvas(20, 22);
    const ctx = c.getContext("2d");
    const rand = rng(101 + f * 7);
    // Flames: a tapered column with a per-row wobble.
    for (let y = 2; y < 18; y++) {
      const t = (18 - y) / 16; // 0 at base, 1 at tip
      const half = Math.max(0, 5.2 * Math.pow(1 - t, 0.8) + (rand() - 0.5) * 1.2);
      const wob = Math.sin(y * 0.9 + f * 2.1) * 1.3 * t;
      for (let x = 0; x < 20; x++) {
        const d = Math.abs(x + 0.5 - 10 - wob) / Math.max(half, 0.01);
        if (d > 1) continue;
        const heat = (1 - d) * (1 - t * 0.7);
        ctx.fillStyle = heat > 0.55 ? "#fff3b0" : heat > 0.32 ? "#ffcd3c" : heat > 0.14 ? "#f77622" : "#d23a2a";
        ctx.fillRect(x, y, 1, 1);
      }
    }
    // Loose embers above the tip.
    ctx.fillStyle = "#ffcd3c";
    ctx.fillRect(8 + f * 2, 1 + (f % 2), 1, 1);
    ctx.fillStyle = "#f77622";
    ctx.fillRect(12 - f, 3 - (f % 2), 1, 1);
    // Crossed logs and stones.
    ctx.fillStyle = "#6b3e26";
    for (let i = 0; i < 12; i++) {
      ctx.fillRect(4 + i, 17 + Math.floor(i / 6), 1, 2);
      ctx.fillRect(15 - i, 17 + Math.floor(i / 6), 1, 2);
    }
    ctx.fillStyle = "#8f5a3a";
    ctx.fillRect(6, 17, 3, 1);
    ctx.fillRect(11, 17, 3, 1);
    const stones = [[2, 19], [5, 20], [9, 20], [13, 20], [16, 19], [1, 18], [17, 18]];
    for (const [sx, sy] of stones) {
      ctx.fillStyle = "#5d6482";
      ctx.fillRect(sx, sy, 3, 2);
      ctx.fillStyle = "#9aa1bd";
      ctx.fillRect(sx, sy, 2, 1);
    }
    frames.push(c);
  }
  SPRITES.fire = frames;
}

// ---------------------------------------------------------------- pixel font

const FONT = {
  A: [".###.", "#...#", "#...#", "#####", "#...#", "#...#", "#...#"],
  B: ["####.", "#...#", "#...#", "####.", "#...#", "#...#", "####."],
  C: [".###.", "#...#", "#....", "#....", "#....", "#...#", ".###."],
  D: ["####.", "#...#", "#...#", "#...#", "#...#", "#...#", "####."],
  E: ["#####", "#....", "#....", "####.", "#....", "#....", "#####"],
  F: ["#####", "#....", "#....", "####.", "#....", "#....", "#...."],
  G: [".###.", "#...#", "#....", "#.###", "#...#", "#...#", ".####"],
  H: ["#...#", "#...#", "#...#", "#####", "#...#", "#...#", "#...#"],
  I: ["#####", "..#..", "..#..", "..#..", "..#..", "..#..", "#####"],
  J: ["..###", "...#.", "...#.", "...#.", "#..#.", "#..#.", ".##.."],
  K: ["#...#", "#..#.", "#.#..", "##...", "#.#..", "#..#.", "#...#"],
  L: ["#....", "#....", "#....", "#....", "#....", "#....", "#####"],
  M: ["#...#", "##.##", "#.#.#", "#.#.#", "#...#", "#...#", "#...#"],
  N: ["#...#", "##..#", "#.#.#", "#..##", "#...#", "#...#", "#...#"],
  O: [".###.", "#...#", "#...#", "#...#", "#...#", "#...#", ".###."],
  P: ["####.", "#...#", "#...#", "####.", "#....", "#....", "#...."],
  Q: [".###.", "#...#", "#...#", "#...#", "#.#.#", "#..#.", ".##.#"],
  R: ["####.", "#...#", "#...#", "####.", "#.#..", "#..#.", "#...#"],
  S: [".####", "#....", "#....", ".###.", "....#", "....#", "####."],
  T: ["#####", "..#..", "..#..", "..#..", "..#..", "..#..", "..#.."],
  U: ["#...#", "#...#", "#...#", "#...#", "#...#", "#...#", ".###."],
  V: ["#...#", "#...#", "#...#", "#...#", "#...#", ".#.#.", "..#.."],
  W: ["#...#", "#...#", "#...#", "#.#.#", "#.#.#", "#.#.#", ".#.#."],
  X: ["#...#", "#...#", ".#.#.", "..#..", ".#.#.", "#...#", "#...#"],
  Y: ["#...#", "#...#", ".#.#.", "..#..", "..#..", "..#..", "..#.."],
  Z: ["#####", "....#", "...#.", "..#..", ".#...", "#....", "#####"],
  0: [".###.", "#...#", "#..##", "#.#.#", "##..#", "#...#", ".###."],
  1: ["..#..", ".##..", "..#..", "..#..", "..#..", "..#..", ".###."],
  2: [".###.", "#...#", "....#", "...#.", "..#..", ".#...", "#####"],
  3: ["####.", "....#", "....#", ".###.", "....#", "....#", "####."],
  4: ["...#.", "..##.", ".#.#.", "#..#.", "#####", "...#.", "...#."],
  5: ["#####", "#....", "####.", "....#", "....#", "#...#", ".###."],
  6: [".###.", "#....", "#....", "####.", "#...#", "#...#", ".###."],
  7: ["#####", "....#", "...#.", "..#..", ".#...", ".#...", ".#..."],
  8: [".###.", "#...#", "#...#", ".###.", "#...#", "#...#", ".###."],
  9: [".###.", "#...#", "#...#", ".####", "....#", "....#", ".###."],
  "!": ["..#..", "..#..", "..#..", "..#..", "..#..", ".....", "..#.."],
  "?": [".###.", "#...#", "....#", "...#.", "..#..", ".....", "..#.."],
  "-": [".....", ".....", ".....", "#####", ".....", ".....", "....."],
  ".": [".....", ".....", ".....", ".....", ".....", ".....", "..#.."],
  " ": ["...", "...", "...", "...", "...", "...", "..."],
};

/** Width in font pixels (before scaling) of `text` in the pixel font. */
export function textWidth(text) {
  let w = 0;
  for (const ch of text.toUpperCase()) w += (FONT[ch] || FONT["?"])[0].length + 1;
  return Math.max(0, w - 1);
}

const textCache = new Map();

/**
 * Renders fixed UI text (never player-supplied) in the pixel font with a drop outline.
 * Returns a cached canvas; unknown characters render as "?".
 */
export function pixelText(text, color, outline = OUTLINE) {
  const key = text + "|" + color + "|" + outline;
  const hit = textCache.get(key);
  if (hit) return hit;
  const up = text.toUpperCase();
  const w = textWidth(up) + 2;
  const c = canvas(w, 7 + 3);
  const ctx = c.getContext("2d");
  const paint = (ox, oy, col) => {
    ctx.fillStyle = col;
    let x = 0;
    for (const ch of up) {
      const g = FONT[ch] || FONT["?"];
      for (let y = 0; y < 7; y++) {
        for (let i = 0; i < g[y].length; i++) if (g[y][i] === "#") ctx.fillRect(x + i + ox, y + oy, 1, 1);
      }
      x += g[0].length + 1;
    }
  };
  for (const [dx, dy] of [[0, 1], [2, 1], [1, 0], [1, 2], [1, 3], [0, 2], [2, 2]]) paint(dx, dy, outline);
  paint(1, 1, color);
  textCache.set(key, c);
  return c;
}

/** Small thought-cloud icon for the feed's "Thinks" rows; a fresh canvas each call. */
export function thinkIcon() {
  const src = SPRITES.cloud;
  const c = canvas(src.width * 2, src.height * 2);
  const ctx = c.getContext("2d");
  ctx.imageSmoothingEnabled = false;
  ctx.drawImage(src, 0, 0, c.width, c.height);
  c.className = "pixel think-icon";
  return c;
}

/** Role icon scaled up into a fresh canvas, for DOM use (role cards, badges). */
export function iconCanvas(role, scale = 4) {
  const src = ICONS[role];
  const c = canvas(src.width * scale, src.height * scale);
  const ctx = c.getContext("2d");
  ctx.imageSmoothingEnabled = false;
  ctx.drawImage(src, 0, 0, c.width, c.height);
  c.className = "pixel";
  return c;
}

buildShared();
buildFire();

export const CHAR_SIZE = { w: CHAR_W + 2, h: CHAR_H + 2 };

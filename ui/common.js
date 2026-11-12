'use strict';
// Shared by the main window and the desktop widgets: formatting, history series and the
// canvas line chart. Charts draw only when asked (a sample, a hover, a resize).

const HISTORY = 120; // 2 minutes at 1 Hz
const GIB = 2 ** 30;

const $ = id => document.getElementById(id);
const rootStyle = getComputedStyle(document.documentElement);
const token = name => rootStyle.getPropertyValue(name).trim();
const C = {};
function readTokens() {
  Object.assign(C, { accent: token('--accent'), grid: token('--grid'), axis: token('--axis'), ink3: token('--ink-3'), surface: token('--surface') });
}
readTokens();

// Theme: "system" follows Windows through prefers-color-scheme; "light" and "dark" are fixed.
const lightQuery = matchMedia('(prefers-color-scheme: light)');
let themeSetting = 'system';
function applyTheme(theme = themeSetting) {
  themeSetting = theme;
  document.documentElement.dataset.theme = theme === 'system' ? (lightQuery.matches ? 'light' : 'dark') : theme;
  readTokens();
  drawAll();
}
lightQuery.addEventListener('change', () => applyTheme());
const FONT = getComputedStyle(document.body).fontFamily;

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
}

// ---------- formatting ----------

function scaled(value, units, step) {
  let i = 0;
  while (value >= step && i < units.length - 1) { value /= step; i++; }
  return [value >= 100 || i === 0 ? Math.round(value).toString() : value.toFixed(1), units[i]];
}
const rate = bps => scaled(bps, ['B/s', 'KB/s', 'MB/s', 'GB/s'], 1000);
const bytes = b => scaled(b, ['B', 'KB', 'MB', 'GB', 'TB'], 1000).join(' ');
const gib = (b, digits = 1) => (b == null ? '–' : (b / GIB).toFixed(digits));
const pct = v => Math.round(v) + ' %';
const fixed = (v, digits = 0) => (v == null ? '–' : v.toFixed(digits));

// Temperatures stay in °C everywhere (thresholds, history); only the text converts.
let unitF = false;
const tempValue = c => (unitF ? c * 9 / 5 + 32 : c);
const tempUnit = () => (unitF ? '°F' : '°C');
const temp = c => (c == null ? '–' : Math.round(tempValue(c)).toString());
const tempText = c => temp(c) + ' ' + tempUnit();

const STATES = [null, ['warning', 'Hot'], ['serious', 'Very hot'], ['critical', 'Critical']];
const GPU_LIMITS = [75, 83, 88]; // warning / serious / critical, °C (RTX 40 throttles near 90)
const tempState = (t, limits) => STATES[limits.filter(limit => t >= limit).length];

// ---------- history series, shared by every chart that shows the same measure ----------

const series = new Map();
const line = key => series.get(key) ?? series.set(key, []).get(key);
// A missing reading is stored as null and drawn as a gap, never as a fake zero.
function add(key, v) {
  const s = line(key);
  s.push(v ?? null);
  if (s.length > HISTORY) s.shift();
}

// ---------- charts ----------

const charts = [];

function niceMax(v) {
  const p = 10 ** Math.floor(Math.log10(v));
  return [1, 2, 5, 10].map(m => m * p).find(n => n >= v);
}

function hexAlpha(hex, a) {
  const n = parseInt(hex.slice(1), 16);
  return `rgba(${n >> 16}, ${(n >> 8) & 255}, ${n & 255}, ${a})`;
}

// max: a number, 'auto' (rounded up from the data) or a function returning one.
// bare: a sparkline for widgets, with no axes, labels or hover.
function chart(canvas, key, { max = 100, fmt = pct, bare = false } = {}) {
  const c = { el: canvas, ctx: canvas.getContext('2d'), key, max, fmt, bare, hover: null };
  if (!bare) {
    canvas.addEventListener('pointermove', e => { c.hover = { x: e.offsetX, cx: e.clientX, cy: e.clientY }; draw(c); });
    canvas.addEventListener('pointerleave', () => { c.hover = null; $('tip').hidden = true; draw(c); });
  }
  charts.push(c);
  return c;
}

const visible = c => c.el.isConnected && c.el.offsetParent !== null;
function drawAll() {
  for (let i = charts.length - 1; i >= 0; i--) if (!charts[i].el.isConnected) charts.splice(i, 1);
  charts.filter(visible).forEach(draw);
}

function draw(c) {
  const { el: canvas, ctx } = c;
  const data = line(c.key);
  const w = canvas.clientWidth, h = canvas.clientHeight, dpr = window.devicePixelRatio || 1;
  if (!w || !h) return;
  if (canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(h * dpr)) {
    canvas.width = Math.round(w * dpr);
    canvas.height = Math.round(h * dpr);
  }
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);

  const max = typeof c.max === 'function' ? c.max() : c.max === 'auto' ? niceMax(Math.max(1, ...data)) : c.max;
  ctx.font = `10px ${FONT}`;
  const ticks = c.bare ? [] : [0, max / 2, max].map(t => [t, c.fmt(t).replace('.0 ', ' ')]);
  // The plot starts right after the widest tick label, so labels like "50 MB/s" never clip.
  const L = c.bare ? 1 : Math.ceil(Math.max(...ticks.map(([, s]) => ctx.measureText(s).width))) + 10;
  const R = c.bare ? 4 : 8, T = c.bare ? 4 : 6, B = c.bare ? 2 : 18;
  const pw = w - L - R, ph = h - T - B;
  const x = i => L + (i + HISTORY - data.length) * pw / (HISTORY - 1);
  const y = v => T + ph - Math.min(v, max) / max * ph;

  ctx.lineWidth = 1;
  ctx.textAlign = 'right';
  ctx.textBaseline = 'middle';
  ctx.fillStyle = C.ink3;
  for (const [t, label] of ticks) {
    const ty = Math.round(y(t)) + 0.5;
    ctx.strokeStyle = t === 0 ? C.axis : C.grid;
    ctx.beginPath(); ctx.moveTo(L, ty); ctx.lineTo(w - R, ty); ctx.stroke();
    ctx.fillText(label, L - 8, ty);
  }
  if (!c.bare) {
    ctx.textBaseline = 'alphabetic';
    ctx.fillText('now', w - R, h - 3);
    ctx.textAlign = 'left';
    ctx.fillText('2 min ago', L, h - 3);
  }
  if (!data.length) return;

  // Runs of known values; each gets an area wash, then the 2 px line on top.
  const runs = [];
  data.forEach((v, i) => {
    if (v == null) return;
    if (i && data[i - 1] != null) runs.at(-1).push(i);
    else runs.push([i]);
  });
  if (!runs.length) return;
  const wash = ctx.createLinearGradient(0, T, 0, T + ph);
  wash.addColorStop(0, hexAlpha(C.accent, 0.22));
  wash.addColorStop(1, hexAlpha(C.accent, 0.02));
  ctx.lineJoin = ctx.lineCap = 'round';
  for (const run of runs) {
    const trace = () => run.forEach((i, k) => (k ? ctx.lineTo(x(i), y(data[i])) : ctx.moveTo(x(i), y(data[i]))));
    ctx.beginPath();
    trace();
    ctx.lineTo(x(run.at(-1)), y(0));
    ctx.lineTo(x(run[0]), y(0));
    ctx.closePath();
    ctx.fillStyle = wash;
    ctx.fill();
    ctx.beginPath();
    trace();
    ctx.lineWidth = 2;
    ctx.strokeStyle = C.accent;
    ctx.stroke();
  }

  // Hover crosshair, or the end dot on the latest known value.
  let i = runs.at(-1).at(-1);
  if (c.hover) {
    const tip = $('tip');
    i = Math.round((c.hover.x - L) / pw * (HISTORY - 1)) - (HISTORY - data.length);
    i = Math.max(0, Math.min(data.length - 1, i));
    ctx.strokeStyle = C.axis;
    ctx.lineWidth = 1;
    const hx = Math.round(x(i)) + 0.5;
    ctx.beginPath(); ctx.moveTo(hx, T); ctx.lineTo(hx, T + ph); ctx.stroke();
    const ago = data.length - 1 - i;
    tip.innerHTML = `<b>${data[i] == null ? '–' : c.fmt(data[i])}</b> · ${ago ? ago + ' s ago' : 'now'}`;
    tip.hidden = false;
    tip.style.left = Math.min(c.hover.cx + 12, window.innerWidth - tip.offsetWidth - 8) + 'px';
    tip.style.top = (c.hover.cy - 34) + 'px';
  }
  if (data[i] == null) return;
  ctx.beginPath();
  ctx.arc(x(i), y(data[i]), c.bare ? 3 : 4, 0, Math.PI * 2);
  ctx.fillStyle = C.accent;
  ctx.lineWidth = 2;
  ctx.strokeStyle = C.surface;
  ctx.fill();
  ctx.stroke();
}

// ---------- recording: every series any window may chart ----------

function record(s, info) {
  add('cpu', s.cpu);
  add('cpu-temp', s.cpu_temp);
  add('cpu-power', s.cpu_power);
  add('cpu-clock', s.clock_ghz);
  add('gpu', s.gpu?.load);
  add('gpu-temp', s.gpu?.temp);
  add('gpu-power', s.gpu?.power_w);
  add('vram', s.gpu?.vram_used);
  add('ram', s.ram_used / info.ram_total * 100);
  add('commit', s.ram_commit);
  add('down', s.net_down);
  add('up', s.net_up);
  add('disk-io', s.disks.reduce((a, d) => a + d.read + d.write, 0));
  for (const d of s.disks) { add('read:' + d.mount, d.read); add('write:' + d.mount, d.write); }
  for (const a of s.adapters) { add('down:' + a.name, a.down); add('up:' + a.name, a.up); }
}

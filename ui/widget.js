'use strict';
// One desktop widget; the component comes from the URL hash (widget.html#cpu).

const kind = location.hash.slice(1);
const tauri = window.__TAURI__;
let info = null;
let cpuLimits = [85, 93, 98];

// Per component: title, chart series and its scale, and how to fill the text.
const WIDGETS = {
  cpu: {
    title: 'Processor', series: 'cpu', max: 100,
    fill: s => [Math.round(s.cpu), '%', s.cpu_temp == null ? '' : tempText(s.cpu_temp),
      [s.cpu_power != null && Math.round(s.cpu_power) + ' W', s.clock_ghz != null && s.clock_ghz.toFixed(2) + ' GHz'], tempState(s.cpu_temp ?? 0, cpuLimits)],
  },
  gpu: {
    title: 'Graphics', series: 'gpu', max: 100,
    fill: s => {
      const g = s.gpu || {};
      return [fixed(g.load), '%', g.temp == null ? '' : tempText(g.temp),
        [g.power_w != null && Math.round(g.power_w) + ' W', g.vram_used != null && `${gib(g.vram_used)} of ${gib(info.vram_total, 0)} GB`], tempState(g.temp ?? 0, GPU_LIMITS)];
    },
  },
  memory: {
    title: 'Memory', series: 'ram', max: 100,
    fill: s => [gib(s.ram_used), 'GB', Math.round(s.ram_used / info.ram_total * 100) + ' %', [`${gib(info.ram_total - s.ram_used)} GB available`]],
  },
  disks: {
    title: 'Disks', series: 'disk-io', max: () => niceMax(Math.max(1e5, ...line('disk-io'))),
    fill: s => {
      const [v, unit] = rate(s.disks.reduce((a, d) => a + d.read + d.write, 0));
      return [v, unit, 'read + write', s.disks.slice(0, 2).map(d => `${d.mount} ${bytes(d.free)} free`)];
    },
  },
  network: {
    title: 'Network', series: 'down', max: () => niceMax(Math.max(1e5, ...line('down'))),
    fill: s => {
      const [v, unit] = rate(s.net_down);
      return [v, unit, '↑ ' + rate(s.net_up).join(' '), ['Download', `${bytes(s.net_rx)} received`]];
    },
  },
};

const w = WIDGETS[kind] || WIDGETS.cpu;
$('w-title').textContent = w.title;
const spark = chart($('w-chart'), w.series, { max: w.max, bare: true });

function render(s) {
  const [main, unit, side, foot, state] = w.fill(s);
  $('w-main').textContent = main;
  $('w-unit').textContent = unit;
  const sideEl = $('w-side');
  sideEl.textContent = side;
  sideEl.className = 'w-side' + (state ? ' hot ' + state[0] : '');
  sideEl.title = state ? state[1] : '';
  $('w-foot').textContent = foot.filter(Boolean).join(' · ');
  draw(spark);
}

let lastSample = null;
function applySettings(s) {
  unitF = s.unit === 'F';
  applyTheme(s.theme);
  if (lastSample) render(lastSample);
}

function open() {
  tauri.core.invoke('open_page', { page: kind });
}
$('widget').addEventListener('click', open);
$('widget').addEventListener('keydown', e => { if (e.key === 'Enter' || e.key === ' ') open(); });

async function boot() {
  info = await tauri.core.invoke('get_info');
  if (info.tjmax) cpuLimits = [info.tjmax - 15, info.tjmax - 7, info.tjmax - 2];
  applySettings(await tauri.core.invoke('get_settings'));
  await tauri.event.listen('settings', e => applySettings(e.payload));
  const history = await tauri.core.invoke('get_history');
  history.forEach(s => record(s, info));
  if (history.length) render(lastSample = history.at(-1));
  await tauri.event.listen('sample', e => { record(e.payload, info); render(lastSample = e.payload); });
}

new ResizeObserver(() => draw(spark)).observe(document.body);
boot().catch(e => { $('w-foot').textContent = 'Not connected: ' + e; });

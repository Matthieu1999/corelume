'use strict';
// Corelume main window: an overview, one page per component, and settings. Only the page
// on screen is rendered; every series keeps recording so any page opens with its history.
// Inside Tauri the data is live; in a plain browser it is a demo.

const tauri = window.__TAURI__;
const invoke = (cmd, args) => tauri.core.invoke(cmd, args);
let info = null;
let last = null;
let cpuLimits = [85, 93, 98];
let page = 'overview';

// ---------- charts declared in the HTML: data-series, data-fmt, data-max ----------

const FMT = {
  pct,
  rate: v => rate(v).join(' '),
  temp: v => tempText(v),
  watts: v => Math.round(v) + ' W',
  ghz: v => v.toFixed(1) + ' GHz',
  gib: v => gib(v, v >= 10 * GIB ? 0 : 1) + ' GB',
};
const autoMax = key => () => niceMax(Math.max(1, ...line(key)));
const MAX = {
  'cpu-max': () => (info?.cpu_max_mhz ? info.cpu_max_mhz / 1000 : 6),
  'gpu-power': () => last?.gpu?.power_limit_w || autoMax('gpu-power')(),
  vram: () => info?.vram_total || autoMax('vram')(),
  commit: () => last?.ram_commit_limit || autoMax('commit')(),
};

function bindCharts(root) {
  for (const canvas of root.querySelectorAll('canvas[data-series]')) {
    if (canvas.bound) continue;
    canvas.bound = true;
    const m = canvas.dataset.max;
    const key = canvas.dataset.series;
    // Auto scales start at a readable size, so an idle line never gets ticks like "1 B/s, 1 B/s".
    const floor = { rate: 1000, watts: 10 }[canvas.dataset.fmt] ?? 1;
    const max = m == null ? 100 : m === 'auto' ? () => niceMax(Math.max(floor, ...line(key))) : MAX[m] ?? Number(m);
    chart(canvas, key, { max, fmt: FMT[canvas.dataset.fmt] ?? pct });
  }
}

// ---------- small DOM helpers ----------

function text(id, value) { $(id).textContent = value; }
function setAll(selector, value) { document.querySelectorAll(selector).forEach(e => { e.textContent = value; }); }

function notes(cls, message) {
  document.querySelectorAll('.' + cls).forEach(n => { n.textContent = message || ''; n.hidden = !message; });
}

function specs(id, rows) {
  $(id).replaceChildren(...rows.filter(([, v]) => v != null && v !== '').flatMap(([k, v]) => [el('dt', null, k), el('dd', null, v)]));
}

// Table rows; a cell is text or a node. numLast right-aligns the last column (a value).
function rows(id, items, cells, numLast = false) {
  $(id).replaceChildren(...items.map(item => {
    const tr = el('tr');
    const list = cells(item);
    list.forEach((c, i) => {
      const td = el('td', numLast && i === list.length - 1 ? 'num' : null);
      td.append(c);
      tr.append(td);
    });
    return tr;
  }));
}

function procCell(p) {
  const span = el('span', null, p.name.replace(/\.exe$/i, ''));
  if (p.count > 1) span.append(el('small', null, ' ×' + p.count));
  return span;
}

function setTemp(id, value, limits) {
  $(id).textContent = temp(value);
  const badge = $(id + '-state');
  const state = value == null ? null : tempState(value, limits);
  badge.hidden = !state;
  if (state) { badge.className = 'state ' + state[0]; badge.textContent = state[1]; }
}

// The CPU's own flags: a power limit is normal operation; heat means it reached TjMax.
function setLimit(id, s) {
  const badge = $(id);
  badge.hidden = !s.cpu_limit;
  if (s.cpu_limit === 'heat') {
    badge.className = 'state critical';
    badge.textContent = 'Heat limit';
    badge.title = 'The processor reached its maximum temperature and is slowing down to cool off';
  } else if (s.cpu_limit) {
    // Name the limit it sits at: PL1 (long) or PL2 (short), whichever is nearer.
    const near = [s.cpu_pl1, s.cpu_pl2].filter(v => v != null)
      .sort((a, b) => Math.abs(a - s.cpu_power) - Math.abs(b - s.cpu_power))[0];
    badge.className = 'state limit';
    badge.textContent = near ? `Limit ${Math.round(near)} W` : 'Power limit';
    badge.title = 'The processor is held at the power limit set by the BIOS or the PC maker\'s app';
  }
}

// ---------- layout built once from the machine's Info ----------

let tiles = [];
let coreRows = [];

function coreNames(cores) {
  const n = { P: 0, E: 0, C: 0 };
  return cores.map(c => c.kind + ++n[c.kind]);
}

function setup(i) {
  info = i;
  if (i.tjmax) cpuLimits = [i.tjmax - 15, i.tjmax - 7, i.tjmax - 2];
  const ram = Math.round(i.ram_total / GIB) + ' GB';
  const count = kind => i.cores.filter(c => c.kind === kind).length;
  const threads = i.cores.reduce((a, c) => a + c.threads.length, 0);
  const topo = count('P') ? `${count('P')} P-cores + ${count('E')} E-cores` : `${i.cores.length} cores`;
  text('cpu-sub', topo);
  setAll('.cpu-name', i.cpu_name);
  setAll('.cpu-topo', `${topo} · ${threads} threads`);
  text('gpu-sub', (i.gpu_name || '').replace(/^NVIDIA (GeForce )?/, ''));
  setAll('.gpu-name', i.gpu_name || 'Graphics');
  text('mem-sub', ram);
  text('p-mem-sub', ram + ' installed');
  text('vram-total', i.vram_total ? Math.round(i.vram_total / GIB) : '–');
  $('vram-meter').setAttribute('aria-valuemax', i.vram_total ? (i.vram_total / GIB).toFixed(1) : 0);
  notes('cpu-note', i.temp_error);
  notes('gpu-note', i.gpu_error);
  document.querySelectorAll('.gpu-card').forEach(c => c.classList.toggle('unavailable', !!i.gpu_error));
  buildCores(i.cores);
  buildCoreRows(i.cores);
  const r = i.gpu_rated || {};
  specs('gpu-specs', [
    ['Name', i.gpu_name], ['Driver', r.driver], ['Video memory', i.vram_total && Math.round(i.vram_total / GIB) + ' GB'],
    ['Max core clock', r.max_clock_mhz && r.max_clock_mhz + ' MHz'], ['Max memory clock', r.max_mem_clock_mhz && r.max_mem_clock_mhz + ' MHz'],
    ['Default power limit', r.default_power_limit_w && Math.round(r.default_power_limit_w) + ' W'], ['Fastest PCIe link', r.pcie_max],
  ]);
  specs('about', [['Version', i.version], ['Running as administrator', i.elevated ? 'Yes' : 'No'],
    ['CPU temperatures and power', i.temp_error || 'Available'], ['Graphics', i.gpu_error || 'NVIDIA, through the driver (NVML)']]);
}

// One tile per core; E-cores get one tile per L2 cluster because the cluster shares a
// single temperature sensor. Every load bar spans one grid column, so bars line up across rows.
function buildCores(cores) {
  const root = $('cores');
  root.replaceChildren();
  tiles = [];
  let cols = 1;
  for (const [kind, title] of [['P', 'Performance cores'], ['E', 'Efficient cores'], ['C', 'Cores']]) {
    const members = cores.map((c, index) => ({ ...c, index })).filter(c => c.kind === kind);
    if (!members.length) continue;
    const units = kind === 'E' ? Object.values(Object.groupBy(members, c => c.cluster)) : members.map(c => [c]);
    root.append(el('div', 'group-label', title));
    let n = 0;
    for (const unit of units) {
      const first = n + 1;
      n += unit.length;
      const name = unit.length > 1 ? `${kind}${first}–${kind}${n}` : `${kind}${first}`;
      const tile = el('div', 'core');
      const bars = el('div', 'bars');
      const fills = unit.flatMap(c => c.threads).map(th => {
        const bar = el('div', 'bar');
        const fill = el('i');
        bar.append(fill);
        bars.append(bar);
        return [fill, th];
      });
      tile.style.gridColumn = `span ${Math.min(fills.length, 16)}`;
      const t = el('span', 't', '–');
      tile.append(t, bars, el('span', 'n', name));
      root.append(tile);
      tiles.push({ tile, fills, temp: t, name, cores: unit.map(c => c.index) });
    }
    cols = Math.max(cols, members.reduce((sum, c) => sum + c.threads.length, 0));
  }
  root.style.setProperty('--cols', Math.min(cols, 16));
}

function buildCoreRows(cores) {
  const names = coreNames(cores);
  coreRows = cores.map((c, i) => {
    const tr = el('tr');
    const bar = el('span', 'mini');
    const fill = el('i');
    bar.append(fill);
    const load = el('span', 'num-inline', '–');
    const loadCell = el('td');
    loadCell.append(bar, load);
    const clock = el('td', 'num', '–');
    const t = el('td', 'num', '–');
    tr.append(el('td', null, names[i] + (c.threads.length > 1 ? ' · 2 threads' : '')), loadCell, clock, t);
    return { tr, fill, load, clock, temp: t, threads: c.threads };
  });
  $('core-rows').replaceChildren(...coreRows.map(r => r.tr));
}

// ---------- pages ----------

function renderOverview(s) {
  text('cpu-load', Math.round(s.cpu));
  setTemp('cpu-temp', s.cpu_temp, cpuLimits);
  text('cpu-power', fixed(s.cpu_power));
  setLimit('cpu-limit', s);
  text('cpu-clock', fixed(s.clock_ghz, 2));
  let hottest = null;
  for (const t of tiles) {
    const known = t.cores.map(i => s.core_temps[i]).filter(v => v != null);
    const hot = known.length ? Math.max(...known) : null;
    const state = hot == null ? null : tempState(hot, cpuLimits);
    t.temp.textContent = hot == null ? '–' : temp(hot) + '°';
    t.temp.className = 't' + (state ? ' ' + state[0] : '');
    const loads = t.fills.map(([fill, th]) => {
      const v = s.threads[th] ?? 0;
      fill.style.height = v.toFixed(1) + '%';
      return Math.round(v) + ' %';
    });
    const heat = hot == null ? '' : `, ${tempText(hot)}${state ? ' (' + state[1].toLowerCase() + ')' : ''}`;
    t.tile.title = `${t.name}: ${loads.join(', ')}${heat}`;
    if (hot != null && (!hottest || hot > hottest.temp)) hottest = { name: t.name, temp: hot };
  }
  text('cpu-hot', hottest ? `${hottest.name} · ${temp(hottest.temp)}` : '–');

  const g = s.gpu || {};
  text('gpu-load', fixed(g.load));
  setTemp('gpu-temp', g.temp, GPU_LIMITS);
  text('vram-used', gib(g.vram_used));
  $('vram-fill').style.width = g.vram_used == null || !info.vram_total ? '0' : (g.vram_used / info.vram_total * 100).toFixed(1) + '%';
  $('vram-meter').setAttribute('aria-valuenow', gib(g.vram_used));
  text('gpu-power', fixed(g.power_w));
  text('gpu-clock', g.clock_mhz == null ? '–' : (g.clock_mhz / 1000).toFixed(2));
  text('gpu-fan', fixed(g.fan));

  const ramPct = s.ram_used / info.ram_total * 100;
  text('ram-used', gib(s.ram_used));
  text('ram-free', gib(info.ram_total - s.ram_used));
  text('ram-pct', Math.round(ramPct));
  $('ram-fill').style.width = ramPct.toFixed(1) + '%';
  $('ram-meter').setAttribute('aria-valuenow', Math.round(ramPct));

  const [rd, rdUnit] = rate(s.disks.reduce((a, d) => a + d.read, 0));
  const [wr, wrUnit] = rate(s.disks.reduce((a, d) => a + d.write, 0));
  text('disk-read', rd); text('disk-read-unit', rdUnit);
  text('disk-write', wr); text('disk-write-unit', wrUnit);
  text('disk-sub', s.disks.length === 1 ? '1 drive' : s.disks.length + ' drives');
  renderDriveList(s.disks);

  const [down, downUnit] = rate(s.net_down);
  const [up, upUnit] = rate(s.net_up);
  text('net-down', down); text('net-down-unit', downUnit);
  text('net-up', up); text('net-up-unit', upUnit);
  text('net-rx', bytes(s.net_rx));
  text('net-tx', bytes(s.net_tx));
}

let driveKey = '';
function renderDriveList(disks) {
  const key = disks.map(d => d.mount).join();
  if (key !== driveKey) {
    driveKey = key;
    $('drive-list').replaceChildren(...disks.map(d => {
      const row = el('div', 'meter-row');
      row.dataset.mount = d.mount;
      const label = el('div', 'meter-label');
      label.append(el('span', null, d.mount + (d.label ? ' ' + d.label : '')), el('span'));
      const meter = el('div', 'meter');
      meter.setAttribute('role', 'meter');
      meter.setAttribute('aria-label', d.mount + ' space used');
      meter.append(el('i'));
      row.append(label, meter);
      return row;
    }));
  }
  for (const d of disks) {
    const row = $('drive-list').querySelector(`[data-mount="${d.mount}"]`);
    const used = d.total ? (1 - d.free / d.total) * 100 : 0;
    const free = row.querySelector('.meter-label span:last-child');
    free.innerHTML = `<b>${bytes(d.free)}</b> free`;
    row.title = `${d.mount} ${d.label}: ${bytes(d.free)} free of ${bytes(d.total)}`;
    row.querySelector('.meter i').style.width = used.toFixed(1) + '%';
    row.querySelector('.meter').setAttribute('aria-valuenow', Math.round(used));
  }
}

function renderCpu(s) {
  text('p-cpu-load', Math.round(s.cpu));
  setTemp('p-cpu-temp', s.cpu_temp, cpuLimits);
  text('p-cpu-power', fixed(s.cpu_power));
  setLimit('p-cpu-limit', s);
  text('p-cpu-clock', fixed(s.clock_ghz, 2));
  text('p-cpu-vid', fixed(s.cpu_vid, 3));
  coreRows.forEach((r, i) => {
    const load = r.threads.reduce((a, th) => a + (s.threads[th] ?? 0), 0) / r.threads.length;
    r.fill.style.width = load.toFixed(1) + '%';
    r.load.textContent = Math.round(load) + ' %';
    const mhz = s.core_clocks[i];
    r.clock.textContent = mhz == null ? '–' : Math.round(mhz) + ' MHz';
    const t = s.core_temps[i];
    r.temp.textContent = t == null ? '–' : tempText(t);
  });
  specs('cpu-specs', [
    ['Base clock', info.cpu_base_mhz && (info.cpu_base_mhz / 1000).toFixed(1) + ' GHz'],
    ['Max turbo', info.cpu_max_mhz && (info.cpu_max_mhz / 1000).toFixed(1) + ' GHz'],
    ['Thermal limit (TjMax)', info.tjmax && tempText(info.tjmax)],
    ['Long power limit (PL1)', s.cpu_pl1 && Math.round(s.cpu_pl1) + ' W'],
    ['Short power limit (PL2)', s.cpu_pl2 && Math.round(s.cpu_pl2) + ' W'],
  ]);
  const top = s.detail?.top_cpu;
  if (top) rows('top-cpu', top, p => [procCell(p), p.cpu.toFixed(1) + ' %'], true);
}

function renderGpu(s) {
  const g = s.gpu || {};
  text('p-gpu-load', fixed(g.load));
  setTemp('p-gpu-temp', g.temp, GPU_LIMITS);
  text('p-gpu-power', fixed(g.power_w));
  text('p-gpu-power-limit', g.power_limit_w ? `of ${Math.round(g.power_limit_w)} W` : 'W');
  text('p-gpu-clock', fixed(g.clock_mhz));
  text('p-gpu-mem-clock', fixed(g.mem_clock_mhz));
  text('p-gpu-fan', fixed(g.fan));
  specs('gpu-now', [
    ['Video memory', g.vram_used != null && `${gib(g.vram_used)} of ${gib(info.vram_total, 0)} GB`],
    ['Video encoder', g.encoder != null && g.encoder + ' %'],
    ['Video decoder', g.decoder != null && g.decoder + ' %'],
    ['PCIe link', g.pcie],
    ['Held back by', g.held_by ? (g.held_by.length ? g.held_by.join(', ') : 'Nothing') : null],
  ]);
}

// The breakdown's rows: [key, label, explanation], with group headers in between.
// Explanations given as functions see the breakdown, for numbers inside the text.
const memSize = b => (b >= GIB / 10 ? gib(b) + ' GB' : Math.round(b / 2 ** 20) + ' MB');
const USAGE = [
  { group: 'In use', keys: ['programs', 'compressed', 'kernel', 'other', 'modified'] },
  { key: 'programs', label: 'Programs', text: 'Private memory of the running programs, listed next to this.' },
  { key: 'compressed', label: 'Compressed memory',
    text: 'Program memory Windows compressed instead of writing it to disk. Held by the Memory Compression process, which Task Manager does not list.' },
  { key: 'kernel', label: 'Windows kernel and drivers',
    text: b => `Kernel pools (${memSize(b.nonpaged)} that never leaves RAM, ${memSize(b.paged)} that can), kernel and driver code, system file cache.` },
  { key: 'other', label: 'Shared files and other system memory',
    text: 'Files and DLLs shared by programs, counted once, plus page tables and memory locked by drivers.' },
  { key: 'modified', label: 'Waiting to be written to disk', text: 'Changed data Windows saves to disk before reusing that memory.' },
  { group: 'Available', keys: ['standby', 'free'] },
  { key: 'standby', label: 'File cache', text: 'Recently used files kept in memory to open them faster, handed to programs as soon as they need it.', idle: true },
  { key: 'free', label: 'Free', text: 'Not used at all.', idle: true },
  { group: 'Not usable', keys: ['reserved'] },
  { key: 'reserved', label: 'Reserved by the hardware', text: 'Kept by the firmware and devices: Windows never gets it.', idle: true },
];
const usageOf = (b, key) => (key === 'kernel' ? b.nonpaged + b.paged + b.code + b.system_cache : b[key]);
let usageRows = null;

function renderUsage(b) {
  if (!usageRows) {
    usageRows = USAGE.map(u => {
      if (u.group) {
        const head = el('div', 'group');
        head.append(el('span', null, u.group), el('span', 'size'));
        $('mem-usage').append(head);
        return { ...u, size: head.lastChild };
      }
      const what = el('div', 'what');
      const small = el('small');
      what.append(el('b', null, u.label), small);
      const size = el('div', 'size');
      const meter = el('div', 'meter' + (u.idle ? ' idle' : ''));
      meter.setAttribute('aria-hidden', 'true');
      meter.append(el('i'));
      $('mem-usage').append(what, size, meter);
      return { ...u, small, size, fill: meter.firstChild };
    });
  }
  for (const r of usageRows) {
    const v = r.group ? r.keys.reduce((a, k) => a + usageOf(b, k), 0) : usageOf(b, r.key);
    const share = b.installed ? v / b.installed * 100 : 0;
    r.size.innerHTML = `<b>${memSize(v)}</b> <small>${share < 1 && v > 0 ? '<1' : Math.round(share)} %</small>`;
    if (r.group) continue;
    r.small.textContent = typeof r.text === 'function' ? r.text(b) : r.text;
    r.fill.style.width = share.toFixed(2) + '%';
  }
}

function renderMemory(s) {
  text('p-ram-used', gib(s.ram_used));
  text('p-ram-free', gib(info.ram_total - s.ram_used));
  text('p-ram-commit', gib(s.ram_commit));
  text('p-ram-commit-of', s.ram_commit_limit ? `of ${gib(s.ram_commit_limit, 0)} GB` : 'GB');
  const d = s.detail || {};
  // Same figure as the breakdown's "File cache" line once the breakdown is there.
  text('p-ram-cached', gib(d.memory ? d.memory.standby : s.ram_cached));
  if (d.modules) {
    rows('modules', d.modules, m => [m.slot, m.size_mb >= 1024 ? m.size_mb / 1024 + ' GB' : m.size_mb + ' MB', m.kind || '–',
      m.configured ? `${m.configured} MT/s` + (m.speed && m.speed !== m.configured ? ` (rated ${m.speed})` : '') : (m.speed ? m.speed + ' MT/s' : '–'),
      m.maker || '–', m.part || '–']);
  }
  if (d.memory) renderUsage(d.memory);
  if (d.top_ram) rows('top-ram', d.top_ram, p => [procCell(p), memSize(p.ram)], true);
}

// Disks and Network pages: one card per drive or adapter, rebuilt when the set changes.
function cardList(root, items, keyOf, build) {
  const key = items.map(keyOf).join('|');
  if (root.dataset.key !== key) {
    root.dataset.key = key;
    root.replaceChildren(...items.map(build));
    if (!items.length) root.append(el('p', 'hint', 'Nothing to show.'));
    bindCharts(root);
  }
  return new Map([...root.children].map(c => [c.dataset.key, c]));
}

function ioColumn(label, series, cls) {
  const col = el('div');
  col.innerHTML = `<div class="fig"><span class="label">${label}</span><span class="value"><b class="${cls}">–</b><i class="${cls}-unit">B/s</i></span></div>`;
  const canvas = el('canvas', 'chart');
  canvas.dataset.series = series;
  canvas.dataset.fmt = 'rate';
  canvas.dataset.max = 'auto';
  canvas.setAttribute('role', 'img');
  canvas.setAttribute('aria-label', label + ' over the last 2 minutes');
  col.append(canvas);
  return col;
}

function setRate(card, cls, value) {
  const [v, unit] = rate(value);
  card.querySelector('.' + cls).textContent = v;
  card.querySelector('.' + cls + '-unit').textContent = unit;
}

function renderDisks(s) {
  const cards = cardList($('page-disks'), s.disks, d => d.mount, d => {
    const card = el('section', 'card span-all');
    card.dataset.key = d.mount;
    card.innerHTML = `<div class="card-head"><h2></h2><span class="sub"></span></div>
      <div class="meter-row"><div class="meter-label"><span>Space used</span><span class="free"></span></div>
      <div class="meter" role="meter" aria-label="Space used"><i></i></div></div>`;
    card.querySelector('h2').textContent = d.mount + (d.label ? ' ' + d.label : '');
    card.querySelector('.sub').textContent = [d.kind, d.fs, d.removable ? 'Removable' : ''].filter(Boolean).join(' · ');
    const cols = el('div', 'net-cols');
    cols.append(ioColumn('Read', 'read:' + d.mount, 'rd'), ioColumn('Write', 'write:' + d.mount, 'wr'));
    card.append(cols);
    return card;
  });
  for (const d of s.disks) {
    const card = cards.get(d.mount);
    const used = d.total ? (1 - d.free / d.total) * 100 : 0;
    card.querySelector('.free').innerHTML = `<b>${bytes(d.free)}</b> free of ${bytes(d.total)}`;
    card.querySelector('.meter i').style.width = used.toFixed(1) + '%';
    card.querySelector('.meter').setAttribute('aria-valuenow', Math.round(used));
    setRate(card, 'rd', d.read);
    setRate(card, 'wr', d.write);
  }
}

const linkSpeed = bps => (bps >= 1e9 ? bps / 1e9 + ' Gbit/s' : bps >= 1e6 ? Math.round(bps / 1e6) + ' Mbit/s' : '');

function renderNetwork(s) {
  const cards = cardList($('page-network'), s.adapters, a => a.name, a => {
    const card = el('section', 'card span-all');
    card.dataset.key = a.name;
    card.innerHTML = '<div class="card-head"><h2></h2><span class="sub"></span></div>';
    card.querySelector('h2').textContent = a.name;
    const cols = el('div', 'net-cols');
    cols.append(ioColumn('Download', 'down:' + a.name, 'dn'), ioColumn('Upload', 'up:' + a.name, 'upl'));
    card.append(cols, el('dl', 'specs'));
    return card;
  });
  const details = new Map((s.detail?.adapters || []).map(d => [d.name, d]));
  for (const a of s.adapters) {
    const card = cards.get(a.name);
    setRate(card, 'dn', a.down);
    setRate(card, 'upl', a.up);
    const d = details.get(a.name);
    if (d) card.querySelector('.sub').textContent = d.description;
    const dl = card.querySelector('dl');
    dl.replaceChildren(...[['Link speed', d && linkSpeed(d.link_bps)], ['Addresses', d?.ips.join(', ')], ['MAC address', d?.mac],
      ['Received since start', bytes(a.rx)], ['Sent since start', bytes(a.tx)]]
      .filter(([, v]) => v).flatMap(([k, v]) => [el('dt', null, k), el('dd', null, v)]));
  }
}

const RENDER = { overview: renderOverview, cpu: renderCpu, gpu: renderGpu, memory: renderMemory, disks: renderDisks, network: renderNetwork };

function render(s) {
  RENDER[page]?.(s);
  drawAll();
}

function onSample(s) {
  record(s, info);
  last = s;
  if (!document.hidden) render(s); // minimised: keep history, skip drawing
}

// ---------- navigation ----------

const PAGES = ['overview', 'cpu', 'gpu', 'memory', 'disks', 'network', 'settings'];

function go(name) {
  page = PAGES.includes(name) ? name : 'overview';
  for (const p of PAGES) $('page-' + p).hidden = p !== page;
  window.scrollTo(0, 0); // a page opens at its top, not at the last page's scroll
  document.querySelectorAll('.tabs a').forEach(a => a.setAttribute('aria-current', a.hash === '#' + page ? 'page' : 'false'));
  // The sampler adds the page's extra data (processes, modules, adapter details) only while it is open.
  if (tauri) invoke('set_page', { page: ['cpu', 'memory', 'network'].includes(page) ? page : '' });
  if (page === 'settings') listScreens().catch(e => console.error(e));
  if (last) render(last);
}

window.addEventListener('hashchange', () => go(location.hash.slice(1)));

// ---------- settings ----------

let settings = { unit: 'C', theme: 'system', widgets: {}, screen: '', row: 0, col: 4, layout: 'column', align: 'left' };

// The 5 x 5 position picker, one radio per cell, named like a compass.
const ROWS = ['Top', 'Upper', 'Middle', 'Lower', 'Bottom'];
const COLS = ['left', 'left of centre', 'centre', 'right of centre', 'right'];
for (let r = 0; r < 5; r++) {
  for (let c = 0; c < 5; c++) {
    const label = el('label');
    const input = Object.assign(el('input'), { type: 'radio', name: 'pos', value: `${r},${c}` });
    input.setAttribute('aria-label', `${ROWS[r]} ${COLS[c]}`);
    input.addEventListener('change', () => { settings.row = r; settings.col = c; save(); });
    label.title = `${ROWS[r]} ${COLS[c]}`;
    label.append(input, el('span'));
    $('pos-grid').append(label);
  }
}

function applySettings(s) {
  settings = s;
  unitF = s.unit === 'F';
  applyTheme(s.theme);
  document.querySelectorAll('.tu').forEach(e => { e.textContent = tempUnit(); });
  for (const name of ['unit', 'theme', 'layout', 'align']) {
    document.querySelectorAll(`input[name="${name}"]`).forEach(r => { r.checked = r.value === s[name]; });
  }
  // The last-row choice only means something when rows hold two or three widgets.
  $('align-row').hidden = !['grid2', 'grid3'].includes(s.layout);
  $('set-screen').value = s.screen;
  document.querySelectorAll('input[name="pos"]').forEach(r => { r.checked = r.value === `${s.row},${s.col}`; });
  document.querySelectorAll('input[data-widget]').forEach(c => { c.checked = !!s.widgets[c.dataset.widget]; });
  if (last) render(last);
}

function save() {
  if (tauri) invoke('set_settings', { settings }).catch(e => console.error(e));
  else applySettings(settings);
}

for (const name of ['unit', 'theme', 'layout', 'align']) {
  document.querySelectorAll(`input[name="${name}"]`).forEach(r => r.addEventListener('change', () => { settings[name] = r.value; save(); }));
}
$('set-screen').addEventListener('change', e => { settings.screen = e.target.value; save(); });

// Screens are listed each time Settings opens, so a screen plugged in meanwhile shows up.
// An empty value means "the main screen", which is also where an unplugged choice falls back.
async function listScreens() {
  const screens = tauri ? await invoke('list_screens') : [{ name: 'demo', label: 'Screen 1 · 1920 × 1080 (main)' }];
  const box = $('set-screen');
  box.replaceChildren(el('option', null, 'Main screen'), ...screens.map(sc => Object.assign(el('option', null, sc.label), { value: sc.name })));
  box.firstChild.value = '';
  // A saved screen that is unplugged stays selected, so the choice is not lost.
  if (settings.screen && !screens.some(sc => sc.name === settings.screen)) {
    box.append(Object.assign(el('option', null, 'Screen not connected (using the main one)'), { value: settings.screen }));
  }
  box.value = settings.screen;
}
document.querySelectorAll('input[data-widget]').forEach(c => c.addEventListener('change', () => {
  settings.widgets = { ...settings.widgets, [c.dataset.widget]: c.checked };
  save();
}));

async function setupAutostart() {
  const box = $('set-autostart');
  const sync = state => {
    box.checked = state.enabled;
    box.disabled = !state.allowed;
    if (!state.allowed) text('autostart-note', 'Needs Corelume to run as administrator, as it does after installation.');
  };
  sync(await invoke('get_autostart'));
  box.addEventListener('change', async () => sync(await invoke('set_autostart', { enabled: box.checked })));
}

// ---------- demo source, same shapes as the Rust Info and Sample ----------

function demoSource() {
  // A made-up PC: none of these values describe a real machine.
  const P = 6, E = 8;
  const cores = [
    ...Array.from({ length: P }, (_, i) => ({ kind: 'P', threads: [2 * i, 2 * i + 1], cluster: i })),
    ...Array.from({ length: E }, (_, i) => ({ kind: 'E', threads: [2 * P + i], cluster: P + (i >> 2) })),
  ];
  const threads = 2 * P + E;
  const info = {
    version: 'demo', elevated: false, cpu_name: 'Demo processor', cores, tjmax: 100, cpu_base_mhz: 3000, cpu_max_mhz: 5000,
    temp_error: null, gpu_name: 'Demo graphics', vram_total: 12 * GIB, gpu_error: null, ram_total: 16 * GIB,
    gpu_rated: { driver: '000.00', max_clock_mhz: 2800, max_mem_clock_mhz: 10000, default_power_limit_w: 250, pcie_max: 'Gen 4 ×16' },
  };
  const rand = (a, b) => a + Math.random() * (b - a);
  const clamp = (v, a, b) => Math.max(a, Math.min(b, v));
  const toward = (v, target, k, noise, lo, hi) => clamp(v + (target - v) * k + rand(-noise, noise), lo, hi);
  const d = {
    t: 0, threads: Array(threads).fill(8), temps: Array(P + E).fill(45), pkg: 45, power: 24, clock: 1.2, ram: 8.8 * GIB, rx: 0, tx: 0,
    gpu: { load: 5, temp: 36, power: 30, clock: 210, fan: 0, vram: 1.8 * GIB }, read: 0, write: 0,
  };
  const procs = [['browser.exe', 24], ['editor.exe', 12], ['chat.exe', 5], ['explorer.exe', 1], ['game.exe', 1]]
    .map(([name, count], i) => ({ name, count, cpu: 4 / (i + 1), ram: (1.6 - i * 0.25) * GIB }));
  const memory = { installed: 16 * GIB, programs: 4.2 * GIB, compressed: 1.1 * GIB, nonpaged: 0.6 * GIB, paged: 0.5 * GIB,
    code: 0.05 * GIB, system_cache: 0.2 * GIB, modified: 0.2 * GIB, other: 1.9 * GIB, standby: 3.8 * GIB, free: 3.3 * GIB, reserved: 0.15 * GIB };
  function next() {
    const t = ++d.t;
    const cpuBurst = t % 45 > 33, gpuBurst = t % 60 > 38, netBurst = t % 70 > 56, diskBurst = t % 50 > 42;
    d.threads = d.threads.map((v, i) => toward(v, cpuBurst ? (i < 2 * P ? 90 : 75) : (i % 2 ? 4 : 12), 0.35, 8, 0, 100));
    const cpu = d.threads.reduce((a, b) => a + b, 0) / d.threads.length;
    d.clock = toward(d.clock, 0.9 + cpu / 100 * 4.6, 0.5, 0.12, 0.8, 5.6);
    d.pkg = toward(d.pkg, 48 + cpu * 0.5, 0.4, 0.8, 30, 100);
    d.power = toward(d.power, cpuBurst ? 125 : 24, 0.5, 3, 10, 125);
    d.temps = d.temps.map((v, i) => i < P
      ? toward(v, d.pkg - 3 + (i % 3) * 2, 0.4, 1.2, 30, 100)
      : toward(v, d.pkg - 14 - ((i - P) >> 2) * 2, 0.4, 0.3, 30, 100));
    // E-cores in one cluster report the same sensor.
    for (let i = P; i < P + E; i++) d.temps[i] = d.temps[P + ((i - P) & ~3)];
    const g = d.gpu;
    g.load = toward(g.load, gpuBurst ? 94 : 6, 0.3, 4, 0, 100);
    g.temp = toward(g.temp, 33 + g.load * 0.38, 0.12, 0.4, 25, 95);
    g.power = toward(g.power, 22 + g.load * 2.2, 0.4, 6, 15, 250);
    g.clock = toward(g.clock, g.load > 15 ? 2600 : 210, 0.6, 20, 210, 2800);
    g.fan = g.temp > 55 ? toward(g.fan, 30 + (g.temp - 55) * 2, 0.3, 1, 0, 100) : Math.max(0, g.fan - 4);
    g.vram = toward(g.vram, (gpuBurst ? 7.2 : 1.8) * GIB, 0.2, 0.05 * GIB, GIB, 12 * GIB);
    d.ram = toward(d.ram, (cpuBurst ? 10.4 : 8.8) * GIB, 0.15, 0.05 * GIB, 2 * GIB, 15.5 * GIB);
    const down = netBurst ? rand(38e6, 52e6) : rand(15e3, 220e3);
    const up = netBurst ? rand(0.6e6, 1.1e6) : rand(4e3, 60e3);
    d.rx += down;
    d.tx += up;
    const read = diskBurst ? rand(300e6, 900e6) : rand(0, 2e6), write = diskBurst ? rand(50e6, 200e6) : rand(0, 1e6);
    return {
      cpu, threads: d.threads, clock_ghz: d.clock, cpu_temp: d.pkg, core_temps: d.temps, ram_used: d.ram,
      core_clocks: d.threads.filter((_, i) => i < 2 * P ? i % 2 === 0 : true).map(v => (v > 20 ? 4800 : 3000) + rand(-80, 80)),
      cpu_vid: 1.2 + cpu / 600, cpu_power: d.power, cpu_pl1: 125, cpu_pl2: 180, cpu_limit: d.power > 120 ? 'power' : null,
      ram_commit: d.ram * 1.5, ram_commit_limit: 24 * GIB, ram_cached: 3.8 * GIB,
      net_down: down, net_up: up, net_rx: d.rx, net_tx: d.tx,
      adapters: [{ name: 'Ethernet', down, up, rx: d.rx, tx: d.tx }],
      disks: [
        { mount: 'C:', label: 'System', kind: 'SSD', fs: 'NTFS', removable: false, total: 1000e9, free: 412e9, read, write },
        { mount: 'D:', label: 'Data', kind: 'HDD', fs: 'NTFS', removable: false, total: 2000e9, free: 930e9, read: read / 4, write: 0 },
      ],
      gpu: { load: g.load, temp: g.temp, vram_used: g.vram, power_w: g.power, power_limit_w: 250, clock_mhz: g.clock, mem_clock_mhz: g.load > 15 ? 10000 : 405,
        fan: g.fan, encoder: 0, decoder: 0, pcie: g.load > 15 ? 'Gen 4 ×16' : 'Gen 1 ×16', held_by: g.load > 90 ? ['power limit'] : [] },
      detail: {
        top_cpu: procs, top_ram: procs, memory,
        modules: [1, 2].map(n => ({ slot: 'DIMM ' + n, size_mb: 8192, kind: 'DDR5', speed: 5600, configured: 5600, maker: 'Example', part: 'EX-DDR5-8G' })),
        adapters: [{ name: 'Ethernet', description: 'Demo network adapter', mac: '00-00-00-00-00-00', link_bps: 1e9, ips: ['192.0.2.20'] }],
      },
    };
  }
  return { info, next };
}

// ---------- start ----------

async function boot() {
  bindCharts(document);
  go(location.hash.slice(1));
  if (tauri) {
    text('source', 'Live');
    setup(await invoke('get_info'));
    applySettings(await invoke('get_settings'));
    await tauri.event.listen('settings', e => applySettings(e.payload));
    await tauri.event.listen('navigate', e => { location.hash = e.payload; });
    setupAutostart();
    const history = await invoke('get_history');
    history.forEach(s => record(s, info));
    if (history.length) { last = history.at(-1); render(last); }
    await tauri.event.listen('sample', e => onSample(e.payload));
    const wanted = await invoke('take_page');
    if (wanted) location.hash = wanted;
  } else {
    text('source', 'Demo data');
    const demo = demoSource();
    setup(demo.info);
    applySettings(settings);
    for (let i = 0; i < HISTORY; i++) { last = demo.next(); record(last, demo.info); }
    render(last);
    setInterval(() => onSample(demo.next()), 1000);
  }
}

document.addEventListener('visibilitychange', () => { if (!document.hidden && last) render(last); });
new ResizeObserver(() => drawAll()).observe(document.body);
boot().catch(e => { text('source', 'Not connected: ' + e); });

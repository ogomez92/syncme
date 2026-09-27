'use strict';

const $ = (id) => document.getElementById(id);
let state = null;
let ws = null;
let refreshTimer = null;
const lastRendered = new Map();

// ---------------------------------------------------------------- helpers

function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === null || v === undefined || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k === 'text') el.textContent = v;
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else el.setAttribute(k, v === true ? '' : v);
  }
  for (const c of children.flat()) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

const vh = (text) => h('span', { class: 'visually-hidden', text });
const enc = encodeURIComponent;

async function api(path, body) {
  const opts = body === undefined ? {} : {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  };
  const r = await fetch(path, opts);
  let data = {};
  try { data = await r.json(); } catch { /* empty body */ }
  if (!r.ok) throw new Error(data.error || `Request failed (${r.status})`);
  return data;
}

function fmtSize(b) {
  if (b < 1024) return `${b} bytes`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let v = b / 1024, i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}

function fmtTime(ms) {
  const d = new Date(ms);
  const today = new Date().toDateString() === d.toDateString();
  return today ? d.toLocaleTimeString() : d.toLocaleString();
}

function plural(n, one, many) { return `${n} ${n === 1 ? one : many}`; }

function isFocused() { return document.visibilityState === 'visible' && document.hasFocus(); }

// ---------------------------------------------------------------- live region

const live = $('live');
let liveQueue = [];
let liveTimer = null;

// Content outside an open modal dialog is inert and would not be read,
// so the live region moves into whichever dialog is open.
function liveHost() {
  const open = [...document.querySelectorAll('dialog[open]')];
  return open.length ? open[open.length - 1] : document.body;
}

function announce(text) {
  liveQueue.push(text);
  clearTimeout(liveTimer);
  liveTimer = setTimeout(() => {
    const host = liveHost();
    if (live.parentElement !== host) host.append(live);
    const msg = liveQueue.join('. ');
    liveQueue = [];
    live.textContent = '';
    setTimeout(() => { live.textContent = msg; }, 60);
  }, 300);
}

// ---------------------------------------------------------------- connection

function reportVisibility() {
  if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ visible: isFocused() }));
}
window.addEventListener('focus', reportVisibility);
window.addEventListener('blur', reportVisibility);
document.addEventListener('visibilitychange', reportVisibility);

function connect() {
  ws = new WebSocket(`ws://${location.host}/api/ws`);
  ws.onopen = () => {
    $('conn-error').hidden = true;
    reportVisibility();
    refresh();
  };
  ws.onmessage = (m) => {
    let ev;
    try { ev = JSON.parse(m.data); } catch { return; }
    if (ev.type === 'event' && isFocused() && !(state && state.settings.announce_when_visible)) announce(ev.text);
    scheduleRefresh();
  };
  ws.onclose = () => {
    const banner = $('conn-error');
    if (banner.hidden) {
      banner.textContent = 'Lost the connection to SyncMe. It may have been closed. Trying again…';
      banner.hidden = false;
    }
    setTimeout(connect, 2000);
  };
}

function scheduleRefresh() {
  if (refreshTimer) return;
  refreshTimer = setTimeout(() => { refreshTimer = null; refresh(); }, 250);
}

async function refresh() {
  try {
    state = await api('/api/state');
    render();
  } catch (e) {
    console.error(e);
  }
}

// ---------------------------------------------------------------- rendering

/** Replaces a container's children only when its data changed, keeping focus on the same control. */
function renderList(container, key, build) {
  const k = JSON.stringify(key);
  if (lastRendered.get(container.id) === k) return;
  lastRendered.set(container.id, k);
  const active = document.activeElement;
  const activeId = container.contains(active) ? active.id : null;
  container.replaceChildren(...build());
  if (activeId) {
    const again = document.getElementById(activeId);
    if (again) again.focus();
    else {
      const heading = container.closest('section')?.querySelector('h2');
      if (heading) { heading.tabIndex = -1; heading.focus(); }
    }
  }
}

function render() {
  const s = state;
  $('me-line').textContent = `This computer: ${s.me.name}`;
  $('status-line').textContent = `Status: ${s.status.text}`;
  document.title = s.status.busy ? 'Syncing | SyncMe' : 'SyncMe';

  $('no-shares').hidden = s.shares.length > 0;
  renderList($('share-list'), s.shares, () => s.shares.map(shareCard));

  $('transfers-section').hidden = s.transfers.length === 0;
  renderList($('transfer-list'), s.transfers.map((t) => [t.path, t.peer, t.upload, Math.floor((t.done / Math.max(t.total, 1)) * 20)]), () => s.transfers.map((t) => {
    const pct = t.total ? Math.round((t.done / t.total) * 100) : 0;
    const label = `${t.upload ? 'Sending' : 'Receiving'} ${t.path} ${t.upload ? 'to' : 'from'} ${t.peer} (${t.share})`;
    return h('li', {}, label, ' ', h('progress', { max: '100', value: String(pct), 'aria-label': `${label}: ${pct}%` }));
  }));

  $('incoming-block').hidden = s.incoming.length === 0;
  renderList($('incoming-list'), s.incoming, () => s.incoming.map((d) => h('li', {},
    h('span', {}, h('strong', { text: d.name }), ` (${osName(d.os)}, ${d.addr}) wants to pair with this computer.`),
    h('span', { class: 'buttons' },
      h('button', { type: 'button', class: 'primary', id: `inc-${d.id}-accept`, onclick: () => act(`/api/pair/accept`, { id: d.id }) }, 'Accept', vh(` pairing with ${d.name}`)),
      h('button', { type: 'button', id: `inc-${d.id}-decline`, onclick: () => act(`/api/pair/decline`, { id: d.id }) }, 'Decline', vh(` pairing with ${d.name}`))))));

  $('no-devices').hidden = s.devices.length > 0;
  renderList($('device-list'), s.devices, () => s.devices.map((d) => h('li', {},
    h('span', {}, h('strong', { text: d.name }), ` (${osName(d.os)}) `,
      h('span', { class: d.online ? 'state-ok' : 'muted', text: d.online ? `✓ Connected at ${d.addr}` : '○ Offline' })),
    h('button', { type: 'button', class: 'danger', id: `dev-${d.id}-unpair`, onclick: () => unpair(d) }, 'Unpair', vh(` ${d.name}`)))));

  $('no-discovered').hidden = s.discovered.length > 0;
  $('no-discovered').textContent = 'No other SyncMe devices found yet. Searching the local network and Tailscale…';
  renderList($('discovered-list'), [s.discovered, s.outgoing], () => s.discovered.map((d) => {
    const sent = s.outgoing.includes(d.id);
    return h('li', {},
      h('span', {}, h('strong', { text: d.name }), ` (${osName(d.os)}, found via ${d.via} at ${d.addr})`,
        sent ? h('span', { class: 'muted', text: ` Request sent: accept it in SyncMe on ${d.name}.` }) : null),
      h('button', { type: 'button', class: sent ? '' : 'primary', id: `disc-${d.id}-pair`, onclick: () => act('/api/pair', { id: d.id }) },
        sent ? 'Send request again' : 'Pair', vh(` with ${d.name}`)));
  }));

  $('no-activity').hidden = s.activity.length > 0;
  renderList($('activity-list'), s.activity.map((a) => a.time_ms), () => s.activity.map((a) =>
    h('li', { class: a.level === 'warning' ? 'level-warning' : '' },
      h('time', { datetime: new Date(a.time_ms).toISOString(), text: fmtTime(a.time_ms) }),
      a.level === 'warning' ? '⚠ ' : '', a.text)));

  fillSettings();
}

function osName(os) {
  return { windows: 'Windows', macos: 'Mac', linux: 'Linux' }[os] || os;
}

function replicaState(r) {
  if (!r.local) {
    return r.online ? { text: '✓ Device connected', cls: 'state-ok' } : { text: '○ Device offline; syncs when it connects', cls: 'muted' };
  }
  const st = r.status;
  if (!st) return { text: 'Starting', cls: 'muted' };
  const info = `${plural(st.files, 'file', 'files')}, ${fmtSize(st.bytes)}`;
  switch (st.state) {
    case 'scanning': return { text: `Checking for changes (${info})`, cls: 'muted' };
    case 'syncing': return { text: `Syncing (${info})`, cls: 'muted' };
    case 'error': return { text: `⚠ Problem: ${st.error}`, cls: 'state-error' };
    case 'missing': return { text: `⚠ Not available: ${st.error}`, cls: 'state-error' };
    default: return { text: `✓ Up to date (${info}${st.last_sync_ms ? `, checked ${fmtTime(st.last_sync_ms)}` : ''})`, cls: 'state-ok' };
  }
}

function shareCard(sh) {
  const locations = h('ul', { class: 'locations' }, sh.replicas.map((r) => {
    const st = replicaState(r);
    return h('li', {},
      h('strong', { text: r.local ? `${r.node_name} (this computer)` : r.node_name }), ': ',
      h('span', { class: 'path', text: r.path }), '. ',
      h('span', { class: st.cls, text: st.text }),
      r.local ? h('span', { class: 'buttons' },
        h('button', { type: 'button', id: `rep-${r.id}-open`, onclick: () => act(`/api/replicas/${enc(r.id)}/open`, {}) }, 'Open', vh(` ${r.path}`)),
        r.status && r.status.state === 'missing'
          ? h('button', { type: 'button', id: `rep-${r.id}-reset`, onclick: () => resetReplica(r) }, 'Reset', vh(` ${r.path}`))
          : null) : null);
  }));
  return h('li', {},
    h('h3', { text: sh.name }),
    locations,
    h('div', { class: 'buttons' },
      h('button', { type: 'button', id: `share-${sh.id}-edit`, onclick: (e) => openFolderDialog(sh, e.currentTarget) }, 'Edit…', vh(` ${sh.name}`)),
      h('button', { type: 'button', id: `share-${sh.id}-rescan`, onclick: () => act(`/api/shares/${enc(sh.id)}/rescan`, {}, `Checking ${sh.name} now`) }, 'Sync now', vh(` ${sh.name}`)),
      h('button', { type: 'button', class: 'danger', id: `share-${sh.id}-delete`, onclick: () => stopShare(sh) }, 'Stop syncing', vh(` ${sh.name}`))));
}

async function act(path, body, okMessage) {
  try {
    await api(path, body);
    if (okMessage) announce(okMessage);
    scheduleRefresh();
  } catch (e) {
    announce(e.message);
    alert(e.message);
  }
}

function unpair(d) {
  if (confirm(`Unpair ${d.name}? Folders stop syncing with it. No files are deleted.`)) act(`/api/peers/${enc(d.id)}/unpair`, {});
}

function stopShare(sh) {
  if (confirm(`Stop syncing ${sh.name} on all devices? No files are deleted; each copy stays where it is.`)) act(`/api/shares/${enc(sh.id)}/delete`, {});
}

function resetReplica(r) {
  if (confirm(`Reset ${r.path}? SyncMe forgets its history and merges it again with the other copies. Nothing is deleted anywhere; files that differ are kept as conflict copies.`)) {
    act(`/api/replicas/${enc(r.id)}/reset`, {});
  }
}

// ---------------------------------------------------------------- settings

let settingsFilled = false;
function fillSettings() {
  const form = $('settings-form');
  const nameInput = $('device-name');
  if (document.activeElement !== nameInput && !settingsFilled) nameInput.value = state.me.name;
  if (form.contains(document.activeElement) && settingsFilled) return;
  const s = state.settings;
  $('set-announce').checked = s.announce;
  $('set-announce-visible').checked = s.announce_when_visible;
  $('set-tts').checked = s.allow_tts;
  $('set-login').checked = s.start_at_login;
  $('set-browser').checked = s.open_browser_on_start;
  $('set-trash').value = s.trash_days;
  settingsFilled = true;
}

$('settings-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const days = parseInt($('set-trash').value, 10);
  if (!(days >= 1 && days <= 3650)) {
    $('set-trash').setAttribute('aria-invalid', 'true');
    announce('Enter a number of days between 1 and 3650.');
    $('set-trash').focus();
    return;
  }
  $('set-trash').removeAttribute('aria-invalid');
  const body = {
    announce: $('set-announce').checked,
    announce_when_visible: $('set-announce-visible').checked,
    allow_tts: $('set-tts').checked,
    start_at_login: $('set-login').checked,
    open_browser_on_start: $('set-browser').checked,
    trash_days: days,
  };
  try {
    await api('/api/settings', body);
    announce('Settings saved');
    scheduleRefresh();
  } catch (err) {
    announce(err.message);
  }
});

$('name-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  try {
    await api('/api/name', { name: $('device-name').value });
    announce('Name saved');
    scheduleRefresh();
  } catch (err) {
    announce(err.message);
  }
});

$('add-peer-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const input = $('peer-addr');
  const err = $('peer-addr-error');
  err.hidden = true;
  input.removeAttribute('aria-invalid');
  announce('Looking for SyncMe at that address…');
  try {
    const r = await api('/api/peers/add', { address: input.value });
    input.value = '';
    announce(`Found ${r.name}. Use the Pair button next to it under Found nearby.`);
    scheduleRefresh();
  } catch (ex) {
    err.textContent = ex.message;
    err.hidden = false;
    input.setAttribute('aria-invalid', 'true');
    input.focus();
  }
});

// ---------------------------------------------------------------- folder dialog

const folderDialog = $('folder-dialog');
let editing = null;
let folderOpener = null;
let rowCounter = 0;

function allDevices() {
  const me = { id: state.me.id, name: state.me.name, os: state.me.os, online: true, isMe: true };
  const list = [me, ...state.devices.map((d) => ({ ...d, isMe: false }))];
  if (editing) {
    for (const r of editing.replicas) {
      if (!list.some((d) => d.id === r.node)) list.push({ id: r.node, name: r.node_name, os: '', online: false, isMe: false, unpaired: true });
    }
  }
  return list;
}

function baseName() {
  const first = [...document.querySelectorAll('#device-fieldsets input.path-input')].find((i) => i.dataset.node === state.me.id && i.value.trim());
  if (first) {
    const parts = first.value.trim().split(/[\\/]+/).filter(Boolean);
    if (parts.length) return parts[parts.length - 1];
  }
  return $('folder-name').value.trim() || 'SyncMe';
}

function openFolderDialog(share, opener) {
  editing = share;
  folderOpener = opener;
  $('folder-dialog-title').textContent = share ? `Edit folder: ${share.name}` : 'Add folder';
  $('folder-name').value = share ? share.name : '';
  $('folder-name').removeAttribute('aria-invalid');
  $('folder-error').hidden = true;
  const holder = $('device-fieldsets');
  holder.replaceChildren();
  for (const dev of allDevices()) holder.append(deviceFieldset(dev, share));
  folderDialog.showModal();
  $('folder-name').focus();
}

function deviceFieldset(dev, share) {
  const existing = share ? share.replicas.filter((r) => r.node === dev.id) : [];
  const legendText = dev.isMe ? `${dev.name} (this computer)` : `${dev.name}${dev.unpaired ? ' (not paired anymore)' : dev.online ? '' : ' (offline)'}`;
  const rows = h('div', { class: 'rows-holder', id: `rows-${dev.id}` });
  const addBtn = h('button', { type: 'button', id: `add-row-${dev.id}` }, 'Add another folder', vh(` on ${dev.name}`));
  const body = h('div', { class: 'device-body', id: `body-${dev.id}` }, rows, addBtn);
  const fs = h('fieldset', { 'data-node': dev.id }, h('legend', { text: legendText }));

  addBtn.addEventListener('click', () => {
    const input = addRow(dev, rows, null);
    relabel(dev, rows);
    input.focus();
  });

  if (dev.isMe) {
    fs.append(h('p', { class: 'hint', text: 'Folders on this computer. Add a second one to keep two drives or folders in sync.' }));
    if (!existing.length) addRow(dev, rows, null);
  } else {
    const cb = h('input', { type: 'checkbox', id: `dev-${dev.id}-on`, 'aria-controls': `body-${dev.id}` });
    cb.checked = existing.length > 0;
    body.hidden = !cb.checked;
    cb.addEventListener('change', () => {
      body.hidden = !cb.checked;
      if (cb.checked && !rows.children.length) {
        const input = addRow(dev, rows, null);
        relabel(dev, rows);
        suggestPath(dev, input);
      }
    });
    fs.append(h('div', { class: 'check' }, cb, h('label', { for: cb.id, text: `Sync to ${dev.name}` })));
    if (!dev.online && !dev.unpaired) fs.append(h('p', { class: 'hint', text: `${dev.name} is offline. You can type the path; it gets the folder when it connects.` }));
  }
  for (const r of existing) addRow(dev, rows, r);
  relabel(dev, rows);
  fs.append(body);
  return fs;
}

async function suggestPath(dev, input) {
  try {
    const r = await api(`/api/browse?node=${enc(dev.id)}&path=`);
    if (!input.value.trim() && r.home) input.value = `${r.home}${r.sep}${baseName()}`;
  } catch { /* offline: leave empty */ }
}

function addRow(dev, rows, rep) {
  const n = ++rowCounter;
  const inputId = `loc-${n}`;
  const errId = `loc-${n}-err`;
  const input = h('input', {
    type: 'text', id: inputId, class: 'path-input', spellcheck: 'false', autocomplete: 'off',
    'data-node': dev.id, 'aria-describedby': errId,
  });
  if (rep) { input.value = rep.path; input.dataset.replicaId = rep.id; }
  input.addEventListener('input', () => { input.removeAttribute('aria-invalid'); $(errId).hidden = true; });
  const label = h('label', { for: inputId });
  const browseBtn = h('button', { type: 'button' }, 'Browse…', h('span', { class: 'visually-hidden browse-vh' }));
  browseBtn.addEventListener('click', () => openBrowse(dev, input, browseBtn));
  const removeBtn = h('button', { type: 'button', class: 'remove-row' }, 'Remove', h('span', { class: 'visually-hidden remove-vh' }));
  const row = h('div', { class: 'location-row' },
    h('div', { class: 'field' }, label, input, h('p', { class: 'error-text', id: errId, hidden: true })),
    browseBtn, removeBtn);
  removeBtn.addEventListener('click', () => {
    row.remove();
    relabel(dev, rows);
    announce('Location removed');
    $(`add-row-${dev.id}`).focus();
  });
  rows.append(row);
  return input;
}

function relabel(dev, rows) {
  const list = [...rows.querySelectorAll('.location-row')];
  list.forEach((row, i) => {
    const name = list.length > 1 ? `Folder ${i + 1} on ${dev.name}` : `Folder on ${dev.name}`;
    row.querySelector('label').textContent = name;
    row.querySelector('.browse-vh').textContent = ` for ${name.toLowerCase().startsWith('folder') ? name.charAt(0).toLowerCase() + name.slice(1) : name}`;
    row.querySelector('.remove-vh').textContent = ` ${name.charAt(0).toLowerCase() + name.slice(1)}`;
    // This computer always needs at least one location.
    row.querySelector('.remove-row').hidden = dev.isMe && list.length === 1;
  });
}

$('folder-cancel').addEventListener('click', () => folderDialog.close());
folderDialog.addEventListener('close', () => {
  document.body.append(live);
  if (folderOpener && document.contains(folderOpener)) folderOpener.focus();
  else $('add-folder').focus();
});

$('folder-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const errors = [];
  const nameInput = $('folder-name');
  const name = nameInput.value.trim();
  nameInput.removeAttribute('aria-invalid');
  if (!name) { nameInput.setAttribute('aria-invalid', 'true'); errors.push({ el: nameInput, msg: 'Give the folder a name.' }); }

  const replicas = [];
  for (const fs of $('device-fieldsets').querySelectorAll('fieldset')) {
    const node = fs.dataset.node;
    const cb = $(`dev-${node}-on`);
    if (cb && !cb.checked) continue;
    for (const input of fs.querySelectorAll('input.path-input')) {
      const path = input.value.trim();
      const err = $(input.getAttribute('aria-describedby'));
      if (!path) {
        const msg = `Enter the ${input.labels[0].textContent.toLowerCase()}, or remove it.`;
        input.setAttribute('aria-invalid', 'true');
        err.textContent = msg;
        err.hidden = false;
        errors.push({ el: input, msg });
        continue;
      }
      replicas.push({ id: input.dataset.replicaId || null, node, path });
    }
  }
  const banner = $('folder-error');
  if (errors.length) {
    banner.textContent = errors.length === 1 ? errors[0].msg : `${errors.length} things need fixing. ${errors[0].msg}`;
    banner.hidden = false;
    errors[0].el.focus();
    return;
  }
  try {
    await api('/api/shares', { id: editing ? editing.id : null, name, replicas });
    folderDialog.close();
    scheduleRefresh();
  } catch (ex) {
    banner.textContent = ex.message;
    banner.hidden = false;
  }
});

$('add-folder').addEventListener('click', (e) => {
  if (!state) return;
  openFolderDialog(null, e.currentTarget);
});

// ---------------------------------------------------------------- folder browser

const browseDialog = $('browse-dialog');
let browseCtx = null;

function openBrowse(dev, input, opener) {
  browseCtx = { dev, input, opener, current: null };
  $('browse-title').textContent = `Choose a folder on ${dev.name}`;
  $('browse-list').replaceChildren();
  $('browse-roots').replaceChildren();
  browseDialog.showModal();
  loadBrowse(input.value.trim(), true);
}

async function loadBrowse(path, first) {
  const status = $('browse-status');
  status.textContent = 'Loading…';
  let r;
  try {
    r = await api(`/api/browse?node=${enc(browseCtx.dev.id)}&path=${enc(path)}`);
  } catch (e) {
    status.textContent = e.message;
    $('browse-path').focus();
    return;
  }
  if (r.error && first && path) {
    // A typed path that doesn't exist yet: start from the home folder instead.
    return loadBrowse('', false);
  }
  browseCtx.current = r;
  $('browse-path').value = r.path;
  const up = $('browse-up');
  up.setAttribute('aria-disabled', r.parent ? 'false' : 'true');
  $('browse-roots').replaceChildren(...[{ name: 'Home folder', path: r.home }, ...r.roots].map((d) =>
    h('li', {}, h('button', { type: 'button', onclick: () => loadBrowse(d.path) }, d.name))));
  $('browse-list').replaceChildren(...r.dirs.map((d) =>
    h('li', {}, h('button', { type: 'button', onclick: () => loadBrowse(d.path) }, d.name))));
  status.textContent = r.error ? r.error : `${r.path}: ${r.dirs.length ? plural(r.dirs.length, 'folder', 'folders') + ' inside' : 'no folders inside'}`;
  const firstItem = $('browse-list').querySelector('button');
  (firstItem || $('browse-choose')).focus();
}

$('browse-go').addEventListener('submit', (e) => { e.preventDefault(); loadBrowse($('browse-path').value.trim()); });
$('browse-up').addEventListener('click', () => {
  const c = browseCtx.current;
  if (c && c.parent) loadBrowse(c.parent);
});
$('browse-home').addEventListener('click', () => loadBrowse(''));
$('browse-cancel').addEventListener('click', () => browseDialog.close());
$('browse-choose').addEventListener('click', () => {
  const typed = $('browse-path').value.trim();
  const path = typed || (browseCtx.current && browseCtx.current.path) || '';
  browseCtx.input.value = path;
  browseCtx.input.dispatchEvent(new Event('input'));
  browseDialog.close();
  announce(`Chose ${path}`);
});
browseDialog.addEventListener('close', () => {
  if (folderDialog.open) folderDialog.append(live);
  if (browseCtx) browseCtx.input.focus();
});

connect();

'use strict';

// Opening index.html straight in a browser (no Tauri) gets a tiny stand-in
// backend so the layout can be previewed. The real app always has __TAURI__.
const TAURI = window.__TAURI__ || previewBackend();
const invoke = (cmd, args) => TAURI.core.invoke(cmd, args);
const listen = (event, cb) => TAURI.event.listen(event, cb);

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => Array.from(root.querySelectorAll(sel));

let S = null; // settings, mirrored from the backend
const live = {
  target: null, join: { active: false, team: null, clicks: 0 }, holding: false, reviving: false, rate: 10,
  party: { active: false }, builtInSpots: [], steamName: null,
};
let listeningAction = null;

// ---------------------------------------------------------------- key names

const KEY_NAMES = {
  1: 'Left click', 2: 'Right click', 4: 'Middle click', 5: 'Mouse 4', 6: 'Mouse 5',
  8: 'Backspace', 9: 'Tab', 13: 'Enter', 16: 'Shift', 17: 'Ctrl', 18: 'Alt', 19: 'Pause', 20: 'Caps Lock',
  27: 'Esc', 32: 'Space', 33: 'Page Up', 34: 'Page Down', 35: 'End', 36: 'Home',
  37: '←', 38: '↑', 39: '→', 40: '↓', 44: 'Print', 45: 'Insert', 46: 'Delete', 91: 'Win', 92: 'Win', 93: 'Menu',
  106: 'Num *', 107: 'Num +', 109: 'Num −', 110: 'Num .', 111: 'Num /', 144: 'Num Lock', 145: 'Scroll Lock',
  160: 'Left Shift', 161: 'Right Shift', 162: 'Left Ctrl', 163: 'Right Ctrl', 164: 'Left Alt', 165: 'Right Alt',
  186: ';', 187: '=', 188: ',', 189: '-', 190: '.', 191: '/', 192: '`', 219: '[', 220: '\\', 221: ']', 222: "'",
};

function baseKeyName(vk) {
  if (KEY_NAMES[vk]) return KEY_NAMES[vk];
  if ((vk >= 48 && vk <= 57) || (vk >= 65 && vk <= 90)) return String.fromCharCode(vk);
  if (vk >= 112 && vk <= 135) return 'F' + (vk - 111);
  if (vk >= 96 && vk <= 105) return 'Num ' + (vk - 96);
  return 'Key ' + vk;
}

/** Hotkeys are `vk | mods << 8` (1 Ctrl, 2 Alt, 4 Shift, 8 Win). */
function keyName(code) {
  if (!code) return 'None';
  const vk = code & 0xff, mods = code >> 8;
  const parts = [];
  if (mods & 1) parts.push('Ctrl');
  if (mods & 2) parts.push('Alt');
  if (mods & 4) parts.push('Shift');
  if (mods & 8) parts.push('Win');
  parts.push(baseKeyName(vk));
  return parts.join(' + ');
}

// ---------------------------------------------------------------- settings plumbing

const get = (path) => path.split('.').reduce((o, k) => (o == null ? o : o[k]), S);
const nest = (path, value) => path.split('.').reduceRight((acc, k) => ({ [k]: acc }), value);

function deepMerge(a, b) {
  for (const [k, v] of Object.entries(b)) {
    const plain = (x) => x && typeof x === 'object' && !Array.isArray(x);
    if (plain(v) && plain(a[k])) deepMerge(a[k], v);
    else a[k] = v;
  }
  return a;
}

let queue = Promise.resolve();
function commit(patch) {
  deepMerge(S, structuredClone(patch)); // optimistic
  render();
  queue = queue.then(async () => {
    try {
      S = await invoke('patch_settings', { patch });
      render();
    } catch (e) {
      toast(String(e));
    }
  });
  return queue;
}
const set = (path, value) => commit(nest(path, value));

function keyValue(action) {
  switch (action) {
    case 'join': return S.keys.join;
    case 'capture': return S.keys.capture;
    case 'sprint': return S.sprint.hotkey;
    case 'sprint_key': return S.sprint.sprint_key;
    case 'forward_key': return S.sprint.forward_key;
    case 'back_key': return S.sprint.back_key;
    case 'revive': return S.revive.hotkey;
    case 'revive_key': return S.revive.key;
    default: {
      const t = S.teams.find((x) => 'team:' + x.id === action);
      return t ? t.hotkey : 0;
    }
  }
}

// ---------------------------------------------------------------- helpers

const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
const selectedTeam = () => S.teams.find((t) => t.id === S.selected) || S.teams[0];
const teamById = (id) => S.teams.find((t) => t.id === id);
const following = () => live.party.active && !live.party.leader;
/** Where this team's JOIN spot comes from: saved here, the party leader, built in, or nowhere yet. */
function spotSource(t) {
  if (t.calibrated) return 'saved';
  if (live.builtInSpots.includes(t.id)) return 'auto';
  if (following() && (live.party.layout || []).includes(t.id)) return 'leader';
  return null;
}

let toastTimer;
function toast(msg) {
  const el = $('#toast');
  el.textContent = msg;
  el.classList.add('show');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.remove('show'), 2600);
}

const EMBLEMS = {
  lonestar: '<svg viewBox="0 0 24 24"><path class="fill" d="M12 2.6l2.8 5.7 6.2.9-4.5 4.4 1.1 6.2L12 16.9l-5.6 2.9 1.1-6.2L3 9.2l6.2-.9z"/></svg>',
  valkyra: '<svg viewBox="0 0 24 24"><path d="M12 6.5V20"/><path d="M12 9C9.2 5.2 5.4 4.3 2.5 4.8c1 3.2 3.4 5.3 7 5.7M12 9c2.8-3.8 6.6-4.7 9.5-4.2-1 3.2-3.4 5.3-7 5.7"/><path d="M12 13.2c-2.4-1.6-5.2-1.6-7.2-1 1 2.1 3.1 3.2 5.7 3.1M12 13.2c2.4-1.6 5.2-1.6 7.2-1-1 2.1-3.1 3.2-5.7 3.1"/></svg>',
  manticore: '<svg viewBox="0 0 24 24"><path d="M5.5 4.5c3.2 3.4 4.2 8.6 2.8 15"/><path d="M11.2 3.5c2.8 3.8 3.4 9.5 1.6 16"/><path d="M16.8 4.5c2.2 3.2 2.4 8.4.2 14"/></svg>',
};
const CHECK = '<svg viewBox="0 0 12 12"><path d="M2.5 6.2l2.3 2.3 4.7-5"/></svg>';

// ---------------------------------------------------------------- render

function render() {
  if (!S) return;
  renderGame();
  renderTeams();
  renderJoin();
  renderParty();
  renderKeys();
  $$('.switch[data-bind]').forEach((el) => {
    const on = !!get(el.dataset.bind);
    el.classList.toggle('on', on);
    el.setAttribute('aria-checked', on);
  });
  $$('.key[data-rebind]').forEach((el) => {
    const listening = el.dataset.rebind === listeningAction;
    const code = keyValue(el.dataset.rebind);
    el.classList.toggle('listening', listening);
    el.classList.toggle('none', !code && !listening);
    el.textContent = listening ? 'Press…' : keyName(code);
  });
  $('#btnPin').classList.toggle('on', S.general.always_on_top);
  $('#capKey').textContent = keyName(S.keys.capture);
}

function renderGame() {
  const t = live.target;
  const pill = $('#gamePill');
  pill.className = 'game-pill ' + (t ? t.state : '');
  $('#gameText').textContent = !t ? 'Waiting'
    : t.state === 'front' ? 'WARDOGS'
    : t.state === 'back' ? 'In background'
    : 'Game closed';
  pill.title = t ? `${t.title || t.process} · ${t.process}` : 'Waiting for WARDOGS';
}

function renderTeams() {
  const box = $('#teams');
  if (box.dataset.built !== '1') {
    box.dataset.built = '1';
    box.innerHTML = S.teams.map((t) => `
      <button class="glass team" data-team="${esc(t.id)}" style="--tc:${esc(t.color)}">
        <span class="check">${CHECK}</span>
        <span class="emblem">${EMBLEMS[t.id] || ''}</span>
        <span class="tname">${esc(t.name)}</span>
        <span class="key sm" data-rebind="team:${esc(t.id)}" role="button" title="Key that joins ${esc(t.name)}"></span>
        <span class="tstate"></span>
      </button>`).join('');
  }
  for (const t of S.teams) {
    const card = $(`.team[data-team="${t.id}"]`, box);
    card.classList.toggle('on', t.id === S.selected);
    card.classList.toggle('locked', following());
    const state = $('.tstate', card);
    const joining = live.join.active && live.join.team === t.id;
    const src = spotSource(t);
    state.textContent = joining ? 'Joining…'
      : following() && t.id === S.selected ? "Leader's pick"
      : src === 'saved' ? 'Ready'
      : src === 'leader' ? 'Spot from leader'
      : src === 'auto' ? 'Ready · auto'
      : 'Not set up';
    state.classList.toggle('ok', !!src && !joining);
  }
}

function renderParty() {
  const p = live.party;
  $('#partySolo').hidden = p.active;
  $('#partyOn').hidden = !p.active;
  const status = $('#partyStatus');
  status.className = 'party-status ' + (!p.active ? '' : p.online ? 'online' : 'offline');
  $('span', status).textContent = !p.active ? 'Join as a group' : p.online ? 'Synced' : 'Connecting…';

  const name = $('#nameInput');
  name.placeholder = live.steamName || 'Your name';
  if (document.activeElement !== name) name.value = S.general.name;
  if (!p.active) return;

  $('#partyCode').textContent = p.code.slice(0, 3) + '-' + p.code.slice(3);
  const teamName = (teamById(p.leader ? S.selected : p.team) || selectedTeam()).name;
  $('#partyRole').innerHTML = p.leader
    ? `You're the <b>leader</b>. Your team pick and JOIN go to everyone.`
    : `Following <b>${esc(p.leader_name || 'the leader')}</b>. When they press JOIN, you join <b>${esc(teamName)}</b> as soon as your team screen is open.`;

  const crown = '<svg viewBox="0 0 16 16"><path class="fill" d="M2 5l3 2.5L8 3l3 4.5L14 5l-1.2 7H3.2z"/></svg>';
  $('#members').innerHTML = (p.members || []).map((m) => `
    <div class="member ${m.leader ? 'leader' : ''}">
      <span class="avatar">${esc((m.name || '?').trim().charAt(0).toUpperCase() || '?')}</span>
      <span class="mname">${esc(m.name || 'Player')}${m.me ? '<small>you</small>' : ''}</span>
      ${m.leader ? `<span class="crown">${crown}Leader</span>`
        : p.leader && !m.me ? `<button class="btn sm ghost" data-lead="${esc(m.id)}">Make leader</button>` : ''}
    </div>`).join('');
}

function renderJoin() {
  const sel = selectedTeam();
  const j = live.join;
  const joining = j.active ? teamById(j.team) || sel : null;
  const team = joining || sel;
  const btn = $('#btnJoin');
  btn.style.setProperty('--tc', team.color);
  btn.classList.toggle('active', !!joining);
  $('#joinLabel').textContent = joining ? 'STOP' : `JOIN ${team.name.toUpperCase()}`;
  $('#joinKey').textContent = keyName(S.keys.join);
  $('#joinKey').hidden = !S.keys.join;
  $('#joinSub').textContent = joining
    ? `Joining ${team.name} · ${j.clicks.toLocaleString()} clicks`
    : spotSource(team) ? (live.party.active && live.party.leader ? 'Your whole party joins with you' : `Clicks until you're in, then stops`)
    : `First, show it where ${team.name}'s JOIN button is`;

  const note = $('#joinNote');
  const src = spotSource(team);
  if (joining) {
    note.textContent = `Press ${keyName(S.keys.join)} or move the mouse to stop.`;
  } else if (following()) {
    note.textContent = src
      ? `Your leader starts it for everyone. You can still press ${keyName(S.keys.join)} yourself.`
      : `Set ${team.name}'s JOIN spot once so you can follow your leader automatically.`;
  } else if (src && src !== 'saved') {
    note.innerHTML = `Using ${src === 'leader' ? "your leader's" : 'the built-in'} spot for ${esc(team.name)}. Clicks only while the team screen is open. <button class="link" data-reposition="${esc(team.id)}">Set my own</button>`;
  } else if (team.calibrated) {
    note.innerHTML = `${live.rate} clicks a second on ${esc(team.name)}'s JOIN button. Stops when you're in, when you move the mouse, or after 10 minutes. <button class="link" data-reposition="${esc(team.id)}">Set spot again</button>`;
  } else {
    note.textContent = `The first time, you'll click ${team.name}'s JOIN button once in WARDOGS so the app knows where it is.`;
  }
}

/** One list of every key: what it does, what it's bound to, click to change. */
function renderKeys() {
  const box = $('#keys');
  const rev = (S.revive.hold_ms / 1000).toFixed(1).replace(/\.0$/, '');
  const rows = [
    { a: 'join', t: 'Join selected team', s: 'Start / stop' },
    ...S.teams.map((t) => ({ a: 'team:' + t.id, t: `Join ${t.name}`, s: 'Picks the team and starts', dot: t.color })),
    S.sprint.mode === 'run'
      ? { a: 'sprint', t: 'Auto-run on / off', s: `Holds ${keyName(S.sprint.forward_key)} + ${keyName(S.sprint.sprint_key)} so you sprint forward`, badge: 'sprint', group: true }
      : { a: 'sprint', t: 'Auto sprint on / off', s: `Holds ${keyName(S.sprint.sprint_key)} while you hold ${keyName(S.sprint.forward_key)}`, badge: 'sprint', group: true },
    { a: 'sprint_key', t: 'Sprint key', s: 'Held for you' },
    { a: 'forward_key', t: 'Forward key', s: S.sprint.mode === 'run' ? 'Held for you · tapping it stops auto-run' : undefined },
    ...(S.sprint.mode === 'run' ? [{ a: 'back_key', t: 'Stop key', s: 'Tapping it stops auto-run' }] : []),
    { a: 'revive', t: 'Auto revive', s: `Holds ${keyName(S.revive.key)} for ${rev} s · press again to cancel`, badge: 'revive', group: true, stepper: true },
    { a: 'revive_key', t: 'Revive key' },
    { a: 'capture', t: 'Save JOIN spot', s: 'While setting a team up', group: true },
  ];
  const sig = rows.map((r) => r.a + r.t + (r.s || '')).join('|');
  if (box.dataset.sig !== sig) {
    box.dataset.sig = sig;
    box.innerHTML = rows.map((r) => `
      <div class="krow ${r.group ? 'group' : ''}" ${r.dot ? `style="--tc:${esc(r.dot)}"` : ''}>
        ${r.dot ? '<span class="tdot"></span>' : ''}
        <div class="kl"><span class="kt">${esc(r.t)}</span>${r.s ? `<span class="ks">${esc(r.s)}</span>` : ''}</div>
        ${r.stepper ? `<span class="stepper" title="How long to hold ${esc(keyName(S.revive.key))}"><button data-hold="-1" aria-label="Shorter">−</button><b>${rev}s</b><button data-hold="1" aria-label="Longer">+</button></span>` : ''}
        ${r.badge ? `<span class="badge" data-badge="${r.badge}"></span>` : ''}
        <button class="key" data-rebind="${esc(r.a)}"></button>
      </div>`).join('');
  }
  const sprint = $('[data-badge="sprint"]', box);
  sprint.className = 'badge ' + (live.holding ? 'hot' : S.sprint.enabled ? 'on' : '');
  sprint.textContent = live.holding ? (S.sprint.mode === 'run' ? 'Running' : 'Sprinting') : S.sprint.enabled ? 'On' : 'Off';
  const revive = $('[data-badge="revive"]', box);
  revive.className = 'badge ' + (live.reviving ? 'hot' : '');
  revive.textContent = live.reviving ? 'Reviving' : 'Ready';
}

// ---------------------------------------------------------------- picker & sheet

function showPicker(active, cancellable) {
  document.body.classList.toggle('picking', active);
  $('#pickerCancel').hidden = !cancellable;
  if (active) setHover({ ok: false });
}

function setHover(p) {
  $('#hoverPill').classList.toggle('ok', !!p.ok);
  const text = $('#hoverText');
  if (p.ok) {
    const name = p.title || p.process.replace(/\.exe$/i, '');
    text.innerHTML = `<b>${esc(name)}</b> · ${esc(p.process)} · click to use it`;
  } else {
    text.textContent = p.own ? 'That’s this app. Click the game instead'
      : p.shell ? 'Taskbar or desktop. Open WARDOGS'
      : 'Or click the game window to use it';
  }
}

function showSheet(p) {
  if (p.kind === 'key') {
    listeningAction = p.action;
    render();
    toast(p.text);
    return;
  }
  $('#sheetTitle').textContent = p.title;
  $('#sheetText').textContent = p.text;
  $('#livePos').textContent = 'Move your mouse';
  $('#sheetLive').classList.remove('outside');
  $('#sheet').classList.add('show');
}

function hideSheet() {
  listeningAction = null;
  $('#sheet').classList.remove('show');
  render();
}

// ---------------------------------------------------------------- wiring

function wire() {
  document.addEventListener('click', (e) => {
    const key = e.target.closest('.key[data-rebind]');
    if (key) {
      if (listeningAction === key.dataset.rebind) return invoke('cancel_capture');
      return invoke('start_rebind', { action: key.dataset.rebind });
    }
    // Clicking anywhere else while waiting for a key cancels it.
    if (listeningAction) invoke('cancel_capture');

    const team = e.target.closest('.team[data-team]');
    if (team) {
      if (following()) return toast('Your party leader picks the team');
      return set('selected', team.dataset.team);
    }
    const lead = e.target.closest('[data-lead]');
    if (lead) return invoke('party_make_leader', { id: lead.dataset.lead });
    const hold = e.target.closest('[data-hold]');
    if (hold) {
      const ms = Math.min(60000, Math.max(1000, Math.round(S.revive.hold_ms / 1000 + +hold.dataset.hold) * 1000));
      return set('revive.hold_ms', ms);
    }
    const repos = e.target.closest('[data-reposition]');
    if (repos) return invoke('set_position', { id: repos.dataset.reposition }).catch((err) => toast(String(err)));
    const sw = e.target.closest('.switch[data-bind]');
    if (sw) return set(sw.dataset.bind, !get(sw.dataset.bind));
  });

  $('#btnJoin').addEventListener('click', () => invoke('toggle_join'));

  // Party
  const name = $('#nameInput');
  name.addEventListener('change', () => set('general.name', name.value.trim()));
  name.addEventListener('keydown', (e) => { if (e.key === 'Enter') name.blur(); });
  const code = $('#codeInput');
  code.addEventListener('input', () => {
    const raw = code.value.toUpperCase().replace(/[^0-9A-Z]/g, '').slice(0, 6);
    code.value = raw.length > 3 ? raw.slice(0, 3) + '-' + raw.slice(3) : raw;
  });
  const joinParty = () => invoke('party_join', { code: code.value })
    .then((c) => { code.value = ''; toast(`Joined party ${c.slice(0, 3)}-${c.slice(3)}`); })
    .catch((err) => toast(String(err)));
  code.addEventListener('keydown', (e) => { if (e.key === 'Enter') joinParty(); });
  $('#btnJoinParty').addEventListener('click', joinParty);
  $('#btnCreate').addEventListener('click', () => invoke('party_create').then((c) => toast(`Party ${c.slice(0, 3)}-${c.slice(3)} created. Share the code`)));
  $('#btnLeave').addEventListener('click', () => invoke('party_leave'));
  $('#btnCopyCode').addEventListener('click', async () => {
    const text = $('#partyCode').textContent;
    try { await navigator.clipboard.writeText(text); toast(`Copied ${text}. Paste it to your friends`); }
    catch { toast(`Code: ${text}`); }
  });
  $('#btnChangeGame').addEventListener('click', () => invoke('select_target'));
  $('#pickerCancel').addEventListener('click', () => invoke('cancel_capture'));
  $('#sheetCancel').addEventListener('click', () => invoke('cancel_capture'));
  $('#btnClose').addEventListener('click', () => invoke('win_close'));
  $('#btnMin').addEventListener('click', () => invoke('win_minimize'));
  $('#btnPin').addEventListener('click', () => set('general.always_on_top', !S.general.always_on_top));

  document.addEventListener('contextmenu', (e) => { if (!e.target.closest('input')) e.preventDefault(); });
  document.addEventListener('keydown', (e) => {
    // No browser reload / find / print inside the app.
    if ((e.ctrlKey && ['r', 'f', 'p', 'g', 'u'].includes(e.key.toLowerCase())) || e.key === 'F5') e.preventDefault();
  });
}

async function boot() {
  let state = null;
  for (let i = 0; i < 60 && !state; i++) {
    try { state = await invoke('get_state'); } catch { await new Promise((r) => setTimeout(r, 50)); }
  }
  if (!state) return;
  S = state.settings;
  Object.assign(live, {
    target: state.target, join: state.join, holding: state.holding, reviving: state.reviving, rate: state.rate,
    party: state.party, builtInSpots: state.shared_spots || [], steamName: state.steam_name,
  });

  wire();
  await listen('settings', (e) => { S = e.payload; render(); });
  await listen('join', (e) => { live.join = e.payload; renderTeams(); renderJoin(); });
  await listen('sprint', (e) => { live.holding = e.payload.holding; renderKeys(); });
  await listen('revive', (e) => { live.reviving = e.payload.active; renderKeys(); });
  await listen('party', (e) => { live.party = e.payload; renderParty(); renderTeams(); renderJoin(); });
  await listen('toast', (e) => toast(e.payload));
  await listen('picker', (e) => showPicker(e.payload.active, e.payload.cancellable));
  await listen('target-hover', (e) => setHover(e.payload));
  await listen('target', (e) => { live.target = e.payload; renderGame(); });
  await listen('capture', (e) => (e.payload.active ? showSheet(e.payload) : hideSheet()));
  await listen('capture-live', (e) => {
    const p = e.payload;
    $('#livePos').textContent = p.inside ? `x ${p.x} · y ${p.y}` : `x ${p.x} · y ${p.y} · outside WARDOGS`;
    $('#sheetLive').classList.toggle('outside', !p.inside);
  });

  showPicker(state.picking || !state.target, !!state.target);
  render();
  requestAnimationFrame(() => {
    document.body.classList.add('ready');
    invoke('ui_ready');
  });
}

boot();

// ---------------------------------------------------------------- preview stand-in

function previewBackend() {
  const mode = location.hash.slice(1); // '', 'main' or 'joining'
  const team = (id, name, color, hotkey, calibrated) => ({ id, name, color, x: 960, y: 620, calibrated, patch: '', hotkey });
  const settings = {
    teams: [team('lonestar', 'Lonestar', '#3B82F6', 119, true), team('valkyra', 'Valkyra', '#E5484D', 120, false), team('manticore', 'Manticore', '#46A758', 121, false)],
    selected: 'lonestar',
    keys: { join: 117, capture: 13 },
    sprint: { enabled: true, mode: 'run', sprint_key: 160, forward_key: 87, back_key: 83, hotkey: 118 },
    revive: { hotkey: 116, key: 70, hold_ms: 22000, stop_on_move: true },
    general: { always_on_top: false, sound: true, name: '' },
  };
  const target = { hwnd: 1, pid: 4242, process: 'WardogsClient-Win64-Shipping.exe', title: 'Wardogs', state: 'front' };
  const members = [
    { id: 'a', name: 'Alex', leader: mode === 'party', me: true },
    { id: 'b', name: 'Dave', leader: mode === 'member', me: false },
    { id: 'c', name: 'Sam', leader: false, me: false },
  ];
  const party = mode === 'party' || mode === 'member'
    ? { active: true, code: 'K7Q2XM', online: true, leader: mode === 'party', leader_name: mode === 'party' ? 'Alex' : 'Dave', team: 'valkyra', layout: ['valkyra', 'manticore'], members }
    : { active: false };
  if (mode === 'member') settings.selected = 'valkyra';
  const merge = (a, b) => { for (const [k, v] of Object.entries(b)) { if (v && typeof v === 'object' && !Array.isArray(v) && a[k]) merge(a[k], v); else a[k] = v; } };
  return {
    core: {
      invoke: async (cmd, args) => {
        if (cmd === 'get_state') {
          return {
            settings: structuredClone(settings), target: mode ? target : null, picking: !mode,
            join: { active: mode === 'joining', team: mode === 'joining' ? 'lonestar' : null, clicks: 137 },
            holding: false, reviving: false, rate: 10, party, shared_spots: [], steam_name: 'Alex',
          };
        }
        if (cmd === 'patch_settings') { merge(settings, args.patch); return structuredClone(settings); }
        return null;
      },
    },
    event: { listen: async () => () => {} },
  };
}

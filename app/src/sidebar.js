const MOD = /Mac/.test(navigator.platform) ? '⌘' : 'Ctrl+';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import './sidebar.css';

const $ = (s) => document.querySelector(s);
const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
const initials = (name) => name.split(/\s+/).filter(Boolean).slice(0, 2).map((w) => w[0].toUpperCase()).join('') || '?';
const COLORS = ['#d97757', '#6a9bcc', '#788c5d', '#b07cc6', '#c9a227', '#5fa8a0', '#c26d7e'];

let state = { accounts: [], active: null, panel: null };
/** hosts[name] = { url, status, error, hostname, sessions: [] } */
const hosts = {};
const hostOrder = [];
let hostsError = null;
/** Account switches in progress, keyed "host/session": { host, session, accountId, accountName, sessionName, status, url } */
const switches = new Map();
const key = (host, session) => `${host}/${session}`;

const call = (cmd, args) => invoke(cmd, args).catch((e) => flash(String(e)));
const hostSend = (host, msg) => call('host_send', { host, msg });
const notify = (title, body) => call('notify_native', { title, body });
const accountByName = (name) => state.accounts.find((a) => a.name.toLowerCase() === String(name).toLowerCase());

function flash(msg, ok = false) {
  const el = document.createElement('div');
  el.className = ok ? 'flash ok' : 'flash';
  el.textContent = msg;
  document.body.append(el);
  setTimeout(() => el.remove(), 6000);
}

// ---- rendering --------------------------------------------------------------

// Elements that are re-rendered often get an `html` property that skips identical HTML.
for (const sel of ['#accounts', '#acct-list', '#switches', '#hosts']) {
  Object.defineProperty($(sel), 'html', {
    set(v) {
      if (this.__html !== v) {
        this.innerHTML = v;
        this.__html = v;
      }
    },
  });
}

function render() {
  const { accounts, active, panel } = state;
  document.body.classList.toggle('wide', !!panel);
  $('#panel-accounts').hidden = panel !== 'accounts';
  $('#panel-sessions').hidden = panel !== 'sessions';

  $('#accounts').html = accounts.map((a, i) => `
    <button class="acct ${a.id === active && !panel ? 'active' : ''}" data-id="${esc(a.id)}"
      title="${esc(a.name)}${i < 9 ? `  ${MOD}${i + 1}` : ''}" style="--c:${esc(a.color)}">
      <span class="avatar">${esc(initials(a.name))}</span>
      ${a.attention ? '<span class="badge"></span>' : ''}
      <span class="acct-name">${esc(a.name)}</span>
    </button>`).join('');

  const limited = hostOrder.some((h) => hosts[h].sessions.some((s) => s.limited && s.running));
  $('#sessions-badge').hidden = !limited;
  $('#sessions-btn').classList.toggle('on', panel === 'sessions');
  $('#gear').classList.toggle('on', panel === 'accounts');

  if (panel === 'accounts') renderAccountsPanel();
  if (panel === 'sessions') renderSessionsPanel();
}

function renderAccountsPanel() {
  const { accounts } = state;
  $('#acct-list').html = accounts.length ? accounts.map((a, i) => `
    <li data-id="${esc(a.id)}">
      <span class="avatar sm" style="--c:${esc(a.color)}">${esc(initials(a.name))}</span>
      <input class="rename" value="${esc(a.name)}" data-id="${esc(a.id)}" />
      <span class="swatches">${COLORS.map((c) => `<button class="sw ${c === a.color ? 'on' : ''}" style="--c:${c}" data-color="${c}" data-id="${esc(a.id)}"></button>`).join('')}</span>
      <button class="mini" data-move="-1" data-id="${esc(a.id)}" ${i === 0 ? 'disabled' : ''} title="Move up">↑</button>
      <button class="mini" data-move="1" data-id="${esc(a.id)}" ${i === accounts.length - 1 ? 'disabled' : ''} title="Move down">↓</button>
      <button class="mini danger" data-remove="${esc(a.id)}" title="Remove">Remove</button>
    </li>`).join('') : '<li class="empty">No accounts yet. Add one, then sign in to claude.ai in it.</li>';
  $('#hotkey').textContent = (state.hotkey ?? '').replace('ctrl', '⌃').replace('alt', '⌥').replace(/\+/g, '').toUpperCase();
  $('#cfgpath').textContent = state.config_path ?? '';
}

function ago(ts) {
  const s = Math.max(0, Date.now() / 1000 - ts);
  if (s < 60) return 'just now';
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

const newFormOpen = new Set();

function renderSessionsPanel() {
  $('#switches').html = [...switches.values()].map((w) => `
    <div class="switch" data-key="${esc(key(w.host, w.session))}">
      <div class="switch-text">
        <b>${esc(w.sessionName)}</b> → <b>${esc(w.accountName)}</b>:
        ${w.status === 'starting' ? 'starting login on the host…'
          : w.status === 'approve' ? `approve in the <b>${esc(w.accountName)}</b> tab (click Authorize). The code is sent back automatically.`
          : w.status === 'sent' ? 'code sent, finishing sign-in…'
          : esc(w.status)}
      </div>
      ${w.status === 'approve' ? `<form class="paste"><input name="code" placeholder="…or paste the code here" autocomplete="off" /><button>Send</button></form>` : ''}
      <button class="mini" data-cancel-switch>Cancel</button>
    </div>`).join('');

  $('#hosts').html = hostOrder.length ? hostOrder.map((name) => {
    const h = hosts[name];
    const sessions = h.sessions.filter((s) => s.mode === 'terminal' || !s.mode).map((s) => {
      const acct = accountByName(s.account);
      const pills = [
        s.logging_in ? '<span class="pill busy">logging in…</span>'
          : s.needs_login && s.running ? '<span class="pill warn">not logged in</span>'
          : s.limited ? '<span class="pill bad">limit reached</span>' : '',
        !s.managed ? '<span class="pill" title="Started by hand. Right-click → Manage to autostart it.">unmanaged</span>' : '',
        s.managed && !s.autostart ? '<span class="pill" title="Won\'t start automatically">manual</span>' : '',
      ].join('');
      return `<li class="sess ${s.running ? '' : 'dead'} ${s.limited ? 'limited' : ''} ${s.needs_login && s.running ? 'nologin' : ''}" data-host="${esc(name)}" data-session="${esc(s.id)}" title="${esc(s.cwd)}">
        <span class="dot"></span>
        <span class="s-main">
          <span class="s-name">${esc(s.name)}</span>
          <span class="s-meta">${esc(s.cwd)} · ${s.running ? (s.started_at ? `up since ${ago(s.started_at).replace(' ago', '')}` : 'starting') : 'stopped'}</span>
        </span>
        ${pills}
        ${s.needs_login && !s.logging_in ? '' : `<span class="pill acct-pill" style="--c:${esc(acct?.color ?? '#888')}">${esc(s.account)}</span>`}
        <button class="mini more" data-more>⋯</button>
      </li>`;
    }).join('');
    const form = newFormOpen.has(name) ? `<form class="new" data-host="${esc(name)}">
        <input name="cwd" placeholder="Folder on ${esc(h.hostname ?? name)}, e.g. ~/21wild" autocomplete="off" />
        <input name="name" placeholder="Name (defaults to folder name)" autocomplete="off" />
        <label class="chk"><input type="checkbox" name="resume" checked /> Resume the folder’s last conversation</label>
        <label class="chk"><input type="checkbox" name="skip" checked /> Skip permissions (--dangerously-skip-permissions)</label>
        <button type="submit">Start</button><button type="button" data-cancel-new="${esc(name)}">Cancel</button>
      </form>` : '';
    return `<section class="host">
      <div class="host-head">
        <span class="status ${esc(h.status)}" title="${esc(h.error ?? h.status)}"></span>
        <b class="host-name">${esc(name)}</b>
        <span class="host-url">${esc(h.url)}</span>
        <button class="mini" data-new="${esc(name)}" ${h.status === 'connected' ? '' : 'disabled'}>New session</button>
      </div>
      ${h.status !== 'connected' && h.error ? `<div class="host-err">${esc(h.error)}</div>` : ''}
      ${form}
      <ul>${sessions || (h.status === 'connected' ? '<li class="none">No sessions yet. Add a game folder with “New session”.</li>' : '')}</ul>
    </section>`;
  }).join('') : hostsError
    ? `<p class="host-err big">The hosts file has an error, so no hosts were loaded. Fix it and restart the app:<br><code>${esc(hostsError)}</code></p>`
    : '<p class="empty-block">No hosts configured yet. Add your machines to the hosts file below, then restart the app.</p>';
}

// ---- context menu ---------------------------------------------------------------

function openMenu(x, y, host, sessionId) {
  const s = hosts[host]?.sessions.find((z) => z.id === sessionId);
  if (!s) return;
  const loggedOut = s.needs_login;
  const targets = state.accounts.filter((a) => loggedOut || a.name.toLowerCase() !== String(s.account).toLowerCase());
  const canLogin = s.running && !s.logging_in && targets.length;
  const cur = accountByName(s.account);
  const item = (act, label, enabled = true, cls = '') => `<button data-act="${act}" class="${cls}" ${enabled ? '' : 'disabled'}>${label}</button>`;
  const menu = $('#ctx');
  menu.innerHTML = `
    <div class="ctx-title">${esc(s.name)}</div>
    <div class="ctx-label">${loggedOut ? 'Log in as' : 'Move to account'}</div>
    ${targets.length ? targets.map((a) => `<button data-act="login" data-acct="${esc(a.id)}" ${canLogin ? '' : 'disabled'}>
        <span class="avatar xs" style="--c:${esc(a.color)}">${esc(initials(a.name))}</span>${esc(a.name)}</button>`).join('')
      : '<div class="ctx-note">Add an account in the sidebar first</div>'}
    <hr>
    ${cur ? item('open', `Open ${esc(cur.name)} tab`) : ''}
    ${item('copy', 'Copy “tmux attach” command')}
    ${s.running ? item('rc', 'Re-enable Remote Control') : ''}
    <hr>
    ${s.managed ? (s.running ? item('restart', 'Restart') + item('stop', 'Stop') : item('start', 'Start'))
      : item('manage', 'Manage (autostart + restart)')}
    ${s.managed ? `<hr><div class="ctx-label">On start (applies after Restart)</div>
      ${item(s.autostart ? 'autostart_off' : 'autostart_on', `${s.autostart ? '✓' : '<span class="nochk"></span>'} Start automatically`)}
      ${item(s.resume ? 'resume_off' : 'resume_on', `${s.resume ? '✓' : '<span class="nochk"></span>'} Resume last conversation`)}
      ${item(s.skip_permissions ? 'skip_off' : 'skip_on', `${s.skip_permissions ? '✓' : '<span class="nochk"></span>'} Skip permissions`)}<hr>` : ''}
    ${s.managed ? item('unmanage', 'Remove from list', true, 'danger') : ''}`;
  menu.dataset.host = host;
  menu.dataset.session = sessionId;
  menu.hidden = false;
  const r = menu.getBoundingClientRect();
  menu.style.left = `${Math.min(x, innerWidth - r.width - 8)}px`;
  menu.style.top = `${Math.max(8, Math.min(y, innerHeight - r.height - 8))}px`;
}
const closeMenu = () => { $('#ctx').hidden = true; };

$('#ctx').addEventListener('click', (e) => {
  const b = e.target.closest('button');
  if (!b || b.disabled) return;
  const { host, session } = $('#ctx').dataset;
  const s = hosts[host]?.sessions.find((z) => z.id === session);
  closeMenu();
  if (!s) return;
  switch (b.dataset.act) {
    case 'login': startSwitch(host, s, state.accounts.find((a) => a.id === b.dataset.acct)); break;
    case 'open': { const a = accountByName(s.account); if (a) call('select_account', { id: a.id }); break; }
    case 'copy': navigator.clipboard.writeText(`tmux attach -t ${s.name}`).then(() => flash(`Copied: tmux attach -t ${s.name}`, true)); break;
    case 'rc': hostSend(host, { type: 'session_action', session, action: 'remote_control' }); break;
    case 'unmanage':
      hostSend(host, { type: 'session_action', session, action: 'unmanage' });
      flash(`${s.name} removed from the list${s.running ? ' (its tmux session keeps running)' : ''}`, true);
      break;
    default: hostSend(host, { type: 'session_action', session, action: b.dataset.act });
  }
});
document.addEventListener('mousedown', (e) => { if (!e.target.closest('#ctx')) closeMenu(); });
document.addEventListener('contextmenu', (e) => {
  const li = e.target.closest('.sess');
  e.preventDefault();
  if (li) openMenu(e.clientX, e.clientY, li.dataset.host, li.dataset.session);
});

// ---- account switching ------------------------------------------------------------

function startSwitch(host, s, account) {
  if (!account) return;
  switches.set(key(host, s.id), { host, session: s.id, accountId: account.id, accountName: account.name, sessionName: s.name, status: 'starting' });
  hostSend(host, { type: 'login_start', session: s.id, account: account.name });
  render();
}

function endSwitch(host, session, ok, message) {
  const w = switches.get(key(host, session));
  if (!w) return;
  switches.delete(key(host, session));
  call('cancel_oauth', { session });
  if (ok) notify(`"${w.sessionName}" is now on ${w.accountName}`, 'Logged in. The session continues on this account.');
  else notify(`Couldn't log "${w.sessionName}" in to ${w.accountName}`, message || 'The login failed.');
  render();
}

$('#switches').addEventListener('click', (e) => {
  if (!e.target.closest('[data-cancel-switch]')) return;
  const w = switches.get(e.target.closest('.switch').dataset.key);
  if (w) {
    hostSend(w.host, { type: 'login_cancel', session: w.session });
    endSwitch(w.host, w.session, false, 'Cancelled');
  }
});
$('#switches').addEventListener('submit', (e) => {
  e.preventDefault();
  const w = switches.get(e.target.closest('.switch').dataset.key);
  const code = e.target.code.value.trim();
  if (!w || !code) return;
  hostSend(w.host, { type: 'login_code', session: w.session, code });
  w.status = 'sent';
  render();
});

listen('oauth-captured', ({ payload: { host, session } }) => {
  const w = switches.get(key(host, session));
  if (w) { w.status = 'sent'; render(); }
});

// ---- hosts ---------------------------------------------------------------------

const hostOf = (name) => (hosts[name] ??= { status: 'connecting', sessions: [] });

listen('host-status', ({ payload: { host, status, error } }) => {
  Object.assign(hostOf(host), { status, error });
  render();
});

listen('daemon-msg', ({ payload: { host, msg } }) => {
  const h = hostOf(host);
  switch (msg.type) {
    case 'welcome': h.hostname = msg.host; break;
    case 'sessions': {
      const before = new Set(h.sessions.filter((s) => s.limited).map((s) => s.id));
      h.sessions = msg.sessions;
      for (const s of msg.sessions) {
        if (s.limited && s.running && !before.has(s.id)) {
          notify(`"${s.name}" hit its usage limit`, `On ${host} (${s.account}). Right-click it in Host Sessions to move it.`);
        }
      }
      break;
    }
    case 'session_started': newFormOpen.delete(host); break;
    case 'login_url': {
      const w = switches.get(key(host, msg.session));
      if (!w) break;
      w.status = 'approve';
      w.url = msg.url;
      call('begin_oauth', { account: w.accountId, host, session: msg.session, url: msg.url });
      break;
    }
    case 'login_status':
      if (msg.status === 'success') endSwitch(host, msg.session, true);
      else if (msg.status === 'failed') endSwitch(host, msg.session, false, msg.message);
      break;
    case 'error': flash(`${host}: ${msg.message}`); break;
    default: return;
  }
  render();
});

$('#hosts').addEventListener('click', (e) => {
  const t = e.target.closest('[data-new],[data-cancel-new],[data-more]');
  if (!t) return;
  if (t.dataset.new) { newFormOpen.add(t.dataset.new); render(); $(`form.new[data-host="${CSS.escape(t.dataset.new)}"] input`)?.focus(); }
  else if (t.dataset.cancelNew) { newFormOpen.delete(t.dataset.cancelNew); render(); }
  else if (t.hasAttribute('data-more')) {
    const li = t.closest('.sess');
    const r = t.getBoundingClientRect();
    openMenu(r.left, r.bottom + 4, li.dataset.host, li.dataset.session);
  }
});
$('#hosts').addEventListener('submit', (e) => {
  e.preventDefault();
  const f = e.target;
  hostSend(f.dataset.host, {
    type: 'start_session', account: 'default', cwd: f.cwd.value.trim() || '~', name: f.name.value.trim() || null, mode: 'terminal',
    resume: f.resume.checked, skip_permissions: f.skip.checked,
  });
});

// ---- accounts panel / rail -------------------------------------------------------

$('#accounts').addEventListener('click', (e) => {
  const b = e.target.closest('.acct');
  if (b) call('select_account', { id: b.dataset.id });
});
const togglePanel = (p) => call('set_panel', { panel: state.panel === p ? null : p });
$('#add').onclick = () => call('set_panel', { panel: 'accounts' }).then(() => setTimeout(() => $('#add-form input').focus(), 50));
$('#gear').onclick = () => togglePanel('accounts');
$('#sessions-btn').onclick = () => togglePanel('sessions');
document.querySelectorAll('.close-panel').forEach((b) => { b.onclick = () => call('set_panel', { panel: null }); });
document.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape') return;
  if (!$('#ctx').hidden) return closeMenu();
  if (state.panel && state.accounts.length) call('set_panel', { panel: null });
});

$('#add-form').addEventListener('submit', (e) => {
  e.preventDefault();
  const name = e.target.name.value.trim();
  if (!name) return;
  call('add_account', { name });
  e.target.reset();
});
$('#acct-list').addEventListener('change', (e) => {
  if (e.target.classList.contains('rename')) call('rename_account', { id: e.target.dataset.id, name: e.target.value });
});
const armed = new Set();
$('#acct-list').addEventListener('click', (e) => {
  const t = e.target.closest('button');
  if (!t) return;
  if (t.dataset.color) call('set_account_color', { id: t.dataset.id, color: t.dataset.color });
  else if (t.dataset.move) call('move_account', { id: t.dataset.id, delta: Number(t.dataset.move) });
  else if (t.dataset.remove) {
    const id = t.dataset.remove;
    if (!armed.has(id)) {
      armed.add(id);
      t.textContent = 'Confirm';
      t.classList.add('armed');
      setTimeout(() => { armed.delete(id); render(); }, 3000);
      return;
    }
    armed.delete(id);
    call('remove_account', { id });
  }
});
$('#autostart').addEventListener('change', (e) => call('set_autostart', { enabled: e.target.checked }));

listen('state', ({ payload }) => { state = payload; render(); });
setInterval(render, 60_000);

(async () => {
  state = await invoke('get_state');
  $('#autostart').checked = await invoke('get_autostart');
  const hp = await invoke('hosts_path');
  $('#hostspath').textContent = hp.path;
  hostsError = hp.error;
  for (const h of await invoke('list_hosts')) {
    hostOrder.push(h.name);
    Object.assign(hostOf(h.name), { url: h.url, status: h.status.status, error: h.status.error });
    if (h.status.status === 'connected') hostSend(h.name, { type: 'list_sessions' });
  }
  render();
})();

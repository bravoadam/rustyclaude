import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import '@xterm/xterm/css/xterm.css';
import './style.css';
import { ChatView, b64ToUrl, esc } from './chat.js';

// ---- state ---------------------------------------------------------------
/** hosts[name] = { url, status, error, hostname, accounts: [], sessions: [], form: bool } */
const hosts = {};
const order = [];
let active = null; // { host, session, mode }

const $ = (sel) => document.querySelector(sel);
const enc = new TextEncoder();
const b64 = (bytes) => { let s = ''; for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000)); return btoa(s); };
const unb64 = (str) => Uint8Array.from(atob(str), (c) => c.charCodeAt(0));
const send = (host, msg) => invoke('send', { host, msg }).catch((e) => toast(`send failed: ${e}`));
const hostOf = (name) => (hosts[name] ??= { status: 'connecting', accounts: [], sessions: [], form: false });
const sessionOf = (host, id) => hosts[host]?.sessions.find((s) => s.id === id);

function toast(text) {
  const t = document.createElement('div');
  t.className = 'toast';
  t.textContent = text;
  $('#toasts').append(t);
  setTimeout(() => t.remove(), 6000);
}

// ---- file fetching + viewer -------------------------------------------------
const fileRequests = new Map(); // request id -> { resolve, reject }
let reqSeq = 0;

function readFile(host, session, path) {
  const request = `r${++reqSeq}`;
  return new Promise((resolve, reject) => {
    fileRequests.set(request, { resolve, reject });
    send(host, { type: 'read_file', request, session, path });
    setTimeout(() => { if (fileRequests.delete(request)) reject(new Error(`timed out reading ${path}`)); }, 60_000);
  });
}

let viewerUrl = null;
function openViewer(file) {
  const url = file.url ?? b64ToUrl(file.data, file.mime);
  if (viewerUrl && viewerUrl !== url) URL.revokeObjectURL(viewerUrl);
  viewerUrl = url;
  const name = (file.path ?? 'file').split('/').pop();
  $('#viewer-name').textContent = file.path ?? name;
  $('#viewer-save').href = url;
  $('#viewer-save').download = name;
  const body = $('#viewer-body');
  const m = file.mime ?? '';
  if (m.startsWith('image/')) body.innerHTML = `<img src="${url}" alt="${esc(name)}">`;
  else if (m.startsWith('video/')) body.innerHTML = `<video src="${url}" controls autoplay></video>`;
  else if (m.startsWith('audio/')) body.innerHTML = `<audio src="${url}" controls autoplay></audio>`;
  else if (m === 'application/pdf') body.innerHTML = `<iframe src="${url}"></iframe>`;
  else {
    const pre = document.createElement('pre');
    pre.textContent = file.data ? new TextDecoder().decode(unb64(file.data)) : '';
    body.replaceChildren(pre);
  }
  $('#viewer').hidden = false;
}
$('#viewer-close').onclick = () => { $('#viewer').hidden = true; $('#viewer-body').innerHTML = ''; };
document.addEventListener('keydown', (e) => { if (e.key === 'Escape' && !$('#viewer').hidden) $('#viewer-close').click(); });

$('#open-file').addEventListener('submit', async (e) => {
  e.preventDefault();
  const path = e.target.path.value.trim();
  if (!path || !active) return;
  try { openViewer(await readFile(active.host, active.session, path)); } catch (err) { toast(String(err.message ?? err)); }
});

// ---- terminal ---------------------------------------------------------------
const term = new Terminal({
  fontFamily: 'Menlo, "SF Mono", monospace',
  fontSize: 13,
  cursorBlink: true,
  scrollback: 10000,
  theme: { background: '#1a1a1a', foreground: '#e6e6e6', cursor: '#d97757', selectionBackground: '#d9775766' },
});
const fit = new FitAddon();
term.loadAddon(fit);
term.open($('#term'));

const sendInput = (bytes) => { if (active?.mode === 'terminal') send(active.host, { type: 'input', session: active.session, data: b64(bytes) }); };
term.onData((d) => sendInput(enc.encode(d)));
term.onBinary((d) => sendInput(Uint8Array.from(d, (c) => c.charCodeAt(0))));

let resizeTimer;
new ResizeObserver(() => {
  clearTimeout(resizeTimer);
  resizeTimer = setTimeout(() => {
    if (active?.mode !== 'terminal') return;
    fit.fit();
    send(active.host, { type: 'resize', session: active.session, cols: term.cols, rows: term.rows });
  }, 50);
}).observe($('#view'));

// ---- chat -------------------------------------------------------------------
const chat = new ChatView($('#chat'), {
  send: (msg) => active && send(active.host, { ...msg, session: active.session }),
  readFile: (path) => readFile(active.host, active.session, path),
  openViewer,
  toast,
});

// ---- attach / switch --------------------------------------------------------
function attach(host, session) {
  if (active && (active.host !== host || active.session !== session)) {
    send(active.host, { type: 'detach', session: active.session });
  }
  const mode = sessionOf(host, session)?.mode ?? 'terminal';
  active = { host, session, mode };
  $('#term-wrap').hidden = mode !== 'terminal';
  $('#chat').hidden = mode !== 'chat';
  $('#open-file').hidden = false;
  if (mode === 'terminal') {
    term.reset();
    fit.fit();
    send(host, { type: 'attach', session, cols: term.cols, rows: term.rows });
    term.focus();
  } else {
    chat.reset();
    chat.setRunning(sessionOf(host, session)?.running ?? true);
    send(host, { type: 'attach', session, cols: 0, rows: 0 });
    chat.focus();
  }
  render();
}

function clearActive() {
  active = null;
  $('#term-wrap').hidden = true;
  $('#chat').hidden = true;
  $('#open-file').hidden = true;
  render();
}

// ---- events from the Rust side ---------------------------------------------
listen('host-status', ({ payload: { host, status, error } }) => {
  const h = hostOf(host);
  h.status = status;
  h.error = error;
  // Daemon-side attachments die with the connection; re-attach on reconnect.
  if (status === 'connected' && active?.host === host) attach(host, active.session);
  render();
});

listen('daemon-msg', ({ payload: { host, msg } }) => {
  const h = hostOf(host);
  const isActive = (id) => active?.host === host && active.session === id;
  switch (msg.type) {
    case 'welcome': h.hostname = msg.host; break;
    case 'accounts': h.accounts = msg.accounts; break;
    case 'sessions': h.sessions = msg.sessions; break;
    case 'session_started':
      h.form = false;
      if (!sessionOf(host, msg.session.id)) h.sessions.push(msg.session);
      attach(host, msg.session.id);
      break;
    case 'output':
      if (isActive(msg.session)) term.write(unb64(msg.data));
      return; // hot path: no re-render
    case 'chat':
      if (isActive(msg.session)) chat.handle(msg.event);
      return;
    case 'file':
    case 'file_error': {
      const r = fileRequests.get(msg.request);
      if (!r) return;
      fileRequests.delete(msg.request);
      msg.type === 'file' ? r.resolve(msg) : r.reject(new Error(msg.message));
      return;
    }
    case 'exited': {
      const s = sessionOf(host, msg.session);
      if (s) s.running = false;
      if (isActive(msg.session)) {
        if (active.mode === 'terminal') {
          term.write(`\r\n\x1b[2m[session exited${msg.code != null ? ` with code ${msg.code}` : ''}]\x1b[0m\r\n`);
        } else {
          chat.setBusy(false);
          chat.setRunning(false);
          chat.handle({ type: 'stderr', text: `[session exited${msg.code != null ? ` with code ${msg.code}` : ''}]` });
        }
      }
      break;
    }
    case 'error': toast(`${host}: ${msg.message}`); break;
  }
  render();
});

// ---- sidebar ---------------------------------------------------------------
function ago(ts) {
  const s = Math.max(0, Date.now() / 1000 - ts);
  if (s < 60) return 'just now';
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

// Form values survive re-renders (status events re-render the sidebar).
const formDraft = {};

function render() {
  $('#hosts').innerHTML = order.map((name) => {
    const h = hosts[name];
    const d = formDraft[name] ?? {};
    const sessions = h.sessions.map((s) => {
      const isActive = active?.host === name && active.session === s.id;
      return `<li class="session ${isActive ? 'active' : ''} ${s.running ? '' : 'dead'}" data-host="${esc(name)}" data-session="${esc(s.id)}" title="${esc(s.cwd)}">
        <span class="dot"></span>
        <span class="s-main"><span class="s-name">${esc(s.name)}</span><span class="s-meta">${esc(s.account)} · ${ago(s.started_at)}</span></span>
        <span class="mode-badge ${esc(s.mode)}">${s.mode === 'chat' ? 'chat' : 'term'}</span>
        <button class="kill" data-kill="${esc(s.id)}" data-host="${esc(name)}" title="${s.running ? 'Kill session' : 'Remove'}">×</button>
      </li>`;
    }).join('');
    const opt = (v, label, cur) => `<option value="${esc(v)}" ${cur === v ? 'selected' : ''}>${esc(label)}</option>`;
    const mode = d.mode ?? 'chat';
    const form = h.form ? `<form class="new" data-host="${esc(name)}">
        <div class="seg">
          <label><input type="radio" name="mode" value="chat" ${mode === 'chat' ? 'checked' : ''}> Chat</label>
          <label><input type="radio" name="mode" value="terminal" ${mode === 'terminal' ? 'checked' : ''}> Terminal</label>
        </div>
        <div class="hint">${mode === 'chat' ? 'Rich UI with attachments and media. Not visible in the official Claude app.' : 'Raw Claude Code terminal. Enable /remote-control in it to use the official Claude app too.'}</div>
        <select name="account">${h.accounts.map((a) => opt(a, a, d.account)).join('')}</select>
        <input name="cwd" placeholder="~/project (directory on ${esc(h.hostname ?? name)})" value="${esc(d.cwd ?? '')}" />
        <input name="name" placeholder="name (optional)" value="${esc(d.name ?? '')}" />
        ${mode === 'chat' ? `<select name="permission_mode" title="Tool permissions">
          ${opt('acceptEdits', 'Permissions: accept edits', d.permission_mode ?? 'acceptEdits')}
          ${opt('default', 'Permissions: default (unapproved tools are denied)', d.permission_mode)}
          ${opt('plan', 'Permissions: plan only', d.permission_mode)}
          ${opt('bypassPermissions', 'Permissions: bypass all', d.permission_mode)}
        </select>` : ''}
        <div class="row"><button type="submit">Start</button><button type="button" data-cancel="${esc(name)}">Cancel</button></div>
      </form>` : '';
    return `<section class="host">
      <div class="host-head">
        <span class="status ${esc(h.status)}" title="${esc(h.error ?? h.status)}"></span>
        <span class="host-name" title="${esc(h.url)}">${esc(name)}</span>
        <span class="host-accts">${h.accounts.length ? `${h.accounts.length} acct` : ''}</span>
        <button class="add" data-add="${esc(name)}" ${h.status === 'connected' ? '' : 'disabled'} title="New session">+</button>
      </div>
      ${h.status !== 'connected' && h.error ? `<div class="host-err">${esc(h.error)}</div>` : ''}
      ${form}
      <ul>${sessions || (h.status === 'connected' ? '<li class="none">No sessions</li>' : '')}</ul>
    </section>`;
  }).join('');

  const cur = active && sessionOf(active.host, active.session);
  $('#title').textContent = cur ? cur.name : 'No session selected';
  $('#title-meta').textContent = cur ? `${active.host} · ${cur.account} · ${cur.cwd}` : '';
  $('#empty').hidden = !!active;
}

$('#hosts').addEventListener('click', (e) => {
  const t = e.target.closest('[data-kill],[data-add],[data-cancel],[data-session]');
  if (!t) return;
  if (t.dataset.kill) {
    e.stopPropagation();
    const s = sessionOf(t.dataset.host, t.dataset.kill);
    // Running sessions need a second click within 3s (window.confirm isn't reliable in webviews).
    if (s?.running && t.dataset.armed !== '1') {
      t.dataset.armed = '1';
      t.textContent = 'kill?';
      t.classList.add('armed');
      setTimeout(() => render(), 3000);
      return;
    }
    send(t.dataset.host, { type: 'kill', session: t.dataset.kill });
    if (active?.session === t.dataset.kill && !s?.running) clearActive();
  } else if (t.dataset.add) {
    const h = hosts[t.dataset.add];
    h.form = !h.form;
    render();
    if (h.form) $(`form.new[data-host="${CSS.escape(t.dataset.add)}"] input[name=cwd]`)?.focus();
  } else if (t.dataset.cancel) {
    hosts[t.dataset.cancel].form = false;
    render();
  } else if (t.dataset.session) {
    attach(t.dataset.host, t.dataset.session);
  }
});

$('#hosts').addEventListener('input', (e) => {
  const f = e.target.closest('form.new');
  if (!f) return;
  formDraft[f.dataset.host] = Object.fromEntries(new FormData(f));
  if (e.target.name === 'mode') render();
});

$('#hosts').addEventListener('submit', (e) => {
  e.preventDefault();
  const f = e.target;
  const d = Object.fromEntries(new FormData(f));
  send(f.dataset.host, {
    type: 'start_session',
    account: d.account,
    cwd: d.cwd || '~',
    name: d.name || null,
    mode: d.mode,
    permission_mode: d.mode === 'chat' ? d.permission_mode : null,
  });
  delete formDraft[f.dataset.host];
});

setInterval(render, 60_000); // keep "5m ago" fresh

// ---- boot ------------------------------------------------------------------
(async () => {
  for (const h of await invoke('list_hosts')) {
    order.push(h.name);
    Object.assign(hostOf(h.name), { url: h.url, status: h.status.status, error: h.status.error });
    if (h.status.status === 'connected') {
      send(h.name, { type: 'list_accounts' });
      send(h.name, { type: 'list_sessions' });
    }
  }
  const p = await invoke('config_path');
  $('#config-hint').innerHTML = order.length
    ? `Hosts: <code>${esc(p)}</code>`
    : `No hosts configured. Add some to <code>${esc(p)}</code> and restart.`;
  render();
})();

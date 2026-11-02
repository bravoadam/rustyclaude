// Rich chat view for chat-mode sessions (Claude Code stream-json events).
import { marked } from 'marked';
import DOMPurify from 'dompurify';

marked.setOptions({ gfm: true, breaks: true });

const MEDIA_RE = /(?:~|\.{1,2})?\/?[\w\-.~/]*[\w\-]\.(png|jpe?g|gif|webp|svg|mp4|webm|mov|m4v|mp3|wav|pdf)\b/gi;
const INLINE_EXT = /\.(png|jpe?g|gif|webp|svg|mp4|webm|mov|m4v)$/i;
const MAX_ATTACH = 30 * 1024 * 1024;

export const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);

export function b64ToUrl(data, mime) {
  const bin = atob(data);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return URL.createObjectURL(new Blob([bytes], { type: mime }));
}

function el(tag, cls, html) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (html != null) e.innerHTML = html;
  return e;
}

function md(text) {
  return DOMPurify.sanitize(marked.parse(text ?? ''));
}

/** Wrap media-looking paths in text nodes with clickable links; returns found paths. */
function linkifyPaths(root) {
  const found = [];
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, {
    acceptNode: (n) => (n.parentElement?.closest('a, .file-link') ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT),
  });
  const nodes = [];
  while (walker.nextNode()) nodes.push(walker.currentNode);
  for (const node of nodes) {
    const text = node.nodeValue;
    MEDIA_RE.lastIndex = 0;
    if (!MEDIA_RE.test(text)) continue;
    MEDIA_RE.lastIndex = 0;
    const frag = document.createDocumentFragment();
    let last = 0;
    for (const m of text.matchAll(MEDIA_RE)) {
      frag.append(text.slice(last, m.index));
      const a = el('span', 'file-link');
      a.textContent = m[0];
      a.dataset.path = m[0];
      frag.append(a);
      found.push(m[0]);
      last = m.index + m[0].length;
    }
    frag.append(text.slice(last));
    node.replaceWith(frag);
  }
  return [...new Set(found)];
}

export class ChatView {
  /**
   * @param {HTMLElement} root
   * @param {{ send(msg): void, readFile(path): Promise<{path,mime,data}>, openViewer(file): void, toast(msg): void }} io
   */
  constructor(root, io) {
    this.io = io;
    this.root = root;
    root.innerHTML = `
      <div class="chat-log"></div>
      <div class="busy" hidden><span class="spinner"></span>Claude is working…</div>
      <div class="composer">
        <div class="chips"></div>
        <div class="composer-row">
          <button class="attach" title="Attach files">＋</button>
          <textarea rows="1" placeholder="Message Claude…  (Enter to send, Shift+Enter for newline)"></textarea>
          <button class="sendbtn" title="Send">↑</button>
        </div>
        <input type="file" multiple hidden />
      </div>
      <div class="drop-hint">Drop files to attach</div>`;
    this.log = root.querySelector('.chat-log');
    this.busyEl = root.querySelector('.busy');
    this.chips = root.querySelector('.chips');
    this.input = root.querySelector('textarea');
    this.fileInput = root.querySelector('input[type=file]');
    this.pending = []; // { name, mime, data, url }
    this.tools = new Map(); // tool_use_id -> card element
    this.running = true;

    root.querySelector('.attach').onclick = () => this.fileInput.click();
    root.querySelector('.sendbtn').onclick = () => this.submit();
    this.fileInput.onchange = () => { this.addFiles(this.fileInput.files); this.fileInput.value = ''; };
    this.input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) { e.preventDefault(); this.submit(); }
    });
    this.input.addEventListener('input', () => this.autosize());
    this.input.addEventListener('paste', (e) => {
      const files = [...(e.clipboardData?.files ?? [])];
      if (files.length) { e.preventDefault(); this.addFiles(files); }
    });
    root.addEventListener('dragover', (e) => { e.preventDefault(); root.classList.add('dragging'); });
    root.addEventListener('dragleave', (e) => { if (!root.contains(e.relatedTarget)) root.classList.remove('dragging'); });
    root.addEventListener('drop', (e) => {
      e.preventDefault();
      root.classList.remove('dragging');
      this.addFiles(e.dataTransfer.files);
    });
    this.log.addEventListener('click', (e) => {
      const link = e.target.closest('.file-link');
      if (link) return this.preview(link.dataset.path);
      const img = e.target.closest('img[data-full]');
      if (img) return this.io.openViewer({ path: img.alt, mime: img.dataset.mime, url: img.src });
      const head = e.target.closest('.tool-head');
      if (head) head.parentElement.classList.toggle('open');
    });
    this.chips.addEventListener('click', (e) => {
      const x = e.target.closest('[data-remove]');
      if (!x) return;
      const [p] = this.pending.splice(Number(x.dataset.remove), 1);
      if (p?.url) URL.revokeObjectURL(p.url);
      this.renderChips();
    });
  }

  reset() {
    this.log.innerHTML = '';
    this.tools.clear();
    this.setBusy(false);
    this.setRunning(true);
  }

  focus() { this.input.focus(); }

  autosize() {
    this.input.style.height = 'auto';
    this.input.style.height = Math.min(this.input.scrollHeight, 220) + 'px';
  }

  setBusy(b) { this.busyEl.hidden = !b; }

  setRunning(r) {
    this.running = r;
    this.input.disabled = !r;
    this.input.placeholder = r ? 'Message Claude…  (Enter to send, Shift+Enter for newline)' : 'Session has exited';
  }

  async addFiles(list) {
    for (const f of list) {
      if (f.size > MAX_ATTACH) { this.io.toast(`${f.name} is over 30 MB`); continue; }
      const data = await new Promise((res, rej) => {
        const r = new FileReader();
        r.onload = () => res(String(r.result).split(',')[1] ?? '');
        r.onerror = rej;
        r.readAsDataURL(f);
      });
      const mime = f.type || 'application/octet-stream';
      this.pending.push({ name: f.name || 'pasted', mime, data, url: mime.startsWith('image/') ? URL.createObjectURL(f) : null });
    }
    this.renderChips();
    this.focus();
  }

  renderChips() {
    this.chips.innerHTML = this.pending.map((p, i) => `
      <span class="chip">${p.url ? `<img src="${p.url}" alt="">` : '<span class="fileicon">📄</span>'}
        <span class="chip-name">${esc(p.name)}</span><button data-remove="${i}">×</button></span>`).join('');
  }

  submit() {
    const text = this.input.value.trim();
    if (!this.running || (!text && !this.pending.length)) return;
    this.io.send({ type: 'send_chat', text, attachments: this.pending.map(({ name, mime, data }) => ({ name, mime, data })) });
    for (const p of this.pending) if (p.url) URL.revokeObjectURL(p.url);
    this.pending = [];
    this.renderChips();
    this.input.value = '';
    this.autosize();
  }

  async preview(path) {
    try {
      this.io.openViewer(await this.io.readFile(path));
    } catch (e) {
      this.io.toast(String(e));
    }
  }

  stick(fn) {
    const atBottom = this.log.scrollHeight - this.log.scrollTop - this.log.clientHeight < 80;
    fn();
    if (atBottom) this.log.scrollTop = this.log.scrollHeight;
  }

  /** Auto-embed images/videos referenced by path in assistant text. */
  async embedMedia(container, paths) {
    const inline = paths.filter((p) => INLINE_EXT.test(p)).slice(0, 4);
    if (!inline.length) return;
    const strip = el('div', 'media-strip');
    container.append(strip);
    for (const p of inline) {
      try {
        const f = await this.io.readFile(p);
        const url = b64ToUrl(f.data, f.mime);
        this.stick(() => {
          if (f.mime.startsWith('video/')) {
            const v = el('video');
            v.src = url; v.controls = true; v.preload = 'metadata';
            strip.append(v);
          } else {
            const img = el('img');
            img.src = url; img.alt = f.path; img.dataset.full = '1'; img.dataset.mime = f.mime;
            strip.append(img);
          }
        });
      } catch { /* missing file: leave the link only */ }
    }
  }

  handle(ev) {
    this.stick(() => this.render(ev));
  }

  render(ev) {
    switch (ev.type) {
      case 'local_user': {
        this.setBusy(true);
        const b = el('div', 'msg user');
        if (ev.text) b.append(el('div', 'bubble', md(ev.text)));
        if (ev.attachments?.length) {
          const strip = el('div', 'media-strip');
          for (const a of ev.attachments) {
            if (a.data) {
              const img = el('img');
              img.src = b64ToUrl(a.data, a.mime); img.alt = a.name; img.dataset.full = '1'; img.dataset.mime = a.mime;
              strip.append(img);
            } else {
              const f = el('span', 'file-link file-chip');
              f.dataset.path = a.path; f.textContent = `📄 ${a.name}`;
              strip.append(f);
            }
          }
          b.append(strip);
        }
        this.log.append(b);
        break;
      }
      case 'system':
        if (ev.subtype === 'init') {
          this.log.append(el('div', 'sys', `Session ready · <b>${esc(ev.model ?? '')}</b>${ev.permissionMode ? ` · ${esc(ev.permissionMode)}` : ''}`));
        }
        break;
      case 'assistant':
        for (const block of ev.message?.content ?? []) this.renderAssistantBlock(block);
        break;
      case 'user':
        for (const block of ev.message?.content ?? []) {
          if (block.type === 'tool_result') this.renderToolResult(block);
        }
        break;
      case 'result': {
        this.setBusy(false);
        const bits = [];
        if (ev.duration_ms != null) bits.push(`${(ev.duration_ms / 1000).toFixed(1)}s`);
        if (ev.num_turns != null) bits.push(`${ev.num_turns} turns`);
        if (ev.total_cost_usd != null) bits.push(`$${ev.total_cost_usd.toFixed(4)}`);
        const cls = ev.is_error ? 'sys err' : 'sys';
        const label = ev.is_error ? `Error: ${esc(ev.result ?? ev.subtype)}` : 'Done';
        this.log.append(el('div', cls, `${label}${bits.length ? ` · ${bits.join(' · ')}` : ''}`));
        break;
      }
      case 'stderr':
        this.log.append(el('div', 'stderr', esc(ev.text)));
        break;
      default:
        break; // stream_event etc. — ignored for now
    }
  }

  renderAssistantBlock(block) {
    if (block.type === 'text') {
      const m = el('div', 'msg assistant', md(block.text));
      this.log.append(m);
      this.embedMedia(m, linkifyPaths(m));
    } else if (block.type === 'thinking') {
      const t = el('details', 'thinking', `<summary>Thinking</summary><div>${md(block.thinking)}</div>`);
      this.log.append(t);
    } else if (block.type === 'tool_use') {
      const card = el('div', 'tool');
      const summary = toolSummary(block);
      card.innerHTML = `<div class="tool-head"><span class="tool-status running"></span><b>${esc(block.name)}</b> <span class="tool-sum">${esc(summary)}</span></div>
        <div class="tool-body"><pre class="tool-input">${esc(JSON.stringify(block.input, null, 2))}</pre><div class="tool-output"></div></div>`;
      linkifyPaths(card.querySelector('.tool-sum'));
      this.tools.set(block.id, card);
      this.log.append(card);
    }
  }

  renderToolResult(block) {
    const card = this.tools.get(block.tool_use_id);
    if (!card) return;
    const status = card.querySelector('.tool-status');
    status.className = `tool-status ${block.is_error ? 'error' : 'ok'}`;
    const out = card.querySelector('.tool-output');
    const parts = typeof block.content === 'string' ? [{ type: 'text', text: block.content }] : block.content ?? [];
    for (const p of parts) {
      if (p.type === 'text') {
        const pre = el('pre', block.is_error ? 'err' : '');
        const t = p.text.length > 20000 ? p.text.slice(0, 20000) + '\n… (truncated)' : p.text;
        pre.textContent = t;
        out.append(pre);
      } else if (p.type === 'image' && p.source?.type === 'base64') {
        const img = el('img');
        img.src = b64ToUrl(p.source.data, p.source.media_type);
        img.alt = 'tool image'; img.dataset.full = '1'; img.dataset.mime = p.source.media_type;
        out.append(img);
        card.classList.add('open'); // images are worth seeing without a click
      }
    }
  }
}

function toolSummary({ name, input = {} }) {
  switch (name) {
    case 'Bash': return input.command ?? '';
    case 'Read': case 'Write': case 'Edit': case 'MultiEdit': case 'NotebookEdit': return input.file_path ?? input.notebook_path ?? '';
    case 'Glob': case 'Grep': return input.pattern ?? '';
    case 'WebFetch': return input.url ?? '';
    case 'WebSearch': return input.query ?? '';
    case 'Task': case 'Agent': return input.description ?? '';
    case 'TodoWrite': return `${input.todos?.length ?? 0} todos`;
    default: {
      const first = Object.values(input).find((v) => typeof v === 'string');
      return first ? first.slice(0, 120) : '';
    }
  }
}

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

// The whole strip drags the window; double-click zooms (handled by Tauri).
document.body.innerHTML = `<div id="tb" data-tauri-drag-region><span id="dot"></span><span id="name" data-tauri-drag-region></span></div>`;
const style = document.createElement('style');
style.textContent = `
  :root { --bg: #191817; --line: #3a3936; --text: #ece9e1; --muted: #9a978e; color-scheme: dark; }
  @media (prefers-color-scheme: light) { :root { --bg: #e9e7df; --line: #d9d6cc; --text: #2b2a27; --muted: #77746b; color-scheme: light; } }
  html, body { margin: 0; height: 100%; overflow: hidden; background: var(--bg); -webkit-user-select: none; user-select: none; cursor: default; }
  #tb { height: 100%; display: flex; align-items: center; justify-content: center; gap: 8px; border-bottom: 1px solid var(--line);
        font: 600 12px -apple-system, BlinkMacSystemFont, sans-serif; color: var(--muted); }
  #dot { width: 8px; height: 8px; border-radius: 50%; background: var(--c, transparent); pointer-events: none; }
`;
document.head.append(style);

function render(state) {
  const a = state.accounts.find((x) => x.id === state.active);
  document.querySelector('#name').textContent = a ? a.name : 'RustyClaude';
  document.querySelector('#tb').style.setProperty('--c', a?.color ?? 'transparent');
}
listen('state', ({ payload }) => render(payload));
invoke('get_state').then(render);

// CEF on Linux: clicking a child browser doesn't give it keyboard focus by itself.
window.addEventListener('pointerdown', () => { invoke('focus_self').catch(() => {}); }, true);

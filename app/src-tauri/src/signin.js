// Linux only: claude.ai's bot check rejects this browser engine for email and Apple
// sign-in, so the login page gets a button that signs in through Brave/Chromium instead
// (src/browser_signin.rs). Google sign-in works here and is unaffected.
(() => {
  if (window !== window.top || location.hostname !== 'claude.ai') return;
  const invoke = (cmd, args) => window.__TAURI_INTERNALS__.invoke(cmd, args);
  let bar, busy = false;

  const make = () => {
    bar = document.createElement('div');
    Object.assign(bar.style, {
      position: 'fixed', left: '50%', bottom: '18px', transform: 'translateX(-50%)', zIndex: '2147483647',
      display: 'flex', alignItems: 'center', gap: '10px', padding: '10px 14px', borderRadius: '12px',
      background: '#262624', color: '#f5f4ef', font: '14px system-ui, sans-serif',
      boxShadow: '0 8px 30px rgba(0,0,0,.35)', maxWidth: 'calc(100% - 32px)',
    });
    const text = document.createElement('span');
    const btn = document.createElement('button');
    Object.assign(btn.style, {
      background: '#d97757', color: '#fff', border: '0', borderRadius: '8px', padding: '7px 12px',
      font: 'inherit', cursor: 'pointer', whiteSpace: 'nowrap',
    });
    bar.append(text, btn);
    const idle = (msg) => {
      busy = false;
      text.textContent = msg || 'Email or Apple sign-in failing here?';
      btn.textContent = 'Sign in with browser';
      btn.style.display = '';
    };
    btn.onclick = async () => {
      if (busy) return;
      busy = true;
      text.textContent = 'Finish signing in in the browser window that just opened…';
      btn.style.display = 'none';
      try {
        await invoke('browser_signin');
      } catch (e) {
        idle(String(e?.message ?? e));
      }
    };
    idle();
  };

  const update = () => {
    const onLogin = location.pathname.startsWith('/login');
    if (onLogin && !bar) make();
    if (bar) {
      if (onLogin && !bar.isConnected && document.body) document.body.appendChild(bar);
      if (!onLogin && bar.isConnected) bar.remove();
    }
  };
  setInterval(update, 1000);
  document.addEventListener('DOMContentLoaded', update);
})();

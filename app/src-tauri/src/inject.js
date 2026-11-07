// Injected into every claude.ai account webview, before the page's own scripts.
// Turns web notifications into native desktop notifications (tagged with the account).
(() => {
  if (window.__rustyClaude) return;
  window.__rustyClaude = true;

  const notify = (title, opts) => {
    try {
      window.__TAURI_INTERNALS__?.invoke('web_notify', {
        title: String(title ?? ''),
        body: String(opts?.body ?? ''),
      });
    } catch (_) {}
  };

  class NativeNotification extends EventTarget {
    constructor(title, opts = {}) {
      super();
      this.title = title;
      this.body = opts.body ?? '';
      this.tag = opts.tag ?? '';
      this.data = opts.data;
      this.onclick = this.onclose = this.onerror = this.onshow = null;
      notify(title, opts);
    }
    static get permission() { return 'granted'; }
    static requestPermission(cb) {
      if (typeof cb === 'function') cb('granted');
      return Promise.resolve('granted');
    }
    close() {}
  }
  try {
    Object.defineProperty(window, 'Notification', { value: NativeNotification, writable: true, configurable: true });
  } catch (_) {}

  if (window.ServiceWorkerRegistration) {
    ServiceWorkerRegistration.prototype.showNotification = function (title, opts) {
      notify(title, opts);
      return Promise.resolve();
    };
  }

  const query = navigator.permissions?.query?.bind(navigator.permissions);
  if (query) {
    navigator.permissions.query = (desc) =>
      desc?.name === 'notifications'
        ? Promise.resolve({ state: 'granted', status: 'granted', onchange: null, addEventListener() {}, removeEventListener() {} })
        : query(desc);
  }
})();

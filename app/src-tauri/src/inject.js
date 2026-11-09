// Injected into every claude.ai account webview, before the page's own scripts.
// Turns web notifications into native desktop notifications (tagged with the account).
(() => {
  if (window.__rustyClaude) return;
  window.__rustyClaude = true;

  // CEF on Linux: clicking a child browser doesn't give it keyboard focus by itself.
  window.addEventListener('pointerdown', () => {
    window.__TAURI_INTERNALS__?.invoke('focus_self').catch(() => {});
  }, true);

  // CEF on Linux is built without H.264, H.265 and AAC, so most MP4s won't play. When a
  // video or audio element can't play its source, the app converts the file to WebM with
  // ffmpeg (see media.rs) and the element plays that instead.
  (() => {
    const invoke = (cmd, args) => window.__TAURI_INTERNALS__.invoke(cmd, args);
    const CHUNK = 4 << 20;
    const results = new Map(); // source URL -> Promise of a blob: URL for the converted file
    const ours = new Set(); // blob: URLs of converted files
    let queue = Promise.resolve(); // one conversion at a time
    let pending = 0;

    const toB64 = (u8) => {
      if (u8.toBase64) return u8.toBase64();
      let s = '';
      for (let i = 0; i < u8.length; i += 0x8000) s += String.fromCharCode.apply(null, u8.subarray(i, i + 0x8000));
      return btoa(s);
    };
    const fromB64 = (s) => (Uint8Array.fromBase64 ? Uint8Array.fromBase64(s) : Uint8Array.from(atob(s), (c) => c.charCodeAt(0)));

    let toastEl, toastTimer;
    const hideToast = () => toastEl?.remove();
    const toast = (text, ms) => {
      if (!document.body) return;
      if (!toastEl) {
        toastEl = document.createElement('div');
        toastEl.setAttribute('role', 'status');
        toastEl.style.cssText =
          'position:fixed;left:50%;bottom:24px;transform:translateX(-50%);z-index:2147483647;' +
          'max-width:80vw;padding:8px 14px;border-radius:8px;background:rgba(20,20,20,.88);color:#fff;' +
          'font:13px/1.4 system-ui,sans-serif;pointer-events:none';
      }
      toastEl.textContent = text;
      if (!toastEl.isConnected) document.body.appendChild(toastEl);
      clearTimeout(toastTimer);
      if (ms) toastTimer = setTimeout(hideToast, ms);
    };

    async function convert(src) {
      toast('Converting video for playback…');
      const res = await fetch(src);
      if (!res.ok) throw new Error(`couldn't load it (${res.status})`);
      const bytes = new Uint8Array(await res.arrayBuffer());
      const id = crypto.randomUUID();
      try {
        for (let off = 0; off < bytes.length; off += CHUNK) {
          await invoke('media_upload', { id, offset: off, data: toB64(bytes.subarray(off, off + CHUNK)) });
        }
        await invoke('media_convert', { id });
        let st;
        while ((st = await invoke('media_status', { id })).state === 'running') {
          toast(`Converting video for playback… ${Math.round(st.progress * 100)}%`);
          await new Promise((r) => setTimeout(r, 400));
        }
        if (st.state === 'failed') throw new Error(st.error);
        const parts = [];
        for (let off = 0; off < st.size; off += CHUNK) {
          parts.push(fromB64(await invoke('media_read', { id, offset: off, len: CHUNK })));
        }
        return URL.createObjectURL(new Blob(parts, { type: 'video/webm' }));
      } finally {
        invoke('media_done', { id }).catch(() => {});
      }
    }

    function play(el, src) {
      if (!src || ours.has(src) || el.__rcConverting === src || !window.__TAURI_INTERNALS__) return;
      el.__rcConverting = src;
      let p = results.get(src);
      if (!p) {
        p = queue = queue.catch(() => {}).then(() => convert(src));
        results.set(src, p);
        pending++;
        const settle = () => { if (!--pending) hideToast(); };
        p.then(settle, settle);
      }
      p.then((url) => {
        ours.add(url);
        if (el.isConnected && el.__rcConverting === src) el.src = url;
      }, (e) => {
        console.warn('[RustyClaude] video conversion failed:', e);
        toast(`Couldn't convert this video: ${e?.message || e}`, 6000);
      });
    }

    window.addEventListener('error', (e) => {
      const t = e.target;
      if (t instanceof HTMLMediaElement) {
        const code = t.error?.code;
        if (code !== MediaError.MEDIA_ERR_SRC_NOT_SUPPORTED && code !== MediaError.MEDIA_ERR_DECODE) return;
        const src = t.currentSrc || t.src;
        if (ours.has(src)) {
          console.warn('[RustyClaude] converted video failed to play:', t.error?.message);
          toast("Couldn't play the converted video", 6000);
        } else {
          play(t, src);
        }
      } else if (t instanceof HTMLSourceElement && t.parentElement instanceof HTMLMediaElement) {
        // Fires for each <source> that fails; convert once the element has run out of them.
        const el = t.parentElement;
        setTimeout(() => {
          if (el.networkState === HTMLMediaElement.NETWORK_NO_SOURCE && !el.currentSrc) play(el, t.src);
        });
      }
    }, true);

    // An H.264 video whose audio the build can play (Opus, MP3) doesn't fail: it plays the
    // sound and shows nothing.
    window.addEventListener('loadedmetadata', (e) => {
      const t = e.target;
      if (t instanceof HTMLVideoElement && t.videoWidth === 0 && !ours.has(t.currentSrc)) play(t, t.currentSrc);
    }, true);
  })();

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

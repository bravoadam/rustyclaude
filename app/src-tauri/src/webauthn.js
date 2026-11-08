// Linux only: WebKitGTK has no WebAuthn, so security-key sign-in requests go to the app
// (src/webauthn.rs), which talks to the key. Injected into account tabs and sign-in popups.
(() => {
  if (window.__rustyWebAuthn || window !== window.top) return;
  window.__rustyWebAuthn = true;
  const invoke = (cmd, args) => window.__TAURI_INTERNALS__.invoke(cmd, args);

  const toB64 = (buf) => {
    const u8 = buf instanceof ArrayBuffer ? new Uint8Array(buf) : new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength);
    let s = '';
    for (const b of u8) s += String.fromCharCode(b);
    return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  };
  const fromB64 = (s) => {
    const bin = atob(s.replace(/-/g, '+').replace(/_/g, '/') + '==='.slice((s.length + 3) % 4));
    const u8 = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) u8[i] = bin.charCodeAt(i);
    return u8.buffer;
  };
  // Options -> JSON, with every BufferSource as base64url.
  const encode = (v) => {
    if (v instanceof ArrayBuffer || ArrayBuffer.isView(v)) return toB64(v);
    if (Array.isArray(v)) return v.map(encode);
    if (v && typeof v === 'object') {
      const o = {};
      for (const k in v) if (k !== 'signal') o[k] = encode(v[k]);
      return o;
    }
    return v;
  };

  class AuthenticatorResponse {}
  class AuthenticatorAssertionResponse extends AuthenticatorResponse {}
  class AuthenticatorAttestationResponse extends AuthenticatorResponse {
    getTransports() { return this._transports; }
    getAuthenticatorData() { return this._authData; }
    getPublicKey() { return null; }
    getPublicKeyAlgorithm() { return -7; }
  }
  class PublicKeyCredential {
    get type() { return 'public-key'; }
    getClientExtensionResults() { return this._ext; }
    toJSON() {
      const r = {};
      for (const k of ['clientDataJSON', 'authenticatorData', 'signature', 'userHandle', 'attestationObject'])
        if (this.response[k]) r[k] = toB64(this.response[k]);
      return { id: this.id, rawId: this.id, type: 'public-key', authenticatorAttachment: 'cross-platform', response: r, clientExtensionResults: this._ext };
    }
    static isUserVerifyingPlatformAuthenticatorAvailable() { return Promise.resolve(false); }
    static isConditionalMediationAvailable() { return Promise.resolve(false); }
    static getClientCapabilities() { return Promise.resolve({ hybridTransport: false, passkeyPlatformAuthenticator: false, conditionalGet: false, userVerifyingPlatformAuthenticator: false }); }
  }
  const def = (name, value) => { try { Object.defineProperty(window, name, { value, writable: true, configurable: true }); } catch (_) {} };
  def('PublicKeyCredential', PublicKeyCredential);
  def('AuthenticatorResponse', AuthenticatorResponse);
  def('AuthenticatorAssertionResponse', AuthenticatorAssertionResponse);
  def('AuthenticatorAttestationResponse', AuthenticatorAttestationResponse);

  const build = (kind, r) => {
    const cred = Object.create(PublicKeyCredential.prototype);
    const resp = Object.create((kind === 'get' ? AuthenticatorAssertionResponse : AuthenticatorAttestationResponse).prototype);
    resp.clientDataJSON = fromB64(r.clientDataJSON);
    if (kind === 'get') {
      resp.authenticatorData = fromB64(r.authenticatorData);
      resp.signature = fromB64(r.signature);
      resp.userHandle = r.userHandle ? fromB64(r.userHandle) : null;
    } else {
      resp.attestationObject = fromB64(r.attestationObject);
      resp._authData = fromB64(r.authenticatorData);
      resp._transports = r.transports;
    }
    Object.defineProperties(cred, {
      id: { value: r.id, enumerable: true },
      rawId: { value: fromB64(r.id), enumerable: true },
      response: { value: resp, enumerable: true },
      authenticatorAttachment: { value: 'cross-platform', enumerable: true },
      _ext: { value: r.extensions || {} },
    });
    return cred;
  };

  // A small prompt while the key is waiting for a touch.
  let overlay;
  const showPrompt = (text) => {
    if (!overlay) {
      // Built with DOM calls and inline styles only: Google's sign-in enforces Trusted Types
      // (no HTML strings) and a strict CSP (no <style> blocks).
      const h = (tag, style = {}, props = {}, kids = []) => {
        const el = document.createElement(tag);
        Object.assign(el.style, style);
        Object.assign(el, props);
        for (const k of kids) el.appendChild(k);
        return el;
      };
      const btn = { marginTop: '14px', background: '#3a3a37', color: 'inherit', border: '0', borderRadius: '8px', padding: '7px 16px', font: 'inherit', cursor: 'pointer' };
      const row = { display: 'flex', gap: '8px', justifyContent: 'center' };
      const t = h('div');
      const p = h('input', { display: 'block', width: '100%', boxSizing: 'border-box', marginTop: '12px', padding: '8px 10px', borderRadius: '8px', border: '1px solid #555', background: '#1a1a19', color: 'inherit', font: 'inherit' }, { type: 'password', autocomplete: 'off', placeholder: 'PIN' });
      const c = h('button', btn, { type: 'button', textContent: 'Cancel' });
      const f = h('form', {}, {}, [p, h('div', row, {}, [c, h('button', btn, { type: 'submit', textContent: 'Continue' })])]);
      const c2 = h('button', btn, { type: 'button', textContent: 'Cancel' });
      const box = h('div', { background: '#262624', color: '#f5f4ef', padding: '22px 26px', borderRadius: '12px', maxWidth: '320px', textAlign: 'center', boxShadow: '0 10px 40px rgba(0,0,0,.5)' }, {}, [
        h('div', { fontSize: '34px', marginBottom: '8px' }, { textContent: '🔑' }), t, f, h('div', row, {}, [c2]),
      ]);
      overlay = h('div', { position: 'fixed', inset: '0', zIndex: '2147483647', display: 'flex', alignItems: 'center', justifyContent: 'center', background: 'rgba(0,0,0,.45)', font: '15px system-ui, sans-serif' }, {}, [box]);
      overlay._t = t;
      overlay._f = f;
      overlay._p = p;
      overlay._c = c;
      overlay._c2 = c2;
      overlay._c2.onclick = () => invoke('webauthn_cancel').catch(() => {});
    }
    overlay._t.textContent = text;
    overlay._f.style.display = 'none';
    overlay._c2.parentElement.style.display = '';
    (document.body || document.documentElement).appendChild(overlay);
  };
  // PIN entry inside the page: window.prompt() opened a dialog that could end up hidden.
  const askPin = (text) =>
    new Promise((resolve) => {
      showPrompt(text);
      const { _f: f, _p: p } = overlay;
      overlay._c2.parentElement.style.display = 'none';
      f.style.display = '';
      p.value = '';
      setTimeout(() => p.focus(), 0);
      const done = (v) => {
        f.onsubmit = null;
        overlay._c.onclick = null;
        p.value = '';
        resolve(v);
      };
      f.onsubmit = (e) => {
        e.preventDefault();
        done(p.value || null);
      };
      overlay._c.onclick = () => done(null);
    });
  const hidePrompt = () => overlay && overlay.remove();

  const request = async (kind, publicKey, signal) => {
    if (signal?.aborted) throw new DOMException('The operation was aborted.', 'AbortError');
    const onAbort = () => invoke('webauthn_cancel').catch(() => {});
    signal?.addEventListener('abort', onAbort);
    const options = encode(publicKey);
    let pin = null;
    try {
      for (;;) {
        showPrompt(kind === 'get' ? 'Touch your security key to sign in.' : 'Touch your security key to register it.');
        try {
          return build(kind, await invoke('webauthn', { req: { kind, options, pin } }));
        } catch (e) {
          const msg = String(e?.message ?? e);
          const [name, ...rest] = msg.split(': ');
          const detail = rest.join(': ');
          if (name === 'PinRequired' || name === 'PinInvalid') {
            const left = name === 'PinInvalid' && detail ? ` (${detail} tries left)` : '';
            pin = await askPin(name === 'PinInvalid' ? `Wrong PIN${left}. Enter your security key PIN.` : 'Enter your security key PIN.');
            if (pin) continue;
            throw new DOMException('Cancelled.', 'NotAllowedError');
          }
          if (signal?.aborted) throw new DOMException('The operation was aborted.', 'AbortError');
          const known = ['NotAllowedError', 'InvalidStateError', 'SecurityError', 'NotSupportedError', 'TypeError'];
          throw new DOMException(detail || msg, known.includes(name) ? name : 'NotAllowedError');
        }
      }
    } finally {
      hidePrompt();
      signal?.removeEventListener('abort', onAbort);
    }
  };

  // WebKitGTK has no navigator.credentials at all; create it if needed.
  let creds = navigator.credentials;
  if (!creds) {
    class CredentialsContainer {}
    creds = Object.create(CredentialsContainer.prototype);
    creds.store = () => Promise.reject(new DOMException('Not supported.', 'NotSupportedError'));
    creds.preventSilentAccess = () => Promise.resolve();
    def('CredentialsContainer', CredentialsContainer);
    try {
      Object.defineProperty(Navigator.prototype, 'credentials', { get: () => creds, configurable: true, enumerable: true });
    } catch (_) {}
  }
  const origGet = creds.get?.bind(creds);
  const origCreate = creds.create?.bind(creds);
  creds.get = (opts = {}) => (opts.publicKey ? request('get', opts.publicKey, opts.signal) : origGet ? origGet(opts) : Promise.resolve(null));
  creds.create = (opts = {}) => (opts.publicKey ? request('create', opts.publicKey, opts.signal) : origCreate ? origCreate(opts) : Promise.resolve(null));
})();

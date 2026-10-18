// Test instrumentation for the viewer page, served only by the e2e harness (run.ts copies
// viewer/dist and loads this as a classic script before the viewer's module). It replaces the
// DevTools protocol, which rd Desktop's WebView2 does not expose: wry sets the browser
// arguments itself, and WebView2 ignores WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS and the registry
// policy then.
//
// Records: DOM key events, window.rdNative calls (the app's hook path), and the input frames the
// page sends. Reports:
//   - before a session, via document.title = 'rd-selftest {...}' (debug rd Desktop logs such titles;
//     in Edge it is the window title): layout for the harness's clicks;
//   - during a session, over the viewer's own session WebSocket (the gateway relays any text frame):
//     the fake agent sends {"t":"rd_e2e","cmd":"report"|"race",id} and gets {"t":"rd_e2e_report",id,state}.
// The viewer ignores both message types.
(() => {
  'use strict';
  const P = (window.__rdE2E = { dom: [], nat: [], sent: [], errors: [] });
  addEventListener('keydown', (e) => P.dom.push('d:' + e.code), true);
  addEventListener('keyup', (e) => P.dom.push('u:' + e.code), true);
  addEventListener('error', (e) => P.errors.push(String(e.message)), true);

  // window.rdNative: the viewer assigns it (DesktopBridge); the app calls window.rdNative.key(...).
  let real;
  const wrapped = Object.freeze({
    get version() {
      return real ? real.version : undefined;
    },
    key(c, d, r) {
      P.nat.push((d ? 'd:' : 'u:') + c + (r ? '(r)' : ''));
      return real && real.key(c, d, r);
    },
    release() {
      P.nat.push('release');
      return real && real.release();
    },
  });
  Object.defineProperty(window, 'rdNative', {
    configurable: true,
    get: () => (real ? wrapped : undefined),
    set: (v) => {
      real = v;
    },
  });

  const hex = (n) => n.toString(16);
  const describe = (b) => {
    if (b[0] === 0x14 && b.length >= 8) return (b[1] ? 'd:' : 'u:') + hex(b[4] | (b[5] << 8) | (b[6] << 16) | (b[7] << 24));
    if (b[0] === 0x15) return 'release_all';
    if (b[0] === 0x12) return 'btn:' + b[1] + (b[2] ? 'd' : 'u');
    return null; // moves, wheel
  };
  const OrigWS = window.WebSocket;
  const origSend = OrigWS.prototype.send;
  OrigWS.prototype.send = function (data) {
    try {
      if (data instanceof ArrayBuffer || ArrayBuffer.isView(data)) {
        const b = data instanceof ArrayBuffer ? new Uint8Array(data) : new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
        const d = describe(b);
        if (d) P.sent.push(d);
      }
    } catch (e) {
      P.errors.push('send probe: ' + e);
    }
    return origSend.call(this, data);
  };

  let session = null;
  window.WebSocket = new Proxy(OrigWS, {
    construct(target, args) {
      const ws = Reflect.construct(target, args);
      if (String(args[0]).includes('/ws/view/')) {
        session = ws;
        ws.addEventListener('message', (e) => {
          if (typeof e.data !== 'string' || e.data.indexOf('"rd_e2e"') < 0) return;
          let m;
          try {
            m = JSON.parse(e.data);
          } catch {
            return;
          }
          if (m.t === 'rd_e2e') void command(ws, m);
        });
      }
      return ws;
    },
  });

  const rect = (sel) => {
    const e = document.querySelector(sel);
    if (!e) return null;
    const r = e.getBoundingClientRect();
    return { x: r.left, y: r.top, w: r.width, h: r.height, disabled: !!e.disabled, hidden: !!e.closest('[hidden]') };
  };
  const tag = (e) => (e ? e.tagName + (e.className ? '.' + e.className : '') : null);
  const layout = () => ({
    probe: 1,
    connect: rect('button.primary.big'),
    canvas: rect('.stage canvas'),
    inner: [innerWidth, innerHeight],
    dpr: devicePixelRatio,
    hasFocus: document.hasFocus(),
    active: tag(document.activeElement),
    live: !!session && session.readyState === 1,
  });
  P.state = () => {
    let stored = null;
    try {
      stored = localStorage.getItem('rd.settings.mbp');
    } catch {
      // storage blocked
    }
    return Object.assign(layout(), {
      dom: P.dom.slice(),
      nat: P.nat.slice(),
      sent: P.sent.slice(),
      errors: P.errors.slice(),
      tauri: !!window.__TAURI_INTERNALS__,
      rdNative: real ? 'set' : 'unset',
      ua: navigator.userAgent,
      platform: navigator.userAgentData ? navigator.userAgentData.platform : navigator.platform,
      storedSettings: stored,
      visibility: document.visibilityState,
    });
  };

  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  async function command(ws, m) {
    const out = { t: 'rd_e2e_report', id: m.id, cmd: m.cmd };
    try {
      if (m.cmd === 'race') {
        // Quick focus changes between a viewer control and the video: each pair is an
        // rd_set_grab(false) + rd_set_grab(true), all in flight at once. Ends on the video.
        // With yieldMs every change happens in its own task (like separate clicks), so the page
        // cannot fold a pair into one decision and really sends the calls.
        const h = document.querySelector('button.toolbar-handle');
        const c = document.querySelector('.stage canvas');
        const spread = typeof m.yieldMs === 'number';
        for (let k = 0; k < (m.rounds || 5); k++) {
          for (let i = 0; i < (m.n || 100); i++) {
            h.focus();
            if (spread) await sleep(m.yieldMs);
            c.focus();
            if (spread) await sleep(m.yieldMs);
          }
          await sleep(m.pauseMs || 150);
        }
        out.activeIsCanvas = document.activeElement === c;
      }
      out.state = P.state();
    } catch (e) {
      out.error = String(e);
    }
    origSend.call(ws, JSON.stringify(out));
  }

  // Before a session there is no socket: report the layout through the title.
  let lastTitle = '';
  setInterval(() => {
    const t = 'rd-selftest ' + JSON.stringify(layout());
    if (t !== lastTitle || document.title !== t) {
      lastTitle = t;
      document.title = t;
    }
  }, 250);
})();

'use strict';

/* Muse Browser Bridge (unofficial) — MV3 service worker.
 *
 * Data flow is popup -> worker -> chrome.tabs, and ONLY that way:
 *  - No content scripts, no host permissions, no <all_urls>.
 *  - No `externally_connectable`: no web page or other extension can
 *    message this worker; the listener below additionally drops any
 *    sender that is not this extension's own popup.
 *  - No polling, no chrome.tabs listeners, no script injection.
 *  - Every tab operation below originates from an explicit click on
 *    the action popup (List / Open / Close) = per-action user consent.
 *  - Tab bodies are never read: only id/title/url metadata is listed.
 *
 * The `muse-browser` native-host crate (shipped alongside) defines the
 * framed JSON protocol reserved for a future agent-driven path; this
 * worker does not call it yet because a stateless stdio host cannot
 * approve actions or reach browser tabs on its own. Wiring that path
 * (app-side outbox + approval queue) is explicitly future work, see
 * docs/browser-tabs.md.
 */

const ALLOWED_OPS = new Set(['tabs.list', 'tabs.open', 'tabs.close']);

// Validate a popup message before it may touch chrome.tabs.
// Throws a static-only Error; user input is never echoed.
function validateRelay(msg) {
  if (typeof msg !== 'object' || msg === null || Array.isArray(msg)) {
    throw new Error('bad message');
  }
  if (!ALLOWED_OPS.has(msg.op)) {
    throw new Error('unsupported op');
  }
  if (typeof msg.id !== 'number' || !Number.isInteger(msg.id) || msg.id < 0) {
    throw new Error('bad id');
  }
  const params =
    typeof msg.params === 'object' && msg.params !== null ? msg.params : null;
  if (params === null || Array.isArray(params)) {
    throw new Error('bad params');
  }
  if (msg.op === 'tabs.list' && Object.keys(params).length !== 0) {
    throw new Error('bad params');
  }
  if (msg.op === 'tabs.open') {
    const keys = Object.keys(params);
    if (keys.length !== 1 || keys[0] !== 'url' || typeof params.url !== 'string') {
      throw new Error('bad params');
    }
    // Only http(s) destinations; the same rule the native host enforces.
    if (!/^https?:\/\/[^/@\s]+/i.test(params.url) || params.url.length > 2048) {
      throw new Error('url rejected');
    }
  }
  if (msg.op === 'tabs.close') {
    const keys = Object.keys(params);
    if (
      keys.length !== 1 ||
      keys[0] !== 'id' ||
      typeof params.id !== 'number' ||
      !Number.isInteger(params.id) ||
      params.id < 0
    ) {
      throw new Error('bad params');
    }
  }
  return { id: msg.id, op: msg.op, params };
}

chrome.runtime.onMessage.addListener((msg, sender, sendResponse) => {
  // Gate: only this extension's own popup (no tab sender, own id).
  // External messages are already impossible without
  // `externally_connectable` in the manifest; this is belt and braces.
  if (!sender || sender.id !== chrome.runtime.id || sender.tab) {
    return false;
  }
  let wire;
  try {
    wire = validateRelay(msg);
  } catch (e) {
    sendResponse({ id: null, ok: false, error: { code: 'invalid_request', detail: e.message } });
    return false;
  }
  (async () => {
    try {
      if (wire.op === 'tabs.list') {
        const tabs = await chrome.tabs.query({});
        sendResponse({
          id: wire.id,
          ok: true,
          result: {
            tabs: tabs.map((t) => ({ id: t.id, title: t.title || '', url: t.url || '' })),
          },
        });
      } else if (wire.op === 'tabs.open') {
        const tab = await chrome.tabs.create({ url: wire.params.url });
        sendResponse({
          id: wire.id,
          ok: true,
          result: { tab: { id: tab.id, title: tab.title || '', url: tab.url || '' } },
        });
      } else {
        await chrome.tabs.remove(wire.params.id);
        sendResponse({ id: wire.id, ok: true, result: {} });
      }
    } catch (e) {
      // Static detail: never forward browser error text (it can embed paths).
      sendResponse({
        id: wire.id,
        ok: false,
        error: { code: 'backend_error', detail: 'tab operation failed' },
      });
    }
  })();
  return true; // async sendResponse
});

/* =====================================================================
 * EMBEDDED POPUP SOURCES
 *
 * This task's file budget allows only manifest.json + background.js, so
 * the popup page sources ship inside this comment block. Extract them
 * into the extension dir before loading unpacked (see
 * docs/browser-tabs.md):
 *
 *   python3 -c "
 *   import re,pathlib
 *   src = pathlib.Path('background.js').read_text()
 *   for stem in ('HTML', 'JS'):
 *       m = re.search(r'__POPUP_' + stem + r'_BEGIN__\n(.*?)__POPUP_' + stem + r'_END__', src, re.S)
 *       lines = [re.sub(r'^\s*\* ?', '', ln).rstrip() for ln in m.group(1).splitlines()]
 *       lines = [ln for ln in lines if ln != '---']
 *       pathlib.Path('popup.' + stem.lower()).write_text('\n'.join(lines).strip() + '\n')
 *   "
 *
 * --- __POPUP_HTML_BEGIN__
 * <!doctype html>
 * <html lang="en">
 * <head>
 * <meta charset="utf-8">
 * <title>Muse tabs</title>
 * <style>
 *   body { font: 13px system-ui, sans-serif; width: 360px; margin: 12px; }
 *   input { width: 100%; box-sizing: border-box; margin: 4px 0; }
 *   button { margin: 4px 4px 4px 0; }
 *   table { border-collapse: collapse; width: 100%; margin-top: 8px; }
 *   td, th { border: 1px solid #ccc; padding: 2px 6px; text-align: left; }
 *   pre { white-space: pre-wrap; word-break: break-all; color: #a00; }
 * </style>
 * </head>
 * <body>
 * <h3>Muse tabs (local bridge)</h3>
 * <button id="list">List tabs</button>
 * <div>
 *   <input id="openUrl" type="url" placeholder="https://example.com/">
 *   <button id="open">Open URL</button>
 * </div>
 * <div>
 *   <input id="closeId" type="number" min="0" placeholder="tab id">
 *   <button id="close">Close tab</button>
 * </div>
 * <table id="tabs" hidden><thead><tr><th>id</th><th>title</th><th>url</th></tr></thead><tbody></tbody></table>
 * <pre id="err"></pre>
 * <script src="popup.js"></script>
 * </body>
 * </html>
 * --- __POPUP_HTML_END__
 *
 * --- __POPUP_JS_BEGIN__
 * 'use strict';
 * let nextId = 1;
 * const $ = (id) => document.getElementById(id);
 *
 * function showError(detail) {
 *   $('err').textContent = detail || '';
 * }
 *
 * function renderList(tabs) {
 *   const table = $('tabs');
 *   const body = table.querySelector('tbody');
 *   body.textContent = '';
 *   for (const t of tabs) {
 *     const tr = document.createElement('tr');
 *     // textContent only: tab titles/URLs are never interpreted as HTML.
 *     for (const key of ['id', 'title', 'url']) {
 *       const td = document.createElement('td');
 *       td.textContent = String(t[key]);
 *       tr.appendChild(td);
 *     }
 *     body.appendChild(tr);
 *   }
 *   table.hidden = false;
 * }
 *
 * function callHost(op, params, onResult) {
 *   showError('');
 *   chrome.runtime.sendMessage({ id: nextId++, op, params }, (reply) => {
 *     if (!reply || !reply.ok) {
 *       showError(reply && reply.error ? reply.error.detail : 'no reply');
 *       return;
 *     }
 *     onResult(reply.result);
 *   });
 * }
 *
 * $('list').addEventListener('click', () => {
 *   callHost('tabs.list', {}, (result) => renderList(result.tabs));
 * });
 * $('open').addEventListener('click', () => {
 *   const url = $('openUrl').value;
 *   callHost('tabs.open', { url }, (result) => renderList([result.tab]));
 * });
 * $('close').addEventListener('click', () => {
 *   const raw = $('closeId').value.trim();
 *   if (raw === '') {
 *     showError('tab id is missing');
 *     return;
 *   }
 *   const id = Number(raw);
 *   if (!Number.isInteger(id) || id < 0) {
 *     showError('tab id must be an unsigned integer');
 *     return;
 *   }
 *   callHost('tabs.close', { id }, () => $('list').click());
 * });
 * --- __POPUP_JS_END__
 * ===================================================================== */

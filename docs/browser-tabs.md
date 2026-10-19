# Browser tabs bridge

User-gesture tab list/open/close for the Linux port (Mac parity row
"browser tabs"). The extension popup is the only trigger: each List, Open
and Close click is explicit per-action consent executed directly against
`chrome.tabs`. There is no agent-driven tab control, no polling, and no
remote trigger path.

## Architecture

```
popup (List/Open/Close click)
  -> chrome.runtime.sendMessage (own extension only)
  -> background.js service worker: allowlist-validate {id, op, params}
  -> chrome.tabs.query / .create / .remove (titles/URLs metadata only)
  -> reply rendered in the popup (textContent only, never HTML)
```

* `extension/` — MV3 sources: `manifest.json`, `background.js`,
  `popup.html`, `popup.js`. The popup page sources also ship embedded in
  `background.js` (comment block) so the two-file budget survives; the
  extracted `popup.*` files are authoritative when both exist.
* `crates/muse-browser/` — native-host protocol core (framing, strict
  schema, URL rules, `TabBackend` trait) reserved for a future
  agent-driven path. See `crates/muse-browser/API.md`. It is tested and
  documented but **not called by the extension today**: a stateless stdio
  host can neither approve actions nor reach browser tabs, so wiring it
  requires an app-side outbox + approval queue that does not exist yet.

## Install

The `extension/` dir ships complete (`manifest.json`, `background.js`,
`popup.html`, `popup.js`). If the popup files are ever missing,
re-extract them from the comment block at the end of `background.js`
with the python3 one-liner quoted there.

1. Load the extension: `chrome://extensions` (or `about:debugging`)
   -> "Load unpacked" -> `extension/`.
2. Click the toolbar button: List shows open tabs, Open takes an
   `http(s)` URL, Close takes a tab id. Every click executes exactly
   one `chrome.tabs` call; results render in the popup.

No native-host install is needed for the popup path.

## Future agent path (not wired)

Registering a host manifest so the browser can spawn a native host is
documented here for when the app-side outbox exists. Example:

   ```json
   {
     "name": "org.unofficial_muse_for_linux",
     "description": "Muse tab bridge (local, user-gesture only)",
     "path": "/opt/muse/Muse.AppImage",
     "args": ["--native-host"],
     "type": "stdio",
     "allowed_origins": ["chrome-extension://<YOUR_EXTENSION_ID>/"]
   }
   ```

   Firefox uses `allowed_extensions` instead of `allowed_origins`:

   ```json
   {
     "name": "org.unofficial_muse_for_linux",
     "description": "Muse tab bridge (local, user-gesture only)",
     "path": "/opt/muse/Muse.AppImage",
     "args": ["--native-host"],
     "type": "stdio",
     "allowed_extensions": ["muse-tabs@unofficial.example"]
   }
   ```

   Manifest locations differ per browser (this is a pairing step, not
   automatic setup):

   | Browser | Host manifest path |
   | ------- | ------------------ |
   | Chrome / Chromium | `~/.config/google-chrome/NativeMessagingHosts/org.unofficial_muse_for_linux.json` |
   | Firefox | `~/.mozilla/native-messaging-hosts/org.unofficial_muse_for_linux.json` |

## Permission disclosure

* `manifest.json` requests the `tabs` permission **only**. No
  `<all_urls>`, no host permissions, no content scripts.
* What `tabs` exposes: the **title and URL of open tabs** (returned by
  `tabs.list`), plus the ability to open/close tabs when the user clicks.
* What it does NOT expose: page contents (no tab-body API exists in the
  host protocol by design), cookies, history, bookmarks, or anything
  beyond the clicked action. Error replies use static-only detail
  strings; submitted URLs are never echoed.

## Honest gaps

* **No agent-driven tab control yet.** The native host is stateless
  request/reply over stdio; an agent initiating tab actions on its own
  would need an app-side outbox/queue the host mode cannot provide. Until
  that exists, every tab action requires a human click.
* **No form filling.** Filling or submitting page content is out of scope
  for the bridge (no content scripts, no per-action remote approval
  protocol to gate it safely).
* **No per-action remote approval.** Approval today is the click itself;
  a protocol for a remote party to approve individual actions does not
  exist yet.
* **Chrome/Firefox manifest paths differ** (table above); Firefox also
  needs an `allowed_extensions` ID rather than a Chrome-style origin, and
  temporary `about:debugging` loads get a new ID each session unless the
  extension is signed/pinned.

# Explicit browser account sign-in

`scripts/muse_account_login.py` is a subprocess helper for the app's **Sign in**
button. Importing the script does not launch a browser. Do not invoke it at app
startup, in polling, or as a fallback for an expired session without another
explicit user action. Passwords, passkeys, MFA, consent, and any identity-provider
pages are handled by the user in the official browser UI. The helper has no
password/OTP/cookie input field and performs no DOM scripting or form automation.

## Invocation and integration contract

Use the selected account-bridge Python interpreter, or another Python 3.10+
interpreter. The sign-in helper itself uses only the Python standard library,
including its bounded WebSocket/CDP transport. No transport dependency or
browser is downloaded or bundled in the AppImage. The separate account bridge
still needs its own community package dependencies.

```text
/path/to/account-bridge/.venv/bin/python /path/to/scripts/muse_account_login.py
```

Write exactly one UTF-8 JSON object to stdin, then **close stdin**:

```json
{"config_dir":"/absolute/private/app-config/account"}
```

Optional properties:

- `browser`: a recognized executable name, an absolute trusted browser executable
  path, or an allowed Flatpak ID prefixed with `flatpak:` (for example
  `flatpak:com.brave.Browser`). This is one executable selector, never a shell
  command or an argument list. Omit it for native-first automatic discovery.
- `timeout_seconds`: integer from 1 through 900; default 300. This bounds browser
  startup/CDP/sign-in polling after browser discovery. Browser discovery can add
  at most four three-second Flatpak metadata checks; bounded cleanup adds time.

Unknown fields, relative configuration paths, parent-directory traversal, NULs,
non-integer/out-of-range timeouts, and oversized input are rejected. Input is
limited to 16,384 characters. Do not pass secrets in arguments, environment
variables, or JSON. The selected Python executable and browser executable are
trusted local code, not arbitrary files supplied by a web page.

Stdout contains one JSON line, with no cookie values:

```json
{"ok":true,"cookie_file":"/absolute/private/app-config/account/cookies.txt","message":"Account sign-in completed."}
```

Failure returns exit status 1 and `{ "ok": false, "error": "stable_code",
"message": "fixed safe message" }`. Codes include `invalid_request`,
`missing_browser`, `unsafe_config`, `browser_launch_failed`,
`browser_closed`, `unsafe_devtools`, `devtools_error`, `timeout`, `cancelled`,
`cleanup_failed`, and `internal_error`. Exception text, server errors, browser
output, CDP responses, target titles, and session material are never reflected.
Success exits 0. Cancellation is **SIGTERM or SIGINT to this helper**, not a
password or another JSON message. Allow the helper to finish browser cleanup;
SIGKILL, parent hard-kill, crashes, and power loss cannot run cleanup.

After success the caller must validate the session using the account bridge
before showing a connected identity or task history. Cookie presence is not
proof that a server will accept the account/session. The helper does not itself
query account APIs, merge task histories, or alter the external community CLI's
configuration. Caller should use the returned app-owned jar explicitly.

## Profile, endpoint, and cookie isolation

- Every request creates a new, unpredictable `.login-…` directory with mode
  `0700` beneath the selected private app configuration directory. There is no
  migration, copy, reuse, or inspection of the user's normal browser profile.
- Browser launch supplies the dedicated `--user-data-dir`, disables extensions
  and sync, and navigates to exactly `https://muse.ai/`. Known inherited browser
  flag/profile overrides and TLS-key/debug-log output variables are cleared;
  browser XDG config/cache/data/state are redirected into the temporary profile.
  Existing browser windows and profiles are not attached. The browser sandbox
  is not disabled.
- This version uses `--remote-debugging-port=0` with an explicit
  `127.0.0.1` binding. The pipe alternative requires reliable FD 3/4 forwarding
  through browser wrappers and Flatpak and is not used here. There is no fixed
  debugging port, public bind, discovery over a remote host, or `/json` endpoint
  fallback.
- `DevToolsActivePort` is opened relative to the new profile's directory FD with
  `O_NOFOLLOW`, and must be a bounded, regular, current-user-owned, single-link,
  non-group/world-writable file with timestamps at least as recent as launch.
  Missing files are retried; stale, malformed, symlinked, and unsafe files fail
  closed. The launched process must still be alive.
- Linux `/proc/net/tcp`, `/proc/net/tcp6`, process parentage, and socket FD ownership
  must prove the listener belongs to the just-launched process or one of its
  descendants and binds only loopback. If that proof is unavailable, sign-in is
  rejected. Only the validated `ws://127.0.0.1:<port>/devtools/browser/<token>`
  endpoint is accepted. A preconnected IPv4 loopback socket prevents proxy/DNS
  substitution, the peer is checked, and the standard-library WebSocket client
  accepts only an HTTP 101 upgrade with a valid nonce/accept handshake. It has
  no redirect, DNS, proxy, cookie-header, subprotocol, or compression machinery.
  Frames/messages are bounded to 1 MiB; text fragmentation and ping/pong are
  supported. Masked server frames, reserved bits/opcodes, unsolicited
  extensions, invalid UTF-8, and oversized frames fail closed.
- CDP attaches only to HTTPS pages with the **exact `muse.ai` host**. It requests
  `Network.getCookies` with `urls: ["https://muse.ai/"]`, never all-domain cookie
  methods, network-event capture, or JavaScript evaluation. Identity-provider
  pages are left entirely to the user and their cookies are never requested.
- From the URL-scoped response, only the single nonempty `hatch_sess` cookie for
  `muse.ai` or the equivalent `.muse.ai` domain spelling, secure root path `/`,
  is retained. Subdomains, suffix lookalikes, wrong names, partitioned cookies,
  expired values, invalid cookie octets/control characters, and ambiguous
  duplicate candidates are rejected. Other same-site cookies can occur in the
  URL-scoped CDP response but are not exported or logged.
- The resulting Netscape jar contains **one host-only `muse.ai` cookie**; the
  leading domain dot is intentionally removed to narrow export scope. Secure,
  HttpOnly, and expiration are preserved (session expiration is represented by
  `0`). Callers using `MozillaCookieJar` must load session cookies with
  `ignore_discard=True, ignore_expires=True`, as the bridge does.
- Browser stdout/stderr are discarded and the transport has no trace logging.
  The browser process group is terminated and checked for live members, and the
  temporary profile is deleted before the jar is published. If termination
  cannot be verified, cleanup fails rather than publishing a new jar. User
  browser sessions outside that launched process group are never terminated.

## Filesystem policy

Use a dedicated app-owned configuration directory, not `~/.config/muse-cli`.
New directories are `0700`; an existing destination must already be owned by the
current user and private (`0700` or stricter). Symlink components, unsafe
ownership, and writable non-sticky ancestors are refused. The helper does not
silently chmod a preexisting shared configuration directory.

The jar is staged as a random exclusive `0600` file, flushed and fsynced, then
atomically replaced using directory-relative operations; the directory is
fsynced too. A preexisting `cookies.txt` can be replaced **only** if it is a
private, single-link, current-user-owned regular file with this helper's exact
ownership marker in its header. Symlinks, hardlinks, and unrelated jars are
refused. A destination directory named `muse-cli` is also refused. Existing
external CLI credentials are neither read nor overwritten. A failed sign-in
leaves a previously saved app jar unchanged.

The ordinary same-user Linux security boundary applies: root, a compromised
same-UID process, a malicious selected browser/runtime, ptrace/debuggers, and
memory inspection are out of scope. This is not a security boundary against
other code already able to read the user's private files. Secure deletion from
snapshots/SSD blocks is not promised. Hard-killed attempts may leave a private
`.login-…` directory; remove it only after confirming its dedicated browser has
closed. It is never reused on a later attempt.

## Supported setup and limits

- Linux, Python 3.10+ with only its standard library, graphical
  browser access, and readable same-user `/proc` socket/process information.
- Native discovery recognizes Chrome/Chromium, Brave, Microsoft Edge, and
  Vivaldi executable names. Absolute executable paths are explicit trusted
  overrides, not a compatibility guarantee for a different browser engine.
- Flatpak discovery recognizes `com.brave.Browser`, `com.microsoft.Edge`,
  `com.vivaldi.Vivaldi`, and `org.chromium.Chromium`. The launch uses
  `--die-with-parent`, resets inherited host filesystem grants, grants only the
  temporary profile, and redirects XDG config/cache/data/state into that
  profile. Chromium's new user-data directory provides instance isolation;
  `flatpak run` has no `--new-instance` option.
- Flatpak browser support is a guarded compatibility path, **not live-login
  verified**. Distribution wrappers, enterprise policy, PID namespace behavior,
  portal/Wayland/X11 restrictions, AppImage environment contamination, or
  restricted `/proc` may prevent startup/ownership proof. Fail closed and select
  a supported native browser instead; never relax endpoint or profile checks.
- The official service or identity provider may change login/session behavior or
  reject a debug-enabled browser. No CAPTCHA/MFA bypass, default-profile
  fallback, all-domain export, password prompt in the app, or live-login unit
  test is provided.

## Offline verification

```text
/usr/bin/python3 -m unittest discover -s scripts/tests -p test_account_login.py -v
./scripts/test-account.sh
```

The suite runs a fake browser subprocess and controlled loopback WebSocket/CDP
server. It verifies a full sign-in exchange with synthetic cookies, exact URL
scoping, target/domain boundaries, one-cookie jars, private permissions,
atomic-replace failure, unrelated-file protection, symlinks, stale DevTools
files, listener/process ownership, cancellation, timeout, browser reaping/profile
cleanup, CDP error redaction, and rejected WebSocket redirects. It never launches
a real browser, reads a normal browser profile, accesses user credentials, or
requests a third-party page. All cases, including the real controlled-loopback
WebSocket exchange and subprocess cancellation/cleanup, execute with the system
Python standard library; there are no dependency skips or network installs.

# Goal-ready plan: full-feature Linux port of Muse for Mac

**Plan status:** account-linked second batch, updated October 17, 2026 (America/Detroit). This is the target contract, not a claim of full parity or verified fresh sign-in.

## Copy-ready goal

Build a standalone, Linux-native companion/local host for the user's existing Muse personal agent, behaving like Muse for Mac on Linux: same identity, conversations, tasks, plans, approvals and history, with Linux-native files/apps/browser/desktop capabilities under explicit permissions. Do not substitute a separate developer-key model client, wrap the website, or require/control a Mac. An explicitly disclosed unsupported community account adapter is an accepted development path. Account continuity and safe Linux local-host dispatch both need end-to-end proof; neither replaces the other.

“Feature complete” means every frozen parity row has an implementation, documented support boundary, and passing end-to-end tests on the declared Linux matrix. A packaged shell, mocked response, successful account probe, or separate model API is not sufficient.

## Current baseline and gaps

- Native dark GTK4 Chats/Files shell; session-only read-only folder grant, confined browsing, bounded UTF-8 previews and bounded recursive filename search without following symlinks. Earlier AppImage chooser/list/preview and navigation checks ran on KDE Wayland **through XWayland**; native Wayland is unverified.
- Second-batch account code: typed connection, native shared thread/history and scheduled-task/run lists, and one-shot prompt dispatch through external pinned community source and the user's explicitly selected session. See [account integration](account-integration.md) for architecture, source/license, private plaintext session storage and protocol drift.
- Explicit isolated official-browser sign-in helper and opt-in existing local session path. Offline helper tests or authenticated existing-session probes are **not proof of fresh browser authentication**, server-side revocation, or packaged GUI behavior.
- Account linkage is **not Mac-style device/node registration or local-capability dispatch**. Local files stay read-only and separate; no local shell, browser, screen, or app tools are granted by connecting an account.
- Developer-key Model API/runtime crates remain separate experiments. They do not provide personal-account login or continuity.
- Streaming, plans/approvals/cancel/resume, task editing, file writes, desktop/browser/provider connectors, durable Linux background work, and full privacy/revocation controls remain incomplete.

The canonical [feature matrix](feature-parity.md) separates implemented scope from verification. Release evidence must identify the exact artifact, local date/timezone and exercised scope; never put account names/IDs, messages or credentials in public evidence. Record the exact Mac build before freezing its reference behavior—product-wide marketing is not proof of a feature in that build.

## Non-negotiable boundaries

- Native Linux UI and host integration, not an embedded website or Mac relay.
- Use only the user's explicitly authorized account session. No shared hidden credentials, normal-browser profile scraping, credential arguments/logging, or bundled account state.
- The community protocol is internal/unversioned and unsupported by Meta; disclose this and fail clearly on drift. Unsupported status is not a blanket development blocker or a license to claim untested parity.
- Our distribution contains our code, not Meta app binaries/assets or the external community protocol checkout/descriptors/routes. Install the dependency separately per user with source/license provenance intact.
- Never expose an unrestricted shell or whole-home-directory access to the agent. Account sign-in does not authorize local OS access.
- Distinguish read, ordinary write and sensitive write. Consequential actions need exact preview and fresh approval. Denial/cancel leaves no side effect; uncertain sends are never retried blindly.
- Keep account protocol, Linux connectors and history behind replaceable typed/testable interfaces. Unsupported capabilities must be unavailable, not simulated.

## Account integration versus local-host pairing

The current account adapter is the community `muse-mcp` source at the pin documented in [account integration](account-integration.md), loaded by an explicitly selected per-user Python interpreter. It reaches the personal-agent gateway using the user's session; the Rust app exposes only connect/history/tasks/send. The task lists are backend schedules and executions, not a new Linux scheduler. The runtime does not run an MCP server or expose arbitrary routes.

The first-party Linux gadget SDK is a separate phone/BLE device-token path, not personal-client login and not a chat-history/task-fetch API. Its library `subscribe_chat()` is not evidence of a complete desktop account protocol; the stock Unix IPC path is send-only. It is not integrated in this batch. Any future local-host route requires its own registration, dispatch, permission and revocation tests; do not add broad `system.run` access as a shortcut.

The account helper's isolated browser login is specifically scoped to official Muse sign-in, not a general browser connector. Its private session jar is a reusable plaintext credential; OS credential-store support and server-side revocation checks are still needed. Selecting an existing session is explicit and does not prove fresh login. A history-confirmed send proves prompt persistence, not the completion of the agent's requested action.

## Work plan and exit gates

### Phase 0 — Freeze reference behavior and integration boundaries

- Record exact Muse-for-Mac build, observed sign-in/local-host flows, and row-by-row capability/permissions/desktop/test requirements.
- Record the accepted unsupported community source pin, provenance, dependency versions and account/session scope. Test real continuity separately from device registration and local dispatch.
- Retain the chooser lifetime regression: GTK native dialogs must remain owned until a response. Repeat actual chooser accept/cancel, list/preview and confinement checks on each release artifact, separately on native Wayland and XWayland.

**Exit:** a frozen parity matrix and explicit account/local-host distinction; real account identity/continuity evidence without exposing credentials; exact artifact folder grants pass or the gap is stated.

### Phase 1 — Native conversation and task continuity

- Exercise account connect, thread selection, scoped history, schedule/run list shapes and single prompt dispatch against the user's actual personal agent, then through the packaged GUI.
- Prove fresh isolated-browser sign-in and missing-browser/session-expiry behavior separately; test disconnect, private storage and actual server-side revocation expectations.
- Extend streaming, plan/progress, structured errors, approvals, cancel/resume and durable state. Do not mistake prompt persistence for task completion, a timeout for cancellation, or list reads for task editing.
- Keep secrets out of prompts/logs/crashes and use an OS credential store when implemented.

**Exit:** genuine same-agent chat/history/task reads through the native artifact, confirmed single send, safe uncertain outcomes, responsive UI and tested auth failure/recovery; remaining plan/cancel/durability scope explicit.

### Phase 2 — Safe account-linked Linux files

- First prove an account-linked local-host registration/dispatch protocol; account login alone does not provide one.
- Expose only typed granted-root search/read and supported document extraction with bounded work and unsupported-format errors.
- Add create/edit/rename/move/delete only behind distinct grants, exact destination/content preview and per-operation approval; provide undo/recovery.
- Test path traversal, symlinks, races, oversized/binary files, prompt injection, revocation and changing files.

**Exit:** the same personal agent finds/summarizes an explicitly granted document and can prepare/write only after approval. Deny/revoke leaves local files unchanged.

### Phase 3 — Browser and computer use

- Add explicit extension/native-messaging browser context/actions, without silently reusing unrelated browser profiles or passwords.
- Use portal-mediated screen/input and accessibility for semantic UI context. Declare supported KDE/GNOME Wayland paths and a separately tested XWayland fallback.
- Show context/action progress and require a fresh detail-specific approval before consequential submit/send/purchase/delete.

**Exit:** real harmless browser/desktop actions work, sensitive submission pauses for approval, and cancel/revoke stops control; results are recorded separately by desktop/backend.

### Phase 4 — Native provider connectors

- Implement only named provider-specific mail/calendar/reminders/contacts/notes/messaging connectors; Linux has no universal API for them.
- Prefer published APIs, explicit scopes, read-only defaults, safe credential/revocation handling and clear rate-limit/errors.
- Separate draft/proposal from commit/send. Verify external mutations by reading back the exact target; never blind-retry uncertain writes.

**Exit:** a real approved workflow for each claimed provider, denied actions cause no mutation, and compatibility/scope/test date are documented.

### Phase 5 — Background work and memory

- Add Linux task persistence, scheduled/long-running work, visible status, notifications, pause/resume/cancel and a global stop control.
- Give memory inspect/edit/export/delete controls. Survive app close, restart, network failure and session expiry without duplicate or unapproved actions.

**Exit:** a real task survives close/restart, notifies on completion/approval, can be stopped and leaves a verified audit trail. Reading backend schedules alone does not pass.

### Phase 6 — Parity hardening and release

- Run shell/Python syntax, offline unittest, Rust fmt/build/test/Clippy, backend contracts, UI/portals and end-to-end acceptance tests. Keep all existing CI gates.
- Security-test prompt injection, path escapes, permissions, secrets, changed approval details, retries, drift and uncertain outcomes.
- Inspect each AppImage: our sidecars present, no external community/Meta-derived protocol assets or credentials. Verify installation, portals, accessibility, upgrade/removal and credential behavior.
- Publish truthful capability/evidence boundaries, privacy/data flow, build instructions and source/license inventory. Do not call an untested row feature complete.

## Release acceptance scenarios

1. **Account continuity:** genuine same-agent native chat/history and schedule/run reads; send once, read back persistence, handle expired session and uncertain outcome without auto-retry.
2. **Fresh sign-in:** isolated official login with user interaction, only scoped private session saved; missing browser and denied/cancelled login fail clearly; revocation behavior tested.
3. **Folder access:** actual packaged chooser remains open; allow one folder, browse/search/preview; outside-root access and cancel are denied.
4. **Research/create:** same agent reads only a granted document, previews a new output and writes only after approval; denial leaves filesystem unchanged.
5. **Cross-app/draft/send:** authorized providers only; exact recipient/content/time preview, fresh approval, one mutation and read-back confirmation.
6. **Browser/desktop:** granted context and harmless actions work; consequential submit waits; revoke halts further actions.
7. **Background/lifecycle:** work survives restart without duplicates, notifies appropriately, can be stopped; permission denial/revocation and recovery are auditable.

## Definition of done

Every frozen feature passes its end-to-end scenario on the declared Linux matrix. Same-account continuity **and** Linux local-host capabilities both pass; neither a community import check nor a developer-API mock substitutes. Fresh sign-in, actual artifact checks, local dispatch, read/write/sensitive approvals, revoke/audit/recovery, native Wayland and XWayland all have separately stated evidence or gaps.

## References

- [Account adapter source, pin and protocol/license disclosure](account-integration.md); [canonical feature/evidence matrix](feature-parity.md).
- [First-party Linux gadget SDK](https://github.com/facebookincubator/muse-gadget-sdk/tree/main/linux), distinct from this community account path.
- [GTK4 GtkNativeDialog lifecycle](https://docs.gtk.org/gtk4/class.NativeDialog.html).
- [XDG desktop portals](https://flatpak.github.io/xdg-desktop-portal/docs/) and [AT-SPI](https://www.freedesktop.org/wiki/Accessibility/AT-SPI2/) for future permissioned Linux integrations.

# Goal-ready plan: full-feature Linux port of Muse for Mac

**Plan status:** proposed scope; feature baseline reviewed 2026-10-17. This is a product plan, not a claim that the current prototype is usable.

## Copy-ready goal

Build a standalone, Linux-native desktop agent that ports the user-visible capabilities of the documented Muse for Mac experience. Reimplement its desktop behavior using Linux portals, accessibility APIs, browser support, and provider-specific connectors; do not require a Mac host, control a Mac app, or wrap the Muse website. Users must be able to converse with the agent, delegate multi-step work, grant Linux files and supported app/service access, review progress, approve consequential actions, and stop or resume work. Use supported Muse service interfaces when available; keep model/service authentication separate from Linux desktop permissions and never claim unsupported account compatibility.

“Feature complete” means every feature in the frozen parity matrix below has an implementation, a documented support boundary, and passing end-to-end tests on the supported Linux desktop matrix. A folder browser, packaged shell, mocked assistant, or separate model API is not sufficient to call this a Muse-for-Mac port.

## Product target and current gap

The supplied TechCrunch report describes Muse for Mac taking actions across local files and native apps, including Messages, Calendar, Notes, and Mail, with opt-in access and approval for sensitive actions. Meta’s current product pages describe a broader agent that plans and completes multi-step tasks, browses and acts on the web, connects to apps, creates documents and images, tracks goals/reminders, and continues work in the background. Meta describes the Mac app as working with desktop files, apps, browser tabs, Messages, Calendar, and Notes. Treat the exact installed Mac build as the authority when validating each row; marketing pages and the article are a starting inventory, not proof that every product-wide feature is exposed in the Mac app.

**Current Linux baseline:** GTK4 shell; filesystem core confines reads to a selected root, supports bounded recursive filename search without following symlinks, and previews bounded UTF-8 text. The app now includes a simple session-only chat UI, a non-streaming Meta Responses API adapter, and a bounded read-only file tool loop. The developer key and chat are held in memory; paid use and filename/content sharing each require separate opt-ins. Adapter/runtime behavior is covered by local mock tests, but no live API request has been made or verified. The freshly rebuilt AppImage launched on KDE Wayland via XWayland, displayed the assistant UI, and rejected a prompt locally when no API key was active; no key was entered and no request was sent. The chooser, listing, and local preview were manually checked on an earlier build; the search UI still needs end-to-end GUI verification. Streaming, plans, durable history, cancel/resume, consumer sign-in, browser and desktop-app connectors, file writes, and background work are not implemented. The AppImage remains a prototype, not a usable feature-complete port.

## Target capability matrix (to freeze in Phase 0)

| Muse capability to validate against the Mac app | Linux equivalent required for parity | Current status |
|---|---|---|
| Conversation, model response, streaming, and task history | Native chat/task UI with a supported backend and persistent sessions | Partial: non-streaming adapter and session-only chat; no live API or GUI verification yet |
| Multi-step plans, tool execution, progress, cancel, and resume | Agent runtime, typed tool registry, policy broker, durable task state | Partial: bounded read-only file tools; no plan UI, cancel/resume, or durable state |
| File context and file actions | Scoped search/read plus approved create/edit/move/delete and recovery | Read-only grant, browse, preview, bounded filename search, separately consented file context; no writes |
| Browser context and web tasks | Supported browser extension/native bridge and visible, user-intervenable actions | Not implemented |
| Computer use across desktop apps | Portal-mediated screen/input plus AT-SPI, with explicit session grants | Not implemented |
| Messages, Mail, Calendar, Notes, Reminders, Contacts | Per-provider Linux/service connectors with explicit read/write scopes | Not implemented |
| Documents/images, web research, reminders/goals, and other assistant skills | Verified backend capabilities exposed through native UI and Linux tools | Not implemented; confirm which are in the Mac app |
| Background work, notifications, and personalization/memory | Durable scheduler, audit/history, pause/cancel, user-visible memory controls | Not implemented; confirm Mac-app scope |
| Per-action permission, sensitive approval, audit, and revocation | Consent broker with exact preview and verified side effects | Not implemented |
| Same consumer Muse account and task continuity | Documented consumer login/API and end-to-end account verification | Unvalidated; separate compatibility question, not a gate for Linux-native features |
| Desktop distribution and host integration | AppImage plus working portal and native Wayland support | AppImage launches via XWayland; native Wayland unverified |

### P0 defect resolved: folder chooser lifetime

In `apps/desktop/src/main.rs`, the chooser used to be a local variable inside the click callback and its only strong reference was dropped immediately after `show()`. GTK documents that `GtkNativeDialog` is not kept alive by GTK and the caller must retain it. A small GTK lifetime probe reproduced finalization on scope exit. The app now keeps the active chooser in `DialogLease` until the response callback hides it and releases the retained reference. A unit test covers lease lifetime; the rebuilt AppImage was exercised on KythOS KDE Wayland via XWayland: the chooser stayed open, a scratch test folder was selected, its file was listed, and the file preview displayed its content. Native Wayland remains unverified.

## Non-negotiable product boundaries

- Native Linux UI and host integrations; do not embed or proxy the Muse website as a substitute.
- No browser cookies, undocumented endpoints, hidden tokens, copied Meta binaries/assets, or internal session routes.
- Never expose an unrestricted shell or whole-home-directory access to the agent.
- Every connector reports its supported provider, scopes, side effects, and failure modes. Unsupported capabilities are shown as unavailable, not simulated.
- Read, ordinary write, and sensitive write are distinct permissions. Sensitive actions (e.g. send, share, publish, purchase, delete) require a fresh, detail-specific approval each time; deny/cancel must leave no side effect.
- Keep model/backend, local OS integrations, and user-visible task history behind replaceable, testable interfaces.

## Muse service backend and account compatibility are separate from the Linux port

The port target is the Mac agent’s user-visible workflow, implemented on Linux. It must run without a Mac host or Mac application. Linux filesystem, browser, screen, and app actions are provided by Linux-native permission and connector APIs; a Muse account/session is not a substitute for those host integrations.

Use a supported Muse/Meta service interface for the assistant backend when one is available. Keep the adapter replaceable, show which service/account is active, and never use browser cookies, private routes, or internal session data. A developer API key or another supported model service may provide responses, but it does not by itself reproduce the Mac agent or establish consumer-account continuity.

Same-consumer-account sign-in and task/history continuity remain a separate compatibility item. Only implement them through documented, authorized flows and verify login, revocation, history, and permission behavior. Do not make unsupported pairing a blocker for Linux-native host features, and do not claim account compatibility until verified.

## Work plan and exit gates

### Phase 0 — Freeze the parity contract and unblock the shell

- Record the exact Muse for Mac version and observed behaviors; split Mac-specific features from product-wide Muse features. Build a row-by-row matrix: capability, Linux equivalent, provider/dependency, permission scope, tests, and status.
- Record the supported assistant-service options separately from Linux host integration. Do not block Linux-native features on undocumented same-account pairing.
- The chooser lifetime regression is fixed and manually verified through the KDE chooser: selected folder, listed file, and text preview. Keep the automated lifecycle regression and repeat the portal check in release qualification; test native Wayland and XWayland separately.

**Exit:** chooser stays open until user action; cancel changes nothing; accept grants only the selected root, lists it, and reads a fixture text file (verified manually on KDE; automate portal coverage where possible). The service-backend boundary and account-compatibility status are documented.

### Phase 1 — Agent conversation and task runtime

- Extend the existing prototype chat with conversation history, streaming response, action plan, current step, approvals, progress, structured errors, cancellation, and resume.
- Harden the existing non-streaming adapter (developer-key auth, Responses API messages/tool calls, error and rate-limit handling); add streaming and cancellation. Current mock tests do not establish live-service compatibility.
- Add a typed tool registry and policy broker. Each tool declares provider, input/output schema, read/write/sensitive classification, required grant, idempotency/status query, and auditable result.
- Persist task state safely across app restart. Keep secrets in an OS credential store; redact them from logs, prompts, and crash reports.

**Exit:** with a real authorized backend, a user prompt produces a plan, invokes a permitted read tool, displays evidence/result, handles denial and errors, and can be cancelled without an unreported side effect.

### Phase 2 — Files as a useful, safe Linux integration

- Deliver scoped file search, metadata, previews, and supported document extraction (text, PDF, office formats) with bounded work and clear unsupported-file errors.
- Add separate read and write grants. Support create/edit/rename/move/delete only behind a preview and operation-specific approval; provide undo or a recovery record where possible.
- Handle symlinks, path traversal, races, oversized/binary files, revocation, and changed files. Treat file contents as untrusted input to the model.

**Exit:** an end-to-end task can find and summarize a selected document, prepare a new document in the granted area, show the exact destination/content, and write only after approval. Denial/cancel leaves the filesystem unchanged.

### Phase 3 — Browser and computer use

- Provide browser-tab context and actions through an explicit browser extension/native-messaging bridge for supported browsers; do not read browser passwords/cookies or silently reuse unrelated profiles.
- Use Linux desktop permission APIs for screen capture and remote input, plus accessibility APIs for semantic UI context. Start with supported KDE and GNOME Wayland paths; define an explicit X11/XWayland fallback and show when a capability is unavailable.
- Support screenshot/context review, app/window selection, bounded keyboard/mouse actions, and visible action progress. Coordinate/vision fallback is allowed only with an active user-granted screen/input session.
- Pause before consequential actions (submit, send, purchase, delete); display target, content, recipient/cost/terms, and consequences. Require fresh approval if any approved detail changes.

**Exit:** tested browser and desktop workflows can read context and perform a harmless action; a form submission stops for approval; cancel/revoke halts further control. Run tests on KDE Wayland, GNOME Wayland, and XWayland as separately reported environments.

### Phase 4 — Native app and service connectors

- Build provider-specific connectors for the target Mac parity set: email, calendar, reminders, contacts, notes, and messaging, plus cloud storage where the reference app supports it. Linux has no universal API for all these apps; document each supported provider and scope rather than pretending one adapter covers every desktop.
- Prefer published service APIs and desktop data services; include an OAuth/credential flow, least-privilege scopes, read-only mode where possible, token revocation, rate-limit handling, and status checks.
- Split draft from send, event proposal from commit, and read from write. Classify sensitive writes so they require approval on every use. Verify every external mutation by reading the exact target back before reporting success.
- Keep a connector compatibility table (provider, desktop, read/write features, auth type, test date, known limitations). A future Muse Connector Platform submission is optional and subject to Meta review; it is not a substitute for the local Linux client.

**Exit:** at least one complete, real workflow for each connector claimed as supported; test/sandbox accounts only in automated tests; denied actions cause no external mutation.

### Phase 5 — Background work, goals, and continuity

- Add scheduled/long-running tasks, persistent progress, pause/resume, retry/status checks, notifications, and a user-visible activity/audit history.
- Add goal/reminder tracking and personalization only with explicit controls: inspect, edit, disable, export, and delete stored memory. Show what is running and provide a global stop/revoke control.
- Ensure app close, network interruption, session expiry, and OS restart do not cause duplicate or unapproved writes. Resume from verified state, not blind retries.

**Exit:** a background task survives app close/restart, notifies on completion or required approval, can be paused/cancelled, and produces an auditable result with no duplicate side effects.

### Phase 6 — Parity hardening and release

- Turn every parity row into a testable acceptance case. Run unit, backend/connector contract, UI/portal integration, and end-to-end tests using fixtures and test accounts.
- Security-test permission boundaries, path escapes, symlinks, prompt injection from files/pages, secret handling, approval-detail changes, revocation, retries, and uncertain outcomes.
- Verify AppImage install/launch, desktop entry, portals, notifications, credential-store behavior, accessibility, upgrade/recovery, and clean removal. Test the target Wayland desktops and XWayland separately; do not call XWayland evidence native Wayland support.
- Publish an honest capability matrix, limitations, privacy/data-flow documentation, reproducible build steps, license/provenance inventory, and release notes. No release is “feature complete” while a target parity row is a stub or untested.

## Release acceptance scenarios

1. **Folder access:** chooser remains open; grant one selected folder; browse/search; read a file; prove access outside that root is denied.
2. **Research and create:** agent finds a granted document, summarizes with source paths, prepares a new file, previews it, and writes only after approval.
3. **Cross-app briefing:** read authorized calendar and mail sources, reconcile conflicts, and return a cited/traceable summary without sending or changing anything.
4. **Draft and send:** compose an email/message or calendar change, show recipient/content/time and consequences, require approval, execute once, then read back and report the confirmed result. Denial leaves no side effect.
5. **Browser task:** use a supported browser to research or fill a form, keep the user able to intervene, and pause for approval before submission.
6. **Background task:** schedule/monitor an explicit goal, continue after app close, notify at a useful point, support pause/cancel, and never duplicate actions after restart.
7. **Permission lifecycle:** deny, allow once, persistent allow for non-sensitive tools, revoke, and verify behavior and audit trail for each.

## Definition of done

- Every frozen target feature is implemented and passes its end-to-end acceptance case on the declared Linux matrix.
- The folder grant works in the actual AppImage on KDE Wayland; the issue is not considered solved by unit tests alone.
- A real, supported assistant backend and every claimed Linux browser/app connector perform end-to-end work; no mock or packaging-only milestone is called a port. If the backend is not the supported Muse service, name it accurately and do not claim Muse account continuity.
- Read/write/sensitive permissions, approval, revoke, audit, and recovery behavior meet the safety rules above.
- Any same-consumer-account compatibility claim is verified through a documented supported flow; this is reported separately from Linux-native behavior parity.

## References

- User-provided [TechCrunch report (September 18, 2026)](https://techcrunch.com/2026/09/18/metas-muse-hits-mac-letting-the-ai-take-actions-on-your-computer/).
- [Meta Muse for Productivity](https://ai.meta.com/muse/productivity/), [Muse product capabilities](https://ai.meta.com/muse/), and [Muse download page](https://ai.meta.com/muse/download/).
- [Meta Muse Connector guidelines](https://platform.muse.ai/docs) (tool scopes, approval, sensitive writes, audit/status handling).
- [Meta Model API Responses API](https://dev.meta.ai/docs/protocols/responses) (developer API; distinct from consumer Muse account continuity).
- [GTK4 GtkNativeDialog lifecycle](https://docs.gtk.org/gtk4/class.NativeDialog.html).
- [XDG FileChooser portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.FileChooser.html), [ScreenCast portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html), [RemoteDesktop portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html), and [AT-SPI](https://www.freedesktop.org/wiki/Accessibility/AT-SPI2/).

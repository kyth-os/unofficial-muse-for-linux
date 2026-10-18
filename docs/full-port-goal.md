# Goal-ready plan: full-feature Linux port of Muse for Mac

**Plan status:** proposed scope; account-link objective clarified 2026-10-18. This is a product plan, not a claim that the current prototype is usable.

## Copy-ready goal

Build a standalone, Linux-native companion/local host for the user's existing Muse.ai personal agent, behaving like Muse for Mac on Linux. Pair through a documented Muse account/device flow permitted for the intended use and distribution, so the user gets the same Muse identity, conversations, tasks, plans, approvals, and history; provide Linux-native files, apps, browser, and desktop capabilities through explicit permissions. Do not substitute a separate developer-key model client, wrap the Muse website, or require/control a Mac. If no permitted account/pairing path fits the intended distribution, treat that as a P0 blocker—not as permission to use private routes, browser cookies, or a different backend and claim parity.

“Feature complete” means every feature in the frozen parity matrix below has an implementation, a documented support boundary, and passing end-to-end tests on the supported Linux desktop matrix. A folder browser, packaged shell, mocked assistant, or separate model API is not sufficient to call this a Muse-for-Mac port.

## Product target and current gap

The supplied TechCrunch report and Meta's current product pages describe Muse as one personal agent available through multiple clients, with Muse for Mac able to manage messages, find local files, and—with permission—control Mac apps. The Linux target is that same account-linked agent paired with a Linux-native local capability host, not a new agent that merely calls the Meta Model API. Treat the exact installed Mac build as the authority when validating each row; public product pages are an initial inventory, not proof that every product-wide feature is exposed in the Mac app.

**Current Linux baseline:** GTK4 shell; filesystem core confines reads to a selected root, supports bounded recursive filename search without following symlinks, and previews bounded UTF-8 text. The app has a session-only chat UI and an experimental direct Meta Model API adapter with a developer-key flow. That API route is separate from the user's Muse.ai personal-agent account: there is no Muse account sign-in, device pairing, shared conversation/task history, or local-host bridge yet, so the current prototype does not meet the core objective. The API adapter/runtime are mock-tested; no live request was made. A dark chat-first shell and Chats/Files navigation now exist. The AppImage launched on KDE Wayland via XWayland; navigation and settings were manually checked. Streaming, account linking, task continuity, desktop-app connectors, file writes, and background work remain unimplemented. The AppImage is a prototype, not a usable Muse.ai Linux companion.

## Target capability matrix (to freeze in Phase 0)

| Muse capability to validate against the Mac app | Linux equivalent required for parity | Current status |
|---|---|---|
| Same Muse.ai account, conversation, model response, and task history | Native client paired to the user's existing Muse agent; synced chats/tasks and persistent history | Not implemented: developer-key API route is separate and session-only; no account auth or pairing |
| Multi-step plans, tool execution, progress, cancel, and resume | Agent runtime, typed tool registry, policy broker, durable task state | Partial: bounded read-only file tools; no plan UI, cancel/resume, or durable state |
| File context and file actions | Scoped search/read plus approved create/edit/move/delete and recovery | Read-only grant, browse, preview, bounded filename search, separately consented file context; no writes |
| Browser context and web tasks | Supported browser extension/native bridge and visible, user-intervenable actions | Not implemented |
| Computer use across desktop apps | Portal-mediated screen/input plus AT-SPI, with explicit session grants | Not implemented |
| Messages, Mail, Calendar, Notes, Reminders, Contacts | Per-provider Linux/service connectors with explicit read/write scopes | Not implemented |
| Documents/images, web research, reminders/goals, and other assistant skills | Verified backend capabilities exposed through native UI and Linux tools | Not implemented; confirm which are in the Mac app |
| Background work, notifications, and personalization/memory | Durable scheduler, audit/history, pause/cancel, user-visible memory controls | Not implemented; confirm Mac-app scope |
| Per-action permission, sensitive approval, audit, and revocation | Consent broker with exact preview and verified side effects | Not implemented |
| Muse.ai account sign-in, device pairing, and same-agent continuity | Account-linked Linux local host paired to the user's existing Muse agent | Not implemented. Meta's Linux Device SDK provides a documented personal-gadget path, but it is unsupported and its terms/fit for public AppImage distribution are unresolved |
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

## Muse.ai account integration and Linux host pairing are core requirements

The target is the user's existing Muse.ai agent, accessed from a Linux-native desktop client/local host. It must run without a Mac host or Mac application, preserve the user's Muse identity and continuity, and provide Linux filesystem, browser, screen, and app actions through Linux-native permissions. The account link and local capability bridge are both required; neither replaces the other.

The direct Meta Model API adapter currently in this repository is only an experimental developer-API path. It does not sign in to muse.ai, invoke the user's existing agent, or preserve the same chats/tasks, and must not be presented as the product authentication path. Meta publishes a Linux Device SDK that pairs a Linux gadget to a Muse account and offers host commands/file read-write. However, it is a personal gadget SDK paired through the Muse phone app, not a Mac-style desktop client; it runs commands with the installed account's permissions. Its token terms are personal/non-commercial, the SDK is unsupported and may change or stop, and they restrict publicly offering token-bearing devices. Whether those terms permit a public AppImage where every user supplies their own token is unclear and needs Meta confirmation. As of 2026-10-18, open issue #154 reports that Muse-to-device command dispatch does not work for at least one account; an Oct. 15 report says it worked on a newly created account but not the reporter's existing account. Treat the SDK as a lead for a personal proof of concept, not a reliable account-link solution or public AppImage foundation until the user's own account and intended use are verified.

Account sign-in, pairing, and same-agent continuity are P0 product requirements. Evaluate the published Linux Device SDK using only the user's own Muse account/token, and verify login, revocation, shared conversation/task state, and app-to-host command dispatch on that exact account—not merely device pairing. The SDK is explicitly personal/non-commercial and unsupported; clarify with Meta whether a public AppImage that requires each user to provide their own token is permitted. Do not publicly distribute a token-bearing device without written permission. Do not scrape the website, reuse browser cookies, call undocumented endpoints, or extract private tokens. If the SDK cannot support the required desktop UX or distribution, obtain an authorized alternative rather than substituting the developer API and claiming this objective is met.

## Work plan and exit gates

### Phase 0 — Freeze the parity contract and establish permitted Muse pairing

- Record the exact Muse for Mac version, sign-in/pairing flow, and observed local-host behavior. Evaluate Meta's published Linux Device SDK as a possible personal account-linked path; it is documented but explicitly unsupported and personal/non-commercial. Confirm on the user's own account that Muse can invoke device commands, and confirm the terms permit the intended distribution and its scopes match the Mac app. Build a row-by-row matrix: capability, Linux equivalent, provider/dependency, permission scope, tests, and status.
- Treat permitted account auth, same-agent continuity, and local-host registration as P0 gates. Keep them distinct from Linux permission APIs, but do not substitute a developer API for them.
- The chooser lifetime regression is fixed and manually verified through the KDE chooser: selected folder, listed file, and text preview. Keep the automated lifecycle regression and repeat the portal check in release qualification; test native Wayland and XWayland separately.

**Exit:** an account-linked Linux path is proven against the user's Muse agent, with terms and distribution boundaries recorded; otherwise the blocker is explicit. The local client can identify the paired account without exposing credentials. The chooser stays open until user action; cancel changes nothing; accept grants only the selected root, lists it, and reads a fixture text file.

### Phase 1 — Agent conversation and task runtime

- Extend the existing prototype chat with conversation history, streaming response, action plan, current step, approvals, progress, structured errors, cancellation, and resume.
- Implement the documented Muse account/client protocol and prove that chat/tasks resolve to the user's existing Muse agent. Do not treat the experimental developer-key Responses API adapter as a substitute; retain it only if useful for isolated backend experiments.
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
- The supported Muse account flow reaches the user's existing agent and shares the expected history/task state; every claimed Linux browser/app connector performs end-to-end work. No developer API mock or packaging-only milestone is called a Muse.ai port.
- Read/write/sensitive permissions, approval, revoke, audit, and recovery behavior meet the safety rules above.
- Same-account continuity and Linux-native host capabilities both pass end-to-end; neither can substitute for the other.

## References

- User-provided [TechCrunch report (September 18, 2026)](https://techcrunch.com/2026/09/18/metas-muse-hits-mac-letting-the-ai-take-actions-on-your-computer/).
- [Meta Muse for Productivity](https://ai.meta.com/muse/productivity/), [Muse product capabilities](https://ai.meta.com/muse/), and [Muse download page](https://ai.meta.com/muse/download/).
- [Meta Muse Connector guidelines](https://platform.muse.ai/docs) (tool scopes, approval, sensitive writes, audit/status handling).
- [Muse.ai product](https://muse.ai/) and [Meta Connect 2026 Mac update](https://about.fb.com/news/2026/09/the-biggest-news-from-connect-2026/) (same personal agent across clients; permissioned Mac computer use).
- [Official Muse Linux Device SDK](https://github.com/facebookincubator/muse-gadget-sdk/tree/main/linux) and [Gadget SDK Token Terms](https://gadgets.muse.ai/sdk-terms) (account pairing path and its personal/non-commercial, unsupported scope).
- [Open Linux-gadget invocation issue #154](https://github.com/facebookincubator/muse-gadget-sdk/issues/154) (reports app-to-device dispatch varies by account; recheck before relying on it).
- [Meta Model API Responses API](https://dev.meta.ai/docs/protocols/responses) (developer API; distinct from consumer Muse account continuity).
- [GTK4 GtkNativeDialog lifecycle](https://docs.gtk.org/gtk4/class.NativeDialog.html).
- [XDG FileChooser portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.FileChooser.html), [ScreenCast portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html), [RemoteDesktop portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html), and [AT-SPI](https://www.freedesktop.org/wiki/Accessibility/AT-SPI2/).

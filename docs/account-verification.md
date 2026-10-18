# Account-linked batch verification — October 18, 2026

All dates below use America/Detroit (EDT). This report distinguishes the account backend, packaged GUI, fresh interactive sign-in, and Linux tool-host pairing.

## Executed checks

- Canonical offline account/sign-in suite: **35 tests passed, zero skipped**. Tests use controlled local transport and synthetic credentials, not the user's browser or live account.
- Locked Rust workspace tests: **49 passed**, with the one display-dependent test run separately under Xvfb: **1 passed**. Formatting and all-target warning-denying Clippy passed.
- AppImage build succeeded. The artifact is `dist/unofficial-muse-for-linux-x86_64.AppImage` (ignored by Git).
- The packaged app launched on KDE Wayland through XWayland. The primary view is native account/chat/task UI; the API-key experiment is secondary.
- A real installed Flatpak Chromium-compatible browser was started with a fresh isolated profile in headless bootstrap verification. Process-owned loopback CDP and `Browser.getVersion` succeeded; the dedicated browser was terminated and its profile cleaned. This is **not** a completed Muse browser login.

## Real-account backend checks

The typed Rust probe, using the user's explicitly selected existing local Muse session, successfully returned **1 conversation, 14 schedules, and 25 recent run records**. These are service-returned windows, not a claim of complete account export.

An initial diagnostic exposed a real transport defect: the upstream fire-and-forget send closed its Noise connection before the prompt appeared in shared history. It was not retried automatically. The sidecar now waits for the gateway response before closing, then independently confirms the exact new user message in scoped history. A regression test covers that lifetime boundary.

A distinct post-fix diagnostic performed **one dispatch**, confirmed the exact prompt in shared history, and read back the genuine assistant reply matching a unique marker. The prompt explicitly prohibited browsing, files, commands, settings changes, tasks, and third-party contact. No such remote actions were requested. No account identity, session cookie, message text, or private identifiers are recorded here.

## Packaged GUI outcome and current session limitation

The rebuilt AppImage successfully displayed account configuration, explicit existing-session selection, consent, asynchronous connection state, and a safe authentication error without freezing. During this subsequent check, the same saved session began returning HTTP 401, including from the independently installed community bridge. Source and packaged sidecar hashes matched. Therefore the packaged GUI **did not complete an authenticated shared-history/task display in this run**; do not describe that as verified.

The saved session must be signed in again through the app's separate, isolated-browser sign-in path before another authenticated GUI check. Cookie/session values were not logged or committed, and no existing credential jar was modified to conceal the error. The exact cause of the session rejection is not established; it is not proof of an AppImage-specific failure.

## Remaining boundaries

- Fresh interactive Muse browser sign-in requires the user to complete the official browser flow; it has not been exercised end to end with their credentials.
- Account linkage is implemented; Mac-style device/node registration and agent-to-Linux local-capability dispatch are **not** implemented by this batch.
- The account-linked agent gains no automatic local file, shell, screen, or app access. Local Files remains read-only and separate.
- Shared schedules and run records are read-only; no task editing, retry, or Linux scheduler is implemented.
- History expands explicitly up to 200 service events; this is not full archival synchronization.
- Native Wayland and full Mac-feature parity remain unverified/incomplete.

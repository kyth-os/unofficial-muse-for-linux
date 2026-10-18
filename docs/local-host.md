# Account-linked Linux local host

This is a capability slice toward Muse-for-Mac parity, not a claim of full parity. The personal-account adapter and the Linux tool host are separate layers. No developer API key or Mac relay is used.

## Implemented backend boundary

- A persistent, explicitly started account-gateway control connection opens `POST /link-control`, advertises a narrow command manifest and accepts typed invocations. Registration requires an HTTP success **and** the matching positive registration acknowledgement. Socket creation is not registration.
- The transport executes no local commands. A separate Rust permission broker implements `files.search`, `files.read_text` and `files.create_text`.
- A local folder grant explicitly authorizes disclosure of its relative filenames and bounded UTF-8 contents to Muse. Account sign-in and the app's separate local file-browser grants do not implicitly grant the remote agent access.
- Creation always requires a new, detail-specific approval of the exact immutable relative destination and complete contents. It creates a **new** file only: no overwrite, parent-directory creation, edit, rename, move, delete or Trash operation.
- There is no shell, arbitrary executable, browser automation, screen capture, input injection, app control, provider connector or Linux background-task runner in this slice.

## Privacy and filesystem authority

Roots are pinned directory descriptors, not paths checked once and reopened later. Linux `openat2` beneath/no-symlink resolution confines remote paths. Replacing a granted pathname cannot redirect the capability to a different root. The grant follows the original directory inode after a rename; this is an inode-capability boundary, not a same-UID process sandbox.

The service sees opaque grant IDs and generic folder labels in the registered command definitions, never the absolute local root. Search results and read contents deliberately leave the device only under the explicit remote folder grant. Private absolute paths may be displayed in local settings; do not log proposals or tool results.

Reads reject special files, invalid UTF-8 and files above 64 KiB. Search returns regular-file basename substring matches, with at most 100 results, 10,000 scanned entries and 64 levels. Search is currently case-sensitive; symlink/special-file/error encounters fail closed rather than returning a misleading partial result. Empty queries match all regular filenames within the same bounds.

Approved creation stages an owner-only anonymous `O_TMPFILE`, verifies the complete bytes and publishes with a no-replace link. Unsupported kernel/filesystem operations fail closed; there is no less-safe fallback. Existing files and symlinks are never replaced. Failed/denied/expired/revoked proposals do not publish a new file.

Invocation IDs cannot be replayed; immutable pending approvals expire against a monotonic deadline, capped at 120 seconds and bounded by the remote request's remaining budget. Revocation invalidates pending work and suppresses in-flight results. The UI/transport must stop result delivery before revoking grants, since an already returned result cannot be recalled. Disconnect and window close must stop the host, not merely hide its UI state.

## Account transport and compatibility

The host reuses the explicitly selected external community runtime and private account session through the existing credential-safe helper. That dependency remains installed outside the AppImage. The new sidecar is bundled beside `muse_account_bridge.py`; it imports that exact sibling path and does not duplicate authentication or scrape normal browser profiles.

Protocol source was inspected at first-party gadget SDK revision `f60347aeb851a1fe9438cfc388721776aa24f275`. The source-derived control stream is headerless `POST /link-control`, daemon service 0, with little-endian u32-prefixed JSON. Successful results use wire `payload`, not the local API's `output`. Encoded control messages are capped at 256 KiB, including JSON escape expansion. These facts and fake-protobuf tests do **not** prove that a personal-account session has the required service entitlement.

There is no automatic reconnect or result replay. Authentication failure, forbidden registration, protocol errors, unpair events, reset and EOF are explicit terminal states. The helper environment excludes provider secrets and Python/loader overrides. Stop is nonblocking to the GTK main thread; worker-owned process cleanup includes Linux parent-death protection.

Chat/device attribution must use the actual registered Linux node ID as `device_id` while that host is registered. The existing account's saved CLI/Mac node identity must not be overwritten. Native integration and its lifecycle tests are a separate acceptance gate from the two backend crates.

## Evidence and outstanding gates

Backend source exists and has offline tests. The broker's independent review found a registration-boundary discovery bug; the registered command parameter descriptions/enums now include usable grant IDs, with a regression test that failed before the fix. Focused re-review passed. Transport review found a post-dequeue cancellation race; a new deterministic regression pauses outgoing request, registration and result frames, cancels, and checks none are subsequently sent. All three subcases failed before the guard and passed afterward; independent focused re-review passed. This does not promise atomic socket cancellation or recall of a send already entered. Native integration is tracked separately.

Native integration is implemented and offline-tested: a Local host rail page with explicit consent, folder grants via the owned chooser, immutable Allow-once/Deny approval cards with monotonic expiry, stop-before-revoke ordering on Stop/disconnect/error/window-close, and chat attribution to the session-random Linux node ID only while registered. Device sends preserve the saved CLI node identity and default send path. Coverage: real-transport/fake-helper approval lifecycle tests, deny/expiry/revoke/stale-completion tests, device-dispatch/disconnect coupling tests and three separate-process display smokes; independent UI review passed with no findings.

No real account authentication, live host registration or actual cloud-agent invocation was performed by these backend tests. The last saved-account evidence from the prior batch was HTTP 401; fresh, user-controlled sign-in is required before a live registration/invocation acceptance test. Do not show or report this machine as paired until the service actually acknowledges registration.

Required release checks:

1. The packaged native UI obtains explicit host/folder consent and shows real connecting/registered/error states.
2. A controlled transport invokes real broker reads/searches on scratch files and delivers the real results.
3. A native approval card creates the exact new scratch file only after Allow once; Deny, expiry and revoke do not create it.
4. Account disconnect, scope changes, host stop and window close stop authority and drop stale results/cards.
5. Fresh account authentication and a real registration/read/create workflow are verified separately, or the precise blocking response is recorded without a success claim.
6. The final AppImage contains the verified sidecars, no saved credentials, and no bundled community/Meta protocol assets. Native Wayland and XWayland evidence remain distinct.

## Developer gates

The canonical `scripts/test-account.sh` now includes wire-contract tests using the selected external runtime's real protobuf classes with **fake network IO**. It rejects skipped tests. Install the pinned test runtime with `scripts/setup-account-bridge.sh`, then select its absolute interpreter through `MUSE_ACCOUNT_TEST_PYTHON`. This does not sign in or read credentials.

The AppImage builder installs the pinned runtime as a test-only dependency under `/opt/muse-test`; it is not copied into the distributable artifact. CI likewise installs a separate test runtime. Runtime-specific tests must not be silently skipped because an arbitrary system Python lacks those classes.

See [`muse-local-tools/API.md`](../crates/muse-local-tools/API.md) and [`muse-local-link/API.md`](../crates/muse-local-link/API.md) for exact types, resource bounds, error codes and source-versus-live verification limits.

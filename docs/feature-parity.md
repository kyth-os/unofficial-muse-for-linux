# Muse for Mac feature inventory and Linux port plan

The target is a native client for the user's existing Muse personal agent plus permissioned Linux host capabilities. No Mac relay or embedded website is required. The [full-port goal](full-port-goal.md) defines the acceptance bar; [account integration](account-integration.md) describes the current unsupported community path.

**Status convention:** implemented means code exists; verified means a named test or manual check exercised it. Offline tests, authenticated existing-session probes, fresh browser sign-in, packaged GUI checks, and local-host dispatch are separate evidence categories. Do not infer one from another.

| Reference capability | Linux implementation direction | Current scope / verification boundary |
|---|---|---|
| Same-account conversation and history | Typed community account adapter, native thread/history views and prompt dispatch | Second batch implementation; unsupported internal protocol, external dependency and explicit user session; integrated live/GUI evidence recorded separately |
| Shared scheduled tasks and execution history | Native read-only schedule/run lists | Second batch implementation; `tasks.list` is schedules and `tasks.runs` is executions; no task creation/edit/cancel or Linux scheduler |
| Muse sign-in | User-controlled isolated official browser login, or explicit existing local session | Helper implementation and offline tests; requires compatible Chromium; fresh live authentication/revocation not proven |
| Device/node registration and Linux local-tool dispatch | Account-linked local-host page: explicit start, folder grants, approval cards, stop/revoke; device-attributed chat while registered | Implemented with offline/fake-IO tests (91 Rust, 53 Python, 3 display smokes); live registration/invocation not yet verified, needs fresh sign-in |
| Multi-step execution, streaming, plans, approvals, cancel/resume | Typed tools, visible progress, durable state and action-specific consent | Not complete; old bounded read-only developer-API tool loop remains a separate experiment |
| Background tasks, notifications, goals and memory | Durable Linux task store and user-visible controls | Not implemented; reading backend schedules is not Linux background execution |
| Folder listing and file reads | User-selected canonical root; bounded read-only preview | Implemented, session-only; earlier chooser/list/preview smoke checks via XWayland |
| Filename search | Bounded recursive case-insensitive search, no symlink following | Implemented/tested; GUI end-to-end search still needs verification |
| Agent file-context sharing | Explicit name/content consent and bounded reads | Developer-API runtime experiment only (16 KiB/read, 3 reads/turn); not connected to account-linked personal agent |
| Create/edit/move/delete | Separate write/delete grants and exact preview/confirmation | Planned |
| Calendar, reminders, contacts, notes, mail, messaging | Provider-specific read/write connectors and send confirmation | Planned |
| Browser tabs and form filling | Optional extension/native-messaging bridge | Planned; sign-in browser automation is not an agent browser connector |
| Screen context and desktop actions | ScreenCast/RemoteDesktop portals plus accessibility | Planned; no silent global-input fallback |
| Voice, camera, photos, location | PipeWire and portals if present in frozen Mac baseline | Verify Mac scope; not implemented |
| AppImage | Bundle app/our sidecars; use host Python, browser, desktop services | Packaging updated for second batch; earlier artifact launches via XWayland on KDE Wayland; new artifact/native Wayland need separate checks |

## Safety rules

1. A folder grant covers only the selected canonical directory. No path escape or unrestricted home/shell access.
2. Local files remain read-only and separate from account linkage. Writes/moves/deletes need distinct grants and confirmations.
3. Screen/input access must use the desktop consent mechanism; unavailable APIs remain unavailable.
4. Sending a prompt is an explicit external action and may trigger the existing agent's remote capabilities. Do not auto-retry an uncertain dispatch; history confirmation proves prompt persistence, not task completion.
5. Never commit or log credentials. The current session jar is plaintext protected by private ownership/permissions (directory 0700, file 0600), not encrypted storage; keyring integration remains future work.
6. Existing CLI sessions are opt-in, not silently discovered/reused. Reject symlinks and insecure credentials without changing arbitrary files.
7. Keep unsupported protocol, source/license provenance, dependency trust, and service drift visible. Do not bundle external Meta-derived route/descriptors or a credential-bearing environment.
8. Future local sends/shares/purchases/deletes require fresh detail-specific approval and read-back verification. Account sign-in is not a Linux permission grant.

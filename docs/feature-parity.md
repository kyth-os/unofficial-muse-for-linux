# Muse for Mac agent feature inventory and Linux port plan

The target is a standalone Linux application that reproduces the Mac agent’s user-visible behavior with Linux-native desktop and service integrations. It does not require, control, or bridge to a Mac, and it does not wrap the Muse website. The [goal-ready full-port plan](full-port-goal.md) defines scope, sequencing, external compatibility questions, and acceptance tests.

| Reference capability | Linux implementation direction | Status |
|---|---|---|
| Same Muse.ai account, conversation, model response, and task continuity | Native client paired to the user's existing Muse agent; shared history/tasks and local host integration | Not implemented: current developer-key Responses API path is separate/session-only; no Muse account auth or device pairing |
| Multi-step tool execution and progress | Typed tools, permission broker, visible progress, cancel/resume, and durable task state | Partial: bounded read-only file tool loop and progress; no plan UI, cancel/resume, or persistence |
| Background tasks, goals, notifications, and memory | Durable scheduler/task store and user-visible controls | Not implemented; confirm Mac-app scope |
| Folder listing and file reads | User-selected roots; canonical-path confinement; read-only grant first | Implemented locally (session-only; chooser, listing, and preview smoke-tested) |
| Filename search | Bounded recursive case-insensitive search inside the granted root; omit symlinks | Implemented and tested; GUI end-to-end search still needs verification |
| Agent file-context sharing | Separate filename/content consent; bound reads and clear prior context on permission reduction | Implemented in runtime; 16 KiB/read, 3 reads/turn; regression-tested |
| Create, edit, move, and delete | Separate write/delete grants and per-operation confirmation | Planned |
| Calendar and reminders | Adapter per available provider (e.g. desktop data service or CalDAV); no implicit database scraping | Planned |
| Contacts | Provider adapter with explicit read/update scopes | Planned |
| Notes | App/provider-specific adapters; no universal Linux notes API | Planned |
| Email and messaging | Service/app-specific connectors with explicit send confirmation and privacy filters | Planned |
| Browser tabs and form filling | Optional browser extension/native-messaging bridge | Planned |
| Screen context and desktop actions | XDG ScreenCast/RemoteDesktop portals plus accessibility APIs; require the desktop’s consent flow | Planned |
| Voice input, camera, photos, location | PipeWire and desktop portals, if confirmed in the Mac-app baseline | Verify Mac-app scope |
| Muse.ai account sign-in and local host pairing | Account-linked Linux host plus Linux-native capability bridge | Not implemented; public Linux Device SDK is a personal, unsupported gadget path; distribution terms and desktop parity unresolved |
| AppImage | Bundle the app; use host desktop portals and session services for system integrations | x86_64 build works; launch-smoke-tested via XWayland on KDE Wayland |

## Safety rules

1. A folder grant is scoped to the exact canonical directory selected by the user. Relative paths may not escape that root.
2. The initial filesystem implementation is read-only. Writes, moves, and deletes require distinct grants and an explicit confirmation surface.
3. Screen capture and remote input must go through the desktop’s portal/session mechanism; no silent global-input fallback.
4. Sending messages, emails, or other externally visible actions requires confirmation.
5. Credentials and account tokens must never be committed, logged, or persisted in plaintext.
6. Unsupported app/provider integrations must be shown as unavailable, not simulated as working.
7. Paid model requests, filename sharing, and file-content sharing are separate opt-ins; each defaults off.

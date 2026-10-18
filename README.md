# Unofficial Muse for Linux

An open-source, **Linux-native** client exploring the desktop capabilities of Meta Muse for macOS. This project is independent and is not affiliated with, endorsed by, or supported by Meta Platforms, Inc. “Muse” and related marks belong to their respective owners.

This is not a wrapper that opens Meta’s website. It provides a native GTK shell and Linux-local capabilities, with the assistant using an optional, replaceable service adapter.

## Status

Early development; **not a usable Muse alternative or feature-complete port**. The app now has a non-streaming assistant loop through Meta’s documented Responses API using Muse Spark 1.3, a developer key entered by the user, explicit paid-use consent, and `store:false` stateless reasoning replay. The key and conversation are session-only. It is separate from consumer Muse sign-in and does not establish account/history continuity. Local agent tools are limited to a user-granted folder: filename search and bounded UTF-8 reads; each sharing permission defaults off, reads are capped at 16 KiB each and three per turn. The API adapter and tool loop are tested with local mock responses; **no live API-key request has been made or verified**. The rebuilt AppImage launches on KythOS KDE Wayland via XWayland; the assistant UI rendered, and sending with no key was rejected locally. No key was entered and no API request was attempted. Folder chooser, listing, and preview were manually verified on an earlier build; search UI end-to-end is not yet verified. Native Wayland remains unverified. See the [full-port plan](docs/full-port-goal.md) and [service/account boundary](docs/architecture.md#muse-service-backend-and-account-compatibility). No Meta app code, assets, or credentials are included here.

## Implemented and planned Linux integrations

- Session-only, read-only folder grants, browsing, bounded text preview, and bounded recursive filename search in core/UI code. The search UI still needs an end-to-end GUI check; write/delete flows are not implemented.
- Optional Meta Model API conversation and a bounded, read-only filesystem tool loop. Prompts leave the device only after the user enters a developer key and enables paid-use consent; file names and file contents require separate opt-ins.
- Desktop and browser context through Linux accessibility/portal APIs and an optional browser extension (planned).
- Per-provider calendar, contacts, reminders, notes, mail, and messaging connectors; Linux has no single API that covers every desktop app (planned).
- Explicit user consent for screen capture, input, and external actions (planned).
- Consumer Muse account sign-in/history continuity remains unverified and is distinct from porting Linux host capabilities.
- x86_64 AppImage packaging builds, but the current artifact is not user-ready; see [build instructions](docs/building.md).

## Development

The Rust workspace separates `muse-linux-core` (confined local filesystem access), `muse-meta-api` (the optional Meta Model API adapter), `muse-agent-runtime` (bounded read-only tool execution), and the native GTK4 app in `apps/desktop`.

```sh
cargo test --workspace
cargo run -p unofficial-muse-for-linux
```

Build requirements and the pinned AppImage packaging flow are documented in [docs/building.md](docs/building.md).

## License

GPL-3.0-only. See [LICENSE](LICENSE).

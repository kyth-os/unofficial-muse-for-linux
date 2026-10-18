# Unofficial Muse for Linux

An open-source, **Linux-native companion/local host for the user's existing Muse.ai personal agent**, intended to behave like Muse for Mac on Linux. This project is independent and is not affiliated with, endorsed by, or supported by Meta Platforms, Inc. “Muse” and related marks belong to their respective owners.

This is not a wrapper that opens Muse.ai or a separate Muse-like model client. The target is to pair an existing Muse.ai account with a native Linux shell and permissioned local capabilities. Meta's Linux Device SDK offers a personal gadget-pairing path, but it is unsupported, its terms restrict publicly offered token-bearing devices, whether a user-token AppImage is permitted is unclear, and an open issue reports app-to-device command dispatch problems; this app does not integrate it yet.

## Status

Early development; **not a usable Muse.ai Linux companion or feature-complete port**. The desktop has a first-pass dark, chat-first GTK layout with Chats/Files views and a collapsible permissions panel. The current assistant is an **experimental, separate developer-API path** that requires a developer key; it does not sign in to the user's Muse.ai account, reach their existing agent, or share chat/task history, so it does not meet the product objective. It uses session-only state and local mocks for tests; no live API request was made. Filesystem access remains read-only and restricted to a user-granted folder. The rebuilt AppImage launches via XWayland on KDE; dark UI and view navigation were manually checked. Muse.ai account/device pairing, same-agent continuity, app write actions, browser/app integrations, and native Wayland support remain unimplemented or unverified. See the [full-port plan](docs/full-port-goal.md) and [account-link architecture](docs/architecture.md#museai-account-link-and-local-host-bridge-are-core-requirements). No Meta app code, assets, or credentials are included here.

## Implemented and planned Linux integrations

- Session-only, read-only folder grants, browsing, bounded text preview, and bounded recursive filename search in core/UI code. The search UI still needs an end-to-end GUI check; write/delete flows are not implemented.
- Experimental Meta Model API conversation and a bounded, read-only filesystem tool loop; this is separate from Muse.ai account sign-in and is not the target product auth path.
- Desktop and browser context through Linux accessibility/portal APIs and an optional browser extension (planned).
- Per-provider calendar, contacts, reminders, notes, mail, and messaging connectors; Linux has no single API that covers every desktop app (planned).
- Explicit user consent for screen capture, input, and external actions (planned).
- Muse.ai account sign-in, pairing, and shared history/tasks are core requirements but are not implemented; the developer API path does not satisfy them.
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

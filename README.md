# Unofficial Muse for Linux

A Linux-native companion for the user's existing Muse personal agent. Independent, open source, and **not affiliated with, endorsed by, or supported by Meta**. “Muse” and related marks belong to their respective owners.

## Status

Early development, **not a feature-complete Muse-for-Mac port**. The GTK4 dark desktop has Chats/Files views, session-only read-only folder grants, browsing, bounded text preview, and filename search. The account-linked second batch adds native shared chat/history, prompt dispatch, and scheduled-task/run lists through an explicitly selected **unsupported community adapter** and the user's own Muse session.

Account linkage is **not Mac-style device/node registration or local-capability dispatch**. Connecting an account gives the agent no Linux shell, screen, browser, or filesystem access. Local files stay read-only and separate. Fresh browser authentication and same-agent local-host dispatch are not proven; offline tests or an existing-session backend probe do not establish either. Streaming, approvals/cancel/resume, task editing, file writes, desktop connectors, and durable Linux background work remain incomplete. Earlier AppImage launch checks used KDE Wayland through XWayland, not native Wayland.

The app can launch an isolated, user-controlled official Muse browser login when a compatible Chromium browser is available, or explicitly use an existing local Muse session. It never asks for a password or developer API key for the account path. The separate developer-key Model API crates remain experiments, not Muse account sign-in.

## Account setup

```sh
# Run as your regular user; requires Git and Python 3.10+ with venv.
./scripts/setup-account-bridge.sh
```

The installer pins external community source into `~/.local/share/unofficial-muse-for-linux/account-bridge` (or the XDG data home) and creates its own Python environment. Select the printed `.venv/bin/python` in the app. It refuses existing destinations, never reads credentials, and never runs as root. The AppImage bundles only **our sidecars**, not the external checkout, Meta-derived protocol assets, Python environment, or account data.

See [executed verification and current limits](docs/account-verification.md). See [account integration](docs/account-integration.md) for the source pin, sign-in/session choices, security boundaries, protocol-drift disclosure, and verification limits. A session jar is a private plaintext credential, not encrypted storage.

## Development

```sh
./scripts/test-account.sh
cargo fmt --all -- --check
cargo build --workspace --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo run -p unofficial-muse-for-linux
```

The workspace separates filesystem policy (`muse-linux-core`), the account subprocess adapter (`muse-account`), the experimental developer API/runtime crates, and the native app. See [building](docs/building.md), [architecture](docs/architecture.md), [feature parity](docs/feature-parity.md), and the [full-port goal](docs/full-port-goal.md).

## License

GPL-3.0-only. See [LICENSE](LICENSE). The separately installed community dependency declares MIT licensing; its source and provenance stay external to this repository and AppImage.

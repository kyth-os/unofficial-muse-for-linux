# Building

## Native development

The workspace uses Rust 1.85+ (edition 2024) and GTK4 development headers. On Debian/Ubuntu, install `build-essential`, `pkg-config`, and `libgtk-4-dev`; on Fedora, install `gcc`, `pkgconf-pkg-config`, and `gtk4-devel`.

```sh
cargo test --workspace
cargo run -p unofficial-muse-for-linux
```

## Reproducible x86_64 AppImage build

The build container pins Rust 1.99 on Debian Bookworm and installs GTK4 packaging dependencies. The AppImage builder and GTK plugin are downloaded from their upstream repositories at pinned versions/commits and verified by SHA-256 before execution.

```sh
podman build -f Containerfile.appimage -t muse-linux-appimage-builder .
mkdir -p "$HOME/.cache/muse-linux-cargo"
podman run --rm \
  -e CARGO_HOME=/cargo-cache \
  -v "$PWD:/workspace:Z" \
  -v "$HOME/.cache/muse-linux-cargo:/cargo-cache:Z" \
  -w /workspace \
  muse-linux-appimage-builder \
  ./scripts/build-appimage.sh
```

The artifact is written to `dist/unofficial-muse-for-linux-x86_64.AppImage`. GTK4 and its runtime support files are bundled by linuxdeploy's GTK plugin. The AppImage still uses the host desktop session, display server, portals, accessibility services, and graphics stack; those are intentionally not bundled.

The current GTK packaging hook selects X11 for compatibility. The rebuilt AppImage launched on KythOS KDE Wayland through XWayland; the assistant UI rendered and its no-key submission guard was verified locally. Native Wayland backend support remains a separate validation gate. Implemented Linux-local filesystem access is session-only and read-only. Optional model requests use the Meta Model API after the user enters a developer key and enables paid-use consent; filename and file-content sharing require separate opt-ins. Authenticated chat/API behavior has not been tested in the GUI or against a live service.

# Building

## Native development and offline gates

The workspace uses edition 2024 and is verified with Rust 1.99; a lower minimum compiler version has not been established. Development also requires GTK4 headers, Bash, and `/usr/bin/python3`. On Debian/Ubuntu install `build-essential`, `pkg-config`, `libgtk-4-dev`, and `python3`; on Fedora install `gcc`, `pkgconf-pkg-config`, `gtk4-devel`, and `python3`.

```sh
./scripts/test-account.sh
cargo fmt --all -- --check
cargo build --workspace --locked
cargo test --workspace --locked
# Requires Xvfb and xauth; TMPDIR must be a writable test scratch directory.
xvfb-run -a cargo test -p unofficial-muse-for-linux native_account_page_consent_failure_and_disconnect_signal_smoke --locked -- --ignored --test-threads=1
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo run -p unofficial-muse-for-linux
```

`test-account.sh` syntax-checks all shell scripts under `scripts/`, compiles Python source without writing bytecode, parses the installer's embedded Python via `--help`, and runs the canonical offline test command:

```sh
/usr/bin/python3 -m unittest discover -s scripts/tests -p 'test_*.py' -v
```

Tests use standard-library unittest and fake community imports: no external adapter, network, browser, or real account is needed. CI retains formatting, locked workspace build/test, and warning-denying Clippy checks, and adds this account/script gate. Unit tests do not establish live browser sign-in, service compatibility, GUI behavior, or device registration.

## Optional runtime account dependency

The native account path additionally requires an external source checkout and per-user Python 3.10+ environment. Install as a regular user, outside the AppImage/container:

```sh
./scripts/setup-account-bridge.sh
```

Choose the printed absolute `.venv/bin/python` in the app. Setup pins the community source, installs its dependencies, and verifies offline imports/data loading. It does not read or create an authenticated session. Existing destinations are refused, not updated in place. See [account integration](account-integration.md) for exact pin, paths, ownership checks, sign-in/session choice, license/provenance, and protocol drift. Python dependency versions are recorded after resolution, not fully hash-locked; account runtime setup is not a reproducible build of the Rust artifact.

## Pinned x86_64 AppImage build

The container uses Rust 1.99 on Debian Bookworm with GTK4 packaging dependencies and Python for the offline account gate. The linuxdeploy builder and GTK plugin use pinned version/commit downloads checked by SHA-256 before execution. The base image tag and apt packages are not digest/package-version locked; this is not a bit-for-bit reproducibility claim.

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

The build runs `test-account.sh` and locked workspace Rust tests before creating `dist/unofficial-muse-for-linux-x86_64.AppImage`. It bundles GTK/runtime support through linuxdeploy and **only our** `muse_account_bridge.py` and `muse_account_login.py` under `usr/share/unofficial-muse-for-linux/account/`. It does not bundle the external community checkout, Meta-derived route/descriptors, Python environment, browser, or any credentials. The host must supply the selected Python environment and, for fresh sign-in, a compatible Chromium browser. Host session services, portals, accessibility, display server, and graphics stack are not bundled.

## Release verification boundaries

Inspect the newly built artifact rather than treating source edits or an older AppImage as release evidence. Confirm both sidecars are present and no `routes.json`, `desc*.bin`, community source, venv, or account directory is present. Re-run GTK folder-grant and account/history/task UI checks on that exact artifact. Record dates with timezone and distinguish offline tests, backend checks with an existing session, fresh interactive browser authentication, and actual Linux local-tool dispatch.

Earlier AppImage checks launched on KythOS KDE Wayland through XWayland and exercised the dark UI/navigation and read-only file chooser/list/preview. The GTK packaging hook currently selects X11; that evidence is **not native Wayland support**. It does not verify this batch's new packaged account UI, a fresh login, or Mac-style device pairing. See [the batch verification report](account-verification.md) for the new artifact, real-account backend evidence, and session rejection during the GUI check. Fresh authentication and device pairing still need explicit checks. Account authentication does not turn read-only local files into agent tools; the developer-key adapter remains a separate experiment.

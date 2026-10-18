#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ "$(uname -m)" != "x86_64" ]]; then
    printf 'This build script currently targets x86_64 (found %s).\n' "$(uname -m)" >&2
    exit 2
fi

LINUXDEPLOY_VERSION="1-alpha-20251107-1"
GTK_PLUGIN_COMMIT="7a3fbc31a9e5075073ff8790f26effbac5f84453"
LINUXDEPLOY_SHA256="c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d"
GTK_PLUGIN_SHA256="b0f4cbc684a0103a9651f0955b635eaea0096b3a66c0f5a2c2aa337960375171"
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/unofficial-muse-for-linux/appimage-tools"
LINUXDEPLOY="$CACHE_DIR/linuxdeploy-x86_64.AppImage"
GTK_PLUGIN="$CACHE_DIR/linuxdeploy-plugin-gtk"
APPDIR="$ROOT/target/AppDir"

mkdir -p "$CACHE_DIR"
fetch_pinned() {
    local url="$1" destination="$2" expected_sha256="$3"
    if [[ -f "$destination" ]] && printf '%s  %s\n' "$expected_sha256" "$destination" | sha256sum --check --status; then
        return
    fi
    curl --fail --location --retry 3 --output "$destination.tmp" "$url"
    printf '%s  %s\n' "$expected_sha256" "$destination.tmp" | sha256sum --check --status
    mv "$destination.tmp" "$destination"
    chmod 755 "$destination"
}

fetch_pinned \
    "https://github.com/linuxdeploy/linuxdeploy/releases/download/$LINUXDEPLOY_VERSION/linuxdeploy-x86_64.AppImage" \
    "$LINUXDEPLOY" "$LINUXDEPLOY_SHA256"
fetch_pinned \
    "https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/$GTK_PLUGIN_COMMIT/linuxdeploy-plugin-gtk.sh" \
    "$GTK_PLUGIN" "$GTK_PLUGIN_SHA256"

cd "$ROOT"
./scripts/test-account.sh
cargo test --workspace --locked
cargo build --release --locked -p unofficial-muse-for-linux

rm -rf -- "$APPDIR"
ACCOUNT_DIR="$APPDIR/usr/share/unofficial-muse-for-linux/account"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/metainfo" "$ACCOUNT_DIR"
install -m 755 "$ROOT/target/release/unofficial-muse-for-linux" "$APPDIR/usr/bin/unofficial-muse-for-linux"
install -m 644 "$ROOT/resources/unofficial-muse-for-linux.appdata.xml" "$APPDIR/usr/share/metainfo/unofficial-muse-for-linux.appdata.xml"
# Bundle only this project's typed sidecars, never the external community
# checkout, Meta-derived protocol assets, Python environment, or account state.
install -m 644 "$ROOT/scripts/muse_account_bridge.py" "$ACCOUNT_DIR/muse_account_bridge.py"
install -m 644 "$ROOT/scripts/muse_account_login.py" "$ACCOUNT_DIR/muse_account_login.py"
install -m 644 "$ROOT/scripts/muse_local_host.py" "$ACCOUNT_DIR/muse_local_host.py"

shopt -s nullglob
for previous in "$ROOT"/target/*.AppImage; do
    rm -f -- "$previous"
done
cd "$ROOT/target"
export APPIMAGE_EXTRACT_AND_RUN=1
export DEPLOY_GTK_VERSION=4
export LINUXDEPLOY
export PATH="$CACHE_DIR:$PATH"
"$LINUXDEPLOY" \
    --appdir "$APPDIR" \
    --desktop-file "$ROOT/resources/unofficial-muse-for-linux.desktop" \
    --icon-file "$ROOT/resources/unofficial-muse-for-linux.svg" \
    --plugin gtk \
    --output appimage

artifact=""
for candidate in "$ROOT"/target/*.AppImage; do
    if [[ -f "$candidate" ]]; then
        artifact="$candidate"
        break
    fi
done
if [[ -z "$artifact" ]]; then
    printf 'linuxdeploy completed without producing an AppImage.\n' >&2
    exit 1
fi

install -Dm755 "$artifact" "$ROOT/dist/unofficial-muse-for-linux-x86_64.AppImage"
printf 'Built: %s\n' "$ROOT/dist/unofficial-muse-for-linux-x86_64.AppImage"

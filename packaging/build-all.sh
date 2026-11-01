#!/bin/sh
# Builds TaskMonitor for (nearly) every Linux distro into dist/, for x86_64 and ARM64:
#   TaskMonitor-<ver>-<arch>.AppImage    any distro, no install needed
#   taskmonitor_<ver>-1_<amd64|arm64>.deb Debian, Ubuntu, Mint, Pop!_OS, Raspberry Pi OS
#   taskmonitor-<ver>-1.<arch>.rpm       Fedora, RHEL, openSUSE
#   taskmonitor-<ver>-<arch>.tar.gz      anything else (install.sh inside)
# where <arch> is x86_64 or aarch64. Arch users build from packaging/aur/PKGBUILD.
# Build one architecture only with: ARCHES=aarch64 packaging/build-all.sh
#
# Binaries are linked against glibc 2.28 (2018) so they run on any distro since.
# Needs: zig, squashfs-tools, cargo-zigbuild, cargo-deb, cargo-generate-rpm,
# and `rustup target add aarch64-unknown-linux-gnu`.
set -eu
cd "$(dirname "$0")/.."

VERSION=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
GLIBC=2.28
DIST=dist
mkdir -p "$DIST"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

build() {
    arch=$1
    TARGET=$arch-unknown-linux-gnu
    BIN=target/$TARGET/release

    echo "==> Building $VERSION for $arch against glibc $GLIBC"
    cargo zigbuild --release --target "$TARGET.$GLIBC" -p taskmonitor -p taskmonitor-helper

    # Refuse to ship a binary that needs a newer glibc than promised.
    newest=$(readelf -W --dyn-syms "$BIN/taskmonitor" "$BIN/taskmonitor-helper" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1)
    echo "    newest glibc symbol: $newest"
    [ "$(printf '%s\n' "GLIBC_$GLIBC" "$newest" | sort -V | tail -1)" = "GLIBC_$GLIBC" ] || {
        echo "error: binaries need $newest, more than glibc $GLIBC" >&2
        exit 1
    }

    echo "==> .deb"
    cargo deb -p taskmonitor --no-build --target "$TARGET" -o "$DIST/"

    echo "==> .rpm"
    cargo generate-rpm -p crates/app --target "$TARGET" -o "$DIST/"

    echo "==> tarball"
    root="$stage/$arch/taskmonitor-$VERSION"
    packaging/stage.sh "$BIN" "$root/usr"
    cp packaging/install.sh "$root/"
    tar -C "$stage/$arch" -czf "$DIST/taskmonitor-$VERSION-$arch.tar.gz" "taskmonitor-$VERSION"

    echo "==> AppImage"
    appdir="$stage/$arch/AppDir"
    packaging/stage.sh "$BIN" "$appdir/usr" --no-helper
    cp packaging/io.github.taskmonitor.desktop "$appdir/"
    cp packaging/icons/hicolor/256x256/apps/io.github.taskmonitor.png "$appdir/io.github.taskmonitor.png"
    ln -s io.github.taskmonitor.png "$appdir/.DirIcon"
    ln -s usr/bin/taskmonitor "$appdir/AppRun"
    runtime=$(packaging/appimage-runtime.sh "$arch")
    mksquashfs "$appdir" "$stage/$arch/app.squashfs" -root-owned -noappend -comp zstd -quiet
    out="$DIST/TaskMonitor-$VERSION-$arch.AppImage"
    cat "$runtime" "$stage/$arch/app.squashfs" > "$out"
    chmod +x "$out"
}

for arch in ${ARCHES:-x86_64 aarch64}; do
    build "$arch"
done

echo "==> Done:"
ls -lh "$DIST"

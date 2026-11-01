#!/bin/sh
# Prints the path of the AppImage runtime (the small launcher at the front of every
# AppImage) for an architecture: appimage-runtime.sh [x86_64|aarch64]. Downloads the
# pinned release once and checks its checksum.
set -eu
TAG=20251108
arch=${1:-x86_64}
case "$arch" in
    x86_64) SHA256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d ;;
    aarch64) SHA256=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444 ;;
    *) echo "no AppImage runtime pinned for $arch" >&2; exit 1 ;;
esac
cache="$(dirname "$0")/../target/appimage-runtime-$TAG-$arch"
if [ ! -f "$cache" ]; then
    curl -fsSL -o "$cache.part" "https://github.com/AppImage/type2-runtime/releases/download/$TAG/runtime-$arch"
    echo "$SHA256  $cache.part" | sha256sum -c --quiet - >&2
    mv "$cache.part" "$cache"
fi
echo "$cache"

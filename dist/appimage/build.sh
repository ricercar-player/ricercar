#!/bin/sh
# Build a portable AppImage from the release binaries.
# Usage: dist/appimage/build.sh [VERSION]   (after cargo build --release)
set -eu
cd "$(dirname "$0")/../.."
ARCH="${ARCH:-$(uname -m)}"
VERSION="${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' crates/ricercar-ui/Cargo.toml | head -1)}"
OUT="${OUT:-target/dist}"
APPDIR=target/AppDir

rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" \
    "$APPDIR/usr/share/icons/hicolor/scalable/apps" "$OUT"
cp target/release/ricercar target/release/ricercar-cli target/release/ricercar-daemon "$APPDIR/usr/bin/"
cp dist/ricercar.desktop "$APPDIR/usr/share/applications/"
cp dist/ricercar.svg "$APPDIR/usr/share/icons/hicolor/scalable/apps/"

TOOL="target/linuxdeploy-$ARCH.AppImage"
if [ ! -x "$TOOL" ]; then
    curl -fsSL -o "$TOOL" \
        "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-$ARCH.AppImage"
    chmod +x "$TOOL"
fi

# linuxdeploy bundles libasound, fontconfig & co; GL, glibc and the display
# server libraries (dlopened by winit) come from the host, as they must.
LINUXDEPLOY_OUTPUT_VERSION="$VERSION" "$TOOL" --appimage-extract-and-run \
    --appdir "$APPDIR" \
    --executable "$APPDIR/usr/bin/ricercar" \
    --desktop-file "$APPDIR/usr/share/applications/ricercar.desktop" \
    --icon-file "$APPDIR/usr/share/icons/hicolor/scalable/apps/ricercar.svg" \
    --output appimage
mv ricercar-*"$ARCH".AppImage "$OUT/ricercar-$VERSION-$ARCH.AppImage"
echo "$OUT/ricercar-$VERSION-$ARCH.AppImage"

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
LICENSES="$APPDIR/usr/share/licenses/ricercar"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" "$APPDIR/usr/share/metainfo" \
    "$APPDIR/usr/share/icons/hicolor/scalable/apps" "$LICENSES" "$OUT"
cp target/release/ricercar target/release/ricercar-cli target/release/ricercar-daemon "$APPDIR/usr/bin/"
cp dist/ricercar.desktop "$APPDIR/usr/share/applications/"
cp dist/ricercar.svg "$APPDIR/usr/share/icons/hicolor/scalable/apps/"
for size in 48 128 256; do
    mkdir -p "$APPDIR/usr/share/icons/hicolor/${size}x${size}/apps"
    cp "dist/icons/${size}x${size}/ricercar.png" "$APPDIR/usr/share/icons/hicolor/${size}x${size}/apps/"
done
cp dist/io.github.ricercar_player.ricercar.metainfo.xml "$APPDIR/usr/share/metainfo/"
cp LICENSE "$LICENSES/"
cp crates/ricercar-ui/assets/fonts/OFL.txt "$LICENSES/"
cp crates/ricercar-ui/assets/icons/LICENSE "$LICENSES/icons-LICENSE"
# Written by dist/third-party-licenses.sh (the release workflow runs it first).
if [ -f target/THIRD-PARTY-LICENSES ]; then
    cp target/THIRD-PARTY-LICENSES "$LICENSES/"
fi

TOOL="target/linuxdeploy-$ARCH.AppImage"
if [ ! -x "$TOOL" ]; then
    curl -fsSL -o "$TOOL" \
        "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-$ARCH.AppImage"
    chmod +x "$TOOL"
fi

# GL, glibc, fontconfig and the display server libraries (dlopened by winit)
# come from the host, as they must. So does libasound: a bundled copy would
# not find the host's ALSA plugins and configuration (PipeWire, Bluetooth,
# dmix). linuxdeploy's default excludelist already skips it; be explicit.
LINUXDEPLOY_OUTPUT_VERSION="$VERSION" "$TOOL" --appimage-extract-and-run \
    --appdir "$APPDIR" \
    --exclude-library 'libasound.so*' \
    --executable "$APPDIR/usr/bin/ricercar" \
    --desktop-file "$APPDIR/usr/share/applications/ricercar.desktop" \
    --icon-file "$APPDIR/usr/share/icons/hicolor/scalable/apps/ricercar.svg" \
    --output appimage
mv ricercar-*"$ARCH".AppImage "$OUT/ricercar-$VERSION-$ARCH.AppImage"
echo "$OUT/ricercar-$VERSION-$ARCH.AppImage"

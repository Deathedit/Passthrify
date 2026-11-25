#!/usr/bin/env bash
# Builds Passthrify for Linux and packages a ready-to-run folder in dist/Passthrify.
#
#   ./build.sh                 # test + release build + dist/Passthrify
#   ./build.sh --tarball       # also dist/Passthrify-<version>-linux-x86_64.tar.gz (+ .sha256)
#   ./build.sh --appimage      # also dist/Passthrify-<version>-x86_64.AppImage (+ .sha256)
#   ./build.sh --skip-tests    # no cargo test
#   ./build.sh --clean         # cargo clean + remove dist first
#
# Bundles FFmpeg (BtbN's LGPL shared 8.1 build, as on Windows) in lib/, found through the
# binary's $ORIGIN/lib rpath. The host provides glibc, PipeWire, Vulkan and the NVIDIA driver.
# Needs: cargo, a C toolchain, clang/libclang (bindgen), pkg-config, PipeWire headers, curl.
# On NixOS use `nix develop -c ./build.sh`; the result then only runs on NixOS (its loader is
# in /nix/store). Portable release builds come from CI on Ubuntu.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"
FFMPEG_DIR_LOCAL="$ROOT/third_party/ffmpeg-linux"
FFMPEG_URL="https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n8.1-latest-linux64-lgpl-shared-8.1.tar.xz"
APPIMAGETOOL_URL="https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage"

TARBALL=0 APPIMAGE=0 TESTS=1 CLEAN=0
for a in "$@"; do
    case "$a" in
        --tarball) TARBALL=1 ;;
        --appimage) APPIMAGE=1 ;;
        --skip-tests) TESTS=0 ;;
        --clean) CLEAN=1 ;;
        -h|--help) sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $a (see --help)" >&2; exit 2 ;;
    esac
done

step() { printf '\n\033[36m==> %s\033[0m\n' "$*"; }
fail() { printf '\033[31mERROR: %s\033[0m\n' "$*" >&2; exit 1; }

# --- Prerequisites ------------------------------------------------------------------------

step "Checking prerequisites"
for tool in cargo cc curl tar pkg-config; do
    command -v "$tool" >/dev/null || fail "$tool not found"
done
pkg-config --exists libpipewire-0.3 || fail "PipeWire development files not found (Debian/Ubuntu: libpipewire-0.3-dev)"
VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
[ -n "$VERSION" ] || fail "can't read the version from Cargo.toml"
if [ -e /etc/NIXOS ]; then
    echo "  note: NixOS build; the packages only run on NixOS (portable builds come from CI)"
fi

if [ ! -f "$FFMPEG_DIR_LOCAL/lib/libavcodec.so" ]; then
    step "FFmpeg not found - downloading FFmpeg 8.1 LGPL shared build"
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    curl -fL --retry 3 -o "$tmp/ffmpeg.tar.xz" "$FFMPEG_URL"
    mkdir -p "$tmp/x"
    tar -xJf "$tmp/ffmpeg.tar.xz" -C "$tmp/x"
    inner="$(find "$tmp/x" -mindepth 1 -maxdepth 1 -type d | head -1)"
    [ -f "$inner/lib/libavcodec.so" ] || fail "unexpected FFmpeg archive layout"
    rm -rf "$FFMPEG_DIR_LOCAL"
    mkdir -p "$(dirname "$FFMPEG_DIR_LOCAL")"
    mv "$inner" "$FFMPEG_DIR_LOCAL"
fi
echo "  FFmpeg: $FFMPEG_DIR_LOCAL"
echo "  Version: $VERSION"

# Build against the bundled FFmpeg, not the dev shell's (FFMPEG_DIR may already be set).
export FFMPEG_DIR="$FFMPEG_DIR_LOCAL"
export PKG_CONFIG_PATH="$FFMPEG_DIR_LOCAL/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
export PASSTHRIFY_BUNDLE_LIBS=1

# --- Build --------------------------------------------------------------------------------

if [ "$CLEAN" = 1 ]; then
    step "Cleaning"
    cargo clean
    rm -rf dist
fi
if [ "$TESTS" = 1 ]; then
    step "Running tests"
    LD_LIBRARY_PATH="$FFMPEG_DIR_LOCAL/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" cargo test --quiet
fi
step "Building release"
cargo build --release

# --- Package ------------------------------------------------------------------------------

step "Packaging dist/Passthrify"
DIST="$ROOT/dist/Passthrify"
rm -rf "$DIST"
mkdir -p "$DIST/lib"
install -m755 target/release/passthrify "$DIST/passthrify"
# Only the libraries the app loads, under their sonames (libavcodec.so.62, ...), as plain files.
for f in "$FFMPEG_DIR_LOCAL"/lib/*.so.*; do
    name="$(basename "$f")"
    [[ "$name" =~ \.so\.[0-9]+$ ]] && cp -L "$f" "$DIST/lib/$name"
done
ls "$DIST"/lib/libavcodec.so.* >/dev/null 2>&1 || fail "no FFmpeg libraries were bundled"
cp README.md "$DIST/"
cp "$FFMPEG_DIR_LOCAL/LICENSE.txt" "$DIST/FFMPEG-LICENSE.txt" 2>/dev/null || true
# Inter and JetBrains Mono are embedded in the binary; their OFL licenses travel with it.
cp assets/fonts/OFL-*.txt "$DIST/" 2>/dev/null || true
cp assets/passthrify.desktop "$DIST/"
cp assets/logo/passthrify-mark.svg "$DIST/passthrify.svg"

# Every library the binary needs must resolve to the bundle or the system.
if missing="$(ldd "$DIST/passthrify" | grep 'not found')"; then
    fail "unresolved libraries: $(echo "$missing" | awk '{print $1}' | paste -sd ' ')"
fi
ldd "$DIST/passthrify" | grep -q "$DIST/lib/libavcodec" || fail "the binary doesn't load the bundled FFmpeg (rpath missing?)"
echo "  dist/Passthrify ($(du -sh "$DIST" | cut -f1), v$VERSION)"

checksum() { (cd "$(dirname "$1")" && sha256sum "$(basename "$1")" > "$(basename "$1").sha256"); }

if [ "$TARBALL" = 1 ]; then
    step "Creating the tarball"
    out="$ROOT/dist/Passthrify-$VERSION-linux-x86_64.tar.gz"
    tar -C "$ROOT/dist" --owner=0 --group=0 -czf "$out" Passthrify
    checksum "$out"
    echo "  ${out#"$ROOT"/}"
fi

if [ "$APPIMAGE" = 1 ]; then
    step "Creating the AppImage"
    tool="$ROOT/third_party/appimagetool-x86_64.AppImage"
    if [ ! -x "$tool" ]; then
        curl -fL --retry 3 -o "$tool" "$APPIMAGETOOL_URL"
        chmod +x "$tool"
    fi
    appdir="$ROOT/dist/Passthrify.AppDir"
    rm -rf "$appdir"
    mkdir -p "$appdir/usr/bin" "$appdir/usr/share/applications" "$appdir/usr/share/icons/hicolor/scalable/apps"
    cp -r "$DIST"/. "$appdir/usr/bin/"
    cp assets/passthrify.desktop "$appdir/passthrify.desktop"
    cp assets/passthrify.desktop "$appdir/usr/share/applications/"
    # appimagetool wants a PNG for the AppImage's own icon (.DirIcon); the SVG is for desktops.
    mkdir -p "$appdir/usr/share/icons/hicolor/256x256/apps"
    "$DIST/passthrify" --render-icon 256 "$appdir/passthrify.png"
    cp "$appdir/passthrify.png" "$appdir/usr/share/icons/hicolor/256x256/apps/passthrify.png"
    cp assets/logo/passthrify-mark.svg "$appdir/usr/share/icons/hicolor/scalable/apps/passthrify.svg"
    cat > "$appdir/AppRun" <<'EOF'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/passthrify" "$@"
EOF
    chmod +x "$appdir/AppRun"
    out="$ROOT/dist/Passthrify-$VERSION-x86_64.AppImage"
    # Runs without FUSE (CI containers often lack it).
    ARCH=x86_64 "$tool" --appimage-extract-and-run --no-appstream "$appdir" "$out" >/dev/null
    chmod +x "$out"
    checksum "$out"
    echo "  ${out#"$ROOT"/}"
fi

printf '\n\033[32mDone: %s\033[0m\n' "dist/Passthrify/passthrify"

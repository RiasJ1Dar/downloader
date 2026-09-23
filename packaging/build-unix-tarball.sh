#!/usr/bin/env bash
# Build a self-contained Unix tarball for the current host OS/arch.
#
# Outputs (under target/unix/):
#   Downloader-<ver>-linux-x64.tar.gz
#   Downloader-<ver>-macos-arm64.tar.gz
#   Downloader-<ver>-macos-x64.tar.gz
#
# Notes:
#   - macOS artifacts must be built on a macOS runner (this script refuses
#     cross-OS packaging; Rust/dotnet RID must match the host).
#   - Packages are UNSIGNED. Gatekeeper / SmartScreen will warn.
#   - AppImage is skipped (heavy deps); .deb is a separate script.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="${VERSION:-0.1.1}"
SKIP_UI="${SKIP_UI:-0}"
SKIP_CARGO="${SKIP_CARGO:-0}"

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)
    PLATFORM="linux"
    case "$ARCH" in
      x86_64|amd64) RID="linux-x64"; ARCH_TAG="x64" ;;
      aarch64|arm64) RID="linux-arm64"; ARCH_TAG="arm64" ;;
      *) echo "unsupported Linux arch: $ARCH" >&2; exit 1 ;;
    esac
    ;;
  Darwin)
    PLATFORM="macos"
    case "$ARCH" in
      arm64|aarch64) RID="osx-arm64"; ARCH_TAG="arm64" ;;
      x86_64) RID="osx-x64"; ARCH_TAG="x64" ;;
      *) echo "unsupported macOS arch: $ARCH" >&2; exit 1 ;;
    esac
    ;;
  *)
    echo "unsupported OS: $OS (expected Linux or Darwin)" >&2
    exit 1
    ;;
esac

OUT_DIR="$ROOT/target/unix"
STAGE="$OUT_DIR/stage-${PLATFORM}-${ARCH_TAG}"
TARBALL_NAME="Downloader-${VERSION}-${PLATFORM}-${ARCH_TAG}.tar.gz"
TARBALL="$OUT_DIR/$TARBALL_NAME"

echo "==> Downloader Unix tarball ${VERSION} (${PLATFORM}-${ARCH_TAG}, RID=${RID})"

rm -rf "$STAGE"
mkdir -p "$STAGE/bin" "$STAGE/share/Downloader" "$STAGE/packaging/systemd" "$STAGE/packaging/macos"

if [[ "$SKIP_CARGO" != "1" ]]; then
  echo "==> cargo build --release (cli, core-service, nmhost)"
  cargo build --release \
    -p downloader-cli \
    -p downloader-core-service \
    -p downloader-nmhost
fi

install -m 755 "$ROOT/target/release/downloader-core" "$STAGE/bin/downloader-core"
install -m 755 "$ROOT/target/release/dl" "$STAGE/bin/dl"
install -m 755 "$ROOT/target/release/dl" "$STAGE/bin/downloader-cli"
install -m 755 "$ROOT/target/release/downloader-nmhost" "$STAGE/bin/downloader-nmhost"

if [[ "$SKIP_UI" != "1" ]]; then
  if command -v dotnet >/dev/null 2>&1; then
    echo "==> dotnet publish UI (self-contained $RID)"
    dotnet publish "$ROOT/apps/ui/Downloader.Ui.csproj" \
      -c Release \
      -r "$RID" \
      --self-contained true \
      -p:PublishSingleFile=false \
      -o "$STAGE/share/Downloader/ui"
    if [[ -f "$STAGE/share/Downloader/ui/Downloader.Ui" ]]; then
      chmod +x "$STAGE/share/Downloader/ui/Downloader.Ui"
    fi
    # Convenience wrapper next to binaries
    cat > "$STAGE/bin/Downloader.Ui" <<WRAP
#!/usr/bin/env bash
ROOT_DIR="\$(cd "\$(dirname "\$0")/.." && pwd)"
exec "\$ROOT_DIR/share/Downloader/ui/Downloader.Ui" "\$@"
WRAP
    chmod +x "$STAGE/bin/Downloader.Ui"
  else
    echo "⚠️ dotnet not found — tarball without UI (set SKIP_UI=1 to silence)" >&2
  fi
else
  echo "==> SKIP_UI=1 — omitting Avalonia UI"
fi

# Packaging helpers shipped inside the tarball
install -m 755 "$ROOT/packaging/install-unix.sh" "$STAGE/install-unix.sh"
cp "$ROOT/packaging/systemd/downloader-core.service" "$STAGE/packaging/systemd/"
cp "$ROOT/packaging/macos/com.riasj1dar.downloader-core.plist" "$STAGE/packaging/macos/"

# Short README for the archive
cat > "$STAGE/README-UNIX.txt" <<EOF
Downloader ${VERSION} — ${PLATFORM}-${ARCH_TAG} (experimental, UNSIGNED)

Install (user):
  ./install-unix.sh --user

This copies binaries, optional UI, Native Messaging manifests, and enables
autostart (systemd --user on Linux, LaunchAgent on macOS) when available.

Manual run:
  ./bin/downloader-core &
  ./bin/dl add https://example.com/file.zip
  ./bin/Downloader.Ui   # if UI was included

ffmpeg (YouTube / DASH):
  ./bin/dl ffmpeg-install

Linux runtime deps (tray): libgtk-3-0, libayatana-appindicator3-1
Packages are not code-signed / notarized — Gatekeeper and SmartScreen will warn.
EOF

# Also keep a stable stage/ symlink used by install-unix.sh / build-deb.sh
rm -rf "$OUT_DIR/stage"
cp -a "$STAGE" "$OUT_DIR/stage"

mkdir -p "$OUT_DIR"
# Pack contents of STAGE as top-level Downloader-<ver>-.../
TMP_PACK="$OUT_DIR/_pack"
rm -rf "$TMP_PACK"
mkdir -p "$TMP_PACK"
cp -a "$STAGE" "$TMP_PACK/Downloader-${VERSION}-${PLATFORM}-${ARCH_TAG}"
tar -C "$TMP_PACK" -czf "$TARBALL" "Downloader-${VERSION}-${PLATFORM}-${ARCH_TAG}"
rm -rf "$TMP_PACK"

# SHA256 next to the archive
(
  cd "$OUT_DIR"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$TARBALL_NAME" > "${TARBALL_NAME}.sha256"
  else
    shasum -a 256 "$TARBALL_NAME" > "${TARBALL_NAME}.sha256"
  fi
)

echo "==> wrote $TARBALL"
ls -lh "$TARBALL" "$TARBALL.sha256"

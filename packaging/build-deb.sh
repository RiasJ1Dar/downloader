#!/usr/bin/env bash
# Build a simple .deb from a staged install tree (Linux amd64).
# Prefers dpkg-deb; falls back to fpm if present.
#
# Output: target/unix/Downloader-<ver>-linux-x64.deb
#
# Prereq: run packaging/build-unix-tarball.sh first (creates target/unix/stage),
# or pass STAGE=... pointing at an extracted layout with bin/ + share/.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="${VERSION:-0.1.1}"
ARCH_DEB="${ARCH_DEB:-amd64}"
STAGE="${STAGE:-$ROOT/target/unix/stage}"
OUT_DIR="$ROOT/target/unix"
DEB_NAME="Downloader-${VERSION}-linux-x64.deb"
DEB_PATH="$OUT_DIR/$DEB_NAME"

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "build-deb.sh only runs on Linux" >&2
  exit 1
fi

if [[ ! -x "$STAGE/bin/downloader-core" ]]; then
  echo "missing $STAGE/bin/downloader-core — run packaging/build-unix-tarball.sh first" >&2
  exit 1
fi

mkdir -p "$OUT_DIR"
WORK="$OUT_DIR/deb-root"
rm -rf "$WORK"
mkdir -p "$WORK/usr/local/bin" \
         "$WORK/usr/local/share/Downloader" \
         "$WORK/usr/lib/systemd/user" \
         "$WORK/DEBIAN"

install -m 755 "$STAGE/bin/downloader-core" "$WORK/usr/local/bin/downloader-core"
install -m 755 "$STAGE/bin/dl" "$WORK/usr/local/bin/dl"
install -m 755 "$STAGE/bin/downloader-cli" "$WORK/usr/local/bin/downloader-cli"
install -m 755 "$STAGE/bin/downloader-nmhost" "$WORK/usr/local/bin/downloader-nmhost"

if [[ -d "$STAGE/share/Downloader/ui" ]]; then
  mkdir -p "$WORK/usr/local/share/Downloader/ui"
  cp -a "$STAGE/share/Downloader/ui"/. "$WORK/usr/local/share/Downloader/ui/"
  if [[ -f "$WORK/usr/local/share/Downloader/ui/Downloader.Ui" ]]; then
    chmod +x "$WORK/usr/local/share/Downloader/ui/Downloader.Ui"
  fi
  cat > "$WORK/usr/local/bin/Downloader.Ui" <<'WRAP'
#!/usr/bin/env bash
exec /usr/local/share/Downloader/ui/Downloader.Ui "$@"
WRAP
  chmod +x "$WORK/usr/local/bin/Downloader.Ui"
fi

# systemd user unit with absolute path (no placeholder)
UNIT_SRC="$ROOT/packaging/systemd/downloader-core.service"
sed 's|__DOWNLOADER_CORE__|/usr/local/bin/downloader-core|g' \
  "$UNIT_SRC" > "$WORK/usr/lib/systemd/user/downloader-core.service"
chmod 644 "$WORK/usr/lib/systemd/user/downloader-core.service"

# desktop entry
mkdir -p "$WORK/usr/local/share/applications"
cat > "$WORK/usr/local/share/applications/downloader.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Downloader
Comment=Download manager (core + Avalonia UI)
Exec=sh -c '/usr/local/bin/downloader-core & sleep 0.5; exec /usr/local/bin/Downloader.Ui'
Icon=application-x-executable
Terminal=false
Categories=Network;FileTransfer;
StartupNotify=true
DESKTOP

INSTALLED_SIZE="$(du -sk "$WORK/usr" | awk '{print $1}')"

cat > "$WORK/DEBIAN/control" <<EOF
Package: downloader
Version: ${VERSION}
Section: net
Priority: optional
Architecture: ${ARCH_DEB}
Maintainer: RiasJ1Dar <https://github.com/RiasJ1Dar/downloader>
Installed-Size: ${INSTALLED_SIZE}
Depends: libgtk-3-0 | libgtk-3-0t64, libayatana-appindicator3-1 | libappindicator3-1
Description: Downloader — segmented download manager (experimental Linux build)
 Multi-protocol download manager (HTTP/HLS/DASH/YouTube) with Avalonia UI,
 tray core service, CLI, and browser native-messaging host.
 This package is UNSIGNED and experimental.
Homepage: https://github.com/RiasJ1Dar/downloader
EOF

cat > "$WORK/DEBIAN/postinst" <<'POST'
#!/bin/sh
set -e
if command -v systemctl >/dev/null 2>&1; then
  # Reload user units when possible; do not fail package install.
  systemctl --user daemon-reload 2>/dev/null || true
  echo "Enable autostart with: systemctl --user enable --now downloader-core"
fi
exit 0
POST
chmod 755 "$WORK/DEBIAN/postinst"

if command -v dpkg-deb >/dev/null 2>&1; then
  echo "==> dpkg-deb → $DEB_PATH"
  # root:root ownership inside the archive
  dpkg-deb --root-owner-group --build "$WORK" "$DEB_PATH"
elif command -v fpm >/dev/null 2>&1; then
  echo "==> fpm → $DEB_PATH"
  fpm -s dir -t deb -n downloader -v "$VERSION" -a "$ARCH_DEB" \
    --deb-user root --deb-group root \
    -p "$DEB_PATH" \
    -C "$WORK" \
    usr
else
  echo "need dpkg-deb or fpm to build .deb" >&2
  exit 1
fi

(
  cd "$OUT_DIR"
  sha256sum "$DEB_NAME" > "${DEB_NAME}.sha256"
)

echo "==> wrote $DEB_PATH"
ls -lh "$DEB_PATH" "$DEB_PATH.sha256"
rm -rf "$WORK"

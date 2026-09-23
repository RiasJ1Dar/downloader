#!/usr/bin/env bash
# Встановити Downloader на Linux/macOS з уже зібраних артефактів.
#
# Типово: ~/.local/share/Downloader + ~/.local/bin у PATH + .desktop (Linux).
# Системно: sudo ./packaging/install-unix.sh --system  → /usr/local
#
# Перед цим:
#   cargo build --release -p downloader-cli -p downloader-core-service -p downloader-nmhost
#   dotnet publish -c Release -r linux-x64 --self-contained true apps/ui   # або osx-*
#   # або: ./packaging/build-unix-tarball.sh && розпакувати
#
# Залежності трею (Linux, desktop session):
#   Debian/Ubuntu: libgtk-3-0 libayatana-appindicator3-1
#   (для збірки: libgtk-3-dev libxdo-dev libayatana-appindicator3-dev)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# Two layouts:
#   Release tarball: install-unix.sh sits next to bin/ and share/
#   Dev/repo:        packaging/install-unix.sh → repo root has target/ and Cargo.toml
if [[ -x "$SCRIPT_DIR/bin/downloader-core" ]]; then
  ROOT="$SCRIPT_DIR"
elif [[ -f "$SCRIPT_DIR/../Cargo.toml" ]] || [[ -d "$SCRIPT_DIR/../target" ]]; then
  ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
else
  ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
fi
SYSTEM=0
PREFIX=""
NO_AUTOSTART=0

usage() {
  cat <<EOF
Usage: $0 [--user|--system] [--prefix DIR] [--no-autostart]

  --user          install to ~/.local/share/Downloader (default)
  --system        install to /usr/local (needs root)
  --prefix        override install root (binaries under PREFIX/bin or PREFIX/)
  --no-autostart  skip systemd user unit / LaunchAgent
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --user) SYSTEM=0; shift ;;
    --system) SYSTEM=1; shift ;;
    --prefix) PREFIX="$2"; shift 2 ;;
    --no-autostart) NO_AUTOSTART=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown arg: $1" >&2; usage; exit 1 ;;
  esac
done

if [[ -z "$PREFIX" ]]; then
  if [[ "$SYSTEM" -eq 1 ]]; then
    PREFIX="/usr/local"
  else
    PREFIX="${XDG_DATA_HOME:-$HOME/.local/share}/Downloader"
  fi
fi

BIN_DIR="$PREFIX"
SHARE_DIR="$PREFIX"
if [[ "$SYSTEM" -eq 1 ]] || [[ "$PREFIX" == /usr* ]]; then
  BIN_DIR="$PREFIX/bin"
  SHARE_DIR="$PREFIX/share/Downloader"
fi

mkdir -p "$BIN_DIR" "$SHARE_DIR"

pick() {
  local name="$1"
  local cands=(
    "$ROOT/bin/$name"
    "$ROOT/target/release/$name"
    "$ROOT/target/debug/$name"
    "$ROOT/target/unix/stage/bin/$name"
  )
  local c
  for c in "${cands[@]}"; do
    if [[ -f "$c" ]]; then
      echo "$c"
      return 0
    fi
  done
  return 1
}

need() {
  local name="$1"
  local src
  if ! src="$(pick "$name")"; then
    echo "немає $name — очікується $ROOT/bin/$name (tarball) або cargo build --release -p downloader-cli -p downloader-core-service -p downloader-nmhost" >&2
    exit 1
  fi
  echo "$src"
}

CORE="$(need downloader-core)"
CLI="$(need dl)"
NMHOST="$(need downloader-nmhost)"

install -m 755 "$CORE" "$BIN_DIR/downloader-core"
install -m 755 "$CLI" "$BIN_DIR/dl"
# зручний аліас
install -m 755 "$CLI" "$BIN_DIR/downloader-cli"
install -m 755 "$NMHOST" "$BIN_DIR/downloader-nmhost"

# UI: шукаємо publish / build вихід / staged tarball
UI_SRC=""
for cand in \
  "$ROOT/share/Downloader/ui" \
  "$ROOT/target/unix/stage/share/Downloader/ui" \
  "$ROOT/apps/ui/bin/Release"/net*/linux-*/publish \
  "$ROOT/apps/ui/bin/Release"/net*/osx-*/publish \
  "$ROOT/apps/ui/bin/Release"/net*/publish \
  "$ROOT/apps/ui/bin/Release"/net*
do
  if [[ -d "$cand" ]] && [[ -f "$cand/Downloader.Ui" || -f "$cand/Downloader.Ui.dll" ]]; then
    UI_SRC="$cand"
    break
  fi
done

if [[ -n "$UI_SRC" ]]; then
  mkdir -p "$SHARE_DIR/ui"
  cp -a "$UI_SRC"/. "$SHARE_DIR/ui/"
  if [[ -f "$SHARE_DIR/ui/Downloader.Ui" ]]; then
    chmod +x "$SHARE_DIR/ui/Downloader.Ui"
    cat > "$BIN_DIR/Downloader.Ui" <<WRAP
#!/usr/bin/env bash
exec "$SHARE_DIR/ui/Downloader.Ui" "\$@"
WRAP
    chmod +x "$BIN_DIR/Downloader.Ui"
  elif [[ -f "$SHARE_DIR/ui/Downloader.Ui.dll" ]]; then
    cat > "$BIN_DIR/Downloader.Ui" <<WRAP
#!/usr/bin/env bash
exec dotnet "$SHARE_DIR/ui/Downloader.Ui.dll" "\$@"
WRAP
    chmod +x "$BIN_DIR/Downloader.Ui"
  fi
  echo "UI → $SHARE_DIR/ui"
else
  echo "⚠️ UI не знайдено (dotnet publish apps/ui). Встановлено лише core/cli/nmhost." >&2
fi

# Linux .desktop
if [[ "$(uname -s)" == "Linux" ]]; then
  APP_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
  if [[ "$SYSTEM" -eq 1 ]]; then
    APP_DIR="/usr/local/share/applications"
  fi
  mkdir -p "$APP_DIR"
  EXEC_CORE="$BIN_DIR/downloader-core"
  EXEC_UI="$BIN_DIR/Downloader.Ui"
  if [[ ! -x "$EXEC_UI" ]]; then
    EXEC_UI="$EXEC_CORE"
  fi
  cat > "$APP_DIR/downloader.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Downloader
Comment=Download manager (core + Avalonia UI)
Exec=sh -c '$EXEC_CORE & sleep 0.5; exec $EXEC_UI'
Icon=application-x-executable
Terminal=false
Categories=Network;FileTransfer;
StartupNotify=true
DESKTOP
  echo "desktop → $APP_DIR/downloader.desktop"
fi

# Native Messaging
if [[ -x "$BIN_DIR/downloader-nmhost" ]]; then
  if "$BIN_DIR/downloader-nmhost" --install; then
    echo "nmhost --install OK"
  else
    echo "⚠️ nmhost --install не вдався (браузерів може не бути)" >&2
  fi
fi

# Autostart: systemd user unit (Linux) / LaunchAgent (macOS)
if [[ "$NO_AUTOSTART" -eq 0 ]]; then
  OS="$(uname -s)"
  CORE_ABS="$BIN_DIR/downloader-core"
  if [[ "$OS" == "Linux" ]]; then
    UNIT_SRC="$ROOT/packaging/systemd/downloader-core.service"
    if [[ ! -f "$UNIT_SRC" ]]; then
      echo "⚠️ немає $UNIT_SRC — пропускаю systemd" >&2
    elif ! command -v systemctl >/dev/null 2>&1; then
      echo "⚠️ systemctl відсутній — пропускаю автозапуск systemd" >&2
    else
      UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
      if [[ "$SYSTEM" -eq 1 ]]; then
        UNIT_DIR="/etc/systemd/user"
      fi
      mkdir -p "$UNIT_DIR"
      sed "s|__DOWNLOADER_CORE__|$CORE_ABS|g" "$UNIT_SRC" > "$UNIT_DIR/downloader-core.service"
      chmod 644 "$UNIT_DIR/downloader-core.service"
      echo "systemd unit → $UNIT_DIR/downloader-core.service"
      if [[ "$SYSTEM" -eq 0 ]] && systemctl --user enable --now downloader-core.service 2>/dev/null; then
        echo "systemd: enabled --now downloader-core.service"
      else
        echo "⚠️ systemctl --user enable --now не вдався (немає user bus / не login session)." >&2
        echo "   Пізніше: systemctl --user daemon-reload && systemctl --user enable --now downloader-core" >&2
      fi
    fi
  elif [[ "$OS" == "Darwin" ]]; then
    PLIST_SRC="$ROOT/packaging/macos/com.riasj1dar.downloader-core.plist"
    AGENTS="$HOME/Library/LaunchAgents"
    PLIST_DST="$AGENTS/com.riasj1dar.downloader-core.plist"
    if [[ ! -f "$PLIST_SRC" ]]; then
      echo "⚠️ немає $PLIST_SRC — пропускаю LaunchAgent" >&2
    else
      mkdir -p "$AGENTS"
      if command -v launchctl >/dev/null 2>&1; then
        launchctl unload "$PLIST_DST" 2>/dev/null || true
      fi
      sed "s|__DOWNLOADER_CORE__|$CORE_ABS|g" "$PLIST_SRC" > "$PLIST_DST"
      chmod 644 "$PLIST_DST"
      echo "LaunchAgent → $PLIST_DST"
      if command -v launchctl >/dev/null 2>&1; then
        if launchctl load "$PLIST_DST" 2>/dev/null; then
          echo "launchctl: loaded $PLIST_DST"
        else
          echo "⚠️ launchctl load не вдався — plist встановлено, завантажте вручну" >&2
        fi
      else
        echo "⚠️ launchctl відсутній — plist скопійовано" >&2
      fi
    fi
  fi
fi

cat <<EOF

Встановлено в: $PREFIX
  downloader-core  $BIN_DIR/downloader-core
  dl               $BIN_DIR/dl
  downloader-nmhost $BIN_DIR/downloader-nmhost

Запуск:
  $BIN_DIR/downloader-core &
  $BIN_DIR/dl add https://example.com/file.zip
  ${BIN_DIR}/Downloader.Ui   # якщо зібрано UI

Автозапуск:
  Linux:  systemctl --user status downloader-core
  macOS:  launchctl list | grep downloader-core
  (вимкнути: ./packaging/install-unix.sh --no-autostart після ручного unload/disable)

ffmpeg (YouTube / DASH mux):
  $BIN_DIR/dl ffmpeg-install
  # або системний: apt/brew install ffmpeg

Linux tray (desktop session): пакети libgtk-3 / libayatana-appindicator3.
Без DISPLAY ядро все одно обслуговує IPC.

Додайте $BIN_DIR до PATH, якщо ще немає:
  export PATH="$BIN_DIR:\$PATH"
EOF

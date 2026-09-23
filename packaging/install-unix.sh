#!/usr/bin/env bash
# Встановити Downloader на Linux/macOS з уже зібраних артефактів.
#
# Типово: ~/.local/share/Downloader + ~/.local/bin у PATH + .desktop (Linux).
# Системно: sudo ./packaging/install-unix.sh --system  → /usr/local
#
# Перед цим:
#   cargo build --release -p downloader-cli -p downloader-core-service -p downloader-nmhost
#   dotnet publish -c Release -r linux-x64 --self-contained true apps/ui   # або osx-*
#
# Залежності трею (Linux, desktop session):
#   Debian/Ubuntu: libgtk-3-0 libayatana-appindicator3-1
#   (для збірки: libgtk-3-dev libxdo-dev libayatana-appindicator3-dev)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SYSTEM=0
PREFIX=""

usage() {
  cat <<EOF
Usage: $0 [--user|--system] [--prefix DIR]

  --user     install to ~/.local/share/Downloader (default)
  --system   install to /usr/local (needs root)
  --prefix   override install root (binaries under PREFIX/bin or PREFIX/)
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --user) SYSTEM=0; shift ;;
    --system) SYSTEM=1; shift ;;
    --prefix) PREFIX="$2"; shift 2 ;;
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
    "$ROOT/target/release/$name"
    "$ROOT/target/debug/$name"
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
    echo "немає $name — спочатку: cargo build --release -p downloader-cli -p downloader-core-service -p downloader-nmhost" >&2
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

# UI: шукаємо publish / build вихід
UI_SRC=""
for cand in \
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
    # обгортка в BIN_DIR
    cat > "$BIN_DIR/Downloader.Ui" <<EOF
#!/usr/bin/env bash
exec "$SHARE_DIR/ui/Downloader.Ui" "\$@"
EOF
    chmod +x "$BIN_DIR/Downloader.Ui"
  elif [[ -f "$SHARE_DIR/ui/Downloader.Ui.dll" ]]; then
    cat > "$BIN_DIR/Downloader.Ui" <<EOF
#!/usr/bin/env bash
exec dotnet "$SHARE_DIR/ui/Downloader.Ui.dll" "\$@"
EOF
    chmod +x "$BIN_DIR/Downloader.Ui"
  fi
  # Трей шукає Downloader.Ui поруч із downloader-core — wrapper уже в BIN_DIR.
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
  cat > "$APP_DIR/downloader.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Downloader
Comment=Download manager (core + Avalonia UI)
Exec=sh -c '$EXEC_CORE & sleep 0.5; exec $EXEC_UI'
Icon=application-x-executable
Terminal=false
Categories=Network;FileTransfer;
StartupNotify=true
EOF
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

cat <<EOF

Встановлено в: $PREFIX
  downloader-core  $BIN_DIR/downloader-core
  dl               $BIN_DIR/dl
  downloader-nmhost $BIN_DIR/downloader-nmhost

Запуск:
  $BIN_DIR/downloader-core &
  $BIN_DIR/dl add https://example.com/file.zip
  ${BIN_DIR}/Downloader.Ui   # якщо зібрано UI

ffmpeg (YouTube / DASH mux):
  $BIN_DIR/dl ffmpeg-install
  # або системний: apt/brew install ffmpeg

Linux tray (desktop session): пакети libgtk-3 / libayatana-appindicator3.
Без DISPLAY ядро все одно обслуговує IPC.

Додайте $BIN_DIR до PATH, якщо ще немає:
  export PATH="$BIN_DIR:\$PATH"
EOF

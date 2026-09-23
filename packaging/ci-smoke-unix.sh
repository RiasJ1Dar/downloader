#!/usr/bin/env bash
# Headless Unix smoke (no GUI / Gatekeeper). Used by release-unix.yml and ci.yml.
#
# Usage:
#   packaging/ci-smoke-unix.sh [BIN_DIR]
#
# BIN_DIR defaults to target/release (relative to repo root). Must contain:
#   downloader-core, dl, downloader-nmhost
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

BIN_DIR="${1:-$ROOT/target/release}"
CORE="$BIN_DIR/downloader-core"
DL="$BIN_DIR/dl"
NMHOST="$BIN_DIR/downloader-nmhost"

die() { echo "smoke FAIL: $*" >&2; exit 1; }

[[ -x "$CORE" ]] || die "missing executable: $CORE"
[[ -x "$DL" ]] || die "missing executable: $DL"
[[ -x "$NMHOST" ]] || die "missing executable: $NMHOST"

# Portable soft-timeout (macOS runners lack GNU timeout / gtimeout).
# Runs command; if still alive after SECS, SIGTERM then SIGKILL. Exit status
# of the command wins unless we had to kill it (then 124).
run_timeout() {
  local secs="$1"; shift
  "$@" &
  local pid=$!
  (
    sleep "$secs"
    if kill -0 "$pid" 2>/dev/null; then
      echo "smoke: timeout ${secs}s — killing PID $pid ($*)" >&2
      kill -TERM "$pid" 2>/dev/null || true
      sleep 1
      kill -KILL "$pid" 2>/dev/null || true
    fi
  ) &
  local watcher=$!
  local rc=0
  wait "$pid" || rc=$?
  kill "$watcher" 2>/dev/null || true
  wait "$watcher" 2>/dev/null || true
  # 143 = 128+15 SIGTERM from our watchdog
  if [[ "$rc" -eq 143 || "$rc" -eq 137 ]]; then
    return 124
  fi
  return "$rc"
}

echo "==> smoke: version / help ($BIN_DIR)"
"$CORE" -V
"$CORE" -h >/dev/null
"$DL" -V
"$DL" -h >/dev/null
# nmhost used to hang on --help; must exit quickly (was fixed in 0.1.1).
run_timeout 5 "$NMHOST" --help >/dev/null \
  || die "downloader-nmhost --help did not exit cleanly within 5s"
run_timeout 5 "$NMHOST" -V >/dev/null \
  || die "downloader-nmhost -V did not exit cleanly within 5s"

# Default IPC endpoint (must match crates/ipc default_ipc_endpoint_unix).
# Passing --pipe skips tray + nmhost --install (safe on headless CI) while
# still listening where `dl` connects.
default_pipe() {
  case "$(uname -s)" in
    Darwin)
      echo "${HOME}/Library/Application Support/Downloader/core.sock"
      ;;
    Linux)
      if [[ -n "${XDG_RUNTIME_DIR:-}" ]]; then
        echo "${XDG_RUNTIME_DIR}/downloader/core.sock"
      else
        echo "/tmp/downloader-$(id -u)/core.sock"
      fi
      ;;
    *)
      die "unsupported OS for smoke: $(uname -s)"
      ;;
  esac
}

SMOKE_ROOT="${RUNNER_TEMP:-${TMPDIR:-/tmp}}/downloader-ci-smoke-$$"
mkdir -p "$SMOKE_ROOT/downloads"
DB="$SMOKE_ROOT/tasks.db"
DLDIR="$SMOKE_ROOT/downloads"
PIPE="$(default_pipe)"
mkdir -p "$(dirname "$PIPE")"
rm -f "$PIPE"

cleanup() {
  if [[ -n "${CORE_PID:-}" ]] && kill -0 "$CORE_PID" 2>/dev/null; then
    kill -INT "$CORE_PID" 2>/dev/null || true
    # Give ctrl_c handler a moment; then escalate.
    for _ in 1 2 3 4 5; do
      kill -0 "$CORE_PID" 2>/dev/null || break
      sleep 0.2
    done
    if kill -0 "$CORE_PID" 2>/dev/null; then
      kill -TERM "$CORE_PID" 2>/dev/null || true
      sleep 0.5
      kill -KILL "$CORE_PID" 2>/dev/null || true
    fi
    wait "$CORE_PID" 2>/dev/null || true
  fi
  rm -rf "$SMOKE_ROOT"
}
trap cleanup EXIT

echo "==> smoke: start downloader-core (db=$DB pipe=$PIPE)"
export DOWNLOADER_WATCH_CLIPBOARD=0
"$CORE" --db "$DB" --downloads "$DLDIR" --pipe "$PIPE" \
  >"$SMOKE_ROOT/core.log" 2>&1 &
CORE_PID=$!

ready=0
for _ in $(seq 1 25); do
  if [[ -S "$PIPE" ]]; then ready=1; break; fi
  if ! kill -0 "$CORE_PID" 2>/dev/null; then
    echo "---- core.log ----" >&2
    cat "$SMOKE_ROOT/core.log" >&2 || true
    die "downloader-core exited before socket appeared"
  fi
  sleep 0.2
done
[[ "$ready" -eq 1 ]] || {
  echo "---- core.log ----" >&2
  cat "$SMOKE_ROOT/core.log" >&2 || true
  die "socket not ready: $PIPE"
}

echo "==> smoke: dl list"
"$DL" list >/dev/null

echo "==> smoke: stop core (SIGINT)"
kill -INT "$CORE_PID" 2>/dev/null || true
wait "$CORE_PID" 2>/dev/null || true
CORE_PID=

echo "==> smoke OK"

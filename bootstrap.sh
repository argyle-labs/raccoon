#!/usr/bin/env bash
# bootstrap.sh - Orchestrate a Linux DESKTOP (Bazzite/CachyOS) bring-up from three
# independent concerns, each its own re-runnable script:
#   dev      scripts/setup-dev.sh     — developer toolchain
#   desktop  scripts/setup-kde.sh     — KDE apps + fonts (Arch)
#   gaming   scripts/setup-gaming.sh  — launchers + MangoHud/gamescope/audio configs
# Idempotent: safe to re-run (also serves as a restore step).
#
#   ./bootstrap.sh                     all three concerns
#   ./bootstrap.sh --dev --gaming      only the named concerns (any subset)
#   ./bootstrap.sh --timer             also enable the daily doctor (drift-check) timer
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

WANT_DEV=0 WANT_DESKTOP=0 WANT_GAMING=0 WANT_TIMER=0 ANY_CONCERN=0
for arg in "$@"; do
  case "$arg" in
    --dev)     WANT_DEV=1; ANY_CONCERN=1 ;;
    --desktop) WANT_DESKTOP=1; ANY_CONCERN=1 ;;
    --gaming)  WANT_GAMING=1; ANY_CONCERN=1 ;;
    --timer)   WANT_TIMER=1 ;;
    *) echo "!! unknown flag: $arg" >&2; exit 2 ;;
  esac
done
# No concern flags → do all three.
if [ "$ANY_CONCERN" = 0 ]; then WANT_DEV=1 WANT_DESKTOP=1 WANT_GAMING=1; fi

install_timer() {
  echo "== Installing daily doctor timer (user)"
  mkdir -p "$HOME/.config/systemd/user"
  sed "s#@HERE@#$HERE#g" "$HERE/systemd/raccoon-doctor.service" > "$HOME/.config/systemd/user/raccoon-doctor.service"
  install -m644 "$HERE/systemd/raccoon-doctor.timer" "$HOME/.config/systemd/user/raccoon-doctor.timer"
  systemctl --user daemon-reload
  systemctl --user enable --now raccoon-doctor.timer
  echo "   enabled: $(systemctl --user is-enabled raccoon-doctor.timer)  (journalctl --user -u raccoon-doctor)"
}

[ "$WANT_DEV" = 1 ]     && "$HERE/scripts/setup-dev.sh"
[ "$WANT_DESKTOP" = 1 ] && "$HERE/scripts/setup-kde.sh"
[ "$WANT_GAMING" = 1 ]  && "$HERE/scripts/setup-gaming.sh"
[ "$WANT_TIMER" = 1 ]   && install_timer

echo
echo "== bootstrap complete."

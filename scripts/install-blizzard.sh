#!/usr/bin/env bash
# install-blizzard.sh - Install Battle.net (and other Wine-only launchers) via
# NonSteamLaunchers (NSL), which sets up GE-Proton + the launcher and auto-adds
# it to Steam. NSL is the reliable path on Bazzite/CachyOS (see docs/NOTES.md).
#
# Usage:
#   ./install-blizzard.sh                       # Battle.net
#   ./install-blizzard.sh "Battle.net" "EA App" # multiple launchers
#
# Notes:
#  - Log into each launcher when its window appears (interactive Blizzard/EA login).
#  - NSL RESTARTS STEAM at the end to register shortcuts - do not run mid-game.
#  - Launched as a transient systemd user service so it survives this shell/SSH.
set -euo pipefail

LAUNCHERS=("$@"); [ ${#LAUNCHERS[@]} -eq 0 ] && LAUNCHERS=("Battle.net")
NSL_URL="https://raw.githubusercontent.com/moraroy/NonSteamLaunchers-On-Steam-Deck/main/NonSteamLaunchers.sh"
RUID="$(id -u)"

# Session env so NSL's GUI/dialogs map onto the desktop.
WL="${WAYLAND_DISPLAY:-wayland-0}"
ENVS=(
  "--setenv=DISPLAY=${DISPLAY:-:0}"
  "--setenv=WAYLAND_DISPLAY=$WL"
  "--setenv=XDG_RUNTIME_DIR=/run/user/$RUID"
  "--setenv=DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$RUID/bus"
  "--setenv=HOME=$HOME"
)

# Build the "-- name name ..." argument list for NSL.
printf -v ARGS ' %q' "${LAUNCHERS[@]}"

echo ">> Installing via NonSteamLaunchers:${ARGS}"
echo ">> A login window will appear for each launcher. NSL restarts Steam at the end."

systemctl --user reset-failed nsl-install 2>/dev/null || true
systemd-run --user --collect --unit=nsl-install "${ENVS[@]}" \
  bash -c "curl -Ls '$NSL_URL' | bash -s --${ARGS}"

echo ">> NSL started as user service 'nsl-install'."
echo ">> Watch progress:   journalctl --user -u nsl-install -f"
echo ">>                    tail -f ~/Downloads/NonSteamLaunchers-install.log"
echo ">> Real progress = network/processes, not the (static) zenity dialog. Do NOT hit Cancel."

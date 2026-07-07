#!/usr/bin/env bash
# bootstrap.sh - Bring a Linux gaming machine to the known-good state.
# Detects the distro, installs the launchers, and applies the drop-in configs.
# Idempotent: safe to re-run (also serves as a restore step).
#
# Does NOT log you into anything or install games (those are interactive) and
# does NOT run NSL (it restarts Steam) - run scripts/install-blizzard.sh for that.
#   ./bootstrap.sh --timer   also install + enable a daily doctor (drift-check) timer
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=/dev/null
distro() { [ -r /etc/os-release ] && . /etc/os-release && echo "${ID:-} ${ID_LIKE:-}"; }

install_timer() {
  echo "== Installing daily doctor timer (user)"
  mkdir -p "$HOME/.config/systemd/user"
  sed "s#@HERE@#$HERE#g" "$HERE/systemd/raccoon-doctor.service" > "$HOME/.config/systemd/user/raccoon-doctor.service"
  install -m644 "$HERE/systemd/raccoon-doctor.timer" "$HOME/.config/systemd/user/raccoon-doctor.timer"
  systemctl --user daemon-reload
  systemctl --user enable --now raccoon-doctor.timer
  echo "   enabled: $(systemctl --user is-enabled raccoon-doctor.timer)  (journalctl --user -u raccoon-doctor)"
}

flatpaks() {
  command -v flatpak >/dev/null || { echo "!! flatpak not found; skipping launchers"; return; }
  flatpak install -y --noninteractive flathub \
    com.heroicgameslauncher.hgl net.lutris.Lutris com.vysp3r.ProtonPlus || true
}

echo "== Detected: $(distro)"
case "$(distro)" in
  *bazzite*|*fedora*)
    echo "== Bazzite: Steam/gamescope/mangohud/umu are preinstalled; installing launchers via Flatpak"
    flatpaks
    ;;
  *cachyos*|*arch*)
    echo "== CachyOS/Arch: installing base packages via pacman"
    sudo pacman -S --needed --noconfirm steam lutris mangohud gamescope umu-launcher || true
    sudo pacman -S --needed --noconfirm heroic-games-launcher 2>/dev/null || flatpaks
    flatpak install -y --noninteractive flathub net.davidotek.pupgui2 || true   # ProtonUp-Qt
    ;;
  *)
    echo "!! Unknown distro - install Steam, Heroic, Lutris, mangohud, gamescope, umu-launcher manually."
    flatpaks
    ;;
esac

echo "== Applying drop-in configs"
install -Dm644 "$HERE/configs/MangoHud/MangoHud.conf"               "$HOME/.config/MangoHud/MangoHud.conf"
install -Dm644 "$HERE/configs/environment.d/10-gamescope-refresh.conf" "$HOME/.config/environment.d/10-gamescope-refresh.conf"
install -Dm644 "$HERE/configs/wireplumber/51-alsa-headroom.conf"        "$HOME/.config/wireplumber/wireplumber.conf.d/51-alsa-headroom.conf"
systemctl --user restart wireplumber 2>/dev/null || true   # reload so the audio headroom applies now
echo "   -> MangoHud + gamescope refresh env + ALSA headroom (audio crackle fix) installed"

[ "${1:-}" = "--timer" ] && install_timer

echo
echo "== Next (interactive / privileged):"
echo "   sudo $HERE/scripts/setup-controller-wake.sh   # USB controller wake"
echo "   $HERE/scripts/cpu-mode.sh performance          # CPU power mode (orca power:cpu); balanced|powersave too"
echo "   $HERE/scripts/install-blizzard.sh             # Battle.net via NSL (restarts Steam)"
echo "   Heroic: log in (Epic/GOG/Amazon), set GE-Proton + Add-to-Steam + HDR, install games"
echo "   See docs/SETUP.md for the full runbook."

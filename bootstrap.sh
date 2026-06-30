#!/usr/bin/env bash
# bootstrap.sh - Bring a Linux gaming machine to the known-good state.
# Detects the distro, installs the launchers, and applies the drop-in configs.
# Idempotent: safe to re-run (also serves as a restore step).
#
# Does NOT log you into anything or install games (those are interactive) and
# does NOT run NSL (it restarts Steam) - run scripts/install-blizzard.sh for that.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

distro() { [ -r /etc/os-release ] && . /etc/os-release && echo "${ID:-} ${ID_LIKE:-}"; }

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
echo "   -> MangoHud + gamescope refresh env installed (restart Gaming Mode to apply refresh)"

echo
echo "== Next (interactive / privileged):"
echo "   sudo $HERE/scripts/setup-controller-wake.sh   # USB controller wake"
echo "   $HERE/scripts/install-blizzard.sh             # Battle.net via NSL (restarts Steam)"
echo "   Heroic: log in (Epic/GOG/Amazon), set GE-Proton + Add-to-Steam + HDR, install games"
echo "   See docs/SETUP.md for the full runbook."

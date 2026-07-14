#!/usr/bin/env bash
# setup-gaming.sh - Gaming stack: launchers (Steam/Heroic/Lutris) + the drop-in
# configs (MangoHud, gamescope refresh env, ALSA headroom). Distro-aware,
# idempotent. Does NOT log in or install games; NSL is scripts/install-blizzard.sh.
# This is the GAMING concern only — dev is setup-dev.sh, desktop is setup-kde.sh.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# shellcheck source=/dev/null
distro() { [ -r /etc/os-release ] && . /etc/os-release && echo "${ID:-} ${ID_LIKE:-}"; }

flatpaks() {
  command -v flatpak >/dev/null || { echo "!! flatpak not found; skipping launchers"; return; }
  flatpak install -y --noninteractive flathub \
    com.heroicgameslauncher.hgl net.lutris.Lutris com.vysp3r.ProtonPlus || true
}

echo "== Gaming stack — detected: $(distro)"
case "$(distro)" in
  *bazzite*|*fedora*)
    echo "== Bazzite: Steam/gamescope/mangohud/umu preinstalled; launchers via Flatpak"
    flatpaks
    ;;
  *cachyos*|*arch*)
    echo "== CachyOS/Arch: base packages via pacman"
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
install -Dm644 "$HERE/configs/MangoHud/MangoHud.conf"                  "$HOME/.config/MangoHud/MangoHud.conf"
install -Dm644 "$HERE/configs/environment.d/10-gamescope-refresh.conf" "$HOME/.config/environment.d/10-gamescope-refresh.conf"
install -Dm644 "$HERE/configs/wireplumber/51-alsa-headroom.conf"       "$HOME/.config/wireplumber/wireplumber.conf.d/51-alsa-headroom.conf"
systemctl --user restart wireplumber 2>/dev/null || true   # reload so the audio headroom applies now
echo "   -> MangoHud + gamescope refresh env + ALSA headroom (audio crackle fix) installed"

echo
echo "== Next (interactive / privileged):"
echo "   sudo $HERE/scripts/setup-controller-wake.sh   # USB controller wake"
echo "   $HERE/scripts/cpu-mode.sh performance          # CPU power mode (orca power:cpu); balanced|powersave too"
echo "   $HERE/scripts/install-blizzard.sh             # Battle.net via NSL (restarts Steam)"
echo "   Heroic: log in (Epic/GOG/Amazon), set GE-Proton + Add-to-Steam + HDR, install games"
echo "   See docs/SETUP.md for the full runbook."

#!/usr/bin/env bash
# setup-kde.sh - Practical KDE Plasma apps + fonts (Arch/CachyOS only).
# No theming — just the useful desktop utilities. Idempotent.
set -euo pipefail

command -v pacman >/dev/null 2>&1 || { echo "!! KDE setup is Arch-only (pacman) — skipping"; exit 0; }

echo ">> Installing KDE apps + fonts (pacman)"
sudo pacman -S --noconfirm --needed \
  partitionmanager ffmpegthumbs kio-extras xdg-desktop-portal-kde \
  yakuake kate ark okular gwenview kdeconnect kdegraphics-thumbnailers \
  noto-fonts noto-fonts-emoji ttf-jetbrains-mono papirus-icon-theme

echo "== KDE apps installed (yakuake: F12 drop-down terminal; tiling: Meta+T)."

#!/usr/bin/env bash
# setup-dev.sh - Developer toolchain for a Linux box (Bazzite/CachyOS, apt/dnf).
# Dev tools + AUR helper + 1Password CLI + gcloud + Node (nvm) + Nerd Font, and
# sets zsh as the default shell. Idempotent; safe to re-run.
# This is the DEV concern only — desktop (KDE) is setup-kde.sh, gaming is
# setup-gaming.sh.
set -euo pipefail

info() { echo -e ">> $1"; }
ok()   { echo -e "   ok: $1"; }
warn() { echo -e "!! $1"; }

pkg_mgr() {
  if command -v paru   >/dev/null 2>&1; then echo paru
  elif command -v pacman >/dev/null 2>&1; then echo pacman
  elif command -v apt-get >/dev/null 2>&1; then echo apt
  elif command -v dnf    >/dev/null 2>&1; then echo dnf
  else echo unknown; fi
}

install_paru() {
  command -v paru >/dev/null 2>&1 && { ok "paru present"; return; }
  command -v pacman >/dev/null 2>&1 || return 0
  info "installing paru (AUR helper)"
  sudo pacman -S --noconfirm --needed base-devel git
  tmp="$(mktemp -d)"; git clone https://aur.archlinux.org/paru.git "$tmp/paru"
  ( cd "$tmp/paru" && makepkg -si --noconfirm ); rm -rf "$tmp"
  ok "paru installed"
}

install_packages() {
  case "$(pkg_mgr)" in
    apt)
      sudo apt-get update -qq
      sudo apt-get install -y -qq zsh git curl direnv openjdk-17-jdk libpq-dev alacritty fastfetch
      ;;
    dnf)
      sudo dnf install -y -q zsh git curl direnv java-17-openjdk libpq-devel alacritty fastfetch
      ;;
    pacman|paru)
      sudo pacman -S --noconfirm --needed \
        zsh git curl direnv jdk17-openjdk postgresql-libs alacritty fastfetch eza bat
      install_paru
      command -v paru >/dev/null 2>&1 && { paru -Qi nvm >/dev/null 2>&1 || paru -S --noconfirm nvm; }
      ;;
    *) warn "unknown package manager — install manually: zsh git curl direnv openjdk libpq alacritty fastfetch";;
  esac
  ok "dev packages installed"
}

install_1password_cli() {
  command -v op >/dev/null 2>&1 && { ok "1Password CLI present"; return; }
  if command -v paru >/dev/null 2>&1; then paru -S --noconfirm 1password-cli && ok "1Password CLI installed"
  else warn "1Password CLI: install manually (https://1password.com/downloads/command-line/)"; fi
}

install_gcloud() {
  command -v gcloud >/dev/null 2>&1 && { ok "gcloud present"; return; }
  info "installing Google Cloud SDK"
  curl -sSL https://sdk.cloud.google.com | bash -s -- --disable-prompts --install-dir="$HOME"
  ok "gcloud installed"
}

install_nerd_font() {
  fc-list 2>/dev/null | grep -qi "MesloLGS Nerd Font" && { ok "MesloLGS Nerd Font present"; return; }
  info "installing MesloLGS Nerd Font"
  if command -v paru >/dev/null 2>&1; then paru -S --noconfirm ttf-meslo-nerd
  elif command -v pacman >/dev/null 2>&1; then sudo pacman -S --noconfirm ttf-meslo-nerd
  else
    d="$HOME/.local/share/fonts"; mkdir -p "$d"
    curl -fsSL "https://github.com/ryanoasis/nerd-fonts/releases/latest/download/Meslo.tar.xz" | tar -xJf - -C "$d"
    fc-cache -f "$d"
  fi
  ok "MesloLGS Nerd Font installed"
}

install_node() {
  export NVM_DIR="$HOME/.nvm"; mkdir -p "$NVM_DIR"
  if [ -s /usr/share/nvm/init-nvm.sh ]; then . /usr/share/nvm/init-nvm.sh
  elif [ -s "$NVM_DIR/nvm.sh" ]; then . "$NVM_DIR/nvm.sh"
  else
    info "installing nvm"
    curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.3/install.sh | bash
    . "$NVM_DIR/nvm.sh"
  fi
  if command -v nvm >/dev/null 2>&1; then
    nvm ls --no-colors 2>/dev/null | grep -q lts || { info "installing Node LTS"; nvm install --lts; }
    ok "Node ready"
  fi
}

set_default_shell() {
  [ "$SHELL" = "$(command -v zsh)" ] && { ok "zsh already default shell"; return; }
  info "setting zsh as default shell"; chsh -s "$(command -v zsh)" && ok "default shell set to zsh"
}

echo "== raccoon dev toolchain ($(pkg_mgr))"
install_packages
install_1password_cli
install_gcloud
install_nerd_font
install_node
set_default_shell
echo "== dev toolchain done."

#!/usr/bin/env bash
# doctor.sh - diagnose a raccoon-managed gaming box: compare the known-good
# declarative state (this repo's configs) against what's live, and report drift
# as issues. Optionally repair the ones that are safe to re-apply.
#
#   ./doctor.sh            human-readable report (default)
#   ./doctor.sh --json     machine-readable issues (shape mirrors orca's Issue type)
#   ./doctor.sh --repair   re-run the idempotent action that owns each auto-fixable issue
#
# Exit code: 0 = all OK, 1 = at least one WARN, 2 = at least one CRIT.
# Read-only by default; only --repair mutates (and never anything needing sudo -
# those are reported with the command to run yourself).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

MODE="report"
case "${1:-}" in
  --json)   MODE="json" ;;
  --repair) MODE="repair" ;;
  ""|--report) MODE="report" ;;
  *) echo "usage: $0 [--json|--repair]"; exit 1 ;;
esac

# shellcheck source=/dev/null
distro() { [ -r /etc/os-release ] && . /etc/os-release && echo "${ID:-} ${ID_LIKE:-}"; }

# --- issue accumulator -------------------------------------------------------
# Each issue: id | severity(OK|WARN|CRIT) | title | detail | repair-cmd | automatic(0|1)
# repair-cmd empty = nothing to auto-run. automatic=1 = safe to run under --repair
# (no sudo, no Steam restart). Manual/privileged fixes carry the command but
# automatic=0 so --repair only prints them.
ISSUES=()
issue() { ISSUES+=("$1|$2|$3|$4|${5:-}|${6:-0}"); }

# --- checks ------------------------------------------------------------------
check_gamescope_refresh() {
  case "$(distro)" in *bazzite*|*fedora*) ;; *) return 0 ;; esac  # Gaming Mode only
  local f="$HOME/.config/environment.d/10-gamescope-refresh.conf"
  if [ -f "$f" ] && grep -q '^CUSTOM_REFRESH_RATES=' "$f"; then
    issue gamescope-refresh OK "High-refresh exposed to games" "CUSTOM_REFRESH_RATES set in ${f/#$HOME/\~}"
  else
    issue gamescope-refresh WARN "Games capped at 60Hz" \
      "environment.d refresh config missing/incomplete; gamescope only advertises 60Hz" \
      "$HERE/scripts/setup-gamescope-refresh.sh" 1
  fi
}

check_mangohud() {
  local src="$HERE/configs/MangoHud/MangoHud.conf" dst="$HOME/.config/MangoHud/MangoHud.conf"
  if [ ! -f "$dst" ]; then
    issue mangohud WARN "MangoHud overlay config missing" "no ${dst/#$HOME/\~}" \
      "install -Dm644 '$src' '$dst'" 1
  elif ! cmp -s "$src" "$dst"; then
    issue mangohud WARN "MangoHud config drifted from repo" "${dst/#$HOME/\~} differs from known-good" \
      "install -Dm644 '$src' '$dst'" 1
  else
    issue mangohud OK "MangoHud config in place" "matches repo"
  fi
}

check_ge_proton() {
  local d="$HOME/.steam/root/compatibilitytools.d" alt="$HOME/.local/share/Steam/compatibilitytools.d"
  [ -d "$d" ] || d="$alt"
  if [ -d "$d" ] && compgen -G "$d/GE-Proton*" >/dev/null; then
    issue ge-proton OK "GE-Proton installed" "found in ${d/#$HOME/\~}"
  else
    issue ge-proton WARN "No GE-Proton runner found" \
      "widest-compat runner (and the Battle.net fix) absent; install via ProtonPlus/ProtonUp-Qt" ""
  fi
}

check_controller_wake() {
  local rule=/etc/udev/rules.d/90-usb-wakeup.rules
  if [ -f "$rule" ]; then
    issue controller-wake OK "USB controller wake armed" "$rule present"
  else
    issue controller-wake WARN "Controller can't wake the box" \
      "udev USB-wake rule not installed (USB/dongle/wired only; BT can't wake)" \
      "sudo $HERE/scripts/setup-controller-wake.sh" 0
  fi
}

check_nsl_scanner() {
  if systemctl --user list-unit-files --no-legend 2>/dev/null | grep -q '^NSLGameScanner'; then
    if systemctl --user is-enabled NSLGameScanner.service >/dev/null 2>&1; then
      issue nsl-scanner OK "NSL game scanner enabled" "installed launchers auto-add to Steam"
    else
      issue nsl-scanner WARN "NSL scanner installed but disabled" \
        "NSL games won't auto-add as Steam tiles" \
        "systemctl --user enable --now NSLGameScanner.service" 1
    fi
  fi  # not installed = fine (user may not use NSL)
}

check_launchers() {
  if command -v flatpak >/dev/null && flatpak info com.heroicgameslauncher.hgl >/dev/null 2>&1; then
    issue heroic OK "Heroic installed" "Epic/GOG/Amazon path present"
  else
    issue heroic WARN "Heroic not installed" "Epic/GOG/Amazon launcher missing; run ./bootstrap.sh" \
      "flatpak install -y --noninteractive flathub com.heroicgameslauncher.hgl" 1
  fi
}

check_steam() {
  # Native binary or Flatpak - either counts. Steam is the hub everything feeds.
  if command -v steam >/dev/null 2>&1; then
    issue steam OK "Steam installed" "native binary on PATH"
  elif command -v flatpak >/dev/null && flatpak info com.valvesoftware.Steam >/dev/null 2>&1; then
    issue steam OK "Steam installed" "Flatpak (com.valvesoftware.Steam)"
  else
    issue steam CRIT "Steam not found" "no native or Flatpak Steam; the whole library hub is missing"
  fi
}

check_umu() {
  # umu-launcher backs NSL/Battle.net. Bazzite ships it; CachyOS installs it.
  if command -v umu-run >/dev/null 2>&1; then
    issue umu OK "umu-launcher present" "NSL/Battle.net path available"
  else
    case "$(distro)" in
      *cachyos*|*arch*) issue umu WARN "umu-launcher missing" \
        "needed for NSL/Battle.net launchers" "sudo pacman -S --needed umu-launcher" 0 ;;
      *) issue umu WARN "umu-launcher missing" \
        "needed for NSL/Battle.net launchers; normally preinstalled on Bazzite" "" ;;
    esac
  fi
}

# --- CachyOS / Arch specific -------------------------------------------------
check_aur_helper() {
  case "$(distro)" in *cachyos*|*arch*) ;; *) return 0 ;; esac
  if command -v paru >/dev/null 2>&1 || command -v yay >/dev/null 2>&1; then
    issue aur-helper OK "AUR helper present" "paru/yay available for foreign pkgs"
  else
    issue aur-helper WARN "No AUR helper (paru/yay)" \
      "restoring foreign/AUR packages (beaver pacman-aur.txt) needs one" \
      "sudo pacman -S --needed paru" 0
  fi
}

check_btrfs_snapshots() {
  case "$(distro)" in *cachyos*|*arch*) ;; *) return 0 ;; esac
  # Only relevant on a btrfs root, where instant local rollback is possible.
  local fstype; fstype="$(findmnt -no FSTYPE / 2>/dev/null || echo '')"
  [ "$fstype" = btrfs ] || return 0
  if command -v snapper >/dev/null 2>&1 && snapper list-configs 2>/dev/null | grep -q '^root'; then
    issue btrfs-snapshots OK "Local snapshots configured" "snapper 'root' config present"
  elif command -v timeshift >/dev/null 2>&1; then
    issue btrfs-snapshots OK "Local snapshots configured" "timeshift installed"
  else
    issue btrfs-snapshots WARN "No local btrfs snapshots" \
      "btrfs root with no snapper/timeshift; off-box restic (beaver) still covers you, but instant rollback isn't set up" \
      "sudo pacman -S --needed snapper" 0
  fi
}

# --- Bazzite / Fedora atomic specific ----------------------------------------
check_flathub_remote() {
  case "$(distro)" in *bazzite*|*fedora*) ;; *) return 0 ;; esac
  command -v flatpak >/dev/null || return 0
  if flatpak remotes --columns=name 2>/dev/null | grep -qx flathub; then
    issue flathub OK "Flathub remote configured" "launchers installable via Flatpak"
  else
    issue flathub WARN "Flathub remote missing" "launchers (Heroic/Lutris) can't install without it" \
      "flatpak remote-add --if-not-exists flathub https://flathub.org/repo/flathub.flatpakrepo" 1
  fi
}

# Per-game locks: files we chmod 444 so the game can't overwrite our tweak.
# Format: <path>|<label>. Extend as per-game fixes accrue (see docs/SETUP.md §7).
check_locked_game_files() {
  local steamapps="$HOME/.local/share/Steam/steamapps/common"
  local locks=(
    "$steamapps/Detroit Become Human/GraphicOptions.JSON|Detroit: Become Human framerate unlock"
  )
  local entry path label
  for entry in "${locks[@]}"; do
    path="${entry%%|*}"; label="${entry##*|}"
    [ -e "$path" ] || continue   # game not installed - not an issue
    if [ -w "$path" ]; then
      issue "lock:$label" WARN "Game can overwrite a locked tweak" \
        "$label: file is writable, game may reset it" \
        "chmod 444 '$path'" 1
    else
      issue "lock:$label" OK "Per-game tweak locked" "$label (444)"
    fi
  done
}

run_checks() {
  # cross-distro
  check_steam
  check_launchers
  check_umu
  check_ge_proton
  check_mangohud
  check_controller_wake
  check_nsl_scanner
  check_locked_game_files
  # Bazzite / Fedora atomic
  check_gamescope_refresh
  check_flathub_remote
  # CachyOS / Arch
  check_aur_helper
  check_btrfs_snapshots
}

# --- output ------------------------------------------------------------------
json_escape() { local s="$1"; s="${s//\\/\\\\}"; s="${s//\"/\\\"}"; printf '%s' "$s"; }

emit_json() {
  printf '{"distro":"%s","issues":[' "$(json_escape "$(distro)")"
  local first=1 i id sev title detail repair auto
  for i in "${ISSUES[@]}"; do
    IFS='|' read -r id sev title detail repair auto <<<"$i"
    [ "$first" = 1 ] || printf ','
    first=0
    local autobool=false
    [ "$auto" = 1 ] && autobool=true
    printf '{"id":"%s","severity":"%s","title":"%s","detail":"%s","repair":"%s","automatic":%s}' \
      "$(json_escape "$id")" "$sev" "$(json_escape "$title")" "$(json_escape "$detail")" \
      "$(json_escape "$repair")" "$autobool"
  done
  printf ']}\n'
}

emit_report() {
  local i id sev title detail repair auto n_warn=0 n_crit=0
  for i in "${ISSUES[@]}"; do
    IFS='|' read -r id sev title detail repair auto <<<"$i"
    case "$sev" in
      OK)   printf '  \033[32mOK\033[0m   %s\n' "$title" ;;
      WARN) printf '  \033[33mWARN\033[0m %s\n       %s\n' "$title" "$detail"; n_warn=$((n_warn+1)) ;;
      CRIT) printf '  \033[31mCRIT\033[0m %s\n       %s\n' "$title" "$detail"; n_crit=$((n_crit+1)) ;;
    esac
    if [ "$sev" != OK ] && [ -n "$repair" ]; then
      if [ "$auto" = 1 ]; then printf '       fix: %s   (./doctor.sh --repair)\n' "$repair"
      else printf '       fix: %s\n' "$repair"; fi
    fi
  done
  echo
  echo "== $((${#ISSUES[@]})) checks, ${n_warn} warn, ${n_crit} crit"
}

do_repair() {
  local i id sev title detail repair auto ran=0
  for i in "${ISSUES[@]}"; do
    IFS='|' read -r id sev title detail repair auto <<<"$i"
    if [ "$sev" != OK ] && [ -n "$repair" ] && [ "$auto" = 1 ]; then
      echo ">> repairing [$id]: $repair"
      bash -c "$repair" || echo "   !! repair failed for $id"
      ran=$((ran+1))
    elif [ "$sev" != OK ] && [ -n "$repair" ]; then
      echo ">> [$id] needs manual/privileged fix: $repair"
    fi
  done
  echo "== auto-repaired $ran issue(s); re-run ./doctor.sh to confirm"
}

# --- main --------------------------------------------------------------------
run_checks
case "$MODE" in
  json)   emit_json ;;
  repair) emit_report; echo; do_repair ;;
  report) emit_report ;;
esac

# exit code reflects worst severity
worst=0
for i in "${ISSUES[@]}"; do
  IFS='|' read -r _ sev _ _ _ _ <<<"$i"
  case "$sev" in CRIT) worst=2 ;; WARN) [ "$worst" -lt 1 ] && worst=1 ;; esac
done
exit "$worst"

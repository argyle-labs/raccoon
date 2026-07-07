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
# shellcheck source=lib/settings.sh
[ -f "$HERE/lib/settings.sh" ] && . "$HERE/lib/settings.sh"

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
  # Compare ignoring output_folder= (set per-machine by tune.sh, expected to differ).
  elif ! diff -q <(grep -v '^output_folder=' "$src") <(grep -v '^output_folder=' "$dst") >/dev/null; then
    issue mangohud WARN "MangoHud config drifted from repo" "${dst/#$HOME/\~} differs from known-good (ignoring output_folder)" \
      "install -Dm644 '$src' '$dst'" 1
  else
    issue mangohud OK "MangoHud config in place" "matches repo (output_folder machine-local)"
  fi
}

# Audio crackle/dropout fix: the WirePlumber ALSA-headroom drop-in must be in
# place AND live. Two failure modes - the config file drifted/missing (auto-fix:
# reinstall + reload wireplumber), or the file is present but the running graph
# still shows headroom 0 (wireplumber wasn't restarted since install; same fix).
check_alsa_headroom() {
  local src="$HERE/configs/wireplumber/51-alsa-headroom.conf"
  local dst="$HOME/.config/wireplumber/wireplumber.conf.d/51-alsa-headroom.conf"
  local repair="install -Dm644 '$src' '$dst' && systemctl --user restart wireplumber"
  if [ ! -f "$dst" ]; then
    issue alsa-headroom WARN "Audio has no ALSA headroom (crackle/dropout risk)" \
      "WirePlumber drop-in missing at ${dst/#$HOME/\~}; ALSA sinks run headroom=0 and underrun under load" \
      "$repair" 1
  elif ! diff -q "$src" "$dst" >/dev/null 2>&1; then
    issue alsa-headroom WARN "ALSA headroom config drifted from repo" \
      "${dst/#$HOME/\~} differs from known-good" "$repair" 1
  # File is correct - confirm it's actually live in the running graph.
  elif command -v pw-dump >/dev/null 2>&1 \
       && pw-dump 2>/dev/null | grep -q '"api.alsa.headroom": 0'; then
    issue alsa-headroom WARN "ALSA headroom config present but not applied" \
      "a sink still reports headroom=0; wireplumber needs a restart to pick it up" \
      "$repair" 1
  else
    issue alsa-headroom OK "Audio headroom applied" "ALSA sinks buffered against xruns (no crackle)"
  fi
}

# CPU power mode: desired mode is a runtime orca setting (power:cpu {mode}); the
# machine owns its goal like display:target. raccoon maps it to a tuned profile
# (tuned owns governor+EPP here and persists the profile across reboots). Warn on
# drift; the fix is privileged (sudo tuned-adm), so non-auto. Silent when orca
# has no mode set for this host (nothing to enforce).
check_cpu_mode() {
  command -v tuned-adm >/dev/null 2>&1 || return 0
  local mode; mode="$(orca_setting power cpu mode 2>/dev/null || true)"
  [ -n "$mode" ] || { issue cpu-mode INFO "CPU power mode not managed by orca" \
    "set one with: $HERE/scripts/cpu-mode.sh performance|balanced|powersave"; return 0; }
  local base want live
  case "$mode" in
    performance) base=throughput-performance ;;
    balanced)    base=balanced ;;
    powersave)   base=powersave ;;
    *) issue cpu-mode WARN "Unknown orca CPU mode '$mode'" \
         "power:cpu.mode must be performance|balanced|powersave" \
         "$HERE/scripts/cpu-mode.sh balanced" 0; return 0 ;;
  esac
  if tuned-adm list 2>/dev/null | grep -qE "^- ${base}-bazzite\b"; then want="${base}-bazzite"; else want="$base"; fi
  live="$(tuned-adm active 2>/dev/null | sed -n 's/^Current active profile: //p')"
  if [ "$live" = "$want" ]; then
    issue cpu-mode OK "CPU mode: $mode" "tuned profile '$live' (orca-managed, all cores)"
  else
    issue cpu-mode WARN "CPU mode drifted (want $mode)" \
      "orca wants '$mode' (tuned '$want') but live profile is '${live:-unknown}'" \
      "sudo $HERE/scripts/cpu-mode.sh --apply" 0
  fi
}

check_gamescope_hdr() {
  case "$(distro)" in *bazzite*|*fedora*) ;; *) return 0 ;; esac
  local f="$HOME/.config/environment.d/15-gamescope-hdr.conf"
  if [ -f "$f" ] && grep -q '^ENABLE_GAMESCOPE_HDR=1' "$f"; then
    issue gamescope-hdr OK "HDR wired for Gaming Mode" "ENABLE_GAMESCOPE_HDR set (AMD needs this manually)"
  else
    # Not an error on SDR panels - informational, with the fix if they want HDR.
    issue gamescope-hdr INFO "HDR not forced on" \
      "on AMD, gamescope-session won't set ENABLE_GAMESCOPE_HDR itself; enable if your panel supports HDR" \
      "$HERE/scripts/setup-gamescope-refresh.sh" 1
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

# Does the live display config match this machine's orca target (display:target)?
# The machine owns its goal (bragi 4K@120, hemlock 1440p@144); this checks the
# box is actually set up to chase it. Silent when orca has no target for the host.
check_display_target() {
  command -v orca_setting >/dev/null 2>&1 || return 0
  local w h r hdr; w="$(orca_setting display target width)"; h="$(orca_setting display target height)"
  r="$(orca_setting display target refresh)"; hdr="$(orca_setting display target hdr)"
  [ -n "$r" ] || return 0   # no target configured for this host - nothing to check
  local label="${w}x${h}@${r}"; [ "$hdr" = True ] && label="$label HDR"
  case "$(distro)" in *bazzite*|*fedora*) ;; *)
    issue display-target INFO "Machine target: $label" "orca display:target (set gamescope output to match)"; return 0 ;;
  esac
  local modes="$HOME/.config/gamescope/modes.cfg" refenv="$HOME/.config/environment.d/10-gamescope-refresh.conf"
  local ok=1 detail=""
  if ! { [ -f "$modes" ] && grep -q "${w}x${h}@${r}" "$modes"; }; then
    ok=0; detail="gamescope modes.cfg not forcing ${w}x${h}@${r}"
  fi
  if ! { [ -f "$refenv" ] && grep -q "^CUSTOM_REFRESH_RATES=.*\b${r}\b" "$refenv"; }; then
    ok=0; detail="${detail:+$detail; }refresh env doesn't expose ${r}Hz"
  fi
  if [ "$ok" = 1 ]; then
    issue display-target OK "Display matches machine target" "$label"
  else
    issue display-target WARN "Display not set to machine target ($label)" \
      "$detail" "Gaming Mode -> Display: set ${w}x${h}@${r}; $HERE/scripts/setup-gamescope-refresh.sh" 0
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
  check_alsa_headroom
  check_cpu_mode
  check_controller_wake
  check_nsl_scanner
  check_locked_game_files
  # machine target (orca display:target)
  check_display_target
  # Bazzite / Fedora atomic
  check_gamescope_refresh
  check_gamescope_hdr
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
      INFO) printf '  \033[36mINFO\033[0m %s\n       %s\n' "$title" "$detail" ;;
      WARN) printf '  \033[33mWARN\033[0m %s\n       %s\n' "$title" "$detail"; n_warn=$((n_warn+1)) ;;
      CRIT) printf '  \033[31mCRIT\033[0m %s\n       %s\n' "$title" "$detail"; n_crit=$((n_crit+1)) ;;
    esac
    # INFO carries an optional hint but is never auto-repaired; only WARN/CRIT.
    if { [ "$sev" = WARN ] || [ "$sev" = CRIT ]; } && [ -n "$repair" ]; then
      if [ "$auto" = 1 ]; then printf '       fix: %s   (./doctor.sh --repair)\n' "$repair"
      else printf '       fix: %s\n' "$repair"; fi
    elif [ "$sev" = INFO ] && [ -n "$repair" ]; then
      printf '       enable: %s\n' "$repair"
    fi
  done
  echo
  echo "== $((${#ISSUES[@]})) checks, ${n_warn} warn, ${n_crit} crit"
}

do_repair() {
  local i id sev title detail repair auto ran=0
  for i in "${ISSUES[@]}"; do
    IFS='|' read -r id sev title detail repair auto <<<"$i"
    if { [ "$sev" = WARN ] || [ "$sev" = CRIT ]; } && [ -n "$repair" ] && [ "$auto" = 1 ]; then
      echo ">> repairing [$id]: $repair"
      bash -c "$repair" || echo "   !! repair failed for $id"
      ran=$((ran+1))
    elif { [ "$sev" = WARN ] || [ "$sev" = CRIT ]; } && [ -n "$repair" ]; then
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

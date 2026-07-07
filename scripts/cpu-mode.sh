#!/usr/bin/env bash
# cpu-mode.sh - set / apply this machine's CPU power mode.
#
# The desired mode is a runtime orca setting (power:cpu {mode}), so you can flip
# it per-host without editing the repo. raccoon maps the mode to a real tuned
# profile and applies it - tuned owns the CPU governor + EPP here (amd-pstate-epp
# on bragi), and its active profile persists across reboots, all cores.
#
#   ./cpu-mode.sh                 show desired (orca) vs live (tuned) state
#   ./cpu-mode.sh performance     set orca + apply (max clocks - best for latency/audio/gaming)
#   ./cpu-mode.sh balanced        set orca + apply (dynamic, slight perf bias)
#   ./cpu-mode.sh powersave       set orca + apply (dynamic, biased to save power)
#   ./cpu-mode.sh --apply         apply whatever orca already says (used by doctor --repair)
#
# Applying runs `sudo tuned-adm profile ...` (privileged). Setting the orca value
# does not need sudo. Modes: performance | balanced | powersave.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib/settings.sh
[ -f "$HERE/../lib/settings.sh" ] && . "$HERE/../lib/settings.sh"

# mode -> tuned profile base. We prefer a "<base>-bazzite" variant when tuned
# lists it (keeps Bazzite's extra tweaks), else fall back to the generic base.
mode_to_base() {
  case "$1" in
    performance) echo throughput-performance ;;
    balanced)    echo balanced ;;
    powersave)   echo powersave ;;
    *) echo "unknown mode: $1 (use performance|balanced|powersave)" >&2; return 1 ;;
  esac
}
resolve_profile() {
  local base; base="$(mode_to_base "$1")" || return 1
  if tuned-adm list 2>/dev/null | grep -qE "^- ${base}-bazzite\b"; then
    echo "${base}-bazzite"
  else
    echo "$base"
  fi
}
desired_mode() { orca_setting power cpu mode; }
live_profile() { tuned-adm active 2>/dev/null | sed -n 's/^Current active profile: //p'; }

apply_mode() {
  local mode="$1" prof
  prof="$(resolve_profile "$mode")" || return 1
  echo ">> applying $mode -> tuned profile: $prof (all cores, persists across reboots)"
  sudo tuned-adm profile "$prof"
  echo "   now: $(live_profile)"
}
set_mode() {
  local mode="$1"
  mode_to_base "$mode" >/dev/null || return 1   # validate
  echo ">> setting orca power:cpu mode=$mode"
  orca config set power cpu "{\"mode\":\"$mode\"}" >/dev/null
  apply_mode "$mode"
}

case "${1:-}" in
  ""|--status)
    echo "desired (orca power:cpu): ${_d:=$(desired_mode)}${_d:+ -> profile $(resolve_profile "$_d" 2>/dev/null)}"
    echo "live (tuned active):      $(live_profile)"
    ;;
  --apply)
    m="$(desired_mode)"
    [ -n "$m" ] || { echo "no orca power:cpu mode set for this host; nothing to apply" >&2; exit 1; }
    apply_mode "$m"
    ;;
  performance|balanced|powersave) set_mode "$1" ;;
  *) echo "usage: $0 [performance|balanced|powersave|--apply|--status]" >&2; exit 1 ;;
esac

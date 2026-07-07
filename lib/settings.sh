#!/usr/bin/env bash
# lib/settings.sh - read this machine's gaming settings from the orca DB.
# The source of truth is orca config (host-owned typed rows), not a repo file:
#   display:target   {width,height,refresh,hdr}
#   graphics:prefs   {gpu_vendor,upscaler,upscaler_version,upscaler_quality,frame_gen}
#   power:cpu        {mode}  -> performance | balanced | powersave (see cpu-mode.sh)
# Sourced by tune.sh and doctor.sh. Degrades gracefully when orca isn't present
# (standalone / non-fleet box): callers fall back to their own defaults.

# The read verb differs across orca versions: newer builds use `config detail`,
# rc.10-era builds use `config get`. Resolve it once per shell so a mixed fleet
# (laptop newer, bragi rc.10) reads settings on every box. Empty if orca can't
# fetch rows at all (absent / GNOME screen-reader collision / old CLI).
_orca_get_verb() {
  [ -n "${_ORCA_GET_VERB:-}" ] && { echo "$_ORCA_GET_VERB"; return; }
  local v=""
  if orca config get --help >/dev/null 2>&1; then v='get'
  elif orca config detail --help >/dev/null 2>&1; then v='detail'
  fi
  _ORCA_GET_VERB="$v"; echo "$v"
}

# orca_setting <noun> <name> <field> -> prints the field value, or nothing.
# Uses the orca CLI (our fleet binary; the GNOME screen reader has no `config`
# verb, so a bad match just yields empty). Needs python3 to parse the row.
# Always succeeds (prints empty on any failure) so callers under `set -euo
# pipefail` never abort — orca may be absent, unauthorized (401), or the daemon
# down; all just mean "no setting", handled by falling back.
orca_setting() {
  command -v orca >/dev/null 2>&1 || return 0
  command -v python3 >/dev/null 2>&1 || return 0
  local verb; verb="$(_orca_get_verb)"; [ -n "$verb" ] || return 0
  { orca config "$verb" "$1" "$2" 2>/dev/null | python3 -c "
import json,sys
try:
    row = (json.load(sys.stdin) or {}).get('row') or {}
    val = json.loads(row.get('json','{}') or '{}').get('$3')
    print('' if val is None else val)
except Exception:
    pass
"; } 2>/dev/null || true
}

# True if orca is usable as a settings source on this host.
have_orca_settings() { [ -n "$(orca_setting display target refresh)" ]; }

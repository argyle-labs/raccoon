#!/usr/bin/env bash
# lib/settings.sh - read this machine's gaming settings from the orca DB.
# The source of truth is orca config (host-owned typed rows), not a repo file:
#   display:target   {width,height,refresh,hdr}
#   graphics:prefs   {gpu_vendor,upscaler,upscaler_version,upscaler_quality,frame_gen}
# Sourced by tune.sh and doctor.sh. Degrades gracefully when orca isn't present
# (standalone / non-fleet box): callers fall back to their own defaults.

# orca_setting <noun> <name> <field> -> prints the field value, or nothing.
# Uses the orca CLI (our fleet binary; the GNOME screen reader has no `config`
# verb, so a bad match just yields empty). Needs python3 to parse the row.
# Always succeeds (prints empty on any failure) so callers under `set -euo
# pipefail` never abort — orca may be absent, unauthorized (401), or the daemon
# down; all just mean "no setting", handled by falling back.
orca_setting() {
  command -v orca >/dev/null 2>&1 || return 0
  command -v python3 >/dev/null 2>&1 || return 0
  { orca config get "$1" "$2" 2>/dev/null | python3 -c "
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

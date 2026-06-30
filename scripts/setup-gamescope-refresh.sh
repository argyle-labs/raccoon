#!/usr/bin/env bash
# setup-gamescope-refresh.sh - Bazzite Gaming Mode: expose up to 120Hz to ALL
# games. gamescope-session sources ~/.config/environment.d/*.conf at session
# start; without CUSTOM_REFRESH_RATES it only advertises 60Hz and games cap at 60.
#
# After running, RESTART Gaming Mode (env.d is read at session start).
# Output resolution itself is set in Gaming Mode -> Display (writes
# ~/.config/gamescope/modes.cfg, e.g. "<Display>:3840x2160@120").
set -euo pipefail

# Edit to match your panel's real max (this link tops out at 4K@120).
MAXHZ="${MAXHZ:-120}"
MINHZ="${MINHZ:-60}"

DEST="$HOME/.config/environment.d/10-gamescope-refresh.conf"
mkdir -p "$(dirname "$DEST")"
cat > "$DEST" <<EOF
# Expose up to ${MAXHZ}Hz in Gaming Mode (gamescope) for all games.
# Read by gamescope-session-plus at session start.
STEAM_DISPLAY_REFRESH_LIMITS=${MINHZ},${MAXHZ}
CUSTOM_REFRESH_RATES=${MINHZ},${MAXHZ}
EOF

echo ">> Wrote $DEST:"
cat "$DEST"
echo
echo ">> modes.cfg (current forced output, if any):"
cat "$HOME/.config/gamescope/modes.cfg" 2>/dev/null || echo "   (none - set resolution/refresh in Gaming Mode -> Display)"
echo
echo "RESTART Gaming Mode to apply, then set per-game refresh in QAM (Ctrl+2) -> Performance."

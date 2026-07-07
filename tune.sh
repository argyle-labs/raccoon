#!/usr/bin/env bash
# tune.sh - review in-game metrics (MangoHud logs) and suggest system/software
# tweaks to improve the experience. Turns frame data into findings: stutter,
# GPU/CPU-bound, thermal, uncapped-fps - each with a concrete tweak.
#
#   ./tune.sh enable            configure MangoHud logging (output folder + live 1% low)
#   ./tune.sh analyze [FILE]    analyze a log (default: newest in the output folder)
#   ./tune.sh watch  [FILE]     re-analyze the active log every few seconds (~live)
#   ./tune.sh --json analyze    machine-readable findings (Issue shape)
#
# Env: TARGET_FPS (default from refresh cap, else 60), MANGO_LOGDIR (log folder),
#      WATCH_INTERVAL (seconds, default 5).
# How to capture: play with the overlay up (Shift_R+F12), toggle logging with
# Shift_L+F2 during the rough patch, toggle off, then `./tune.sh analyze`.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/settings.sh
[ -f "$HERE/lib/settings.sh" ] && . "$HERE/lib/settings.sh"

JSON=0
[ "${1:-}" = "--json" ] && { JSON=1; shift; }
CMD="${1:-analyze}"; FILE="${2:-}"

LOGDIR="${MANGO_LOGDIR:-$HOME/.local/share/raccoon/mangohud-logs}"
MANGO_CONF="$HOME/.config/MangoHud/MangoHud.conf"
WATCH_INTERVAL="${WATCH_INTERVAL:-5}"

# Target FPS: prefer the machine's orca setting (display:target.refresh), then
# the refresh cap the box exposes, else 60.
default_target() {
  local r
  if command -v orca_setting >/dev/null 2>&1; then
    r="$(orca_setting display target refresh)"
    [ -n "$r" ] && { echo "$r"; return; }
  fi
  local f="$HOME/.config/environment.d/10-gamescope-refresh.conf" max=60
  if [ -f "$f" ]; then
    max="$(grep '^CUSTOM_REFRESH_RATES=' "$f" 2>/dev/null | tr ',' '\n' | grep -Eo '[0-9]+' | sort -n | tail -1)"
    [ -n "$max" ] || max=60
  fi
  echo "$max"
}
TARGET_FPS="${TARGET_FPS:-$(default_target)}"
# Target resolution + upscaler preference (from orca), for the report + tweaks.
TARGET_W="$(orca_setting display target width 2>/dev/null || true)"
TARGET_H="$(orca_setting display target height 2>/dev/null || true)"
PREF_UPSCALER="$(orca_setting graphics prefs upscaler 2>/dev/null || true)"

# --- enable ------------------------------------------------------------------
cmd_enable() {
  mkdir -p "$LOGDIR"
  [ -f "$MANGO_CONF" ] || install -Dm644 "$HERE/configs/MangoHud/MangoHud.conf" "$MANGO_CONF"
  # Point MangoHud at our folder (absolute path; MangoHud doesn't expand ~).
  if grep -q '^output_folder=' "$MANGO_CONF"; then
    sed -i "s#^output_folder=.*#output_folder=$LOGDIR#" "$MANGO_CONF"
  else
    printf 'output_folder=%s\n' "$LOGDIR" >> "$MANGO_CONF"
  fi
  echo ">> logging to: $LOGDIR"
  echo ">> in-game: Shift_R+F12 = overlay (shows live 1% / 0.1% low), Shift_L+F2 = start/stop a capture"
  echo ">> then:    ./tune.sh analyze     (or ./tune.sh watch for live)"
}

# Total VRAM in MB. MangoHud logs *used* only, so read capacity from the driver.
# AMD/Intel expose it in sysfs; NVIDIA via nvidia-smi. 0 = unknown (skip check).
gpu_vram_total_mb() {
  if [ -n "${VRAM_TOTAL_MB:-}" ]; then echo "$VRAM_TOTAL_MB"; return; fi
  local f b
  for f in /sys/class/drm/card*/device/mem_info_vram_total; do
    [ -r "$f" ] || continue
    b="$(cat "$f" 2>/dev/null)" || continue
    [ -n "$b" ] && { echo $(( b / 1024 / 1024 )); return; }
  done
  if command -v nvidia-smi >/dev/null 2>&1; then
    nvidia-smi --query-gpu=memory.total --format=csv,noheader,nounits 2>/dev/null | head -1 | tr -d ' ' && return
  fi
  echo 0
}

newest_log() {
  [ -d "$LOGDIR" ] || return 1
  find "$LOGDIR" -maxdepth 1 -name '*.csv' -printf '%T@ %p\n' 2>/dev/null \
    | sort -nr | head -1 | cut -d' ' -f2-
}

# --- metrics -----------------------------------------------------------------
# Prints KEY=VALUE lines for eval. Robust to column order: maps names from the
# MangoHud data header (the row containing both "fps" and "frametime").
metrics_of() {
  local f="$1"
  local hln; hln="$(awk -F',' 'index($0,"fps") && index($0,"frametime"){print NR; exit}' "$f")"
  [ -n "$hln" ] || { echo "SAMPLES=0"; return 0; }
  local hdr; hdr="$(sed -n "${hln}p" "$f")"
  idx_of() { echo "$hdr" | awk -F',' -v n="$1" '{for(i=1;i<=NF;i++){g=$i;gsub(/^ +| +$/,"",g);if(g==n){print i;exit}}}'; }
  local cf cft ccpu cgpu cct cgt cpow cvram
  cf="$(idx_of fps)"; cft="$(idx_of frametime)"; ccpu="$(idx_of cpu_load)"; cgpu="$(idx_of gpu_load)"
  cct="$(idx_of cpu_temp)"; cgt="$(idx_of gpu_temp)"; cpow="$(idx_of gpu_power)"; cvram="$(idx_of gpu_vram_used)"
  : "${cf:=1}" "${cft:=2}" "${ccpu:=0}" "${cgpu:=0}" "${cct:=0}" "${cgt:=0}" "${cpow:=0}" "${cvram:=0}"

  # Averages / maxima in one awk pass.
  awk -F',' -v s="$hln" -v cf="$cf" -v cft="$cft" -v ccpu="$ccpu" -v cgpu="$cgpu" \
            -v cct="$cct" -v cgt="$cgt" -v cpow="$cpow" -v cvram="$cvram" '
    NR>s && $cf ~ /^[0-9]/ {
      n++; fps+=$cf; ft+=$cft;
      if(ccpu)cpu+=$ccpu; if(cgpu)gpu+=$cgpu; if(cpow)pow+=$cpow;
      if(cct && $cct>ctm)ctm=$cct; if(cgt && $cgt>gtm)gtm=$cgt;
      if(cvram && $cvram>vram)vram=$cvram;
      # frametime spikes: frame > 2x the running mean once we have a baseline
      if(n>30){ m=ft/n; if($cft > 2*m) spikes++ }
    }
    END{
      if(!n){print "SAMPLES=0"; exit}
      printf "SAMPLES=%d\n", n;
      printf "FPS_AVG=%.1f\n", fps/n;
      printf "FT_AVG=%.2f\n", ft/n;
      printf "CPU_AVG=%.0f\n", (ccpu? cpu/n : 0);
      printf "GPU_AVG=%.0f\n", (cgpu? gpu/n : 0);
      printf "CT_MAX=%.0f\n", ctm;
      printf "GT_MAX=%.0f\n", gtm;
      printf "POW_AVG=%.0f\n", (cpow? pow/n : 0);
      printf "VRAM_MAX=%.0f\n", vram;
      printf "SPIKES=%d\n", spikes;
    }' "$f"

  # Percentiles via sort (portable; no gawk asort needed).
  local n; n="$(awk -F',' -v s="$hln" -v cf="$cf" 'NR>s && $cf ~ /^[0-9]/{c++}END{print c+0}' "$f")"
  if [ "$n" -gt 0 ]; then
    local i_low i_p99
    i_low="$(awk -v n="$n" 'BEGIN{i=int(0.01*n); if(i<1)i=1; print i}')"     # 1% low fps = 1st percentile (sorted asc)
    i_p99="$(awk -v n="$n" 'BEGIN{i=int(0.99*n); if(i<1)i=1; if(i>n)i=n; print i}')" # p99 frametime = worst 1%
    echo "FPS_1LOW=$(awk -F',' -v s="$hln" -v c="$cf" 'NR>s && $c ~ /^[0-9]/{print $c}' "$f" | sort -n | sed -n "${i_low}p")"
    echo "FT_P99=$(awk -F',' -v s="$hln" -v c="$cft" 'NR>s && $c ~ /^[0-9]/{print $c}' "$f" | sort -n | sed -n "${i_p99}p")"
  fi
}

# --- findings ----------------------------------------------------------------
FINDINGS=()  # id|severity|title|detail|tweak
finding() { FINDINGS+=("$1|$2|$3|$4|${5:-}"); }

evaluate() { # consumes the KEY=VALUE metrics already eval'd into scope
  local target="$TARGET_FPS" target_ft
  target_ft="$(awk -v t="$target" 'BEGIN{printf "%.2f", 1000/t}')"

  # Stutter: worst-1% frametime far above the target frame budget, or many spikes.
  if [ -n "${FT_P99:-}" ] && awk -v p="$FT_P99" -v t="$target_ft" 'BEGIN{exit !(p > 2*t)}'; then
    finding stutter WARN "Frame stutter" \
      "p99 frametime ${FT_P99}ms vs ${target_ft}ms budget (${SPIKES:-0} spikes) - hitches, not low average fps" \
      "Pre-compile shaders (DXVK_ASYNC/GE-Proton, let the shader-cache build); enable Steam 'gamescope' frame-limit to ${target}; enable gamemode; close the KDE compositor / background apps; put shader cache + game on fast NVMe"
  fi

  # GPU-bound: GPU pinned but fps under target. Name the machine's preferred
  # upscaler (from graphics:prefs) so the tweak matches what this box uses.
  if [ -n "${GPU_AVG:-}" ] && [ "${GPU_AVG:-0}" -ge 95 ] \
     && awk -v f="${FPS_AVG:-0}" -v t="$target" 'BEGIN{exit !(f < 0.95*t)}'; then
    local up="${PREF_UPSCALER:-FSR/upscaling}"
    finding gpu-bound WARN "GPU-bound below target" \
      "gpu_load ${GPU_AVG}% avg, only ${FPS_AVG} fps vs ${target} target${TARGET_W:+ at ${TARGET_W}x${TARGET_H}}" \
      "Enable ${up} (this machine's upscaler) or drop resolution; lower the heaviest settings (shadows, RT, volumetrics); cap fps to a steady number the GPU can hold"
  fi

  # CPU-bound-ish: high avg CPU load while GPU has headroom.
  if [ -n "${CPU_AVG:-}" ] && [ "${CPU_AVG:-0}" -ge 85 ] && [ "${GPU_AVG:-100}" -lt 90 ]; then
    finding cpu-bound WARN "CPU-bound" \
      "cpu_load ${CPU_AVG}% avg with GPU at ${GPU_AVG}% (GPU has headroom)" \
      "Cap background work; enable gamemode (CPU governor -> performance); lower CPU-heavy settings (crowd/physics/draw distance); check for a single pinned core (bad thread scaling)"
  fi

  # VRAM exhaustion: used near capacity -> texture streaming hitches / evictions.
  if [ "${VRAM_TOTAL_MB:-0}" -gt 0 ] && [ "${VRAM_MAX:-0}" -gt 0 ] \
     && awk -v u="${VRAM_MAX:-0}" -v t="${VRAM_TOTAL_MB:-1}" 'BEGIN{exit !(u > 0.90*t)}'; then
    finding vram WARN "VRAM near capacity" \
      "peak ${VRAM_MAX}MB of ${VRAM_TOTAL_MB}MB - texture streaming stalls/evictions show up as stutter" \
      "Lower texture resolution / texture pool; disable/-reduce RT; drop render resolution (FSR); close other GPU apps (browser, wallpaper engine)"
  fi

  # Thermal: GPU running hot enough to throttle.
  if [ "${GT_MAX:-0}" -ge 84 ]; then
    finding thermal WARN "GPU thermals high" \
      "gpu_temp peaked ${GT_MAX}C - likely throttling, a common stutter source" \
      "Improve case airflow; apply a modest power/temp limit or undervolt; cap fps to cut heat; clean dust/repaste if sustained"
  fi

  # Uncapped: fps well above the panel cap wastes power/heat and can worsen pacing.
  if awk -v f="${FPS_AVG:-0}" -v t="$target" 'BEGIN{exit !(f > 1.15*t)}'; then
    finding uncapped INFO "FPS above the display cap" \
      "${FPS_AVG} fps avg over a ${target}Hz cap - extra heat/coil whine, no visible benefit" \
      "Cap fps at ${target} (gamescope --framerate-limit / MangoHud fps_limit / in-game vsync) for steadier frametimes and cooler running"
  fi

  # System-state correlations: settings that, when off, leave performance on the
  # table - weighted by whether a live symptom above makes them matter right now.
  evaluate_env

  # Nothing flagged.
  if [ "${#FINDINGS[@]}" -eq 0 ]; then
    finding ok OK "No tuning issues found" \
      "${FPS_AVG:-?} fps avg, ${FPS_1LOW:-?} 1% low, p99 frametime ${FT_P99:-?}ms over ${SAMPLES:-0} samples (target ${target})"
  fi
}

# Turn live system state into advice actionable right now. Kept here (not just in
# doctor.sh) so `watch` can flag it in-game and weight it by the live symptom.
evaluate_env() {
  # sched_ext scheduler (scx_lavd) installed but disabled -> frame-pacing left on
  # the table. WARN when we're CPU-bound/stuttering (lavd directly helps), else INFO.
  if [ -d /sys/kernel/sched_ext ] \
     && { command -v scx_loader >/dev/null 2>&1 || command -v scx_lavd >/dev/null 2>&1; } \
     && [ "$(cat /sys/kernel/sched_ext/state 2>/dev/null || echo disabled)" != enabled ]; then
    local sev=INFO
    printf '%s\n' "${FINDINGS[@]}" | grep -qE '^(cpu-bound|stutter)\|' && sev=WARN
    finding scx "$sev" "Scheduler not optimized (scx_lavd off)" \
      "a game-tuned sched_ext scheduler is installed but disabled - lavd smooths 1% lows / frame pacing under load" \
      "Enable it: 'ujust setup-scx' (Bazzite) or 'sudo systemctl enable --now scx_loader', then select lavd"
  fi

  # AMD GPU DPM stuck below auto/high -> GPU may not clock up for the game.
  local f lvl=""
  for f in /sys/class/drm/card*/device/power_dpm_force_performance_level; do
    [ -r "$f" ] || continue; lvl="$(cat "$f" 2>/dev/null)"; break
  done
  if [ -n "$lvl" ] && [ "$lvl" != auto ] && [ "$lvl" != high ]; then
    finding gpu-dpm WARN "GPU clocks capped (DPM=$lvl)" \
      "power_dpm_force_performance_level is '$lvl', not auto/high - the GPU may not boost for the game" \
      "Restore scaling: echo auto | sudo tee /sys/class/drm/card*/device/power_dpm_force_performance_level"
  fi
}

# in-game toast: notify once per newly-appeared WARN finding (watch mode only).
PREV_WARN=""
notify_new_warns() {
  command -v notify-send >/dev/null 2>&1 || return 0
  local x id sev title detail tweak
  for x in "${FINDINGS[@]}"; do
    IFS='|' read -r id sev title detail tweak <<<"$x"
    [ "$sev" = WARN ] || continue
    case ",$PREV_WARN," in *",$id,"*) continue ;; esac    # already toasted this run
    notify-send -u normal -a raccoon "raccoon: $title" "${detail}${tweak:+ - fix: $tweak}" 2>/dev/null || true
  done
  PREV_WARN="$(printf '%s\n' "${FINDINGS[@]}" | awk -F'|' '$2=="WARN"{printf "%s,",$1}')"
}

# --- output ------------------------------------------------------------------
json_escape() { local s="$1"; s="${s//\\/\\\\}"; s="${s//\"/\\\"}"; printf '%s' "$s"; }
emit() {
  if [ "$JSON" = 1 ]; then
    printf '{"file":"%s","target_fps":%s,"metrics":{"fps_avg":%s,"fps_1low":%s,"ft_p99":%s,"gpu_avg":%s,"cpu_avg":%s,"gt_max":%s,"vram_max_mb":%s,"vram_total_mb":%s,"samples":%s},"findings":[' \
      "$(json_escape "${1:-}")" "$TARGET_FPS" "${FPS_AVG:-0}" "${FPS_1LOW:-0}" "${FT_P99:-0}" "${GPU_AVG:-0}" "${CPU_AVG:-0}" "${GT_MAX:-0}" "${VRAM_MAX:-0}" "${VRAM_TOTAL_MB:-0}" "${SAMPLES:-0}"
    local first=1 x id sev title detail tweak
    for x in "${FINDINGS[@]}"; do
      IFS='|' read -r id sev title detail tweak <<<"$x"
      [ "$first" = 1 ] || printf ','; first=0
      printf '{"id":"%s","severity":"%s","title":"%s","detail":"%s","tweak":"%s"}' \
        "$(json_escape "$id")" "$sev" "$(json_escape "$title")" "$(json_escape "$detail")" "$(json_escape "$tweak")"
    done
    printf ']}\n'
  else
    printf '== %s\n' "${1:-(log)}"
    local tgt="$TARGET_FPS"; [ -n "${TARGET_W:-}" ] && tgt="${TARGET_W}x${TARGET_H}@${TARGET_FPS}${PREF_UPSCALER:+ ${PREF_UPSCALER}}"
    printf '   %s fps avg | %s 1%% low | p99 frametime %sms | gpu %s%% cpu %s%% | gpu %sC | vram %s/%sMB | %s samples | target %s\n' \
      "${FPS_AVG:-?}" "${FPS_1LOW:-?}" "${FT_P99:-?}" "${GPU_AVG:-?}" "${CPU_AVG:-?}" "${GT_MAX:-?}" \
      "${VRAM_MAX:-?}" "${VRAM_TOTAL_MB:-?}" "${SAMPLES:-0}" "$tgt"
    echo
    local x id sev title detail tweak
    for x in "${FINDINGS[@]}"; do
      IFS='|' read -r id sev title detail tweak <<<"$x"
      case "$sev" in
        OK)   printf '  \033[32mOK\033[0m   %s\n       %s\n' "$title" "$detail" ;;
        INFO) printf '  \033[36mINFO\033[0m %s\n       %s\n' "$title" "$detail" ;;
        WARN) printf '  \033[33mWARN\033[0m %s\n       %s\n' "$title" "$detail" ;;
      esac
      [ -n "$tweak" ] && printf '       tweak: %s\n' "$tweak"
    done
  fi
}

analyze_one() { # $1=file
  local f="$1"
  [ -f "$f" ] || { echo "no log to analyze (capture one with Shift_L+F2, or ./tune.sh enable)"; exit 1; }
  FINDINGS=()
  # shellcheck disable=SC2046  # KEY=VALUE lines are trusted (from our own awk)
  eval "$(metrics_of "$f")"
  if [ "${SAMPLES:-0}" -eq 0 ]; then echo "no frame samples in $f"; exit 1; fi
  VRAM_TOTAL_MB="$(gpu_vram_total_mb)"
  evaluate
  emit "$f"
}

# --- main --------------------------------------------------------------------
case "$CMD" in
  enable)  cmd_enable ;;
  analyze)
    [ -n "$FILE" ] || FILE="$(newest_log || true)"
    analyze_one "$FILE"
    ;;
  watch)
    echo ">> watching (${WATCH_INTERVAL}s); Ctrl-C to stop"
    command -v notify-send >/dev/null 2>&1 && echo ">> new WARN findings toast via notify-send"
    while true; do
      f="${FILE:-$(newest_log || true)}"
      clear 2>/dev/null || true
      if [ -n "$f" ] && [ -f "$f" ]; then analyze_one "$f" || true; notify_new_warns; else echo "waiting for a log in $LOGDIR ..."; fi
      sleep "$WATCH_INTERVAL"
    done
    ;;
  *) echo "usage: $0 [--json] {enable|analyze [FILE]|watch [FILE]}"; exit 1 ;;
esac

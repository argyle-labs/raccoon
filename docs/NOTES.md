# Field Notes & Gotchas

Hard-won lessons. Read before debugging the same thing twice.

## Battle.net / umu

- **Use NonSteamLaunchers (NSL).** It handles every umu edge case below and
  auto-adds to Steam. Raw `umu-run` works but is fiddly.
- **Detached umu dies with the SSH session.** `nohup &` / `setsid` launched over
  SSH gets killed when the session scope is cleaned (empty log, instant exit).
  Launch as a **transient systemd user service** (`systemd-run --user`) so it
  survives. A foreground SSH run also works (SSH holds it open).
- **Local `PROTONPATH` blocks umu's runtime fetch.** Pointing at a local
  GE-Proton dir errors instantly: *"Failed to match … with a container runtime."*
  Use the keyword `PROTONPATH=GE-Proton` so umu downloads Proton + runtime.
- **`STORE=battlenet` is required**, not `none`, or the Battle.net protonfix
  isn't applied.
- **GE-Proton has the Battle.net fix (≥9-23); UMU-Proton does not.** With the
  wrong Proton the installer hands off and dies before `Battle.net.exe` exists
  (symptom: `Agent.exe` runs, then everything exits, Program Files stays empty).
- **NSL's zenity dialog looks frozen** ("Starting update… please wait… 0%") — it
  never updates the percentage. Check `rx_bytes`/processes for real progress.
  **Don't click Cancel** (kills the install).
- **NSL restarts Steam** at the end — expected. Never run it while a game is
  running through Steam (it'll kill the game).

## gamescope / display

- Gaming Mode reads `~/.config/gamescope/modes.cfg` (per-display output) and
  sources `~/.config/environment.d/*.conf` at session start. `CUSTOM_REFRESH_RATES`
  + `STEAM_DISPLAY_REFRESH_LIMITS` are what expose >60 Hz to games. Without them
  gamescope advertises 60 Hz and games cap at 60. **Restart Gaming Mode** to apply.
- **Desktop (Plasma) mode is different:** a 4K panel at `Scale 2` → 1080p logical
  desktop → Xwayland games only see 1080p. `kscreen-doctor output.<NAME>.scale.1`
  exposes 4K (tiny UI), or just use Gaming Mode where gamescope hands games a
  clean 4K@120 surface.
- This panel/HDMI link maxes at **4K@120** (no 4K@144). 144 Hz only at lower res.

## Diagnosing stutter

Stutter is **not** the same as low fps — the average can look fine while
individual frames arrive late (hitching). Read it from the frame data, not feel:

- **The tell:** a high *p99 frametime* / big *frame_timing* graph spikes while
  average fps is near target. `tune.sh analyze` (or the live overlay's 1% / 0.1%
  lows, Shift_R+F12) surfaces exactly this. Low 1% low + fine average = stutter.
- **Shader-compilation stutter** is the usual cause on first playthroughs — the
  game compiles pipelines on the fly as new effects appear. Fixes: run
  **GE-Proton** (DXVK/VKD3D shader cache), let the cache warm up (first run of an
  area is worst, second is smooth), keep `DXVK_ASYNC`/async where the runner
  supports it, and don't wipe `~/.cache/mesa_shader_cache*` or the DXVK state
  cache. Put the game **and** shader cache on fast NVMe, not a network/HDD mount.
- **Frame-pacing stutter** from an uncapped fps: a game rendering way over the
  refresh rate can pace unevenly. Cap to the panel rate (gamescope
  `--framerate-limit`, MangoHud `fps_limit`, or in-game vsync).
- **Background contention:** the KDE compositor (Desktop mode), a busy CPU core,
  Heroic/NSL still downloading, or thermal throttling (gpu_temp ≥ ~84 °C) all
  show up as periodic spikes. `gamemode` (CPU governor → performance) helps.

**Example — Detroit: Become Human on a gaming box:** DBH shows traversal
stutter as you move through new areas (shader comp) and its in-game limiter
interacts badly if left at a low cap. Run it under GE-Proton, let the shader
cache build on a first pass, unlock the framerate via `GraphicOptions.JSON`
(`FRAME_RATE_LIMIT: 4`, then `chmod 444` — see §7 / doctor's `lock:` check), and
if spikes persist, cap fps to a steady number the GPU holds and confirm gpu_temp
isn't throttling. `tune.sh analyze` on a capture from the rough section will say
which of these it is.

## General

- Headless launches into a session need `DISPLAY=:0`, `WAYLAND_DISPLAY=wayland-0`,
  `XDG_RUNTIME_DIR=/run/user/$(id -u)`, `DBUS_SESSION_BUS_ADDRESS=unix:path=…/bus`.
- The umu/GitHub "Failed to acquire release assets" warning is usually
  **transient**, not rate-limiting — verify with `curl -s api.github.com/rate_limit`.
- **Heroic only downloads while open.** A "stalled" download usually means Heroic
  was closed.
- Bluetooth controllers can't wake the machine from sleep — USB/dongle/wired only.

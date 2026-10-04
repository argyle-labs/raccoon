<p align="center">
  <img src="assets/icon-256.png" width="120" alt="raccoon" />
</p>

# raccoon

Opinionated, reproducible setup for a Linux **desktop** — the dev/desktop base,
practical KDE apps, and the gaming stack with the system tweaks that make it all
work, in one place. Bring up a fresh box (or restore an existing one) to the same
known-good state.

**Tested distros:** [Bazzite](https://bazzite.gg) (Fedora atomic) and
[CachyOS](https://cachyos.org) (Arch). Structured so other distros slot in.

```bash
./bootstrap.sh              # desktop/dev base + gaming + configs
./scripts/setup-kde.sh      # practical KDE apps + fonts (Arch)
```

## As an orca diagnostics plugin

raccoon is also an **orca plugin** (a Rust subprocess plugin) that registers a provider in
orca's `diagnostics` capability domain. Running on the gaming box, it emits typed
`Finding`s (with optional repairs) that surface uniformly on orca's MCP / CLI /
REST — no bespoke scripts required:

```bash
orca diagnostics diagnose                                   # typed findings across all providers
orca diagnostics repair --provider raccoon --repair-id cpu-mode
```

Checks (each a typed `Finding` + optional `Repair`):

- **System/gaming drift:** **alsa-headroom** (audio crackle/dropout), **cpu-mode**
  (vs the `power:cpu` orca setting → tuned profile), **scx** (sched_ext/scx_lavd),
  **gpu-perf** (AMD DPM level), **shader-cache** (Steam pre-caching), **vrr**
  (adaptive-sync).
- **Provisioning** (setup over orca — the repair installs what's missing):
  **dev-toolchain**, **kde-apps**, **gaming-stack**. These are the orca-native
  form of the three `scripts/setup-*.sh`, so a box can be brought up with
  `orca diagnostics repair --provider raccoon --repair-id dev-toolchain` (etc.)
  and stays drift-checked afterward. Package installs are privileged and
  non-automatic (printed if `sudo` isn't cached).

This is the typed port of the `doctor.sh` logic below; the shell scripts remain
for standalone / no-orca / fresh-box use.

Build the plugin (a `[[bin]]` for the target box, spawned by orca's plugin loader):

```bash
cargo build --release                                       # host
cargo zigbuild --release --target x86_64-unknown-linux-gnu  # cross-compile for a Linux box
# install the resulting lib{raccoon}.so via orca's plugin install path
```

The plugin needs an orca daemon that provides the `diagnostics` domain
(≥ the release that adds it). See `docs/SETUP.md` for the per-check detail.

## Game-save backups

raccoon also contributes the `game-saves` backup KIND. Each instance is one game,
named the same on every host (`steam-<appid>`, `heroic-<prefix>`, `umu-<prefix>`,
`battlenet`, `native-<dir>`), so hosts sharing a backup target share each game's
history. Only save-bearing files are captured: Steam `userdata/<steamid>/<appid>`,
and in wine prefixes `AppData/{Roaming,Local,LocalLow}`, `Documents` and
`Saved Games`, minus caches. Restores are newest-wins: a newer local file is kept
and the incoming copy lands beside it as `<name>.orca-conflict-<stamp>`. A prefix
that doesn't exist yet on the restoring host is deferred, not fabricated.

Extra native save dirs (under `$HOME`) come from orca config:

```bash
orca config set game-saves native-paths '{"paths":["~/.local/share/factorio"]}'
```

## The pathway

| Layer | Tool | Notes |
|-------|------|-------|
| Steam + Proton | Steam (native) + GE-Proton | Base; Gaming Mode on Bazzite |
| Epic / GOG / Amazon | **Heroic** | Native (`legendary`/`gogdl`/`nile`); auto-adds games to Steam |
| Battle.net, EA, Ubisoft, … | **NonSteamLaunchers (NSL)** | Wine-only launchers; auto-adds to Steam |
| FPS/overlay | MangoHud | Hotkey overlay (Right‑Shift+F12) |
| Display (Gaming Mode) | gamescope | 4K@120 + expose high refresh to all games |
| Controllers | udev USB wake | Wake the box from sleep with a dongle/wired pad |

**Division of labor:** Heroic for Epic/GOG/Amazon (native, light, better);
NSL for launchers with no good native option (Battle.net, EA App, Ubisoft, …).
Both feed the Steam library, so everything ends up as tiles in Gaming Mode.

## Quick start

```bash
git clone https://github.com/argyle-labs/raccoon.git
cd raccoon
./bootstrap.sh            # desktop/dev base + gaming launchers + configs
                         #   --no-base skips the dev base; --timer adds the doctor timer
```

Then per-component (see [docs/SETUP.md](docs/SETUP.md) for the full runbook):

```bash
./scripts/setup-desktop.sh           # dev/desktop base: packages, paru, 1Password, gcloud, node, fonts
./scripts/setup-kde.sh               # practical KDE apps + fonts (Arch) — see docs/KDE.md
./scripts/install-blizzard.sh        # Battle.net (+ optional EA/Ubisoft) via NSL
./scripts/setup-controller-wake.sh   # wake from sleep via USB controller (needs sudo)
./scripts/setup-gamescope-refresh.sh # Gaming Mode: expose up to 120Hz to all games (Bazzite)
./scripts/cpu-mode.sh performance    # CPU power mode via orca power:cpu (performance|balanced|powersave)
```

## Diagnose & repair

`doctor.sh` compares this repo's known-good config against what's actually live
and reports drift as issues. Checks are distro-aware:

- **Cross-distro:** Steam present, Heroic, umu-launcher, GE-Proton, MangoHud
  config, ALSA audio headroom (crackle/dropout fix), CPU power mode vs the
  `power:cpu` orca setting, controller USB-wake, NSL game scanner, per-game locks.
- **Bazzite:** gamescope high-refresh env, Flathub remote configured.
- **CachyOS:** AUR helper (paru/yay) present, local btrfs snapshots (snapper/timeshift).

```bash
./doctor.sh              # human-readable report (read-only)
./doctor.sh --json       # machine-readable issues (shape mirrors orca's Issue type)
./doctor.sh --repair     # re-apply the safe, non-privileged fixes; prints the rest
./bootstrap.sh --timer   # run doctor daily via a systemd --user timer (logs to journal)
```

Exit code: `0` all-OK, `1` any WARN, `2` any CRIT. `--repair` never runs `sudo`
or restarts Steam — those fixes are printed with the exact command to run.

## Tune from in-game metrics

`tune.sh` reviews MangoHud frame logs and turns them into tuning findings —
stutter, GPU/CPU-bound, thermal, VRAM-exhaustion, uncapped fps — each with a
concrete tweak.

```bash
./tune.sh enable         # point MangoHud at a log folder + show live 1%/0.1% low
# play; toggle a capture during the rough patch with Shift_L+F2, then:
./tune.sh analyze        # analyze the newest log (or pass a FILE)
./tune.sh watch          # re-analyze the active log every few seconds (~live)
./tune.sh --json analyze # machine-readable findings (Issue shape)
```

Target fps is taken from your refresh cap (falls back to 60); override with
`TARGET_FPS=`. The overlay itself (Shift-R+F12) shows live 1% / 0.1% lows, so
stutter is visible in the moment; `tune.sh` explains *why* and what to change.

## Repo layout

```
bootstrap.sh                 # desktop base + gaming + configs (--no-base, --timer)
doctor.sh                    # drift check + repair for the gaming setup
tune.sh                      # review MangoHud metrics -> tuning suggestions
scripts/setup-desktop.sh     # dev/desktop base packages (paru, 1Password, gcloud, node, fonts)
scripts/setup-kde.sh         # practical KDE apps + fonts (Arch)
scripts/                     # individual, re-runnable setup scripts (gaming + system)
configs/                     # drop-in config files (env.d, MangoHud, udev, wireplumber)
systemd/                     # optional daily doctor timer (user)
docs/SETUP.md                # full setup + restore runbook (Bazzite + CachyOS)
docs/KDE.md                  # KDE apps reference
docs/NOTES.md                # field notes / gotchas (Battle.net, umu, gamescope)
```

## License

MIT — see [LICENSE](LICENSE).

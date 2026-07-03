<p align="center">
  <img src="assets/icon-256.png" width="120" alt="raccoon" />
</p>

# raccoon

Opinionated, reproducible setup for a Linux gaming machine — every launcher and
the system tweaks that make them work, in one place. Bring up a fresh box (or
restore an existing one) to the same known-good state.

**Tested distros:** [Bazzite](https://bazzite.gg) (Fedora atomic) and
[CachyOS](https://cachyos.org) (Arch). Structured so other distros slot in.

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
./bootstrap.sh            # detects distro, installs launchers, applies configs
```

Then per-component (see [docs/SETUP.md](docs/SETUP.md) for the full runbook):

```bash
./scripts/install-blizzard.sh        # Battle.net (+ optional EA/Ubisoft) via NSL
./scripts/setup-controller-wake.sh   # wake from sleep via USB controller (needs sudo)
./scripts/setup-gamescope-refresh.sh # Gaming Mode: expose up to 120Hz to all games (Bazzite)
```

## Diagnose & repair

`doctor.sh` compares this repo's known-good config against what's actually live
and reports drift as issues (gamescope refresh cap, MangoHud config, GE-Proton,
controller wake, NSL scanner, per-game locks):

```bash
./doctor.sh              # human-readable report (read-only)
./doctor.sh --json       # machine-readable issues (shape mirrors orca's Issue type)
./doctor.sh --repair     # re-apply the safe, non-privileged fixes; prints the rest
./bootstrap.sh --timer   # run doctor daily via a systemd --user timer (logs to journal)
```

Exit code: `0` all-OK, `1` any WARN, `2` any CRIT. `--repair` never runs `sudo`
or restarts Steam — those fixes are printed with the exact command to run.

## Repo layout

```
bootstrap.sh                 # distro-detecting installer + config applier (--timer)
doctor.sh                    # drift check + repair for the gaming setup
scripts/                     # individual, re-runnable setup scripts
configs/                     # drop-in config files (env.d, MangoHud, udev)
systemd/                     # optional daily doctor timer (user)
docs/SETUP.md                # full setup + restore runbook (Bazzite + CachyOS)
docs/NOTES.md                # field notes / gotchas (Battle.net, umu, gamescope)
```

## License

MIT — see [LICENSE](LICENSE).

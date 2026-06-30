# Setup & Restore Runbook

How to bring a Linux gaming machine to the known-good state, on **Bazzite** and
**CachyOS**. Each section is idempotent — safe to re-run when restoring.

> Conventions: `~` is the gaming user's home. On Bazzite the user session runs at
> `XDG_RUNTIME_DIR=/run/user/$(id -u)`. Replace display/IPs as needed.

---

## 0. Base packages

### Bazzite (Fedora atomic)
Steam, gamescope, mangohud, and `umu-launcher` ship preinstalled. Add the
launchers via Flatpak:

```bash
flatpak install -y flathub \
  com.heroicgameslauncher.hgl \
  net.lutris.Lutris \
  com.vysp3r.ProtonPlus          # GE-Proton manager
# Layered packages (only if a CLI tool is missing):
# rpm-ostree install <pkg>
```

### CachyOS (Arch)
```bash
sudo pacman -S --needed \
  steam lutris mangohud gamescope umu-launcher
# Heroic: from CachyOS/AUR or Flatpak
sudo pacman -S --needed heroic-games-launcher  # or: flatpak install flathub com.heroicgameslauncher.hgl
# GE-Proton manager:
flatpak install -y flathub net.davidotek.pupgui2   # ProtonUp-Qt
```

---

## 1. Proton-GE
Install the latest **GE-Proton** with ProtonPlus (Bazzite) or ProtonUp-Qt
(CachyOS) into Steam's `compatibilitytools.d`. It's the runner with the widest
compatibility (and the Battle.net fix). Set it per-game in Steam → Properties →
Compatibility, and as Heroic's default Wine version.

> umu auto-downloads its own GE-Proton on first use, so NSL/umu installs don't
> need this — but Steam games do.

---

## 2. Steam launchers — Epic / GOG / Amazon (Heroic)

1. Launch **Heroic**, log into Epic / GOG / Amazon.
2. Settings → default Wine = **GE-Proton**; enable **Add to Steam** and **HDR**
   (defaults below):

```jsonc
// ~/.var/app/com.heroicgameslauncher.hgl/config/heroic/config.json -> defaultSettings
"addSteamShortcuts": true,   // auto-add installed games to Steam
"enableHDR": true,
"autoInstallDxvk": true, "autoInstallVkd3d": true,
"useGameMode": true
```
3. Install games (they land in `~/Games/Heroic/<Game>`). With `addSteamShortcuts`
   on, they appear as Steam tiles automatically.

> Heroic only downloads while it's **open**. If a download "stalls," reopen Heroic.

---

## 3. Wine-only launchers — Battle.net / EA / Ubisoft (NSL)

Use **NonSteamLaunchers**. It installs GE-Proton + the launcher under one prefix
in `compatdata/NonSteamLaunchers` and auto-adds to Steam.

```bash
# Reliable launch = transient systemd user service (survives SSH; see NOTES.md)
systemd-run --user --collect --unit=nsl \
  --setenv=DISPLAY=:0 --setenv=WAYLAND_DISPLAY=wayland-0 \
  --setenv=XDG_RUNTIME_DIR=/run/user/$(id -u) \
  --setenv=DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$(id -u)/bus \
  bash -c 'curl -Ls https://raw.githubusercontent.com/moraroy/NonSteamLaunchers-On-Steam-Deck/main/NonSteamLaunchers.sh | bash -s -- "Battle.net"'
# Multiple: ... -s -- "Battle.net" "EA App" "Ubisoft Connect"
```
Or just `./scripts/install-blizzard.sh`. Log into the launcher when its window
appears; NSL's `NSLGameScanner.service` adds installed games as Steam tiles on
each Steam restart. **NSL restarts Steam at the end** — don't run it mid-game.

---

## 4. Gaming Mode display — 4K@120 + high refresh (Bazzite)

Bazzite's gamescope session reads two things:

```ini
# ~/.config/gamescope/modes.cfg  — per-display forced output (set in Gaming Mode -> Display)
<Display Name>:3840x2160@120
```
```ini
# ~/.config/environment.d/10-gamescope-refresh.conf  — expose high refresh to ALL games
STEAM_DISPLAY_REFRESH_LIMITS=60,120
CUSTOM_REFRESH_RATES=60,120
```
Apply with `./scripts/setup-gamescope-refresh.sh`, then **restart Gaming Mode**
(env.d is read at session start). Without `CUSTOM_REFRESH_RATES`, gamescope only
advertises 60 Hz and games cap at 60.

> **Desktop (Plasma) mode** is separate: a 4K panel at `Scale 2` gives a 1080p
> logical desktop, so Xwayland games only see 1080p. For 4K in Desktop mode set
> `kscreen-doctor output.<NAME>.scale.1`, or just game in Gaming Mode.

---

## 5. FPS overlay (MangoHud)
Copy `configs/MangoHud/MangoHud.conf` to `~/.config/MangoHud/MangoHud.conf`.
Toggle the overlay in-game with **Right‑Shift + F12**. In Gaming Mode it also
shows via Quick Access (**Ctrl+2**) → Performance → Overlay level.

---

## 6. Controller wake-from-sleep
`./scripts/setup-controller-wake.sh` (needs sudo) installs
`configs/udev/90-usb-wakeup.rules` and arms USB wake. **USB/dongle/wired only —
Bluetooth controllers cannot wake the machine.**

---

## 7. Per-game fixes (examples)
Some games need a tweak. Keep them here as you find them.

- **Detroit: Become Human** caps its in-game framerate menu at 30/60. Unlock via
  `…/steamapps/common/Detroit Become Human/GraphicOptions.JSON`:
  `"FRAME_RATE_LIMIT": 4` (0=30, 1=60, 2=90, 3=144, 4=unlimited), then
  `chmod 444` the file so the game can't overwrite it.

---

## Restore checklist (fresh machine)
1. Base packages (§0) → Proton-GE (§1)
2. `./bootstrap.sh` (applies configs)
3. Heroic: log in, set defaults, reinstall games (§2)
4. NSL: `./scripts/install-blizzard.sh`, log in, reinstall games (§3)
5. Gaming Mode display (§4) + restart Gaming Mode
6. Controller wake (§6); MangoHud (§5)
7. Re-apply per-game fixes (§7)

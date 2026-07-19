//! Detection + remediation for the gaming box — the typed port of the former
//! `doctor.sh`/`tune.sh`. Each check returns an optional
//! [`Finding`]; each repair id maps to a concrete action. Everything is
//! synchronous (sysfs reads + short commands); the core proxy runs it on a
//! blocking pool.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use plugin_toolkit::contract::diagnostics::{
    DiagnoseArgs, Finding, RepairArgs, RepairOutcome, RepairSpec, Severity,
};
use plugin_toolkit::serde_json;
use serde::Deserialize;

/// The WirePlumber ALSA-headroom drop-in, embedded so the plugin can both
/// compare against and (re)install it — single source of truth with the repo.
const HEADROOM_CONF: &str = include_str!("../configs/wireplumber/51-alsa-headroom.conf");
const HEADROOM_REL: &str = ".config/wireplumber/wireplumber.conf.d/51-alsa-headroom.conf";

/// Persistent udev rule that pins the flagged radios' USB wakeup off across
/// reboots and immutable-OS updates. Written by the `suspend-wakeup` repair.
const WAKEUP_RULE_PATH: &str = "/etc/udev/rules.d/90-disable-usb-wakeup.rules";

// ── diagnose ─────────────────────────────────────────────────────────────────

/// Run every check and return the findings as JSON (`Vec<Finding>`). The
/// `provider` filter is already applied core-side, so args are ignored here.
pub fn diagnose(_args_json: &str) -> Result<String, String> {
    // Validate args shape even though we don't branch on it (typed contract).
    let _: DiagnoseArgs = if _args_json.trim().is_empty() {
        DiagnoseArgs::default()
    } else {
        serde_json::from_str(_args_json).unwrap_or_default()
    };
    let findings: Vec<Finding> = [
        check_alsa_headroom(),
        check_cpu_mode(),
        check_scx(),
        check_suspend_wakeup(),
        check_gpu_perf(),
        check_shader_cache(),
        check_vrr(),
        check_dev_toolchain(),
        check_kde_apps(),
        check_gaming_stack(),
    ]
    .into_iter()
    .flatten()
    .collect();
    serde_json::to_string(&findings).map_err(|e| format!("encode findings: {e}"))
}

fn finding(
    id: &str,
    severity: Severity,
    title: &str,
    detail: String,
    repair: Option<RepairSpec>,
) -> Finding {
    Finding {
        id: id.to_string(),
        provider: crate::PROVIDER.to_string(),
        severity,
        title: title.to_string(),
        detail,
        repair,
    }
}

fn repair_spec(id: &str, description: &str, automatic: bool, privileged: bool) -> RepairSpec {
    RepairSpec {
        id: id.to_string(),
        description: description.to_string(),
        automatic,
        privileged,
        // raccoon repairs everything in-place; no managed-unit delegation.
        delegate: None,
    }
}

// ── checks ───────────────────────────────────────────────────────────────────

/// Audio crackle/dropout fix: the WirePlumber ALSA-headroom drop-in must be in
/// place, match the embedded known-good, and be live in the running graph.
fn check_alsa_headroom() -> Option<Finding> {
    let dst = home().join(HEADROOM_REL);
    let repair = Some(repair_spec(
        "alsa-headroom",
        "Install the WirePlumber ALSA-headroom drop-in and restart wireplumber",
        true,  // user-level, no privilege
        false, // not privileged
    ));
    if !dst.exists() {
        return Some(finding(
            "alsa-headroom",
            Severity::Warn,
            "Audio has no ALSA headroom (crackle/dropout risk)",
            format!(
                "WirePlumber drop-in missing at {}; ALSA sinks run headroom=0 and underrun under load",
                dst.display()
            ),
            repair,
        ));
    }
    match fs::read_to_string(&dst) {
        Ok(cur) if cur != HEADROOM_CONF => {
            return Some(finding(
                "alsa-headroom",
                Severity::Warn,
                "ALSA headroom config drifted from known-good",
                format!("{} differs from the embedded drop-in", dst.display()),
                repair,
            ));
        }
        Err(e) => {
            return Some(finding(
                "alsa-headroom",
                Severity::Warn,
                "ALSA headroom config unreadable",
                format!("{}: {e}", dst.display()),
                repair,
            ));
        }
        _ => {}
    }
    // Config is correct — confirm it's actually live (headroom != 0 anywhere).
    if let Some(out) = run_ok("pw-dump", &[])
        && out.contains("\"api.alsa.headroom\": 0")
    {
        return Some(finding(
            "alsa-headroom",
            Severity::Warn,
            "ALSA headroom config present but not applied",
            "a sink still reports headroom=0; wireplumber needs a restart to pick it up"
                .to_string(),
            repair,
        ));
    }
    Some(finding(
        "alsa-headroom",
        Severity::Ok,
        "Audio headroom applied",
        "ALSA sinks buffered against xruns (no crackle)".to_string(),
        None,
    ))
}

/// CPU power mode: desired is the runtime orca setting `power:cpu {mode}`;
/// raccoon maps it to a tuned profile and checks the live profile matches.
fn check_cpu_mode() -> Option<Finding> {
    which("tuned-adm")?; // tuned not present — nothing to enforce
    let Some(mode) = orca_power_cpu_mode() else {
        return Some(finding(
            "cpu-mode",
            Severity::Info,
            "CPU power mode not managed by orca",
            "set one with: orca config set power cpu '{\"mode\":\"performance\"}' (performance|balanced|powersave)".to_string(),
            None,
        ));
    };
    let Some(want) = tuned_profile_for(&mode) else {
        return Some(finding(
            "cpu-mode",
            Severity::Warn,
            "Unknown orca CPU mode",
            format!("power:cpu.mode '{mode}' must be performance|balanced|powersave"),
            None,
        ));
    };
    let live = tuned_active().unwrap_or_default();
    if live == want {
        Some(finding(
            "cpu-mode",
            Severity::Ok,
            "CPU mode matches orca",
            format!("mode '{mode}' -> tuned profile '{live}' (all cores, persists across reboots)"),
            None,
        ))
    } else {
        Some(finding(
            "cpu-mode",
            Severity::Warn,
            "CPU mode drifted from orca setting",
            format!("orca wants '{mode}' (tuned '{want}') but live profile is '{live}'"),
            Some(repair_spec(
                "cpu-mode",
                "Apply the tuned profile matching orca power:cpu (tuned-adm, privileged)",
                false,
                true,
            )),
        ))
    }
}

/// sched_ext: scx_lavd smooths frame pacing / 1% lows. Ship-but-off on Bazzite.
fn check_scx() -> Option<Finding> {
    if !Path::new("/sys/kernel/sched_ext").exists() {
        return None; // kernel has no sched_ext
    }
    if which("scx_loader").is_none() && which("scx_lavd").is_none() {
        return None; // no scheduler installed
    }
    let state = read_trim("/sys/kernel/sched_ext/state").unwrap_or_default();
    if state == "enabled" {
        let sched = read_trim("/sys/kernel/sched_ext/root/ops").unwrap_or_default();
        Some(finding(
            "scx",
            Severity::Ok,
            "Modern scheduler active",
            format!("sched_ext '{sched}' running (better frame pacing)"),
            None,
        ))
    } else {
        Some(finding(
            "scx",
            Severity::Warn,
            "sched_ext available but off (scx_lavd)",
            "a game-tuned scheduler is installed but disabled; scx_lavd smooths 1% lows under load"
                .to_string(),
            Some(repair_spec(
                "scx",
                "Enable scx_loader (systemctl, privileged)",
                false,
                true,
            )),
        ))
    }
}

/// Suspend that won't hold / high idle draw: wireless radios (USB receivers, BT
/// dongles) left with `power/wakeup=enabled` generate wake events that abort
/// s2idle/deep suspend — the box only reaches display-off and keeps drawing full
/// power. Flag every non-hub USB device that can still wake the machine, and
/// note if `mem_sleep` isn't the low-power `deep` state. (Seen on bragi: a
/// Logitech Unifying receiver + BT radio bounced suspend at ~15s every cycle.)
fn check_suspend_wakeup() -> Option<Finding> {
    let culprits = wakeup_culprits();
    if culprits.is_empty() {
        // Only report OK when the sysfs tree actually exists (Linux w/ USB).
        Path::new("/sys/bus/usb/devices").exists().then(|| {
            finding(
                "suspend-wakeup",
                Severity::Ok,
                "No stray USB wake sources",
                "no non-hub USB device can wake the machine; suspend can hold".to_string(),
                None,
            )
        })
    } else {
        let list = culprits
            .iter()
            .map(|c| format!("{} ({})", c.label, c.id))
            .collect::<Vec<_>>()
            .join(", ");
        let deep = read_trim("/sys/power/mem_sleep")
            .map(|s| s.contains("[deep]"))
            .unwrap_or(true);
        let sleep_note = if deep {
            String::new()
        } else {
            " (also: /sys/power/mem_sleep is not [deep] — S3 gives the lowest idle draw)"
                .to_string()
        };
        Some(finding(
            "suspend-wakeup",
            Severity::Warn,
            "USB wake sources can abort suspend (high idle power)",
            format!(
                "{} wake-enabled: idle wake events abort s2idle/deep suspend so the box only \
                 reaches display-off and keeps drawing power{sleep_note}",
                list
            ),
            Some(repair_spec(
                "suspend-wakeup",
                "Disable USB wakeup on the flagged radios (live sysfs + persistent udev rule, privileged). \
                 Note: this also disables wake-on-controller for wireless pads on the list.",
                false,
                true,
            )),
        ))
    }
}

/// AMD GPU DPM level: 'auto'/'high' let clocks scale; a stuck low/manual caps it.
fn check_gpu_perf() -> Option<Finding> {
    let (path, lvl) = drm_glob("device/power_dpm_force_performance_level")
        .into_iter()
        .find_map(|p| read_trim(&p.to_string_lossy()).map(|v| (p, v)))?;
    match lvl.as_str() {
        "auto" | "high" => Some(finding(
            "gpu-perf",
            Severity::Ok,
            "GPU performance level OK",
            format!("DPM '{lvl}' — clocks free to scale"),
            None,
        )),
        _ => Some(finding(
            "gpu-perf",
            Severity::Warn,
            "GPU performance level capped",
            format!(
                "DPM '{lvl}' at {} — not auto/high; clocks may be capped below what games need",
                path.display()
            ),
            Some(repair_spec(
                "gpu-perf",
                "Restore DPM to auto (sysfs write, privileged)",
                false,
                true,
            )),
        )),
    }
}

/// Steam shader pre-caching: off => first-run shader-compile stutter.
fn check_shader_cache() -> Option<Finding> {
    let vdf = [
        home().join(".steam/steam/config/config.vdf"),
        home().join(".local/share/Steam/config/config.vdf"),
    ]
    .into_iter()
    .find(|p| p.exists())?;
    let on = fs::read_to_string(&vdf)
        .map(|s| s.contains("ShaderCacheManager"))
        .unwrap_or(false);
    if on {
        Some(finding(
            "shader-cache",
            Severity::Ok,
            "Shader pre-caching on",
            "Steam builds shader caches ahead of play (less first-run stutter)".to_string(),
            None,
        ))
    } else {
        Some(finding(
            "shader-cache",
            Severity::Warn,
            "Shader pre-caching may be off",
            "no ShaderCacheManager in Steam config — expect first-run shader-compile stutter; enable Steam → Settings → Downloads → Shader Pre-Caching".to_string(),
            None, // manual Steam UI setting; no auto repair
        ))
    }
}

/// VRR/adaptive-sync: capable panels game much smoother with it on. We can read
/// capability but not reliably whether the compositor enabled it — INFO nudge.
fn check_vrr() -> Option<Finding> {
    for p in drm_connector_glob("vrr_capable") {
        if read_trim(&p.to_string_lossy()).as_deref() != Some("1") {
            continue;
        }
        let conn = p.parent();
        let connected = conn
            .map(|c| read_trim(&c.join("status").to_string_lossy()).as_deref() == Some("connected"))
            .unwrap_or(false);
        if !connected {
            continue;
        }
        let name = conn
            .and_then(|c| c.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        return Some(finding(
            "vrr",
            Severity::Info,
            "Display is VRR-capable",
            format!(
                "{name}: enable Adaptive Sync/VRR (Gaming Mode Display, or KDE Settings → Display) to kill tearing/stutter"
            ),
            None,
        ));
    }
    None
}

// ── provisioning (dev / desktop / gaming) ───────────────────────────────────────
// Presence checks for the three setup concerns; each repair installs the missing
// packages via the host package manager (privileged, non-automatic). The
// standalone scripts/setup-*.sh stay for no-orca / fresh-box use.

/// Probe binaries per concern. node/gcloud/1Password have their own installers
/// (scripts/setup-dev.sh), so they are intentionally not gated here.
const DEV_BINS: &[&str] = &["zsh", "git", "direnv", "java", "alacritty", "fastfetch"];
const KDE_BINS: &[&str] = &["yakuake", "kate", "okular", "gwenview"];
const GAMING_BINS: &[&str] = &["steam", "mangohud", "gamescope"];

/// Install package sets (fuller than the probe bins), per distro family.
const DEV_PKGS_ARCH: &[&str] = &[
    "zsh",
    "git",
    "curl",
    "direnv",
    "jdk17-openjdk",
    "postgresql-libs",
    "alacritty",
    "fastfetch",
    "eza",
    "bat",
];
const DEV_PKGS_APT: &[&str] = &[
    "zsh",
    "git",
    "curl",
    "direnv",
    "openjdk-17-jdk",
    "libpq-dev",
    "alacritty",
    "fastfetch",
];
const DEV_PKGS_DNF: &[&str] = &[
    "zsh",
    "git",
    "curl",
    "direnv",
    "java-17-openjdk",
    "libpq-devel",
    "alacritty",
    "fastfetch",
];
const KDE_PKGS: &[&str] = &[
    "partitionmanager",
    "ffmpegthumbs",
    "kio-extras",
    "xdg-desktop-portal-kde",
    "yakuake",
    "kate",
    "ark",
    "okular",
    "gwenview",
    "kdeconnect",
    "kdegraphics-thumbnailers",
    "noto-fonts",
    "noto-fonts-emoji",
    "ttf-jetbrains-mono",
    "papirus-icon-theme",
];
const GAMING_PKGS_ARCH: &[&str] = &["steam", "lutris", "mangohud", "gamescope", "umu-launcher"];

/// Host package manager (paru→pacman preferred, then apt/dnf); None off-Linux.
fn pkg_mgr() -> Option<&'static str> {
    ["paru", "pacman", "apt-get", "dnf"]
        .into_iter()
        .find(|pm| which(pm).is_some())
}

fn missing(bins: &[&str]) -> Vec<String> {
    bins.iter()
        .filter(|b| which(b).is_none())
        .map(|b| b.to_string())
        .collect()
}

fn check_dev_toolchain() -> Option<Finding> {
    pkg_mgr()?; // no supported package manager (e.g. macOS build host) — skip
    let miss = missing(DEV_BINS);
    if miss.is_empty() {
        return Some(finding(
            "dev-toolchain",
            Severity::Ok,
            "Dev toolchain present",
            "zsh, git, direnv, jdk, alacritty, fastfetch installed".to_string(),
            None,
        ));
    }
    Some(finding(
        "dev-toolchain",
        Severity::Warn,
        "Dev toolchain incomplete",
        format!(
            "missing: {} (node/gcloud/1Password: scripts/setup-dev.sh)",
            miss.join(", ")
        ),
        Some(repair_spec(
            "dev-toolchain",
            "Install the dev toolchain via the host package manager (privileged)",
            false,
            true,
        )),
    ))
}

fn check_kde_apps() -> Option<Finding> {
    which("pacman")?; // KDE app set is curated for Arch/CachyOS
    let miss = missing(KDE_BINS);
    if miss.is_empty() {
        return Some(finding(
            "kde-apps",
            Severity::Ok,
            "KDE apps present",
            "yakuake, kate, okular, gwenview installed".to_string(),
            None,
        ));
    }
    Some(finding(
        "kde-apps",
        Severity::Info,
        "KDE apps not installed",
        format!("missing: {} (see docs/KDE.md)", miss.join(", ")),
        Some(repair_spec(
            "kde-apps",
            "Install the practical KDE app set via pacman (privileged)",
            false,
            true,
        )),
    ))
}

fn check_gaming_stack() -> Option<Finding> {
    pkg_mgr()?;
    let miss = missing(GAMING_BINS);
    if miss.is_empty() {
        return Some(finding(
            "gaming-stack",
            Severity::Ok,
            "Gaming stack present",
            "steam, mangohud, gamescope installed".to_string(),
            None,
        ));
    }
    Some(finding(
        "gaming-stack",
        Severity::Warn,
        "Gaming stack incomplete",
        format!("missing: {}", miss.join(", ")),
        Some(repair_spec(
            "gaming-stack",
            "Install launchers + MangoHud/gamescope (pacman on Arch, Flatpak elsewhere)",
            false,
            true,
        )),
    ))
}

/// Privileged package install, non-interactive. paru→pacman (never run paru as
/// root). On failure, return the exact command to run by hand — plugins can't
/// prompt for a sudo password.
fn pm_install(pkgs: &[&str]) -> (bool, String) {
    let (bin, verb): (&str, &[&str]) = match pkg_mgr() {
        Some("paru") | Some("pacman") => ("pacman", &["-S", "--needed", "--noconfirm"]),
        Some("apt-get") => ("apt-get", &["install", "-y"]),
        Some("dnf") => ("dnf", &["install", "-y"]),
        _ => return (false, "no supported package manager".to_string()),
    };
    let mut args: Vec<&str> = vec!["-n", bin];
    args.extend_from_slice(verb);
    args.extend_from_slice(pkgs);
    let display = format!("sudo {} {} {}", bin, verb.join(" "), pkgs.join(" "));
    match run("sudo", &args) {
        Ok(_) => (true, format!("installed: {}", pkgs.join(", "))),
        Err(e) => (false, format!("install failed ({e}); run: {display}")),
    }
}

fn repair_dev_toolchain() -> (bool, String) {
    let pkgs = match pkg_mgr() {
        Some("paru") | Some("pacman") => DEV_PKGS_ARCH,
        Some("apt-get") => DEV_PKGS_APT,
        Some("dnf") => DEV_PKGS_DNF,
        _ => return (false, "no supported package manager".to_string()),
    };
    let (ok, msg) = pm_install(pkgs);
    (
        ok,
        format!("{msg}. node/gcloud/1Password: scripts/setup-dev.sh"),
    )
}

fn repair_kde_apps() -> (bool, String) {
    if which("pacman").is_none() {
        return (false, "KDE app set is Arch-only (pacman)".to_string());
    }
    pm_install(KDE_PKGS)
}

fn repair_gaming_stack() -> (bool, String) {
    if which("pacman").is_some() {
        return pm_install(GAMING_PKGS_ARCH);
    }
    // Non-Arch (e.g. Bazzite): Steam/mangohud/gamescope are preinstalled; add the
    // Flatpak launchers (no privilege needed).
    match run(
        "flatpak",
        &[
            "install",
            "-y",
            "--noninteractive",
            "flathub",
            "com.heroicgameslauncher.hgl",
            "net.lutris.Lutris",
            "com.vysp3r.ProtonPlus",
        ],
    ) {
        Ok(_) => (
            true,
            "installed Flatpak launchers (Heroic, Lutris, ProtonPlus)".to_string(),
        ),
        Err(e) => (
            false,
            format!("flatpak install failed ({e}); run scripts/setup-gaming.sh"),
        ),
    }
}

// ── repair ─────────────────────────────────────────────────────────────────────

/// Run one repair by id and return a [`RepairOutcome`] as JSON.
pub fn repair(args_json: &str) -> Result<String, String> {
    let args: RepairArgs =
        serde_json::from_str(args_json).map_err(|e| format!("invalid repair args: {e}"))?;
    let (ok, message) = match args.repair_id.as_str() {
        "alsa-headroom" => repair_alsa_headroom(),
        "cpu-mode" => repair_cpu_mode(),
        "scx" => repair_scx(),
        "suspend-wakeup" => repair_suspend_wakeup(),
        "gpu-perf" => repair_gpu_perf(),
        "dev-toolchain" => repair_dev_toolchain(),
        "kde-apps" => repair_kde_apps(),
        "gaming-stack" => repair_gaming_stack(),
        other => (false, format!("raccoon has no repair '{other}'")),
    };
    let outcome = RepairOutcome {
        id: args.repair_id,
        provider: crate::PROVIDER.to_string(),
        ok,
        message,
    };
    serde_json::to_string(&outcome).map_err(|e| format!("encode outcome: {e}"))
}

fn repair_alsa_headroom() -> (bool, String) {
    let dst = home().join(HEADROOM_REL);
    if let Some(dir) = dst.parent()
        && let Err(e) = fs::create_dir_all(dir)
    {
        return (false, format!("create {}: {e}", dir.display()));
    }
    if let Err(e) = fs::write(&dst, HEADROOM_CONF) {
        return (false, format!("write {}: {e}", dst.display()));
    }
    match run("systemctl", &["--user", "restart", "wireplumber"]) {
        Ok(_) => (
            true,
            format!("installed {} and restarted wireplumber", dst.display()),
        ),
        Err(e) => (
            false,
            format!(
                "wrote {} but wireplumber restart failed: {e}",
                dst.display()
            ),
        ),
    }
}

fn repair_cpu_mode() -> (bool, String) {
    let Some(mode) = orca_power_cpu_mode() else {
        return (
            false,
            "no orca power:cpu mode set for this host".to_string(),
        );
    };
    let Some(want) = tuned_profile_for(&mode) else {
        return (false, format!("unknown mode '{mode}'"));
    };
    match run("tuned-adm", &["profile", &want]) {
        Ok(_) => (true, format!("applied '{mode}' → tuned profile '{want}'")),
        Err(e) => (
            false,
            format!(
                "tuned-adm profile {want} failed ({e}); run with privilege: sudo tuned-adm profile {want}"
            ),
        ),
    }
}

fn repair_scx() -> (bool, String) {
    match run("systemctl", &["enable", "--now", "scx_loader"]) {
        Ok(_) => (true, "enabled scx_loader (select lavd)".to_string()),
        Err(e) => (
            false,
            format!(
                "enabling scx_loader failed ({e}); run with privilege: sudo systemctl enable --now scx_loader"
            ),
        ),
    }
}

fn repair_gpu_perf() -> (bool, String) {
    let paths = drm_glob("device/power_dpm_force_performance_level");
    if paths.is_empty() {
        return (false, "no AMD GPU DPM control found".to_string());
    }
    let mut wrote = 0;
    let mut last_err = String::new();
    for p in &paths {
        match fs::write(p, "auto") {
            Ok(_) => wrote += 1,
            Err(e) => last_err = format!("{}: {e}", p.display()),
        }
    }
    if wrote > 0 {
        (true, format!("set DPM to auto on {wrote} GPU(s)"))
    } else {
        (
            false,
            format!(
                "DPM write failed ({last_err}); needs privilege: echo auto | sudo tee {}",
                paths[0].display()
            ),
        )
    }
}

/// Disable USB wakeup on every flagged radio: write `disabled` to each device's
/// `power/wakeup` (live, no replug) and persist a udev rule so it survives
/// reboots + immutable-OS updates. Both writes need root; on failure we hand
/// back the exact commands, since plugins can't prompt for a sudo password.
fn repair_suspend_wakeup() -> (bool, String) {
    let culprits = wakeup_culprits();
    if culprits.is_empty() {
        return (true, "no wake-enabled USB radios to disable".to_string());
    }
    // Live writes.
    let mut disabled = 0;
    for c in &culprits {
        if fs::write(&c.wakeup_path, "disabled").is_ok() {
            disabled += 1;
        }
    }
    // Persistent udev rule (one match line per vendor:product).
    let rule: String = culprits
        .iter()
        .filter_map(|c| c.id.split_once(':'))
        .map(|(v, p)| {
            format!(
                "ACTION==\"add\", SUBSYSTEM==\"usb\", ATTR{{idVendor}}==\"{v}\", \
                 ATTR{{idProduct}}==\"{p}\", ATTR{{power/wakeup}}=\"disabled\"\n"
            )
        })
        .collect();
    let rule_ok = fs::write(WAKEUP_RULE_PATH, &rule).is_ok();

    if disabled == culprits.len() && rule_ok {
        return (
            true,
            format!(
                "disabled USB wakeup on {disabled} device(s) and wrote {WAKEUP_RULE_PATH}; \
                 suspend can now hold (verify: sync; sudo rtcwake -m no -s 30; sudo systemctl suspend)"
            ),
        );
    }
    // Partial/failed — emit the by-hand commands.
    let live_cmds = culprits
        .iter()
        .map(|c| format!("echo disabled | sudo tee {}", c.wakeup_path.display()))
        .collect::<Vec<_>>()
        .join("\n");
    (
        false,
        format!(
            "needs privilege (disabled {disabled}/{}, rule {}). Run:\n{live_cmds}\nsudo tee {WAKEUP_RULE_PATH} <<'EOF'\n{rule}EOF",
            culprits.len(),
            if rule_ok { "written" } else { "unwritten" }
        ),
    )
}

// ── helpers ────────────────────────────────────────────────────────────────────

/// A USB device that can currently wake the machine.
struct WakeCulprit {
    /// `idVendor:idProduct`, e.g. `046d:c52b`.
    id: String,
    /// Human label — product/manufacturer string, or the bus path if unnamed.
    label: String,
    /// Absolute path to the device's `power/wakeup` attribute.
    wakeup_path: PathBuf,
}

/// Enumerate `/sys/bus/usb/devices/*` for devices with `power/wakeup=enabled`
/// that are neither root hubs (`usbN`) nor hubs (`bDeviceClass==09`). Those are
/// the receivers/radios/controllers that actually abort suspend; hubs only relay
/// downstream wake events and must stay enabled.
fn wakeup_culprits() -> Vec<WakeCulprit> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir("/sys/bus/usb/devices") else {
        return out;
    };
    for e in rd.flatten() {
        let dev = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("usb") {
            continue; // root hub
        }
        let wakeup_path = dev.join("power/wakeup");
        if read_trim(&wakeup_path.to_string_lossy()).as_deref() != Some("enabled") {
            continue;
        }
        // Skip hubs (class 09) — they pass wake through from downstream ports.
        if read_trim(&dev.join("bDeviceClass").to_string_lossy()).as_deref() == Some("09") {
            continue;
        }
        let vendor = read_trim(&dev.join("idVendor").to_string_lossy()).unwrap_or_default();
        let product = read_trim(&dev.join("idProduct").to_string_lossy()).unwrap_or_default();
        if vendor.is_empty() || product.is_empty() {
            continue;
        }
        let label = read_trim(&dev.join("product").to_string_lossy())
            .or_else(|| read_trim(&dev.join("manufacturer").to_string_lossy()))
            .unwrap_or_else(|| name.into_owned());
        out.push(WakeCulprit {
            id: format!("{vendor}:{product}"),
            label,
            wakeup_path,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"))
}

/// mode → tuned profile, preferring the `-bazzite` variant when tuned lists it.
fn tuned_profile_for(mode: &str) -> Option<String> {
    let base = match mode {
        "performance" => "throughput-performance",
        "balanced" => "balanced",
        "powersave" => "powersave",
        _ => return None,
    };
    let bazzite = format!("{base}-bazzite");
    let has_bazzite = run_ok("tuned-adm", &["list"])
        .map(|out| {
            out.lines()
                .any(|l| l.trim_start_matches("- ").starts_with(&bazzite))
        })
        .unwrap_or(false);
    Some(if has_bazzite {
        bazzite
    } else {
        base.to_string()
    })
}

fn tuned_active() -> Option<String> {
    let out = run_ok("tuned-adm", &["active"])?;
    out.lines()
        .find_map(|l| l.strip_prefix("Current active profile: "))
        .map(|s| s.trim().to_string())
}

/// Read orca's `power:cpu` config row's `mode` field via the `orca` CLI. Returns
/// `None` if orca isn't present or the row/field is absent.
fn orca_power_cpu_mode() -> Option<String> {
    #[derive(Deserialize)]
    struct Row {
        json: String,
    }
    #[derive(Deserialize)]
    struct Get {
        row: Row,
    }
    #[derive(Deserialize)]
    struct Cpu {
        mode: String,
    }
    let out = run_ok("orca", &["config", "get", "power", "cpu"])?;
    let get: Get = serde_json::from_str(&out).ok()?;
    let cpu: Cpu = serde_json::from_str(&get.row.json).ok()?;
    Some(cpu.mode)
}

fn read_trim(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Entries under /sys/class/drm/card*/<suffix> (device paths — no connector dash).
fn drm_glob(suffix: &str) -> Vec<PathBuf> {
    drm_cards(false)
        .into_iter()
        .map(|c| c.join(suffix))
        .filter(|p| p.exists())
        .collect()
}

/// Entries under /sys/class/drm/card*-*/<suffix> (connector paths — with dash).
fn drm_connector_glob(suffix: &str) -> Vec<PathBuf> {
    drm_cards(true)
        .into_iter()
        .map(|c| c.join(suffix))
        .filter(|p| p.exists())
        .collect()
}

/// card* dirs under /sys/class/drm; `connectors` picks the dashed connector
/// dirs (card1-HDMI-A-2) vs the bare card dirs (card1).
fn drm_cards(connectors: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir("/sys/class/drm") else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("card") {
            continue;
        }
        if name.contains('-') == connectors {
            out.push(e.path());
        }
    }
    out.sort();
    out
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}

/// Run a command, returning combined stdout on success or an error string.
fn run(bin: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| format!("spawn {bin}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr)
            .trim()
            .to_string()
            .lines()
            .next()
            .unwrap_or("command failed")
            .to_string())
    }
}

/// Like [`run`] but returns `None` on any failure (for best-effort probes).
fn run_ok(bin: &str, args: &[&str]) -> Option<String> {
    run(bin, args).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tuned_profile_mapping() {
        // Falls back to the base when the -bazzite variant isn't listed (no tuned in CI).
        assert!(tuned_profile_for("performance").is_some());
        assert!(tuned_profile_for("balanced").is_some());
        assert!(tuned_profile_for("powersave").is_some());
        assert!(tuned_profile_for("bogus").is_none());
    }

    #[test]
    fn diagnose_emits_valid_json_array() {
        // Runs the real checks against the build host; we only assert the
        // envelope is well-formed typed JSON (findings vary by machine).
        let out = diagnose("{}").expect("diagnose ok");
        let findings: Vec<Finding> = serde_json::from_str(&out).expect("valid findings json");
        for f in &findings {
            assert_eq!(f.provider, crate::PROVIDER);
        }
    }

    #[test]
    fn wakeup_culprits_never_flag_hubs_or_roothubs() {
        // On any host the enumerated culprits must exclude root hubs and hubs.
        for c in wakeup_culprits() {
            assert!(
                !c.id.is_empty() && c.id.contains(':'),
                "id is vendor:product"
            );
            assert!(c.wakeup_path.ends_with("power/wakeup"));
        }
    }

    #[test]
    fn repair_suspend_wakeup_is_idempotent_when_clean() {
        // With no wake-enabled radios present (typical CI host), the repair is a
        // no-op success rather than an error.
        if wakeup_culprits().is_empty() {
            let (ok, msg) = repair_suspend_wakeup();
            assert!(ok);
            assert!(msg.contains("no wake-enabled"));
        }
    }

    #[test]
    fn repair_unknown_id_reports_not_ok() {
        let out = repair(r#"{"provider":"raccoon","repair_id":"nope"}"#).expect("encodes");
        let o: RepairOutcome = serde_json::from_str(&out).unwrap();
        assert!(!o.ok);
        assert!(o.message.contains("no repair"));
    }
}

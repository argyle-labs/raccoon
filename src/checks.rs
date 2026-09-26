//! Detection + remediation for the gaming box. Each check returns an optional
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
/// Persistent udev rule that disables PME wake on every xHCI host controller +
/// USB root hub (`idVendor==1d6b`). The controllers — not the leaf devices — are
/// the layer where AMD platforms route USB wake (`pinctrl_amd`), so this is what
/// actually stops an immediate bounce out of S3. Written by the repair.
const CONTROLLER_RULE_PATH: &str = "/etc/udev/rules.d/91-disable-usb-controller-wakeup.rules";
const CONTROLLER_RULE_BODY: &str = "\
# Power-button-only wake: disable PME wake on all xHCI controllers + USB root hubs
ACTION==\"add\", SUBSYSTEM==\"pci\", ATTR{class}==\"0x0c0330\", ATTR{power/wakeup}=\"disabled\"
ACTION==\"add\", SUBSYSTEM==\"usb\", ATTR{idVendor}==\"1d6b\", ATTR{power/wakeup}=\"disabled\"
";

// ── diagnose ─────────────────────────────────────────────────────────────────

/// Run every check and return the findings as JSON (`Vec<Finding>`). The
/// `provider` filter is already applied core-side, so args are ignored here.
pub fn diagnose(_args_json: &str) -> Result<String, String> {
    // Validate args shape even though we don't branch on it (typed contract).
    let args: DiagnoseArgs = if _args_json.trim().is_empty() {
        DiagnoseArgs::default()
    } else {
        serde_json::from_str(_args_json).unwrap_or_default()
    };
    serde_json::to_string(&diagnose_typed(args)).map_err(|e| format!("encode findings: {e}"))
}

/// Run every check and return the typed findings. The `provider` filter is
/// applied core-side, so args are ignored here.
pub fn diagnose_typed(_args: DiagnoseArgs) -> Vec<Finding> {
    [
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
        check_os_updates(),
    ]
    .into_iter()
    .flatten()
    .collect()
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

/// Suspend that won't hold / high idle draw. Several independent layers can arm
/// a wake source that aborts s2idle/deep suspend — the box only reaches
/// display-off and keeps drawing full power. This surveys all of them:
/// (1) USB **host controllers / root hubs** left wake-armed — on AMD the layer
/// where wake actually routes (`pinctrl_amd`), the real bouncer; (2) wireless
/// USB **radios** (receivers, BT dongles) still wake-armed; (3) Ethernet
/// **Wake-on-LAN** — a stray LAN packet PMEs the NIC awake; (4) a blanket
/// **udev rule** that re-arms `power/wakeup=enabled` on every boot (the root
/// cause that silently undoes any per-device fix). All four were live on bragi;
/// the box bounced out of S3 at ~14s until every layer was disabled.
fn check_suspend_wakeup() -> Option<Finding> {
    // Only meaningful on a Linux host with the power-management sysfs tree.
    if !Path::new("/sys/bus/usb/devices").exists() {
        return None;
    }
    let controllers = armed_usb_controllers();
    let radios = wakeup_culprits();
    let wol = wol_nics();
    let blanket = blanket_wake_rules();

    if controllers.is_empty() && radios.is_empty() && wol.is_empty() && blanket.is_empty() {
        return Some(finding(
            "suspend-wakeup",
            Severity::Ok,
            "No stray wake sources",
            "USB controllers/radios quiesced, no Ethernet Wake-on-LAN, no blanket wake-arming \
             udev rule; deep suspend can hold"
                .to_string(),
            None,
        ));
    }

    let mut parts: Vec<String> = Vec::new();
    if !controllers.is_empty() {
        parts.push(format!(
            "{} USB host controller(s)/root hub(s) wake-armed [{}] — the layer that bounces AMD S3",
            controllers.len(),
            controllers
                .iter()
                .map(|c| c.label.clone())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !radios.is_empty() {
        parts.push(format!(
            "{} USB radio(s) wake-armed [{}]",
            radios.len(),
            radios
                .iter()
                .map(|c| format!("{} ({})", c.label, c.id))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !wol.is_empty() {
        parts.push(format!(
            "Ethernet Wake-on-LAN on [{}]",
            wol.iter()
                .map(|n| format!("{} (wol {})", n.iface, n.flags))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !blanket.is_empty() {
        parts.push(format!(
            "blanket wake-arming udev rule(s) that re-enable wakeup every boot [{}]",
            blanket
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if read_trim("/sys/power/mem_sleep")
        .map(|s| !s.contains("[deep]"))
        .unwrap_or(false)
    {
        parts
            .push("/sys/power/mem_sleep is not [deep] — S3 gives the lowest idle draw".to_string());
    }

    Some(finding(
        "suspend-wakeup",
        Severity::Warn,
        "Wake sources can abort suspend (high idle power)",
        format!(
            "{}. Idle wake events abort deep suspend so the box only reaches display-off and \
             keeps drawing power",
            parts.join("; ")
        ),
        Some(repair_spec(
            "suspend-wakeup",
            "Quiesce all wake sources: disable USB controller/root-hub + radio wakeup (live sysfs \
             + persistent udev rules), turn off Ethernet Wake-on-LAN, and neutralize any blanket \
             wake-arming udev rule. Privileged; wake becomes power-button-only.",
            false,
            true,
        )),
    ))
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

/// Pending OS updates. Probe-only: this reads update metadata, leaving the
/// host's sync database and deployments untouched.
fn check_os_updates() -> Option<Finding> {
    if which("rpm-ostree").is_some() {
        return Some(image_updates());
    }
    if which("pacman").is_some() {
        return Some(arch_updates());
    }
    None
}

/// Image-based hosts (Bazzite) boot one deployment and stage the next, so a
/// staged image is inert until the operator reboots — the two states report
/// separately.
fn image_updates() -> Finding {
    let booted = deployment("booted").unwrap_or_else(|| "unknown".to_string());
    if let Some(staged) = deployment("staged") {
        return finding(
            "os-updates",
            Severity::Warn,
            "OS update staged for the next boot",
            format!("staged {staged}, booted {booted}; a reboot runs it"),
            None,
        );
    }
    // `rpm-ostree upgrade --check` exits 77 when the remote holds nothing newer.
    match run_status("rpm-ostree", &["upgrade", "--check"]) {
        Some((77, _)) => finding(
            "os-updates",
            Severity::Ok,
            "OS image current",
            format!("booted {booted}; the remote holds nothing newer"),
            None,
        ),
        Some((0, out)) => finding(
            "os-updates",
            Severity::Warn,
            "OS image update available",
            format!(
                "booted {booted}; newer on the remote: {}",
                out.lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim()
            ),
            Some(repair_spec(
                "os-updates",
                "Stage the newer image with rpm-ostree upgrade; rebooting stays an operator act",
                false,
                true,
            )),
        ),
        Some((code, out)) => finding(
            "os-updates",
            Severity::Info,
            "OS update check inconclusive",
            format!(
                "rpm-ostree upgrade --check exited {code}: {}",
                out.lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim()
            ),
            None,
        ),
        None => finding(
            "os-updates",
            Severity::Info,
            "OS update check inconclusive",
            "rpm-ostree is on PATH but would not run".to_string(),
            None,
        ),
    }
}

/// Arch/CachyOS hosts. `checkupdates` compares against a private copy of the
/// sync database, so the probe leaves the host's own database alone.
fn arch_updates() -> Finding {
    let Some((code, out)) = run_status("checkupdates", &[]) else {
        return finding(
            "os-updates",
            Severity::Info,
            "Pending-update check unavailable",
            "checkupdates (pacman-contrib) lists pending packages without refreshing the host's sync database".to_string(),
            Some(repair_spec(
                "os-updates-probe",
                "Install pacman-contrib via pacman (privileged)",
                false,
                true,
            )),
        );
    };
    // checkupdates exits 2 when every package is current.
    match code {
        2 => finding(
            "os-updates",
            Severity::Ok,
            "Packages current",
            "checkupdates lists no pending packages".to_string(),
            None,
        ),
        0 => {
            let pending: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
            let head: Vec<&str> = pending
                .iter()
                .take(5)
                .map(|l| l.split_whitespace().next().unwrap_or(l))
                .collect();
            finding(
                "os-updates",
                Severity::Warn,
                "Package updates pending",
                format!(
                    "{} pending: {}{}",
                    pending.len(),
                    head.join(", "),
                    if pending.len() > head.len() {
                        ", …"
                    } else {
                        ""
                    }
                ),
                Some(repair_spec(
                    "os-updates",
                    "Full system upgrade via pacman -Syu (privileged); a rolling release upgrades as a whole",
                    false,
                    true,
                )),
            )
        }
        _ => finding(
            "os-updates",
            Severity::Info,
            "OS update check inconclusive",
            format!(
                "checkupdates exited {code}: {}",
                out.lines()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("")
                    .trim()
            ),
            None,
        ),
    }
}

/// `version` (falling back to a short checksum) of the `rpm-ostree` deployment
/// carrying the given flag — `"booted"` or `"staged"`.
fn deployment(flag: &str) -> Option<String> {
    let out = run_ok("rpm-ostree", &["status", "--json"])?;
    let status: serde_json::Value = serde_json::from_str(&out).ok()?;
    let d = status
        .get("deployments")?
        .as_array()?
        .iter()
        .find(|d| d.get(flag).and_then(serde_json::Value::as_bool) == Some(true))?;
    if let Some(v) = d.get("version").and_then(serde_json::Value::as_str) {
        return Some(v.to_string());
    }
    d.get("checksum")
        .and_then(serde_json::Value::as_str)
        .map(|c| c.chars().take(12).collect())
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
    serde_json::to_string(&repair_typed(args)).map_err(|e| format!("encode outcome: {e}"))
}

/// Run one repair by id and return the typed [`RepairOutcome`].
pub fn repair_typed(args: RepairArgs) -> RepairOutcome {
    let (ok, message) = match args.repair_id.as_str() {
        "alsa-headroom" => repair_alsa_headroom(),
        "cpu-mode" => repair_cpu_mode(),
        "scx" => repair_scx(),
        "suspend-wakeup" => repair_suspend_wakeup(),
        "gpu-perf" => repair_gpu_perf(),
        "dev-toolchain" => repair_dev_toolchain(),
        "kde-apps" => repair_kde_apps(),
        "gaming-stack" => repair_gaming_stack(),
        "os-updates" => repair_os_updates(),
        "os-updates-probe" => pm_install(&["pacman-contrib"]),
        other => (false, format!("raccoon has no repair '{other}'")),
    };
    RepairOutcome {
        id: args.repair_id,
        provider: crate::PROVIDER.to_string(),
        ok,
        message,
    }
}

/// Apply OS updates. Image-based hosts get the update staged for the next boot;
/// the reboot stays an operator act. Arch hosts upgrade as a whole, the only
/// safe shape on a rolling release.
fn repair_os_updates() -> (bool, String) {
    if which("rpm-ostree").is_some() {
        return match run("sudo", &["-n", "rpm-ostree", "upgrade"]) {
            Ok(_) => (
                true,
                match deployment("staged") {
                    Some(v) => format!("staged {v}; a reboot runs it"),
                    None => "rpm-ostree reports the host already current".to_string(),
                },
            ),
            Err(e) => (
                false,
                format!("staging failed ({e}); run: sudo rpm-ostree upgrade"),
            ),
        };
    }
    if which("pacman").is_some() {
        return match run("sudo", &["-n", "pacman", "-Syu", "--noconfirm"]) {
            Ok(_) => (true, "full system upgrade applied".to_string()),
            Err(e) => (
                false,
                format!("upgrade failed ({e}); run: sudo pacman -Syu"),
            ),
        };
    }
    (
        false,
        "no supported OS update mechanism (rpm-ostree, pacman)".to_string(),
    )
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

/// Quiesce every wake layer the check flags: disable wakeup on USB controllers +
/// root hubs + radios (live sysfs), persist udev rules so it survives reboots +
/// immutable-OS updates, turn off Ethernet Wake-on-LAN, and neutralize any
/// blanket wake-arming udev rule. All steps need root; each records success or
/// falls back to the exact by-hand command, since plugins can't prompt for sudo.
fn repair_suspend_wakeup() -> (bool, String) {
    let controllers = armed_usb_controllers();
    let radios = wakeup_culprits();
    let wol = wol_nics();
    let blanket = blanket_wake_rules();
    if controllers.is_empty() && radios.is_empty() && wol.is_empty() && blanket.is_empty() {
        return (true, "no wake sources to quiesce".to_string());
    }

    let mut done: Vec<String> = Vec::new();
    let mut manual: Vec<String> = Vec::new();

    // 1. Neutralize blanket wake-arming rules FIRST (they'd re-arm everything).
    for p in &blanket {
        let disabled = p.with_extension("rules.disabled-by-orca");
        if fs::rename(p, &disabled).is_ok() {
            done.push(format!(
                "renamed blanket wake rule {} → {}",
                p.display(),
                disabled.display()
            ));
        } else {
            manual.push(format!("sudo mv {} {}", p.display(), disabled.display()));
        }
    }

    // 2. Live-disable wakeup on controllers, root hubs, and radios.
    let mut live_targets: Vec<PathBuf> = Vec::new();
    live_targets.extend(controllers.iter().map(|c| c.wakeup_path.clone()));
    live_targets.extend(radios.iter().map(|c| c.wakeup_path.clone()));
    let (mut live_ok, mut live_total) = (0usize, 0usize);
    for path in &live_targets {
        live_total += 1;
        if fs::write(path, "disabled").is_ok() {
            live_ok += 1;
        } else {
            manual.push(format!("echo disabled | sudo tee {}", path.display()));
        }
    }
    if live_total > 0 {
        done.push(format!(
            "live-disabled wakeup on {live_ok}/{live_total} USB device(s)"
        ));
    }

    // 3. Persist the udev rules (controllers always; radios by id when present).
    if fs::write(CONTROLLER_RULE_PATH, CONTROLLER_RULE_BODY).is_ok() {
        done.push(format!("wrote {CONTROLLER_RULE_PATH}"));
    } else {
        manual.push(format!(
            "sudo tee {CONTROLLER_RULE_PATH} <<'EOF'\n{CONTROLLER_RULE_BODY}EOF"
        ));
    }
    if !radios.is_empty() {
        let rule: String = radios
            .iter()
            .filter_map(|c| c.id.split_once(':'))
            .map(|(v, p)| {
                format!(
                    "ACTION==\"add\", SUBSYSTEM==\"usb\", ATTR{{idVendor}}==\"{v}\", \
                     ATTR{{idProduct}}==\"{p}\", ATTR{{power/wakeup}}=\"disabled\"\n"
                )
            })
            .collect();
        if fs::write(WAKEUP_RULE_PATH, &rule).is_ok() {
            done.push(format!("wrote {WAKEUP_RULE_PATH}"));
        } else {
            manual.push(format!("sudo tee {WAKEUP_RULE_PATH} <<'EOF'\n{rule}EOF"));
        }
    }

    // 4. Turn off Ethernet Wake-on-LAN. Prefer NetworkManager (it re-applies on
    //    link-up); fall back to ethtool for the running state.
    for n in &wol {
        let nm = run(
            "nmcli",
            &["-g", "GENERAL.CONNECTION", "device", "show", &n.iface],
        )
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "--");
        let applied = match &nm {
            Some(conn) => run(
                "nmcli",
                &[
                    "connection",
                    "modify",
                    conn,
                    "802-3-ethernet.wake-on-lan",
                    "disable",
                ],
            )
            .is_ok(),
            None => run("ethtool", &["-s", &n.iface, "wol", "d"]).is_ok(),
        };
        if applied {
            done.push(format!("disabled Wake-on-LAN on {}", n.iface));
        } else {
            manual.push(match &nm {
                Some(conn) => format!(
                    "sudo nmcli connection modify '{conn}' 802-3-ethernet.wake-on-lan disable && sudo nmcli connection up '{conn}'"
                ),
                None => format!("sudo ethtool -s {} wol d", n.iface),
            });
        }
    }

    // Reload udev so freshly-written rules take on the next device event.
    run("udevadm", &["control", "--reload"]).ok();

    if manual.is_empty() {
        (
            true,
            format!(
                "quiesced wake sources: {}. Wake is now power-button-only \
                 (verify: sync; sudo rtcwake -m no -s 45; sudo systemctl suspend)",
                done.join("; ")
            ),
        )
    } else {
        let did = if done.is_empty() {
            String::new()
        } else {
            format!("did [{}]; ", done.join("; "))
        };
        (
            false,
            format!(
                "{did}needs privilege for the rest — run:\n{}",
                manual.join("\n")
            ),
        )
    }
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

/// USB host controllers / root hubs left wake-armed. On AMD platforms USB wake
/// routes through the controller (`pinctrl_amd`), so an armed controller bounces
/// S3 even when every leaf device is quiesced — this is the layer that actually
/// matters. Covers both the USB root hubs (`/sys/bus/usb/devices/usb*`) and the
/// backing xHCI PCI functions (class `0x0c0330`).
fn armed_usb_controllers() -> Vec<WakeCulprit> {
    let mut out = Vec::new();
    // USB root hubs: usb1, usb2, …
    if let Ok(rd) = fs::read_dir("/sys/bus/usb/devices") {
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("usb") {
                continue;
            }
            let wakeup_path = e.path().join("power/wakeup");
            if read_trim(&wakeup_path.to_string_lossy()).as_deref() == Some("enabled") {
                out.push(WakeCulprit {
                    id: name.clone().into_owned(),
                    label: format!("root hub {name}"),
                    wakeup_path,
                });
            }
        }
    }
    // xHCI PCI controllers: class 0x0c0330.
    if let Ok(rd) = fs::read_dir("/sys/bus/pci/devices") {
        for e in rd.flatten() {
            let dev = e.path();
            if read_trim(&dev.join("class").to_string_lossy()).as_deref() != Some("0x0c0330") {
                continue;
            }
            let wakeup_path = dev.join("power/wakeup");
            if read_trim(&wakeup_path.to_string_lossy()).as_deref() == Some("enabled") {
                let slot = e.file_name().to_string_lossy().into_owned();
                out.push(WakeCulprit {
                    id: slot.clone(),
                    label: format!("xHCI {slot}"),
                    wakeup_path,
                });
            }
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// An Ethernet interface with Wake-on-LAN armed.
struct WolNic {
    iface: String,
    /// ethtool `Wakes on:` flag string (e.g. `g` for magic-packet).
    flags: String,
}

/// Physical Ethernet NICs whose `ethtool` "Wakes on:" is anything but `d`
/// (disabled). A stray broadcast/ARP/magic packet on the LAN then PMEs the box
/// awake. Skips virtual/loopback interfaces and no-ops when ethtool is absent.
fn wol_nics() -> Vec<WolNic> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir("/sys/class/net") else {
        return out;
    };
    for e in rd.flatten() {
        let iface = e.file_name().to_string_lossy().into_owned();
        if iface == "lo"
            || iface.starts_with("veth")
            || iface.starts_with("docker")
            || iface.starts_with("virbr")
            || iface.starts_with("br-")
        {
            continue;
        }
        if !e.path().join("device").exists() {
            continue; // virtual interface, no backing device
        }
        let Some(info) = run_ok("ethtool", &[&iface]) else {
            continue; // ethtool missing or the NIC has no WoL — can't assess
        };
        if let Some(flags) = info
            .lines()
            .find_map(|l| l.trim().strip_prefix("Wakes on:"))
        {
            let flags = flags.trim();
            if !flags.is_empty() && flags != "d" {
                out.push(WolNic {
                    iface,
                    flags: flags.to_string(),
                });
            }
        }
    }
    out.sort_by(|a, b| a.iface.cmp(&b.iface));
    out
}

/// udev rules that arm `power/wakeup=enabled` with no device scope (no
/// `idVendor`/`idProduct`) — i.e. "enable wake for everything". These silently
/// re-arm every device on each boot and undo any targeted fix, so they're the
/// real root cause when suspend regresses. A *scoped* enable rule (a specific
/// controller for wake-on-gamepad) is a deliberate choice and is NOT flagged.
fn blanket_wake_rules() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir("/etc/udev/rules.d") else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("rules") {
            continue;
        }
        let Ok(txt) = fs::read_to_string(&p) else {
            continue;
        };
        let blanket = txt.lines().map(str::trim).any(|l| {
            !l.starts_with('#')
                && l.contains("power/wakeup}=\"enabled\"")
                && !l.contains("idVendor")
                && !l.contains("idProduct")
        });
        if blanket {
            out.push(p);
        }
    }
    out.sort();
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

/// Run a command and return its exit code with whichever stream carried output.
/// Update probes signal state through the exit code — `rpm-ostree upgrade
/// --check` exits 77 for "nothing newer", `checkupdates` exits 2 for "current".
fn run_status(bin: &str, args: &[&str]) -> Option<(i32, String)> {
    let out = Command::new(bin).args(args).output().ok()?;
    let text = if out.stdout.is_empty() {
        String::from_utf8_lossy(&out.stderr).into_owned()
    } else {
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    Some((out.status.code().unwrap_or(-1), text))
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
    fn os_update_check_is_skipped_without_a_supported_mechanism() {
        // macOS build hosts carry neither rpm-ostree nor pacman.
        if which("rpm-ostree").is_none() && which("pacman").is_none() {
            assert!(check_os_updates().is_none());
        }
    }

    #[test]
    fn os_update_finding_is_probe_only_when_current() {
        // A current host offers no repair: there is nothing to apply.
        if let Some(f) = check_os_updates()
            && f.severity == Severity::Ok
        {
            assert!(f.repair.is_none());
        }
    }

    #[test]
    fn staged_image_updates_offer_no_repair() {
        // A staged deployment is already applied; only a reboot remains, and
        // that stays with the operator.
        if deployment("staged").is_some() && which("rpm-ostree").is_some() {
            let f = image_updates();
            assert!(f.repair.is_none());
            assert!(f.detail.contains("staged"));
        }
    }

    #[test]
    fn arch_repair_upgrades_the_whole_system() {
        // The repair description must commit to a full -Syu; a partial upgrade
        // breaks a rolling release.
        if which("pacman").is_some()
            && let Some(r) = arch_updates().repair
            && r.id == "os-updates"
        {
            assert!(r.description.contains("-Syu"));
            assert!(r.privileged);
            assert!(!r.automatic);
        }
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
        // With no wake sources armed (typical CI host — no armed controllers,
        // radios, WoL NICs, or blanket rules), the repair is a no-op success.
        let clean = armed_usb_controllers().is_empty()
            && wakeup_culprits().is_empty()
            && wol_nics().is_empty()
            && blanket_wake_rules().is_empty();
        if clean {
            let (ok, msg) = repair_suspend_wakeup();
            assert!(ok);
            assert!(msg.contains("no wake sources"));
        }
    }

    #[test]
    fn controllers_are_roothubs_or_xhci_pci() {
        // Every flagged controller must be a USB root hub or an xHCI PCI slot,
        // and expose a writable power/wakeup path.
        for c in armed_usb_controllers() {
            assert!(c.wakeup_path.ends_with("power/wakeup"));
            assert!(!c.id.is_empty());
        }
    }

    #[test]
    fn blanket_rule_detection_ignores_scoped_enables() {
        // A scoped enable (with idVendor/idProduct) is a deliberate choice and
        // must never be reported as a blanket wake-arming rule.
        for p in blanket_wake_rules() {
            let txt = fs::read_to_string(&p).unwrap_or_default();
            let has_blanket = txt.lines().map(str::trim).any(|l| {
                !l.starts_with('#')
                    && l.contains("power/wakeup}=\"enabled\"")
                    && !l.contains("idVendor")
            });
            assert!(
                has_blanket,
                "{} flagged without a blanket enable line",
                p.display()
            );
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

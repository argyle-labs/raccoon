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
        check_gpu_perf(),
        check_shader_cache(),
        check_vrr(),
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
            format!("WirePlumber drop-in missing at {}; ALSA sinks run headroom=0 and underrun under load", dst.display()),
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
            "a sink still reports headroom=0; wireplumber needs a restart to pick it up".to_string(),
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
            "a game-tuned scheduler is installed but disabled; scx_lavd smooths 1% lows under load".to_string(),
            Some(repair_spec(
                "scx",
                "Enable scx_loader (systemctl, privileged)",
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
            format!("DPM '{lvl}' at {} — not auto/high; clocks may be capped below what games need", path.display()),
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
            format!("{name}: enable Adaptive Sync/VRR (Gaming Mode Display, or KDE Settings → Display) to kill tearing/stutter"),
            None,
        ));
    }
    None
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
        "gpu-perf" => repair_gpu_perf(),
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
            format!("wrote {} but wireplumber restart failed: {e}", dst.display()),
        ),
    }
}

fn repair_cpu_mode() -> (bool, String) {
    let Some(mode) = orca_power_cpu_mode() else {
        return (false, "no orca power:cpu mode set for this host".to_string());
    };
    let Some(want) = tuned_profile_for(&mode) else {
        return (false, format!("unknown mode '{mode}'"));
    };
    match run("tuned-adm", &["profile", &want]) {
        Ok(_) => (true, format!("applied '{mode}' → tuned profile '{want}'")),
        Err(e) => (
            false,
            format!("tuned-adm profile {want} failed ({e}); run with privilege: sudo tuned-adm profile {want}"),
        ),
    }
}

fn repair_scx() -> (bool, String) {
    match run("systemctl", &["enable", "--now", "scx_loader"]) {
        Ok(_) => (true, "enabled scx_loader (select lavd)".to_string()),
        Err(e) => (
            false,
            format!("enabling scx_loader failed ({e}); run with privilege: sudo systemctl enable --now scx_loader"),
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
            format!("DPM write failed ({last_err}); needs privilege: echo auto | sudo tee {}", paths[0].display()),
        )
    }
}

// ── helpers ────────────────────────────────────────────────────────────────────

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
        .map(|out| out.lines().any(|l| l.trim_start_matches("- ").starts_with(&bazzite)))
        .unwrap_or(false);
    Some(if has_bazzite { bazzite } else { base.to_string() })
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
    fn repair_unknown_id_reports_not_ok() {
        let out = repair(r#"{"provider":"raccoon","repair_id":"nope"}"#).expect("encodes");
        let o: RepairOutcome = serde_json::from_str(&out).unwrap();
        assert!(!o.ok);
        assert!(o.message.contains("no repair"));
    }
}

//! The `game-saves` backup KIND: one instance per game with saves on this host,
//! named identically on every host (see [`crate::saves::sources`]) so that
//! hosts sharing a backup pool share each game's history. Runs as the daemon's
//! user — everything it touches lives under `$HOME`.
//!
//! Layout is the default flat `[kind, instance]` with no host segment: the
//! writer host is recorded in the payload's manifest instead.

use std::path::Path;

use plugin_toolkit::backup::BackupKindPlugin;
use plugin_toolkit::contract::backup::BackupOutcome;
use plugin_toolkit::serde_json;
use serde::Deserialize;

use crate::saves::manifest::{Manifest, capture};
use crate::saves::{restore, sources};

pub const KIND: &str = "game-saves";

pub struct GameSavesKind;

impl BackupKindPlugin for GameSavesKind {
    fn kind(&self) -> &str {
        KIND
    }

    fn title(&self) -> String {
        "Game saves".to_string()
    }

    fn instances(&self) -> Result<Vec<String>, String> {
        Ok(sources::instances(&env()?))
    }

    fn backup(&self, payload_dir: &Path, instance: &str) -> Result<BackupOutcome, String> {
        backup_in(&env()?, &hostname(), payload_dir, instance)
    }

    fn restore(&self, payload_dir: &Path, instance: &str) -> Result<(), String> {
        let report = restore_in(&env()?, payload_dir, instance)?;
        let summary = format!(
            "[game-saves] restore {instance}: {} written, {} unchanged, {} conflict(s), {} deferred",
            report.written,
            report.unchanged,
            report.conflicts.len(),
            report.deferred.len()
        );
        if report.conflicts.is_empty() && report.deferred.is_empty() {
            plugin_toolkit::tracing::info!("{summary}");
        } else {
            plugin_toolkit::tracing::warn!(
                "{summary}; conflicts={:?} deferred={:?}",
                report.conflicts,
                report.deferred
            );
        }
        if report.errors.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "{summary}; {} file(s) failed: {}",
                report.errors.len(),
                report.errors.join("; ")
            ))
        }
    }
}

/// Capture `instance`'s saves on this host into `payload_dir`.
pub fn backup_in(
    env: &sources::Env,
    host: &str,
    payload_dir: &Path,
    instance: &str,
) -> Result<BackupOutcome, String> {
    let parts = sources::games(env).remove(instance).unwrap_or_default();
    if parts.is_empty() {
        return Err(format!("no saves for `{instance}` on this host"));
    }
    let (manifest, checksum, stats) = capture(&parts, payload_dir, instance, host)?;
    let mut note = format!(
        "{instance} from {host}: {} file(s), {} bytes across {}",
        stats.files,
        stats.bytes,
        manifest
            .parts
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    );
    if !stats.skipped.is_empty() {
        note.push_str(&format!("; skipped {} unreadable", stats.skipped.len()));
        plugin_toolkit::tracing::warn!("[game-saves] {instance}: skipped {:?}", stats.skipped);
    }
    Ok(BackupOutcome {
        checksum: Some(format!("sha256:{checksum}")),
        note: Some(note),
    })
}

/// Restore `instance` from `payload_dir` (possibly written by another host).
/// The game need not be known locally: parts whose home doesn't exist here are
/// deferred rather than fabricated.
pub fn restore_in(
    env: &sources::Env,
    payload_dir: &Path,
    instance: &str,
) -> Result<restore::RestoreReport, String> {
    let manifest = Manifest::read(payload_dir)?;
    if manifest.instance != instance {
        return Err(format!(
            "payload holds `{}`, not `{instance}`",
            manifest.instance
        ));
    }
    let local = sources::games(env).remove(instance).unwrap_or_default();
    let stamp = plugin_toolkit::time::now().compact();
    Ok(restore::restore(payload_dir, &manifest, &stamp, |part| {
        sources::resolve(env, instance, part, &local)
    }))
}

fn env() -> Result<sources::Env, String> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .ok_or("HOME is not set")?;
    Ok(sources::Env {
        home: home.into(),
        native_paths: native_paths(),
    })
}

/// `paths` from orca's `game-saves:native-paths` config row; empty when the row
/// or orca is absent.
fn native_paths() -> Vec<String> {
    #[derive(Deserialize)]
    struct Row {
        json: String,
    }
    #[derive(Deserialize)]
    struct Get {
        row: Row,
    }
    #[derive(Deserialize)]
    struct NativePaths {
        paths: Vec<String>,
    }
    crate::checks::run_ok("orca", &["config", "get", "game-saves", "native-paths"])
        .and_then(|out| serde_json::from_str::<Get>(&out).ok())
        .and_then(|get| serde_json::from_str::<NativePaths>(&get.row.json).ok())
        .map(|n| n.paths)
        .unwrap_or_default()
}

/// Read without a subprocess (the plugin may be forked from the daemon).
fn hostname() -> String {
    ["/proc/sys/kernel/hostname", "/etc/hostname"]
        .iter()
        .find_map(|p| {
            std::fs::read_to_string(p)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saves::manifest::mtime_ns;
    use crate::saves::sources::PREFIX_DEFERRED;
    use crate::saves::testutil::{TempDir, set_mtime_secs, write};
    use std::fs;

    fn env(home: &Path) -> sources::Env {
        sources::Env {
            home: home.to_path_buf(),
            native_paths: Vec::new(),
        }
    }

    #[test]
    fn bragi_backup_restores_onto_hemlock_without_the_prefix() {
        let t = TempDir::new();
        let bragi = t.path().join("bragi/home/skey");
        let hemlock = t.path().join("hemlock/home/skey");
        let steam = ".local/share/Steam";
        write(
            &bragi.join(format!("{steam}/userdata/111/440/remote/cloud.sav")),
            "cloud",
        );
        set_mtime_secs(
            &bragi.join(format!("{steam}/userdata/111/440/remote/cloud.sav")),
            5_000,
        );
        write(
            &bragi.join(format!(
                "{steam}/steamapps/compatdata/440/pfx/drive_c/users/steamuser/AppData/Roaming/G/s.sav"
            )),
            "prefix-save",
        );
        write(
            &bragi.join(format!(
                "{steam}/steamapps/compatdata/440/pfx/drive_c/windows/system32/k.dll"
            )),
            "dll",
        );
        // hemlock: same Steam account signed in, game never launched there.
        fs::create_dir_all(hemlock.join(format!("{steam}/userdata/111"))).unwrap();

        let payload = t.path().join("pool/game-saves/steam-440/1/payload");
        fs::create_dir_all(&payload).unwrap();
        let out = backup_in(&env(&bragi), "bragi", &payload, "steam-440").unwrap();
        assert!(out.checksum.unwrap().starts_with("sha256:"));
        let note = out.note.unwrap();
        assert!(note.contains("steam-440 from bragi: 2 file(s)"), "{note}");
        assert_eq!(Manifest::read(&payload).unwrap().host, "bragi");
        assert!(
            !payload.join("files/proton/windows").exists()
                && !payload.join("files/proton/drive_c").exists()
        );

        let he = env(&hemlock);
        assert!(!sources::instances(&he).contains(&"steam-440".to_string()));
        let r = restore_in(&he, &payload, "steam-440").unwrap();
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(r.written, 1);
        assert_eq!(r.deferred, vec![format!("proton: {PREFIX_DEFERRED}")]);
        let restored = hemlock.join(format!("{steam}/userdata/111/440/remote/cloud.sav"));
        assert_eq!(fs::read_to_string(&restored).unwrap(), "cloud");
        assert_eq!(mtime_ns(&restored).unwrap(), 5_000 * 1_000_000_000);
        assert!(
            !hemlock
                .join(format!("{steam}/steamapps/compatdata/440"))
                .exists()
        );

        // Restoring the same payload again is a no-op.
        let r = restore_in(&he, &payload, "steam-440").unwrap();
        assert_eq!((r.written, r.unchanged), (0, 1));
    }

    #[test]
    fn backup_of_a_game_absent_here_fails_and_wrong_payload_is_refused() {
        let t = TempDir::new();
        let payload = t.path().join("payload");
        fs::create_dir_all(&payload).unwrap();
        assert!(backup_in(&env(t.path()), "h", &payload, "steam-1").is_err());

        let home = t.path().join("home");
        write(
            &home.join("Games/Battlenet/drive_c/users/steamuser/AppData/Roaming/b"),
            "b",
        );
        backup_in(&env(&home), "h", &payload, "battlenet").unwrap();
        assert!(restore_in(&env(&home), &payload, "umu-x").is_err());
    }

    #[test]
    fn kind_dispatches_through_the_toolkit_seam() {
        let v = plugin_toolkit::backup::dispatch_kind_op(
            &GameSavesKind,
            "layout",
            serde_json::json!({"instance": "steam-440"}),
        )
        .unwrap();
        assert_eq!(v, serde_json::json!(["game-saves", "steam-440"]));
        let v = plugin_toolkit::backup::dispatch_kind_op(
            &GameSavesKind,
            "title",
            serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(v, serde_json::json!("Game saves"));
    }
}

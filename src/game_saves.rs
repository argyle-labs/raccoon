//! The `game-saves` backup KIND: one instance per game with saves on this host,
//! named from its ludusavi title ([`game_id`]) so every host sharing a backup
//! pool files the same game under the same instance. Runs as the daemon's user
//! — everything it touches lives under `$HOME`.
//!
//! Layout is the default flat `[kind, instance]` with no host segment: the
//! writer host is recorded in the payload's manifest instead.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use plugin_toolkit::backup::BackupKindPlugin;
use plugin_toolkit::contract::backup::BackupOutcome;
use plugin_toolkit::serde_json;
use serde::Deserialize;

use crate::saves::layout::{HOME, Layout, STEAM_COMMON, STEAM_USERDATA};
use crate::saves::ludusavi::{CustomGame, Ludusavi, Preview, Scanner};
use crate::saves::manifest::{FileEntry, Manifest, capture};
use crate::saves::{game_id, restore, select};

pub const KIND: &str = "game-saves";

/// Restoring into a prefix ludusavi didn't find for this game would mean
/// guessing (or fabricating) one, which breaks Proton/Heroic's own prefix
/// initialization on first launch.
pub const PREFIX_DEFERRED: &str = "prefix not initialized";

/// A full scan is reused this long, so `instances` followed by one `backup`
/// per game doesn't rescan the whole library each time.
const SCAN_TTL: Duration = Duration::from_secs(120);
static SCAN_CACHE: Mutex<Option<(Instant, Preview)>> = Mutex::new(None);

pub struct GameSavesKind;

impl BackupKindPlugin for GameSavesKind {
    fn kind(&self) -> &str {
        KIND
    }

    fn title(&self) -> String {
        "Game saves".to_string()
    }

    fn instances(&self) -> Result<Vec<String>, String> {
        let (layout, scanner) = host()?;
        instances_in(&layout, &scanner)
    }

    fn backup(&self, payload_dir: &Path, instance: &str) -> Result<BackupOutcome, String> {
        let (layout, scanner) = host()?;
        backup_in(&layout, &scanner, &hostname(), payload_dir, instance)
    }

    fn restore(&self, payload_dir: &Path, instance: &str) -> Result<(), String> {
        let (layout, scanner) = host()?;
        let report = restore_in(&layout, &scanner, payload_dir, instance)?;
        if report.written > 0 || !report.conflicts.is_empty() {
            invalidate_scan();
        }
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

/// Game ids with at least one save file (after orca's exclusions).
pub fn instances_in(layout: &Layout, scanner: &dyn Scanner) -> Result<Vec<String>, String> {
    let scan = scanner.preview(None)?;
    let mut ids: Vec<(String, &str)> = Vec::new();
    for title in scan.games.keys() {
        if select::select(layout, scan.files(title)).files.is_empty() {
            continue;
        }
        let id = game_id(title);
        if let Some((_, first)) = ids.iter().find(|(i, _)| *i == id) {
            plugin_toolkit::tracing::warn!(
                "[game-saves] `{title}` and `{first}` share instance `{id}`; only `{first}` is backed up"
            );
            continue;
        }
        ids.push((id, title));
    }
    Ok(ids.into_iter().map(|(id, _)| id).collect())
}

/// Capture `instance`'s saves on this host into `payload_dir`.
pub fn backup_in(
    layout: &Layout,
    scanner: &dyn Scanner,
    host: &str,
    payload_dir: &Path,
    instance: &str,
) -> Result<BackupOutcome, String> {
    let scan = scanner.preview(None)?;
    let title = scan
        .games
        .keys()
        .find(|t| game_id(t) == instance)
        .ok_or_else(|| format!("no saves for `{instance}` on this host"))?;
    // A fresh per-title scan: the cached full scan may predate new saves.
    let sel = select::select(layout, scanner.preview(Some(title))?.files(title));
    if sel.files.is_empty() {
        return Err(format!("no saves for `{instance}` on this host"));
    }
    let (manifest, checksum, stats) = capture(&sel.files, payload_dir, instance, title, host)?;
    let mut note = format!(
        "{title} ({instance}) from {host}: {} file(s), {} bytes across {}",
        stats.files,
        stats.bytes,
        manifest
            .parts
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    );
    let skipped = sel.skipped.len() + stats.skipped.len();
    if skipped > 0 {
        note.push_str(&format!("; skipped {skipped}"));
        plugin_toolkit::tracing::info!(
            "[game-saves] {instance}: skipped {:?} {:?}",
            sel.skipped,
            stats.skipped
        );
    }
    Ok(BackupOutcome {
        checksum: Some(format!("sha256:{checksum}")),
        note: Some(note),
    })
}

/// Restore `instance` from `payload_dir`, possibly written by another host for
/// a game this host has never backed up. Each part goes where ludusavi finds
/// the game here; parts with no safe local home are deferred.
pub fn restore_in(
    layout: &Layout,
    scanner: &dyn Scanner,
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
    let local = scanner.preview(Some(&manifest.title))?;
    let anchors = select::select(layout, local.files(&manifest.title)).anchors;
    let stamp = plugin_toolkit::time::now().compact();
    Ok(restore::restore(payload_dir, &manifest, &stamp, |part| {
        let found: Vec<&PathBuf> = anchors.get(part).into_iter().flatten().collect();
        match found.as_slice() {
            [one] => return Ok((*one).clone()),
            [] => {}
            many => return Err(format!("{} local locations; not guessing", many.len())),
        }
        let entries: &[FileEntry] = manifest.parts.get(part).map_or(&[], Vec::as_slice);
        fallback_anchor(layout, part, entries)
    }))
}

/// Where a part goes when ludusavi found no local saves for the game.
fn fallback_anchor(layout: &Layout, part: &str, entries: &[FileEntry]) -> Result<PathBuf, String> {
    match part {
        HOME => Ok(layout.home.clone()),
        STEAM_USERDATA => layout
            .sole_steam_user()
            .ok_or_else(|| "no single signed-in Steam account on this host".to_string()),
        STEAM_COMMON => {
            let dir = entries
                .first()
                .and_then(|e| e.relpath.split('/').next())
                .unwrap_or_default();
            layout
                .steam_common_with(dir)
                .ok_or_else(|| format!("`{dir}` not installed on this host"))
        }
        _ => Err(PREFIX_DEFERRED.to_string()),
    }
}

/// [`Ludusavi`] with full scans cached for [`SCAN_TTL`].
struct CachedScanner(Ludusavi);

impl Scanner for CachedScanner {
    fn preview(&self, title: Option<&str>) -> Result<Preview, String> {
        if title.is_some() {
            return self.0.preview(title);
        }
        let mut cache = SCAN_CACHE.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((at, scan)) = cache.as_ref()
            && at.elapsed() < SCAN_TTL
        {
            return Ok(scan.clone());
        }
        let scan = self.0.preview(None)?;
        *cache = Some((Instant::now(), scan.clone()));
        Ok(scan)
    }
}

fn invalidate_scan() {
    *SCAN_CACHE.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

fn host() -> Result<(Layout, CachedScanner), String> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .ok_or("HOME is not set")?;
    let layout = Layout::detect(Path::new(&home));
    let ludusavi = Ludusavi::prepare(&layout, &custom_games())?;
    Ok((layout, CachedScanner(ludusavi)))
}

/// Custom games from orca's `game-saves:native-paths` config row:
/// `{"games": [{"name": "...", "files": ["~/..."]}], "paths": ["~/..."]}`.
/// Each bare `paths` entry becomes a game named after its last segment.
fn custom_games() -> Vec<CustomGame> {
    #[derive(Deserialize)]
    struct Row {
        json: String,
    }
    #[derive(Deserialize)]
    struct Get {
        row: Row,
    }
    #[derive(Deserialize, Default)]
    struct NativePaths {
        #[serde(default)]
        games: Vec<CustomGame>,
        #[serde(default)]
        paths: Vec<String>,
    }
    let cfg = crate::checks::run_ok("orca", &["config", "get", "game-saves", "native-paths"])
        .and_then(|out| serde_json::from_str::<Get>(&out).ok())
        .and_then(|get| serde_json::from_str::<NativePaths>(&get.row.json).ok())
        .unwrap_or_default();
    let mut games = cfg.games;
    for p in cfg.paths {
        if let Some(name) = p.trim_end_matches('/').rsplit('/').next()
            && !name.is_empty()
        {
            games.push(CustomGame {
                name: name.to_string(),
                files: vec![p],
            });
        }
    }
    games
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
    use crate::saves::testutil::{TempDir, set_mtime_secs, write};
    use std::fs;

    /// Serves a real-schema ludusavi preview fixture rooted at `home`.
    struct Fixture(Preview);

    impl Fixture {
        fn new(raw: &str, home: &Path) -> Self {
            let raw = raw.replace("{HOME}", &home.display().to_string());
            Self(Preview::parse(&raw).unwrap())
        }

        /// Create every non-ignored file the fixture lists.
        fn materialize(&self) {
            for g in self.0.games.values() {
                for (p, f) in &g.files {
                    if !f.ignored {
                        write(Path::new(p), p);
                    }
                }
            }
        }
    }

    impl Scanner for Fixture {
        fn preview(&self, title: Option<&str>) -> Result<Preview, String> {
            let mut p = self.0.clone();
            if let Some(t) = title {
                p.games.retain(|k, _| k == t);
            }
            Ok(p)
        }
    }

    const BRAGI: &str = include_str!("saves/fixtures/bragi-preview.json");
    const HEMLOCK: &str = include_str!("saves/fixtures/hemlock-preview.json");

    struct Hosts {
        _t: TempDir,
        bragi: (Layout, Fixture),
        hemlock: (Layout, Fixture),
        pool: PathBuf,
    }

    fn hosts() -> Hosts {
        let t = TempDir::new();
        let b = t.path().join("bragi/var/home/skey");
        let h = t.path().join("hemlock/home/skey");
        fs::create_dir_all(b.join(".local/share/Steam/userdata/64751656")).unwrap();
        fs::create_dir_all(h.join(".local/share/Steam/userdata/64751656")).unwrap();
        fs::create_dir_all(h.join(".local/share/Steam/steamapps/compatdata/0")).unwrap();
        let bf = Fixture::new(BRAGI, &b);
        bf.materialize();
        let hf = Fixture::new(HEMLOCK, &h);
        hf.materialize();
        let pool = t.path().join("pool");
        Hosts {
            bragi: (Layout::detect(&b), bf),
            hemlock: (Layout::detect(&h), hf),
            pool,
            _t: t,
        }
    }

    fn slot(pool: &Path, instance: &str) -> PathBuf {
        let p = pool.join(KIND).join(instance).join("payload");
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn instances_are_title_ids_with_real_saves() {
        let hs = hosts();
        let (l, f) = &hs.bragi;
        let mut got = instances_in(l, f).unwrap();
        got.sort();
        let bg3 = game_id("Baldur's Gate 3");
        let mut want = vec![
            bg3.as_str(),
            "diablo-iv",
            "elden-ring",
            "hades",
            "stardew-valley",
        ];
        want.sort();
        assert_eq!(got, want);
        let (l, f) = &hs.hemlock;
        assert_eq!(instances_in(l, f).unwrap(), vec!["hades"]);
    }

    #[test]
    fn backup_captures_portable_parts_only() {
        let hs = hosts();
        let (l, f) = &hs.bragi;
        let payload = slot(&hs.pool, "hades");
        let out = backup_in(l, f, "bragi", &payload, "hades").unwrap();
        let note = out.note.unwrap();
        assert!(
            note.starts_with("Hades (hades) from bragi: 1 file(s)"),
            "{note}"
        );
        assert!(note.contains("skipped 2"), "{note}");
        assert!(out.checksum.unwrap().starts_with("sha256:"));
        let m = Manifest::read(&payload).unwrap();
        assert_eq!((m.title.as_str(), m.host.as_str()), ("Hades", "bragi"));
        assert_eq!(m.parts.keys().collect::<Vec<_>>(), vec!["wine-user"]);
        assert_eq!(
            m.parts["wine-user"][0].relpath,
            "Documents/Saved Games/Hades/Profile1.sav"
        );

        let payload = slot(&hs.pool, "elden-ring");
        backup_in(l, f, "bragi", &payload, "elden-ring").unwrap();
        let m = Manifest::read(&payload).unwrap();
        assert_eq!(
            m.parts["steam-userdata"][0].relpath,
            "1245620/remote/steam_autocloud.vdf"
        );
        assert!(
            m.parts["wine-user"][0]
                .relpath
                .starts_with("AppData/Roaming/EldenRing/")
        );

        assert!(backup_in(l, f, "bragi", &slot(&hs.pool, "x"), "not-a-game").is_err());
    }

    #[test]
    fn bragi_saves_restore_into_hemlocks_shared_prefix() {
        let hs = hosts();
        let (bl, bf) = &hs.bragi;
        let (hl, hf) = &hs.hemlock;
        let src = bl.home.join(
            "Games/Heroic/Prefixes/Hades/drive_c/users/steamuser/Documents/Saved Games/Hades/Profile1.sav",
        );
        set_mtime_secs(&src, 7_000);
        let payload = slot(&hs.pool, "hades");
        backup_in(bl, bf, "bragi", &payload, "hades").unwrap();

        let r = restore_in(hl, hf, &payload, "hades").unwrap();
        assert!(r.errors.is_empty() && r.deferred.is_empty(), "{r:?}");
        assert_eq!(r.written, 1);
        let dst = hl.home.join(
            "Games/Heroic/Prefixes/default/drive_c/users/skey/Documents/Saved Games/Hades/Profile1.sav",
        );
        assert_eq!(fs::read(&dst).unwrap(), fs::read(&src).unwrap());
        assert_eq!(mtime_ns(&dst).unwrap(), 7_000 * 1_000_000_000);

        let again = restore_in(hl, hf, &payload, "hades").unwrap();
        assert_eq!((again.written, again.unchanged), (0, 1));
    }

    #[test]
    fn games_missing_here_defer_their_prefix_but_restore_portable_parts() {
        let hs = hosts();
        let (bl, bf) = &hs.bragi;
        let (hl, hf) = &hs.hemlock;

        let bg3 = game_id("Baldur's Gate 3");
        let payload = slot(&hs.pool, &bg3);
        backup_in(bl, bf, "bragi", &payload, &bg3).unwrap();
        let r = restore_in(hl, hf, &payload, &bg3).unwrap();
        assert_eq!(r.written, 0);
        assert_eq!(r.deferred, vec![format!("wine-user: {PREFIX_DEFERRED}")]);
        assert!(
            !hl.home
                .join(".local/share/Steam/steamapps/compatdata/2371341689")
                .exists()
        );

        let payload = slot(&hs.pool, "elden-ring");
        backup_in(bl, bf, "bragi", &payload, "elden-ring").unwrap();
        let r = restore_in(hl, hf, &payload, "elden-ring").unwrap();
        assert_eq!(r.written, 1);
        assert_eq!(r.deferred, vec![format!("wine-user: {PREFIX_DEFERRED}")]);
        assert!(
            hl.home
                .join(".local/share/Steam/userdata/64751656/1245620/remote/steam_autocloud.vdf")
                .is_file()
        );

        let payload = slot(&hs.pool, "stardew-valley");
        backup_in(bl, bf, "bragi", &payload, "stardew-valley").unwrap();
        let r = restore_in(hl, hf, &payload, "stardew-valley").unwrap();
        assert_eq!(r.written, 1);
        assert!(
            hl.home
                .join(".config/StardewValley/Saves/Farm_1/Farm_1")
                .is_file()
        );

        assert!(restore_in(hl, hf, &payload, "hades").is_err());
    }

    #[test]
    fn kind_dispatches_through_the_toolkit_seam() {
        let v = plugin_toolkit::backup::dispatch_kind_op(
            &GameSavesKind,
            "layout",
            serde_json::json!({"instance": "hades"}),
        )
        .unwrap();
        assert_eq!(v, serde_json::json!(["game-saves", "hades"]));
        let v = plugin_toolkit::backup::dispatch_kind_op(
            &GameSavesKind,
            "title",
            serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(v, serde_json::json!("Game saves"));
    }
}

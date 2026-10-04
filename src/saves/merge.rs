//! Backup as a merge: what a host publishes is the latest state it knows
//! (the last manifest it restored or published) with its own local progress
//! laid over it, so a host missing some of a game's parts (no prefix for it,
//! say) still publishes every part — the missing ones from blobs it kept.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use super::manifest::{self, FileEntry, Manifest, Origin, PlannedFile};
use super::select::SourceFile;
use super::state::{Base, key};

#[derive(Debug, Default)]
pub struct MergePlan {
    pub files: Vec<PlannedFile>,
    /// The manifest parts this plan publishes.
    pub parts: BTreeMap<String, Vec<FileEntry>>,
    /// Keys published from local files (their base moves to what's published).
    pub local_keys: BTreeSet<String>,
    /// Keys where both this host and the pool changed since the last sync;
    /// the local copy is published (the pool's survives in older backups and
    /// as the other host's moved-aside copy).
    pub diverged: Vec<String>,
}

impl MergePlan {
    /// Whether publishing would reproduce `last` exactly.
    pub fn unchanged_from(&self, last: Option<&Manifest>) -> bool {
        last.is_some_and(|m| m.parts == self.parts)
    }
}

/// Merge `local` saves over `last` under the three-way base rule. `blob` maps a
/// sha256 to where this host keeps those bytes; a pool file with no local copy
/// and no blob refuses the whole backup rather than publish it partial.
pub fn plan(
    last: Option<&Manifest>,
    base: &Base,
    local: &[SourceFile],
    blob: impl Fn(&str) -> Result<PathBuf, String>,
) -> Result<MergePlan, String> {
    let mut locals: BTreeMap<(String, String), (FileEntry, PathBuf)> = BTreeMap::new();
    for f in local {
        let rel = manifest::slash_path(&f.rel);
        if let Some(entry) = local_entry(f, &rel, base) {
            locals.insert((f.part.to_string(), rel), (entry, f.abs.clone()));
        }
    }
    let mut pool: BTreeMap<(String, String), FileEntry> = BTreeMap::new();
    if let Some(m) = last {
        for (part, entries) in &m.parts {
            for e in entries {
                pool.insert((part.clone(), e.relpath.clone()), e.clone());
            }
        }
    }

    let mut plan = MergePlan::default();
    let mut missing = Vec::new();
    let keys: BTreeSet<&(String, String)> = locals.keys().chain(pool.keys()).collect();
    for k @ (part, rel) in keys {
        let portable = key(part, rel);
        let b = base.get(&portable).map(|b| b.sha256.as_str());
        let take_local = match (locals.get(k), pool.get(k)) {
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some((l, _)), Some(p)) if l.sha256 == p.sha256 => true,
            (Some((l, _)), Some(_)) if b == Some(l.sha256.as_str()) => false,
            (Some(_), Some(p)) if b == Some(p.sha256.as_str()) => true,
            (Some((l, _)), Some(p)) => {
                plan.diverged.push(portable.clone());
                // Never synced on this host: the newer copy wins.
                b.is_some() || l.mtime_ns >= p.mtime_ns
            }
            (None, None) => continue,
        };
        let (entry, file) = if take_local {
            let (l, abs) = &locals[k];
            // Same bytes as the pool: keep the pool's entry (its mtime) so an
            // unchanged game publishes an identical manifest.
            let expect = match pool.get(k) {
                Some(p) if p.sha256 == l.sha256 => p.clone(),
                _ => l.clone(),
            };
            plan.local_keys.insert(portable);
            (
                expect.clone(),
                PlannedFile {
                    part: part.clone(),
                    rel: rel.clone(),
                    src: abs.clone(),
                    origin: Origin::Local,
                    expect: Some(expect),
                },
            )
        } else {
            let p = pool[k].clone();
            let src = blob(&p.sha256)?;
            if !src.is_file() {
                missing.push(portable);
                continue;
            }
            (
                p.clone(),
                PlannedFile {
                    part: part.clone(),
                    rel: rel.clone(),
                    src,
                    origin: Origin::Blob,
                    expect: Some(p),
                },
            )
        };
        plan.parts.entry(part.clone()).or_default().push(entry);
        plan.files.push(file);
    }
    if !missing.is_empty() {
        return Err(format!(
            "refusing to publish a partial backup: no copy of {} file(s) this game had in its latest backup (e.g. `{}`); restore the latest backup on this host first",
            missing.len(),
            missing[0]
        ));
    }
    Ok(plan)
}

/// The local file as a manifest entry; its hash is reused from the base when
/// size and mtime show it untouched since then. `None` if it can't be read.
fn local_entry(f: &SourceFile, rel: &str, base: &Base) -> Option<FileEntry> {
    let meta = fs::metadata(&f.abs).ok()?;
    let mtime_ns = meta.modified().ok().map(manifest::to_ns)?;
    let sha256 = match base.get(&key(f.part, rel)) {
        Some(b) if b.size == meta.len() && b.mtime_ns == mtime_ns => b.sha256.clone(),
        _ => plugin_toolkit::hash::sha256_file(&f.abs).ok()?,
    };
    Some(FileEntry {
        relpath: rel.to_string(),
        size: meta.len(),
        mtime_ns,
        sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saves::state::BaseEntry;
    use crate::saves::testutil::{TempDir, set_mtime_secs, write};

    fn entry(rel: &str, body: &str, secs: i64) -> FileEntry {
        FileEntry {
            relpath: rel.into(),
            size: body.len() as u64,
            mtime_ns: secs * 1_000_000_000,
            sha256: plugin_toolkit::hash::sha256_hex(body.as_bytes()),
        }
    }

    fn manifest(parts: &[(&str, FileEntry)]) -> Manifest {
        let mut m = Manifest {
            version: manifest::MANIFEST_VERSION,
            instance: "g".into(),
            title: "G".into(),
            host: "other".into(),
            created: String::new(),
            parts: BTreeMap::new(),
        };
        for (p, e) in parts {
            m.parts.entry(p.to_string()).or_default().push(e.clone());
        }
        m
    }

    #[test]
    fn local_progress_overlays_the_pool_and_deferred_parts_carry_forward() {
        let t = TempDir::new();
        let blobs = t.path().join("blobs");
        let pool_prefix = entry("AppData/s.sav", "pool-prefix", 10);
        write(&blobs.join(&pool_prefix.sha256), "pool-prefix");
        let last = manifest(&[
            ("wine-user", pool_prefix.clone()),
            ("home", entry(".g/a", "a1", 10)),
        ]);
        let mut base = Base::new();
        base.insert(
            key("home", ".g/a"),
            BaseEntry {
                sha256: entry(".g/a", "a1", 0).sha256,
                size: 2,
                mtime_ns: 0,
            },
        );
        let local_a = t.path().join("home/.g/a");
        write(&local_a, "a2-progress");
        let local = [SourceFile {
            part: "home",
            rel: PathBuf::from(".g/a"),
            abs: local_a,
        }];
        let p = plan(Some(&last), &base, &local, |sha| Ok(blobs.join(sha))).unwrap();
        assert_eq!(
            p.parts["home"][0].sha256,
            entry("", "a2-progress", 0).sha256
        );
        assert_eq!(p.parts["wine-user"][0], pool_prefix);
        assert_eq!(
            p.files.iter().filter(|f| f.origin == Origin::Blob).count(),
            1
        );
        assert!(p.diverged.is_empty());
        assert!(!p.unchanged_from(Some(&last)));
    }

    #[test]
    fn a_missing_blob_refuses_the_backup() {
        let t = TempDir::new();
        let last = manifest(&[("wine-user", entry("s.sav", "x", 1))]);
        let e = plan(Some(&last), &Base::new(), &[], |sha| Ok(t.path().join(sha))).unwrap_err();
        assert!(e.contains("refusing to publish a partial backup"), "{e}");
    }

    #[test]
    fn identical_local_state_is_unchanged() {
        let t = TempDir::new();
        let f = t.path().join("a");
        write(&f, "same");
        set_mtime_secs(&f, 99);
        let last = manifest(&[("home", entry("a", "same", 5))]);
        let local = [SourceFile {
            part: "home",
            rel: PathBuf::from("a"),
            abs: f,
        }];
        let p = plan(Some(&last), &Base::new(), &local, |s| Ok(t.path().join(s))).unwrap();
        assert!(p.unchanged_from(Some(&last)));
        assert!(!p.unchanged_from(None));
    }

    #[test]
    fn stale_local_does_not_override_a_newer_pool_copy() {
        let t = TempDir::new();
        let f = t.path().join("a");
        write(&f, "old");
        let newer = entry("a", "newer", 50);
        write(&t.path().join(&newer.sha256), "newer");
        let last = manifest(&[("home", newer.clone())]);
        let mut base = Base::new();
        base.insert(
            key("home", "a"),
            BaseEntry {
                sha256: entry("a", "old", 0).sha256,
                size: 3,
                mtime_ns: 0,
            },
        );
        let local = [SourceFile {
            part: "home",
            rel: PathBuf::from("a"),
            abs: f,
        }];
        let p = plan(Some(&last), &base, &local, |s| Ok(t.path().join(s))).unwrap();
        assert_eq!(p.parts["home"][0], newer);
        assert!(p.local_keys.is_empty());
    }
}

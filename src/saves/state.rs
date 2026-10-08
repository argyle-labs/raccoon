//! Per-host sync state under `~/.local/share/orca/raccoon/game-saves/`:
//!
//! ```text
//! index.json                       instance → title, from the last full scan
//! instances/<id>/base.json         portable key → what this host last synced
//! instances/<id>/last.json         the manifest this host last restored or published
//! instances/<id>/blobs/<sha256>    bytes this host holds only for the pool
//! ```
//!
//! The base is what makes restore and backup three-way: a local file equal to
//! its base hasn't been touched since the last sync, so the pool's copy may
//! replace it; one that differs from its base is local progress.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use plugin_toolkit::serde_json;
use serde::{Deserialize, Serialize};

use super::fsx;
use super::manifest::Manifest;

/// What a host last synced for one portable key: the bytes, and the local
/// file's size + mtime at that moment (so an untouched file needs no rehash).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseEntry {
    pub sha256: String,
    pub size: u64,
    pub mtime_ns: i64,
}

pub type Base = BTreeMap<String, BaseEntry>;

/// `part:relpath`, the portable key a base or manifest entry is filed under.
pub fn key(part: &str, relpath: &str) -> String {
    format!("{part}:{relpath}")
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    pub scanned_ms: i64,
    /// Instance → ludusavi title.
    pub titles: BTreeMap<String, String>,
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join(".local/share/orca/raccoon/game-saves"),
        }
    }

    #[cfg(test)]
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    fn dir(&self, instance: &str) -> Result<PathBuf, String> {
        if instance.is_empty()
            || !instance
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(format!("invalid game-saves instance `{instance}`"));
        }
        Ok(self.root.join("instances").join(instance))
    }

    pub fn index(&self) -> Option<Index> {
        read_json(&self.root.join("index.json")).ok().flatten()
    }

    pub fn save_index(&self, index: &Index) -> Result<(), String> {
        write_json(&self.root.join("index.json"), index)
    }

    pub fn base(&self, instance: &str) -> Result<Base, String> {
        Ok(read_json(&self.dir(instance)?.join("base.json"))?.unwrap_or_default())
    }

    pub fn save_base(&self, instance: &str, base: &Base) -> Result<(), String> {
        write_json(&self.dir(instance)?.join("base.json"), base)
    }

    pub fn last(&self, instance: &str) -> Result<Option<Manifest>, String> {
        read_json(&self.dir(instance)?.join("last.json"))
    }

    pub fn save_last(&self, instance: &str, m: &Manifest) -> Result<(), String> {
        write_json(&self.dir(instance)?.join("last.json"), m)
    }

    pub fn blob(&self, instance: &str, sha256: &str) -> Result<PathBuf, String> {
        if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("invalid sha256 `{sha256}`"));
        }
        Ok(self.dir(instance)?.join("blobs").join(sha256))
    }

    /// Keep `src`'s bytes as blob `sha256` (verified), unless already held.
    pub fn cache_blob(&self, instance: &str, src: &Path, sha256: &str) -> Result<(), String> {
        let dst = self.blob(instance, sha256)?;
        if dst.is_file() {
            return Ok(());
        }
        let dir = dst.parent().unwrap_or(&self.root);
        fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        let (tmp, _, got) = fsx::copy_hashing_to_temp(src, dir, &format!(".{sha256}.tmp-"))
            .map_err(|e| format!("cache {}: {e}", src.display()))?;
        if got == sha256 {
            fs::rename(&tmp, &dst).map_err(|e| {
                fsx::remove_quietly(&tmp);
                format!("cache {}: {e}", dst.display())
            })
        } else {
            fsx::remove_quietly(&tmp);
            Err(format!(
                "{}: sha256 {got} != manifest {sha256}",
                src.display()
            ))
        }
    }

    /// Drop blobs no longer referenced by `keep`.
    pub fn prune_blobs(&self, instance: &str, keep: &BTreeSet<String>) -> Result<(), String> {
        let dir = self.dir(instance)?.join("blobs");
        let Ok(rd) = fs::read_dir(&dir) else {
            return Ok(());
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !keep.contains(&name) {
                fsx::remove_quietly(&e.path());
            }
        }
        Ok(())
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    match fs::read(path) {
        Ok(raw) => serde_json::from_slice(&raw)
            .map(Some)
            .map_err(|e| format!("parse {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read {}: {e}", path.display())),
    }
}

fn write_json<T: Serialize>(path: &Path, v: &T) -> Result<(), String> {
    let raw =
        serde_json::to_vec_pretty(v).map_err(|e| format!("encode {}: {e}", path.display()))?;
    fsx::atomic_write(path, &raw, fsx::NewMode::Private)
        .map_err(|e| format!("write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saves::testutil::{TempDir, write};

    #[test]
    fn blobs_are_verified_and_pruned() {
        let t = TempDir::new();
        let s = Store::at(t.path().join("state"));
        let src = t.path().join("f");
        write(&src, "abc");
        let sha = plugin_toolkit::hash::sha256_hex(b"abc");
        s.cache_blob("hades", &src, &sha).unwrap();
        assert!(s.blob("hades", &sha).unwrap().is_file());
        let wrong = plugin_toolkit::hash::sha256_hex(b"abd");
        assert!(s.cache_blob("hades", &src, &wrong).is_err());
        assert!(!s.blob("hades", &wrong).unwrap().exists());
        s.prune_blobs("hades", &BTreeSet::new()).unwrap();
        assert!(!s.blob("hades", &sha).unwrap().exists());
        assert!(s.blob("hades", "../x").is_err());
        assert!(s.base("../etc").is_err());
    }

    #[test]
    fn base_and_index_round_trip() {
        let t = TempDir::new();
        let s = Store::at(t.path().join("state"));
        assert!(s.base("hades").unwrap().is_empty());
        let mut b = Base::new();
        b.insert(
            key("wine-user", "a"),
            BaseEntry {
                sha256: "x".into(),
                size: 1,
                mtime_ns: 2,
            },
        );
        s.save_base("hades", &b).unwrap();
        assert_eq!(s.base("hades").unwrap(), b);
        let i = Index {
            scanned_ms: 5,
            titles: [("hades".to_string(), "Hades".to_string())].into(),
        };
        s.save_index(&i).unwrap();
        assert_eq!(s.index(), Some(i));
    }
}

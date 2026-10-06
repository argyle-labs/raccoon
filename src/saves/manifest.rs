//! Capture save files into a payload dir and describe them in `manifest.json`.
//!
//! Payload layout:
//! ```text
//! <payload>/manifest.json
//! <payload>/files/<part>/<relpath>
//! ```
//! Payload files carry no meaningful mtime or mode; the manifest's
//! `mtime_ns` (the source file's exact mtime) is what restore stamps back.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use plugin_toolkit::serde_json;
use serde::{Deserialize, Serialize};

use super::fsx;

pub const MANIFEST_FILE: &str = "manifest.json";
pub const FILES_DIR: &str = "files";
pub const MANIFEST_VERSION: u32 = 1;
/// Largest manifest read; a pool file may come from another host or be crafted.
pub const MANIFEST_MAX: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    /// The game id (backup instance).
    pub instance: String,
    /// The game's ludusavi title, which restore looks up on the local host.
    pub title: String,
    /// Hostname that wrote this payload.
    pub host: String,
    /// RFC 3339 capture time.
    pub created: String,
    /// Part id → its files, sorted by relpath.
    pub parts: BTreeMap<String, Vec<FileEntry>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// `/`-separated, relative to the part's local anchor.
    pub relpath: String,
    pub size: u64,
    /// Nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    pub sha256: String,
}

/// What [`capture`] did, beyond the manifest itself.
#[derive(Debug, Default)]
pub struct CaptureStats {
    pub files: usize,
    pub bytes: u64,
    /// Local files that vanished between scan and copy.
    pub skipped: Vec<String>,
}

impl Manifest {
    pub fn read(payload_dir: &Path) -> Result<Self, String> {
        let path = payload_dir.join(MANIFEST_FILE);
        let raw = fsx::read_regular_capped(&path, MANIFEST_MAX)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        let m: Self =
            serde_json::from_slice(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
        if m.version != MANIFEST_VERSION {
            return Err(format!(
                "unsupported game-saves manifest version {}",
                m.version
            ));
        }
        Ok(m)
    }

    /// Writes `manifest.json` and returns its bytes' sha256.
    pub fn write(&self, payload_dir: &Path) -> Result<String, String> {
        let raw = serde_json::to_vec_pretty(self).map_err(|e| format!("encode manifest: {e}"))?;
        let path = payload_dir.join(MANIFEST_FILE);
        fsx::atomic_write(&path, &raw).map_err(|e| format!("write {}: {e}", path.display()))?;
        Ok(plugin_toolkit::hash::sha256_hex(&raw))
    }
}

/// Where a planned file's bytes come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A live local save.
    Local,
    /// A blob this host keeps for the pool (a part it can't place locally).
    Blob,
}

/// One file of the payload to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub part: String,
    /// `/`-separated.
    pub rel: String,
    pub src: PathBuf,
    pub origin: Origin,
    /// The entry to record when the bytes are already known (a blob, or a
    /// local file identical to the latest); the copy must hash to it.
    pub expect: Option<FileEntry>,
}

/// A local file rewritten mid-copy is retried this many times before the
/// backup gives up rather than publish a torn save.
const TORN_RETRIES: usize = 3;

/// Copy `plan` into `payload_dir` and write the manifest. Returns the
/// manifest, its sha256, and capture stats. Only a local source that no longer
/// exists is skipped; any other failure aborts, and so does capturing nothing.
pub fn capture(
    plan: &[PlannedFile],
    payload_dir: &Path,
    instance: &str,
    title: &str,
    host: &str,
) -> Result<(Manifest, String, CaptureStats), String> {
    let mut stats = CaptureStats::default();
    let mut out: BTreeMap<String, Vec<FileEntry>> = BTreeMap::new();
    for f in plan {
        let dst = payload_dir
            .join(FILES_DIR)
            .join(checked_rel(&f.part)?)
            .join(checked_rel(&f.rel)?);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        let entry = match f.origin {
            Origin::Blob => {
                let want = f
                    .expect
                    .clone()
                    .ok_or_else(|| format!("blob {} has no manifest entry", f.src.display()))?;
                let (_, sha) = fsx::copy_hashing(&f.src, &dst)
                    .map_err(|e| format!("copy {}: {e}", f.src.display()))?;
                if sha != want.sha256 {
                    return Err(format!("{}: blob sha256 {sha} != {}", f.rel, want.sha256));
                }
                want
            }
            Origin::Local => match copy_local(&f.src, &dst, f.expect.as_ref())? {
                Some(e) => FileEntry {
                    relpath: f.rel.clone(),
                    ..e
                },
                None => {
                    stats.skipped.push(format!("{}: vanished", f.src.display()));
                    continue;
                }
            },
        };
        stats.files += 1;
        stats.bytes += entry.size;
        out.entry(f.part.clone()).or_default().push(entry);
    }
    if stats.files == 0 {
        return Err("no save files captured".to_string());
    }
    for entries in out.values_mut() {
        entries.sort_by(|a, b| a.relpath.cmp(&b.relpath));
    }
    let manifest = Manifest {
        version: MANIFEST_VERSION,
        instance: instance.to_string(),
        title: title.to_string(),
        host: host.to_string(),
        created: plugin_toolkit::time::now().to_rfc3339(),
        parts: out,
    };
    let checksum = manifest.write(payload_dir)?;
    Ok((manifest, checksum, stats))
}

/// Copy a live save, retrying if it changes underneath (size or mtime differ
/// before vs after). `None` when the source no longer exists.
fn copy_local(
    src: &Path,
    dst: &Path,
    expect: Option<&FileEntry>,
) -> Result<Option<FileEntry>, String> {
    for _ in 0..TORN_RETRIES {
        let before = match fs::metadata(src) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("stat {}: {e}", src.display())),
        };
        let (size, sha256) = match fsx::copy_hashing(src, dst) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !src.exists() => {
                fsx::remove_quietly(dst);
                return Ok(None);
            }
            Err(e) => return Err(format!("copy {} -> {}: {e}", src.display(), dst.display())),
        };
        let after = fs::metadata(src).map_err(|e| format!("stat {}: {e}", src.display()))?;
        let mtime = before
            .modified()
            .map(to_ns)
            .map_err(|e| format!("stat {}: {e}", src.display()))?;
        let stable = before.len() == after.len() && after.modified().ok().map(to_ns) == Some(mtime);
        if stable && size == before.len() {
            return Ok(Some(match expect {
                Some(e) if e.sha256 == sha256 => e.clone(),
                _ => FileEntry {
                    relpath: String::new(),
                    size,
                    mtime_ns: mtime,
                    sha256,
                },
            }));
        }
        fsx::remove_quietly(dst);
    }
    Err(format!(
        "{} kept changing during backup; is the game running?",
        src.display()
    ))
}

pub fn set_mtime(path: &Path, t: SystemTime) -> Result<(), String> {
    fs::File::open(path)
        .and_then(|f| f.set_modified(t))
        .map_err(|e| format!("set mtime {}: {e}", path.display()))
}

pub fn mtime_ns(path: &Path) -> std::io::Result<i64> {
    fs::metadata(path).and_then(|m| m.modified()).map(to_ns)
}

pub fn to_ns(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}

pub fn from_ns(ns: i64) -> SystemTime {
    if ns >= 0 {
        UNIX_EPOCH + Duration::from_nanos(ns.unsigned_abs())
    } else {
        UNIX_EPOCH - Duration::from_nanos(ns.unsigned_abs())
    }
}

/// `rel` as a path of only normal components — game keys and relpaths come
/// from a payload that may have been produced on another host, so a `..` or
/// absolute path must never let a restore write outside its root. Segments are
/// checked on the raw string because `Path::components` silently drops empty
/// and `.` segments, and a NUL would truncate the path at the syscall.
pub fn checked_rel(rel: &str) -> Result<PathBuf, String> {
    let p = Path::new(rel);
    let segments_ok = rel
        .split('/')
        .all(|s| !s.is_empty() && s != "." && s != ".." && !s.contains('\0'));
    if !segments_ok || !p.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(format!("unsafe relative path in manifest: {rel:?}"));
    }
    Ok(p.to_path_buf())
}

pub fn slash_path(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saves::testutil::{TempDir, set_mtime_secs, write};

    #[test]
    fn capture_records_source_mtime_and_hash() {
        let t = TempDir::new();
        let root = t.path().join("prefix/drive_c/users/steamuser");
        let save = root.join("AppData/Roaming/G/save.dat");
        write(&save, "hello");
        set_mtime_secs(&save, 1_700_000_000);
        let payload = t.path().join("payload");
        fs::create_dir_all(&payload).unwrap();

        let plan = vec![PlannedFile {
            part: "wine-user".into(),
            rel: "AppData/Roaming/G/save.dat".into(),
            src: save,
            origin: Origin::Local,
            expect: None,
        }];
        let (m, sum, stats) = capture(&plan, &payload, "hades", "Hades", "bragi").unwrap();

        assert_eq!((stats.files, stats.bytes), (1, 5));
        assert_eq!(
            (m.host.as_str(), m.instance.as_str(), m.title.as_str()),
            ("bragi", "hades", "Hades")
        );
        let e = &m.parts["wine-user"][0];
        assert_eq!(e.relpath, "AppData/Roaming/G/save.dat");
        assert_eq!(e.size, 5);
        assert_eq!(e.mtime_ns, 1_700_000_000 * 1_000_000_000);
        assert_eq!(e.sha256, plugin_toolkit::hash::sha256_hex(b"hello"));

        let copied = payload.join("files/wine-user/AppData/Roaming/G/save.dat");
        assert_eq!(fs::read_to_string(&copied).unwrap(), "hello");

        let raw = fs::read(payload.join(MANIFEST_FILE)).unwrap();
        assert_eq!(sum, plugin_toolkit::hash::sha256_hex(&raw));
        assert_eq!(Manifest::read(&payload).unwrap(), m);
    }

    #[test]
    fn capture_errors_only_skip_vanished_sources() {
        let t = TempDir::new();
        let payload = t.path().join("payload");
        fs::create_dir_all(&payload).unwrap();
        let src = t.path().join("a.sav");
        write(&src, "a");
        let plan = |src: PathBuf, origin, expect| PlannedFile {
            part: "home".into(),
            rel: "a.sav".into(),
            src,
            origin,
            expect,
        };
        // Vanished local → skip; nothing left → error.
        let gone = plan(t.path().join("gone.sav"), Origin::Local, None);
        let e = capture(std::slice::from_ref(&gone), &payload, "g", "G", "h").unwrap_err();
        assert!(e.contains("no save files"), "{e}");
        let (_, _, stats) = capture(
            &[gone, plan(src.clone(), Origin::Local, None)],
            &t.path().join("p2"),
            "g",
            "G",
            "h",
        )
        .unwrap();
        assert_eq!((stats.files, stats.skipped.len()), (1, 1));

        // A missing or wrong blob is never skipped.
        let entry = FileEntry {
            relpath: "a.sav".into(),
            size: 1,
            mtime_ns: 0,
            sha256: plugin_toolkit::hash::sha256_hex(b"other"),
        };
        let missing = plan(t.path().join("blob"), Origin::Blob, Some(entry.clone()));
        assert!(capture(&[missing], &t.path().join("p3"), "g", "G", "h").is_err());
        let wrong = plan(src.clone(), Origin::Blob, Some(entry));
        assert!(capture(&[wrong], &t.path().join("p4"), "g", "G", "h").is_err());

        // Destination failures propagate: the payload path is a file.
        let blocked = t.path().join("p5");
        write(&blocked.join("files"), "not a dir");
        let e = capture(&[plan(src, Origin::Local, None)], &blocked, "g", "G", "h").unwrap_err();
        assert!(e.contains("mkdir"), "{e}");
    }

    #[test]
    fn checked_rel_rejects_escapes() {
        assert!(checked_rel("123/remote/a").is_ok());
        assert!(checked_rel("../x").is_err());
        assert!(checked_rel("/etc/passwd").is_err());
        assert!(checked_rel("a/../../b").is_err());
        assert!(checked_rel("").is_err());
        for bad in ["a//b", "a/", "/a", "a/./b", "./a", "a/\0b", "a\0"] {
            assert!(checked_rel(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn ns_round_trips() {
        let t = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789);
        assert_eq!(from_ns(to_ns(t)), t);
    }
}

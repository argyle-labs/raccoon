//! Capture save files into a payload dir and describe them in `manifest.json`.
//!
//! Payload layout:
//! ```text
//! <payload>/manifest.json
//! <payload>/files/<part>/<relpath>
//! ```
//! The manifest's `mtime_ns` is authoritative: a payload on SMB/NFS may round
//! file times, and newest-wins comparisons must use the source's exact mtime.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use plugin_toolkit::serde_json;
use serde::{Deserialize, Serialize};

use super::select::SourceFile;

pub const MANIFEST_FILE: &str = "manifest.json";
pub const FILES_DIR: &str = "files";
pub const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    /// The game id (backup instance).
    pub instance: String,
    /// The game's ludusavi title, which restore looks up on the local host.
    pub title: String,
    /// Hostname that wrote this payload; the store's record has no writer field.
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
    /// Source files that vanished or went unreadable mid-capture.
    pub skipped: Vec<String>,
}

impl Manifest {
    pub fn read(payload_dir: &Path) -> Result<Self, String> {
        let path = payload_dir.join(MANIFEST_FILE);
        let raw = fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
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
        fs::write(&path, &raw).map_err(|e| format!("write {}: {e}", path.display()))?;
        Ok(plugin_toolkit::hash::sha256_hex(&raw))
    }
}

/// Copy `files` into `payload_dir` (mtimes preserved) and write the manifest.
/// Returns the manifest, its sha256, and capture stats.
pub fn capture(
    files: &[SourceFile],
    payload_dir: &Path,
    instance: &str,
    title: &str,
    host: &str,
) -> Result<(Manifest, String, CaptureStats), String> {
    let mut stats = CaptureStats::default();
    let mut out: BTreeMap<String, Vec<FileEntry>> = BTreeMap::new();
    for f in files {
        let dst = payload_dir.join(FILES_DIR).join(f.part).join(&f.rel);
        match copy_with_mtime(&f.abs, &dst) {
            Ok(entry_mtime) => {
                let size = fs::metadata(&dst)
                    .map_err(|e| format!("stat {}: {e}", dst.display()))?
                    .len();
                let sha256 = plugin_toolkit::hash::sha256_file(&dst)
                    .map_err(|e| format!("hash {}: {e:#}", dst.display()))?;
                stats.files += 1;
                stats.bytes += size;
                out.entry(f.part.to_string()).or_default().push(FileEntry {
                    relpath: slash_path(&f.rel),
                    size,
                    mtime_ns: entry_mtime,
                    sha256,
                });
            }
            // A save vanishing between scan and copy is a skip, not a failed
            // backup.
            Err(e) => stats.skipped.push(e),
        }
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

/// Copy `src` → `dst` (creating parents) and stamp `dst` with `src`'s mtime.
/// Returns that mtime in ns.
fn copy_with_mtime(src: &Path, dst: &Path) -> Result<i64, String> {
    let mtime = fs::metadata(src)
        .and_then(|m| m.modified())
        .map_err(|e| format!("stat {}: {e}", src.display()))?;
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    fs::copy(src, dst).map_err(|e| format!("copy {}: {e}", src.display()))?;
    set_mtime(dst, mtime)?;
    Ok(to_ns(mtime))
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
/// absolute path must never let a restore write outside its root.
pub fn checked_rel(rel: &str) -> Result<PathBuf, String> {
    let p = Path::new(rel);
    if rel.is_empty() || !p.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(format!("unsafe relative path in manifest: {rel:?}"));
    }
    Ok(p.to_path_buf())
}

fn slash_path(p: &Path) -> String {
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
    fn capture_copies_preserves_mtime_and_describes_files() {
        let t = TempDir::new();
        let root = t.path().join("prefix/drive_c/users/steamuser");
        let save = root.join("AppData/Roaming/G/save.dat");
        write(&save, "hello");
        set_mtime_secs(&save, 1_700_000_000);
        let payload = t.path().join("payload");
        fs::create_dir_all(&payload).unwrap();

        let files = vec![SourceFile {
            part: "wine-user",
            rel: PathBuf::from("AppData/Roaming/G/save.dat"),
            abs: save,
        }];
        let (m, sum, stats) = capture(&files, &payload, "hades", "Hades", "bragi").unwrap();

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
        assert_eq!(mtime_ns(&copied).unwrap(), e.mtime_ns);

        let raw = fs::read(payload.join(MANIFEST_FILE)).unwrap();
        assert_eq!(sum, plugin_toolkit::hash::sha256_hex(&raw));
        assert_eq!(Manifest::read(&payload).unwrap(), m);
    }

    #[test]
    fn checked_rel_rejects_escapes() {
        assert!(checked_rel("123/remote/a").is_ok());
        assert!(checked_rel("../x").is_err());
        assert!(checked_rel("/etc/passwd").is_err());
        assert!(checked_rel("a/../../b").is_err());
        assert!(checked_rel("").is_err());
    }

    #[test]
    fn ns_round_trips() {
        let t = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789);
        assert_eq!(from_ns(to_ns(t)), t);
    }
}

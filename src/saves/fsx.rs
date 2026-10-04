//! File primitives shared by capture, restore and the state store: hashed
//! copies into fresh files, durable atomic writes.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Above this, a copy streams and hashes the written file instead of holding
/// the whole save in memory.
const IN_MEMORY_MAX: u64 = 256 * 1024 * 1024;

/// Copy `src` into a NEW file `dst` (created with default permissions — the
/// source's mode bits are never carried over) and fsync it. Returns
/// `(size, sha256)` of the bytes written.
pub fn copy_hashing(src: &Path, dst: &Path) -> io::Result<(u64, String)> {
    let len = fs::metadata(src)?.len();
    let mut out = OpenOptions::new().write(true).create_new(true).open(dst)?;
    let result = if len <= IN_MEMORY_MAX {
        let bytes = fs::read(src)?;
        out.write_all(&bytes)?;
        Ok((bytes.len() as u64, plugin_toolkit::hash::sha256_hex(&bytes)))
    } else {
        let n = io::copy(&mut File::open(src)?, &mut out)?;
        plugin_toolkit::hash::sha256_file(dst)
            .map(|sha| (n, sha))
            .map_err(|e| io::Error::other(format!("{e:#}")))
    };
    out.sync_all()?;
    result
}

/// Write `bytes` to `path` via a fsynced sibling temp + rename + dir fsync, so
/// a crash leaves either the old file or the new one.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    fs::create_dir_all(dir)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let written = (|| {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if written.is_err() {
        remove_quietly(&tmp);
    }
    written?;
    fsync_dir(dir)
}

/// Persist a directory's entries (a rename is only durable once its dir is).
pub fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Best-effort removal of a temp we created; a failure is logged, not fatal.
pub fn remove_quietly(path: &Path) {
    if let Err(e) = fs::remove_file(path)
        && e.kind() != io::ErrorKind::NotFound
    {
        plugin_toolkit::tracing::warn!("[game-saves] remove {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saves::testutil::{TempDir, write};

    #[cfg(unix)]
    #[test]
    fn copy_hashing_writes_fresh_files_without_source_mode() {
        use std::os::unix::fs::PermissionsExt;
        let t = TempDir::new();
        let src = t.path().join("src");
        write(&src, "hello");
        fs::set_permissions(&src, fs::Permissions::from_mode(0o4755)).unwrap();
        let dst = t.path().join("dst");
        let (n, sha) = copy_hashing(&src, &dst).unwrap();
        assert_eq!((n, sha), (5, plugin_toolkit::hash::sha256_hex(b"hello")));
        let mode = fs::metadata(&dst).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode & 0o4111, 0, "mode bits leaked: {mode:o}");
        assert!(copy_hashing(&src, &dst).is_err(), "must not overwrite");
    }

    #[test]
    fn atomic_write_replaces() {
        let t = TempDir::new();
        let p = t.path().join("a/b.json");
        atomic_write(&p, b"1").unwrap();
        atomic_write(&p, b"2").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"2");
        assert_eq!(fs::read_dir(p.parent().unwrap()).unwrap().count(), 1);
    }
}

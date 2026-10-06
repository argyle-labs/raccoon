//! File primitives shared by capture, restore and the state store: hashed
//! copies into fresh files, durable atomic writes.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

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

/// Mode of a file [`atomic_write`] creates; a replaced regular file keeps its own.
const NEW_FILE_MODE: u32 = 0o600;

/// Write `bytes` to `path` via a fsynced sibling temp + rename + dir fsync, so
/// a crash leaves either the old file or the new one. The temp is created
/// exclusively under a random name and never through a symlink, so nothing
/// planted beside `path` can redirect the write.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    fs::create_dir_all(dir)?;
    let mode = match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => permission_bits(&m),
        _ => NEW_FILE_MODE,
    };
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let (tmp, mut f) = create_temp(dir, &name, mode)?;
    let written = (|| {
        f.write_all(bytes)?;
        // The create mode is filtered by the umask; set it exactly.
        set_mode(&f, mode)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)?;
        fsync_dir(dir)
    })();
    if written.is_err() {
        remove_quietly(&tmp);
    }
    written
}

/// A fresh `.{name}.tmp-<random>` in `dir`, opened `O_EXCL | O_NOFOLLOW`.
fn create_temp(dir: &Path, name: &str, mode: u32) -> io::Result<(PathBuf, File)> {
    const ATTEMPTS: usize = 8;
    let mut last = None;
    for _ in 0..ATTEMPTS {
        let tmp = dir.join(format!(".{name}.tmp-{}", plugin_toolkit::mint_uuidv7()));
        match open_new(&tmp, mode) {
            Ok(f) => return Ok((tmp, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("no temp name available")))
}

/// Create `path`, failing if anything (a symlink included) is already there.
fn open_new(path: &Path, mode: u32) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    let _ = mode;
    opts.open(path)
}

#[cfg(unix)]
fn permission_bits(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn permission_bits(_: &fs::Metadata) -> u32 {
    NEW_FILE_MODE
}

#[cfg(unix)]
fn set_mode(f: &File, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    f.set_permissions(fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_: &File, _: u32) -> io::Result<()> {
    Ok(())
}

/// Read `path`, which must be a regular file of at most `cap` bytes. On unix a
/// symlink at `path` is refused rather than followed, and a FIFO there cannot
/// block the open.
pub fn read_regular_capped(path: &Path, cap: u64) -> io::Result<Vec<u8>> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let f = opts.open(path)?;
    if !f.metadata()?.is_file() {
        return Err(io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    f.take(cap.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        return Err(io::Error::other(format!(
            "{} is over {cap} bytes",
            path.display()
        )));
    }
    Ok(bytes)
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
    fn read_regular_capped_refuses_symlinks_and_oversize() {
        let t = TempDir::new();
        let f = t.path().join("f");
        write(&f, "12345");
        assert_eq!(read_regular_capped(&f, 5).unwrap(), b"12345");
        assert!(read_regular_capped(&f, 4).is_err());
        let link = t.path().join("link");
        std::os::unix::fs::symlink(&f, &link).unwrap();
        assert!(read_regular_capped(&link, 5).is_err());
        assert!(read_regular_capped(t.path(), 5).is_err());
        let fifo = t.path().join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(read_regular_capped(&fifo, 5).is_err());
    }

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

    #[cfg(unix)]
    #[test]
    fn planted_symlinks_are_never_written_through() {
        let t = TempDir::new();
        let victim = t.path().join("victim");
        write(&victim, "precious");
        let dir = t.path().join("d");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("state.json");
        // The temp name the helper used to derive from the pid alone.
        let legacy = dir.join(format!(".state.json.tmp-{}", std::process::id()));
        std::os::unix::fs::symlink(&victim, &legacy).unwrap();
        atomic_write(&p, b"new").unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"precious");
        assert_eq!(fs::read(&p).unwrap(), b"new");

        let planted = dir.join("planted");
        std::os::unix::fs::symlink(&victim, &planted).unwrap();
        assert!(open_new(&planted, 0o600).is_err());
        let dangling = dir.join("dangling");
        std::os::unix::fs::symlink(t.path().join("absent"), &dangling).unwrap();
        assert!(open_new(&dangling, 0o600).is_err());
        assert!(!t.path().join("absent").exists());
        assert_eq!(fs::read(&victim).unwrap(), b"precious");

        // A symlink at the destination is replaced, not written through.
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        atomic_write(&link, b"x").unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().is_file());
        assert_eq!(fs::read(&victim).unwrap(), b"precious");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_keeps_the_replaced_mode() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        let t = TempDir::new();
        let fresh = t.path().join("fresh");
        atomic_write(&fresh, b"1").unwrap();
        assert_eq!(mode(&fresh), NEW_FILE_MODE);
        for kept in [0o640, 0o444, 0o755] {
            let p = t.path().join(format!("kept-{kept:o}"));
            write(&p, "old");
            fs::set_permissions(&p, fs::Permissions::from_mode(kept)).unwrap();
            atomic_write(&p, b"new").unwrap();
            assert_eq!(mode(&p), kept);
            assert_eq!(fs::read(&p).unwrap(), b"new");
        }
    }

    #[test]
    fn atomic_write_leaves_no_temp_on_failure() {
        let t = TempDir::new();
        let p = t.path().join("dir-in-the-way");
        fs::create_dir_all(p.join("child")).unwrap();
        assert!(atomic_write(&p, b"x").is_err());
        let names: Vec<_> = fs::read_dir(t.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("dir-in-the-way")]);
    }
}

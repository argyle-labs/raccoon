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
    let mut out = open_new(dst, DEFAULT_FILE_MODE)?;
    copy_into(src, &mut out, dst)
}

/// [`copy_hashing`] into a fresh [`create_temp`] named `{prefix}<random>` in
/// `dir`, removed again if the copy fails. Returns `(temp, size, sha256)`.
pub fn copy_hashing_to_temp(
    src: &Path,
    dir: &Path,
    prefix: &str,
) -> io::Result<(PathBuf, u64, String)> {
    let (tmp, mut out) = create_temp(dir, prefix, DEFAULT_FILE_MODE)?;
    match copy_into(src, &mut out, &tmp) {
        Ok((n, sha)) => Ok((tmp, n, sha)),
        Err(e) => {
            drop(out);
            remove_quietly(&tmp);
            Err(e)
        }
    }
}

fn copy_into(src: &Path, out: &mut File, dst: &Path) -> io::Result<(u64, String)> {
    let len = fs::metadata(src)?.len();
    let result = if len <= IN_MEMORY_MAX {
        let bytes = fs::read(src)?;
        out.write_all(&bytes)?;
        Ok((bytes.len() as u64, plugin_toolkit::hash::sha256_hex(&bytes)))
    } else {
        let n = io::copy(&mut File::open(src)?, out)?;
        plugin_toolkit::hash::sha256_file(dst)
            .map(|sha| (n, sha))
            .map_err(|e| io::Error::other(format!("{e:#}")))
    };
    out.sync_all()?;
    result
}

/// Mode of a file [`atomic_write`] creates with [`NewMode::Private`].
pub const NEW_FILE_MODE: u32 = 0o600;

/// The mode [`atomic_write`] gives a file it creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewMode {
    /// [`NEW_FILE_MODE`], whatever the umask: this host's own state.
    Private,
    /// `0o666` less the umask, like `File::create`: files another host reads.
    Umask,
}

/// What `File::create` asks for; the umask then applies.
const DEFAULT_FILE_MODE: u32 = 0o666;

/// Write `bytes` to `path` via a fsynced sibling temp + rename + dir fsync, so
/// a crash leaves either the old file or the new one. The temp is created
/// exclusively under a random name and never through a symlink, so nothing
/// planted beside `path` can redirect the write.
pub fn atomic_write(path: &Path, bytes: &[u8], new: NewMode) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    fs::create_dir_all(dir)?;
    // `None`: leave the umask-filtered create mode as is.
    let exact = match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => Some(permission_bits(&m)),
        _ if new == NewMode::Private => Some(NEW_FILE_MODE),
        _ => None,
    };
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let create_mode = exact.unwrap_or(DEFAULT_FILE_MODE);
    let (tmp, mut f) = create_temp(dir, &format!(".{name}.tmp-"), create_mode)?;
    let written = (|| {
        f.write_all(bytes)?;
        // The create mode is filtered by the umask; set it exactly.
        if let Some(mode) = exact {
            set_mode(&f, mode)?;
        }
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

/// A fresh `{prefix}<random>` in `dir`, opened `O_EXCL | O_NOFOLLOW`.
pub fn create_temp(dir: &Path, prefix: &str, mode: u32) -> io::Result<(PathBuf, File)> {
    const ATTEMPTS: usize = 8;
    let mut last = None;
    for _ in 0..ATTEMPTS {
        let tmp = dir.join(format!("{prefix}{}", plugin_toolkit::mint_uuidv7()));
        match open_new(&tmp, mode) {
            Ok(f) => return Ok((tmp, f)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("no temp name available")))
}

/// Create `path`, failing if anything (a symlink included) is already there.
pub fn open_new(path: &Path, mode: u32) -> io::Result<File> {
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
    meta.permissions().mode() & 0o777
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

/// Give `to` the permission bits (setuid/setgid/sticky dropped) of `from`
/// when `from` is a regular file; a symlink there is not followed.
pub fn keep_mode(from: &Path, to: &Path) -> io::Result<()> {
    match fs::symlink_metadata(from) {
        Ok(m) if m.is_file() => set_mode(
            &OpenOptions::new().read(true).open(to)?,
            permission_bits(&m),
        ),
        _ => Ok(()),
    }
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
        atomic_write(&p, b"1", NewMode::Private).unwrap();
        atomic_write(&p, b"2", NewMode::Private).unwrap();
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
        atomic_write(&p, b"new", NewMode::Private).unwrap();
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
        atomic_write(&link, b"x", NewMode::Private).unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().is_file());
        use std::os::unix::fs::PermissionsExt;
        let link_mode = fs::metadata(&link).unwrap().permissions().mode() & 0o7777;
        assert_eq!(link_mode, NEW_FILE_MODE);
        assert_eq!(fs::read(&victim).unwrap(), b"precious");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_keeps_the_replaced_mode() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        let t = TempDir::new();
        let fresh = t.path().join("fresh");
        atomic_write(&fresh, b"1", NewMode::Private).unwrap();
        assert_eq!(mode(&fresh), NEW_FILE_MODE);
        for kept in [0o640, 0o444, 0o755, 0o4755] {
            let p = t.path().join(format!("kept-{kept:o}"));
            write(&p, "old");
            fs::set_permissions(&p, fs::Permissions::from_mode(kept)).unwrap();
            atomic_write(&p, b"new", NewMode::Private).unwrap();
            assert_eq!(mode(&p), kept & 0o777);
            assert_eq!(fs::read(&p).unwrap(), b"new");
        }
    }

    #[cfg(unix)]
    #[test]
    fn keep_mode_drops_special_bits_and_ignores_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        let t = TempDir::new();
        let suid = t.path().join("suid");
        write(&suid, "x");
        fs::set_permissions(&suid, fs::Permissions::from_mode(0o4750)).unwrap();
        let (tmp, _, _) = copy_hashing_to_temp(&suid, t.path(), ".t-").unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600)).unwrap();
        keep_mode(&suid, &tmp).unwrap();
        assert_eq!(mode(&tmp), 0o750);

        let link = t.path().join("link");
        std::os::unix::fs::symlink(&suid, &link).unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600)).unwrap();
        keep_mode(&link, &tmp).unwrap();
        assert_eq!(mode(&tmp), 0o600);
    }

    #[test]
    fn copy_hashing_to_temp_leaves_nothing_on_failure() {
        let t = TempDir::new();
        let d = t.path().join("d");
        fs::create_dir_all(&d).unwrap();
        assert!(copy_hashing_to_temp(&t.path().join("absent"), &d, ".x-").is_err());
        assert_eq!(fs::read_dir(&d).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn umask_mode_follows_the_umask_for_new_files_only() {
        use crate::saves::testutil::under_umask_022;
        use std::os::unix::fs::PermissionsExt;
        if !under_umask_022("saves::fsx::tests::umask_mode_follows_the_umask_for_new_files_only") {
            return;
        }
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        let t = TempDir::new();
        let fresh = t.path().join("manifest.json");
        atomic_write(&fresh, b"1", NewMode::Umask).unwrap();
        assert_eq!(mode(&fresh), 0o644);
        let private = t.path().join("state.json");
        atomic_write(&private, b"1", NewMode::Private).unwrap();
        assert_eq!(mode(&private), NEW_FILE_MODE);
        fs::set_permissions(&fresh, fs::Permissions::from_mode(0o640)).unwrap();
        atomic_write(&fresh, b"2", NewMode::Umask).unwrap();
        assert_eq!(mode(&fresh), 0o640);
    }

    #[test]
    fn atomic_write_leaves_no_temp_on_failure() {
        let t = TempDir::new();
        let p = t.path().join("dir-in-the-way");
        fs::create_dir_all(p.join("child")).unwrap();
        assert!(atomic_write(&p, b"x", NewMode::Private).is_err());
        let names: Vec<_> = fs::read_dir(t.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("dir-in-the-way")]);
    }
}

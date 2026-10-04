//! Game-save capture/restore primitives, independent of the backup seam.
//!
//! - [`sources`] discovers each game's saves on this host (Steam userdata +
//!   Proton prefix, Heroic / umu wine prefixes, operator-configured native
//!   dirs) under a host-independent game id, as one or more [`GameRoot`] parts.
//! - [`walk`] enumerates the save-bearing files under a root (never a whole
//!   wine prefix).
//! - [`manifest`] copies those files into a payload with mtimes preserved and
//!   records `{relpath, size, mtime, sha256}` per part plus the writer host.
//! - [`restore`] puts a payload back newest-wins: a newer local file is kept
//!   and the incoming copy lands beside it as a conflict file.
//!
//! The manifest + [`restore::decide`] are the building blocks for cross-host
//! save sync, so they carry no backup-seam types.

pub mod manifest;
pub mod restore;
pub mod sources;
pub mod walk;

use std::path::PathBuf;

/// Which files under a [`GameRoot`] are saves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    /// Every regular file under the root.
    All,
    /// A wine user dir (`drive_c/users/<user>`): only the save-bearing subtrees.
    WineUser,
}

/// One save root of a game on this host (a Steam game has a userdata part and
/// a Proton-prefix part).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameRoot {
    /// Host-independent part id (`userdata/<steamid>`, `proton`, `prefix`,
    /// `home/<$HOME-relative dir>`). Doubles as the part's directory in a
    /// payload, so it is always a relative path of normal components.
    pub part: String,
    /// Directory the manifest's relpaths are relative to.
    pub root: PathBuf,
    pub filter: Filter,
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, SystemTime};

    /// Self-deleting scratch dir (raccoon carries no `tempfile` dep).
    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "raccoon-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p.canonicalize().unwrap())
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            if let Err(e) = std::fs::remove_dir_all(&self.0) {
                eprintln!("leaked {}: {e}", self.0.display());
            }
        }
    }

    pub fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    pub fn set_mtime_secs(path: &Path, secs: u64) {
        std::fs::File::open(path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }
}

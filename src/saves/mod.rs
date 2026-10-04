//! Game-save capture/restore primitives, independent of the backup seam.
//!
//! - [`ludusavi`] provisions and drives Ludusavi, which knows (via
//!   PCGamingWiki) where each game keeps its saves on this host.
//! - [`layout`] maps each found file to a host-independent `<part>:<rel>` key
//!   (`wine-user:AppData/...`, `steam-userdata:<appid>/...`, `home:...`).
//! - [`select`] applies orca's exclusions (caches, shared registry hives,
//!   escaping symlinks, restore artifacts) on top of ludusavi's.
//! - [`manifest`] copies the files into a payload with mtimes preserved and
//!   records `{relpath, size, mtime, sha256}` per part plus title + writer host.
//! - [`restore`] puts a payload back newest-wins: a newer local file is kept
//!   and the incoming copy lands beside it as a conflict file.
//!
//! The manifest + [`restore::decide`] are the building blocks for cross-host
//! save sync, so they carry no backup-seam types.

pub mod layout;
pub mod ludusavi;
pub mod manifest;
pub mod restore;
pub mod select;

use plugin_toolkit::hash::sha256_hex;

/// The backup instance for a ludusavi game title: identical on every host.
/// Lowercase `[a-z0-9-]`; when the slug drops more than case and spaces
/// (punctuation, accents), a short title hash keeps distinct titles distinct
/// — a pure function of the title, so hosts never disagree.
pub fn game_id(title: &str) -> String {
    let mut slug = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-').to_string();
    let plain = title.to_ascii_lowercase().replace(' ', "-");
    if !slug.is_empty() && plain == slug {
        slug
    } else {
        let hash = &sha256_hex(title.as_bytes())[..8];
        if slug.is_empty() {
            format!("game-{hash}")
        } else {
            format!("{slug}-{hash}")
        }
    }
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

#[cfg(test)]
mod tests {
    use super::game_id;

    #[test]
    fn game_ids_are_stable_and_lossy_titles_are_hashed() {
        assert_eq!(game_id("Hades"), "hades");
        assert_eq!(game_id("Alan Wake 2"), "alan-wake-2");
        assert_eq!(game_id("METAL SLUG 3"), "metal-slug-3");
        let bg3 = game_id("Baldur's Gate 3");
        assert!(bg3.starts_with("baldur-s-gate-3-") && bg3.len() == "baldur-s-gate-3-".len() + 8);
        assert_ne!(game_id("Foo: Bar"), game_id("Foo Bar"));
        assert_eq!(game_id("Foo: Bar"), game_id("Foo: Bar"));
        assert!(game_id("ドラゴン").starts_with("game-"));
        assert!(game_id("Hades  II").starts_with("hades-ii-"));
    }
}

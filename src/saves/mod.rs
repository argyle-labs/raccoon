//! Game-save capture/restore primitives, independent of the backup seam.
//!
//! - [`ludusavi`] provisions and drives Ludusavi, which knows (via
//!   PCGamingWiki) where each game keeps its saves on this host.
//! - [`layout`] maps each found file to a host-independent `<part>:<rel>` key
//!   (`wine-user:AppData/...`, `steam-userdata:<appid>/...`, `home:...`).
//! - [`select`] applies orca's exclusions (caches, shared registry hives,
//!   Steam Cloud files, escaping symlinks, restore artifacts) on top of
//!   ludusavi's.
//! - [`state`] keeps this host's per-game sync base, last manifest and blobs.
//! - [`merge`] plans a backup as the latest state with local progress laid
//!   over it; [`manifest`] writes it, recording `{relpath, size, mtime,
//!   sha256}` per part plus title + writer host.
//! - [`restore`] puts a payload back three-way against the base, never
//!   overwriting local progress; [`guard`] and [`running`] refuse unsafe
//!   destinations and running games.
//!
//! None of it depends on the backup seam, so `backup.sync` and any later sync
//! transport build on the same pieces.

pub mod fsx;
pub mod guard;
pub mod layout;
pub mod ludusavi;
pub mod manifest;
pub mod merge;
pub mod restore;
pub mod running;
pub mod select;
pub mod state;

use std::collections::BTreeMap;

use plugin_toolkit::hash::sha256_hex;

/// The backup instance for a ludusavi game title: identical on every host.
/// Lowercase `[a-z0-9-]`; when the slug drops more than case and spaces
/// (punctuation, accents), a short title hash keeps distinct titles distinct.
/// Titles differing only in case are disambiguated by [`assign_ids`].
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

/// Instance ids for a host's titles. Titles that [`game_id`] maps to the same
/// id (case-only differences) each get their title hash appended instead.
pub fn assign_ids<'a>(titles: impl IntoIterator<Item = &'a str>) -> BTreeMap<String, String> {
    let mut by_id: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for t in titles {
        by_id.entry(game_id(t)).or_default().push(t);
    }
    let mut out = BTreeMap::new();
    for (id, titles) in by_id {
        if let [only] = titles.as_slice() {
            out.insert(id, only.to_string());
            continue;
        }
        plugin_toolkit::tracing::warn!(
            "[game-saves] titles {titles:?} share instance `{id}`; suffixing each with its title hash"
        );
        for t in titles {
            out.insert(
                format!("{id}-{}", &sha256_hex(t.as_bytes())[..8]),
                t.to_string(),
            );
        }
    }
    out
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

    #[test]
    fn case_only_collisions_are_disambiguated() {
        let ids = super::assign_ids(["Foo Bar", "FOO BAR", "Hades"]);
        assert_eq!(ids.len(), 3);
        assert_eq!(ids["hades"], "Hades");
        assert!(ids.keys().filter(|k| k.starts_with("foo-bar-")).count() == 2);
    }
}

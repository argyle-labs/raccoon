//! Enumerate the save files under a [`GameRoot`](super::GameRoot).

use std::fs;
use std::path::{Component, Path, PathBuf};

use super::Filter;

/// Subtrees of a wine user dir that hold saves. Everything else in a prefix
/// (`drive_c/windows`, `Program Files`, …) is reinstallable and never copied.
pub const WINE_SAVE_SUBTREES: &[&str] = &[
    "AppData/Roaming",
    "AppData/Local",
    "AppData/LocalLow",
    "Documents",
    "My Documents",
    "Saved Games",
];

/// Dir names under `AppData/Local` that are caches, not saves.
pub const LOCAL_CACHE_DIRS: &[&str] = &[
    "Cache",
    "cache",
    "ShaderCache",
    "D3DSCache",
    "NVIDIA",
    "Temp",
    "CrashDumps",
    "CEF",
];

/// The files under `root` that `filter` selects, as root-relative paths
/// (sorted). Unreadable entries are skipped and described in the second vec
/// so one bad dir never sinks the whole capture.
pub fn save_files(root: &Path, filter: Filter) -> (Vec<PathBuf>, Vec<String>) {
    let mut w = Walker::new(root, filter, false);
    w.run();
    w.files.sort();
    w.files.dedup();
    (w.files, w.skipped)
}

/// Whether `root` holds at least one save file; stops at the first hit.
pub fn has_save_files(root: &Path, filter: Filter) -> bool {
    let mut w = Walker::new(root, filter, true);
    w.run();
    !w.files.is_empty()
}

/// Restore's own artifacts. Excluded so a conflict copy or an interrupted temp
/// never gets backed up and fanned out to the other hosts as a "save".
fn is_orca_artifact(name: &str) -> bool {
    name.contains(".orca-conflict-") || (name.starts_with('.') && name.contains(".orca-restore-"))
}

struct Walker<'a> {
    root: &'a Path,
    canon_root: Option<PathBuf>,
    filter: Filter,
    first_only: bool,
    files: Vec<PathBuf>,
    skipped: Vec<String>,
}

impl<'a> Walker<'a> {
    fn new(root: &'a Path, filter: Filter, first_only: bool) -> Self {
        Self {
            root,
            canon_root: root.canonicalize().ok(),
            filter,
            first_only,
            files: Vec::new(),
            skipped: Vec::new(),
        }
    }

    fn done(&self) -> bool {
        self.first_only && !self.files.is_empty()
    }

    fn run(&mut self) {
        if self.canon_root.is_none() {
            return;
        }
        let starts: Vec<PathBuf> = match self.filter {
            Filter::All => vec![PathBuf::new()],
            Filter::WineUser => WINE_SAVE_SUBTREES.iter().map(PathBuf::from).collect(),
        };
        for start in starts {
            // Absent subtrees are skipped, and so are symlinked ones (wine's
            // `Documents → ~/Documents`) so the user's home never rides along.
            if fs::symlink_metadata(self.root.join(&start)).is_ok_and(|m| m.is_dir()) {
                self.walk(&start);
            }
            if self.done() {
                return;
            }
        }
    }

    fn walk(&mut self, rel: &Path) {
        let dir = self.root.join(rel);
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                self.skipped.push(format!("{}: {e}", dir.display()));
                return;
            }
        };
        for entry in rd {
            if self.done() {
                return;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    self.skipped.push(format!("{}: {e}", dir.display()));
                    continue;
                }
            };
            let name = entry.file_name();
            if is_orca_artifact(&name.to_string_lossy()) {
                continue;
            }
            let child_rel = rel.join(&name);
            let Ok(ft) = entry.file_type() else {
                self.skipped
                    .push(format!("{}: unreadable type", entry.path().display()));
                continue;
            };
            if ft.is_dir() {
                if self.filter == Filter::WineUser && is_local_cache(&child_rel) {
                    continue;
                }
                self.walk(&child_rel);
            } else if ft.is_file() {
                self.files.push(child_rel);
            } else if ft.is_symlink() {
                // Dir links are skipped (their in-root contents are reached by
                // the real path; following them risks loops). File links are
                // kept only when they resolve inside the root.
                let canon_root = self.canon_root.as_deref().unwrap_or(self.root);
                if let Ok(t) = entry.path().canonicalize()
                    && t.starts_with(canon_root)
                    && t.is_file()
                {
                    self.files.push(child_rel);
                }
            }
        }
    }
}

fn is_local_cache(rel: &Path) -> bool {
    let mut comps = rel.components().filter_map(|c| match c {
        Component::Normal(s) => s.to_str(),
        _ => None,
    });
    if comps.next() != Some("AppData") || comps.next() != Some("Local") {
        return false;
    }
    comps.any(|c| LOCAL_CACHE_DIRS.contains(&c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saves::testutil::{TempDir, write};

    #[test]
    fn wine_filter_keeps_save_subtrees_and_drops_caches_and_system() {
        let t = TempDir::new();
        let u = t.path();
        for p in [
            "AppData/Roaming/Game/save.dat",
            "AppData/Local/Game/Saved/slot1.sav",
            "AppData/Local/Game/Cache/blob.bin",
            "AppData/Local/NVIDIA/DXCache/x.bin",
            "AppData/Local/Temp/t.tmp",
            "AppData/Local/D3DSCache/d.bin",
            "AppData/LocalLow/Dev/Game/prefs.json",
            "AppData/LocalLow/Dev/Game/Cache/kept.bin",
            "Documents/My Games/Game/save1",
            "Saved Games/Game/s.sav",
            "Desktop/shortcut.lnk",
            "Temp/root-temp",
        ] {
            write(&u.join(p), "x");
        }
        let (files, skipped) = save_files(u, Filter::WineUser);
        assert!(skipped.is_empty());
        let got: Vec<String> = files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            got,
            vec![
                "AppData/Local/Game/Saved/slot1.sav",
                "AppData/LocalLow/Dev/Game/Cache/kept.bin",
                "AppData/LocalLow/Dev/Game/prefs.json",
                "AppData/Roaming/Game/save.dat",
                "Documents/My Games/Game/save1",
                "Saved Games/Game/s.sav",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_escaping_the_root_are_skipped() {
        let t = TempDir::new();
        let outside = t.path().join("outside");
        write(&outside.join("secret.txt"), "s");
        let root = t.path().join("root");
        write(&root.join("AppData/Roaming/real.sav"), "r");
        std::os::unix::fs::symlink(&outside, root.join("Documents")).unwrap();
        std::os::unix::fs::symlink(
            outside.join("secret.txt"),
            root.join("AppData/Roaming/leak"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            root.join("AppData/Roaming/real.sav"),
            root.join("AppData/Roaming/inner"),
        )
        .unwrap();
        let (files, _) = save_files(&root, Filter::WineUser);
        let got: Vec<String> = files
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            got,
            vec!["AppData/Roaming/inner", "AppData/Roaming/real.sav"]
        );
    }

    #[test]
    fn orca_artifacts_are_never_saves_and_probe_short_circuits() {
        let t = TempDir::new();
        write(&t.path().join("g/save.sav"), "s");
        write(
            &t.path().join("g/save.sav.orca-conflict-20261004T000000Z"),
            "c",
        );
        write(&t.path().join("g/.save.sav.orca-restore-42"), "tmp");
        let (files, _) = save_files(t.path(), Filter::All);
        assert_eq!(files, vec![PathBuf::from("g/save.sav")]);
        assert!(has_save_files(t.path(), Filter::All));
        assert!(!has_save_files(&t.path().join("nope"), Filter::All));
        let wine = t.path().join("wine");
        write(&wine.join("AppData/Local/Game/Cache/only-cache.bin"), "c");
        write(&wine.join("windows/system32/k.dll"), "d");
        assert!(!has_save_files(&wine, Filter::WineUser));
    }

    #[test]
    fn missing_root_yields_nothing() {
        let t = TempDir::new();
        let (files, skipped) = save_files(&t.path().join("nope"), Filter::All);
        assert!(files.is_empty() && skipped.is_empty());
    }
}

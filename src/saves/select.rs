//! Turn the absolute save paths ludusavi reports into portable source files,
//! applying orca's own exclusions on top of ludusavi's.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use super::layout::{Layout, WINE_USER};
use super::manifest;

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

/// One save file to capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    pub part: &'static str,
    pub rel: PathBuf,
    pub abs: PathBuf,
}

#[derive(Debug, Default)]
pub struct Selection {
    pub files: Vec<SourceFile>,
    /// Paths left out, with why.
    pub skipped: Vec<String>,
    /// Every local anchor seen per part — where restore puts that part back.
    pub anchors: BTreeMap<&'static str, BTreeSet<PathBuf>>,
}

pub fn select(layout: &Layout, paths: impl IntoIterator<Item = PathBuf>) -> Selection {
    let mut sel = Selection::default();
    let mut chosen: BTreeMap<(&'static str, PathBuf), (i64, PathBuf)> = BTreeMap::new();
    for abs in paths {
        let name = abs.file_name().unwrap_or_default().to_string_lossy();
        if is_orca_artifact(&name) {
            continue;
        }
        let Some(p) = layout.classify(&abs) else {
            sel.skipped
                .push(format!("{}: outside known roots", abs.display()));
            continue;
        };
        sel.anchors
            .entry(p.part)
            .or_default()
            .insert(p.anchor.clone());
        if let Some(why) = excluded(p.part, &p.anchor, &p.rel, &abs) {
            sel.skipped.push(format!("{}: {why}", abs.display()));
            continue;
        }
        let Ok(mtime) = manifest::mtime_ns(&abs) else {
            sel.skipped.push(format!("{}: unreadable", abs.display()));
            continue;
        };
        // The same portable key from two local anchors (a game found in two
        // prefixes): newest wins, as it would across hosts.
        let key = (p.part, p.rel);
        match chosen.get(&key) {
            Some((m, prev)) if *m >= mtime => {
                sel.skipped.push(format!(
                    "{}: older duplicate of {}",
                    abs.display(),
                    prev.display()
                ));
            }
            Some((_, prev)) => {
                sel.skipped.push(format!(
                    "{}: older duplicate of {}",
                    prev.display(),
                    abs.display()
                ));
                chosen.insert(key, (mtime, abs));
            }
            None => {
                chosen.insert(key, (mtime, abs));
            }
        }
    }
    sel.files = chosen
        .into_iter()
        .map(|((part, rel), (_, abs))| SourceFile { part, rel, abs })
        .collect();
    sel
}

/// Restore's own artifacts. Excluded so a conflict copy or an interrupted temp
/// never gets backed up and fanned out to the other hosts as a "save".
pub fn is_orca_artifact(name: &str) -> bool {
    name.contains(".orca-conflict-") || (name.starts_with('.') && name.contains(".orca-restore-"))
}

fn excluded(part: &str, anchor: &Path, rel: &Path, abs: &Path) -> Option<&'static str> {
    if part == WINE_USER && is_local_cache(rel) {
        return Some("cache");
    }
    // A prefix's registry hive serves every game in it (Heroic's shared
    // `default`); restoring one game's copy would roll back all the others.
    if abs.extension().is_some_and(|e| e == "reg")
        && abs.parent().is_some_and(|d| d.join("drive_c").is_dir())
    {
        return Some("wine registry hive");
    }
    let meta = fs::symlink_metadata(abs).ok()?;
    if meta.file_type().is_symlink() {
        let inside = match (abs.canonicalize(), anchor.canonicalize()) {
            (Ok(t), Ok(a)) => t.starts_with(a) && t.is_file(),
            _ => false,
        };
        if !inside {
            return Some("symlink leaves its root");
        }
    } else if !meta.is_file() {
        return Some("not a regular file");
    }
    None
}

fn is_local_cache(rel: &Path) -> bool {
    let mut comps = rel.components().filter_map(|c| match c {
        Component::Normal(s) => s.to_str(),
        _ => None,
    });
    if comps.next() != Some("AppData") || comps.next() != Some("Local") {
        return false;
    }
    let mut rest: Vec<&str> = comps.collect();
    rest.pop(); // the file name itself
    rest.iter().any(|c| LOCAL_CACHE_DIRS.contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saves::testutil::{TempDir, set_mtime_secs, write};

    #[test]
    fn exclusions_and_newest_duplicate() {
        let t = TempDir::new();
        let h = t.path();
        let a = h.join("Games/A/drive_c/users/steamuser");
        let b = h.join("Games/B/drive_c/users/skey");
        let files = [
            a.join("AppData/Roaming/G/save.dat"),
            a.join("AppData/Local/G/Cache/blob.bin"),
            a.join("AppData/Local/NVIDIA/x.bin"),
            a.join("AppData/LocalLow/G/Cache/kept.bin"),
            a.join("AppData/Roaming/G/save.dat.orca-conflict-20261004T000000Z"),
            h.join("Games/A/user.reg"),
            b.join("AppData/Roaming/G/save.dat"),
            h.join(".config/G/s"),
        ];
        for f in &files {
            write(f, "x");
        }
        set_mtime_secs(&files[0], 100);
        set_mtime_secs(&files[6], 200);
        let mut all = files.to_vec();
        all.push(PathBuf::from("/opt/elsewhere/s"));
        let sel = select(&Layout::detect(h), all);
        let got: Vec<(String, String, PathBuf)> = sel
            .files
            .iter()
            .map(|f| {
                (
                    f.part.to_string(),
                    f.rel.display().to_string(),
                    f.abs.clone(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("home".into(), ".config/G/s".into(), files[7].clone()),
                (
                    "wine-user".into(),
                    "AppData/LocalLow/G/Cache/kept.bin".into(),
                    files[3].clone()
                ),
                (
                    "wine-user".into(),
                    "AppData/Roaming/G/save.dat".into(),
                    files[6].clone()
                ),
            ]
        );
        assert_eq!(sel.skipped.len(), 5, "{:?}", sel.skipped);
        assert_eq!(sel.anchors[WINE_USER].len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_escaping_the_anchor_are_skipped() {
        let t = TempDir::new();
        let h = t.path();
        write(&h.join("outside/secret"), "s");
        let u = h.join("Games/A/drive_c/users/steamuser");
        write(&u.join("Documents/real.sav"), "r");
        std::os::unix::fs::symlink(h.join("outside/secret"), u.join("Documents/leak")).unwrap();
        std::os::unix::fs::symlink(u.join("Documents/real.sav"), u.join("Documents/inner"))
            .unwrap();
        let sel = select(
            &Layout::detect(h),
            [
                u.join("Documents/real.sav"),
                u.join("Documents/leak"),
                u.join("Documents/inner"),
            ],
        );
        let rels: Vec<String> = sel
            .files
            .iter()
            .map(|f| f.rel.display().to_string())
            .collect();
        assert_eq!(rels, vec!["Documents/inner", "Documents/real.sav"]);
    }
}

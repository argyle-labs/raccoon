//! Turn the absolute save paths ludusavi reports into portable source files,
//! applying orca's own exclusions on top of ludusavi's.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Component, Path, PathBuf};

use super::layout::{HOME, Layout, STEAM_COMMON, STEAM_USERDATA, WINE_USER};
use super::ludusavi::ScanFile;
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

/// Marks a dir Steam Auto-Cloud syncs on its own.
pub const STEAM_AUTOCLOUD: &str = "steam_autocloud.vdf";

/// One local save file to capture.
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
    /// Home-relative paths of every `home` file found, excluded or not: the
    /// save dirs a `home:` restore may write into.
    pub home_rels: Vec<PathBuf>,
    /// Wine registry hives found and left out.
    pub registry_hives: usize,
}

impl Selection {
    /// Paths whose use by a running process means the game is running: its
    /// prefixes, install dirs and `home` save dirs.
    pub fn guards(&self, home: &Path) -> Vec<PathBuf> {
        let mut out = BTreeSet::new();
        for (part, anchors) in &self.anchors {
            if !matches!(*part, STEAM_USERDATA | STEAM_COMMON | HOME) {
                anchors
                    .iter()
                    .for_each(|a| out.extend(super::layout::prefix_dirs(a)));
            }
        }
        for f in self.files.iter().filter(|f| f.part == STEAM_COMMON) {
            if let Some(install) = f
                .abs
                .ancestors()
                .find(|a| a.parent().is_some_and(|p| p.ends_with("steamapps/common")))
            {
                out.insert(install.to_path_buf());
            }
        }
        for rel in &self.home_rels {
            if let Some(dir) = rel.parent()
                && dir.components().count() > 0
            {
                out.insert(home.join(dir));
            }
        }
        out.into_iter().collect()
    }
}

pub fn select(layout: &Layout, found: &[ScanFile]) -> Selection {
    let mut sel = Selection::default();
    let mut autocloud: HashMap<PathBuf, bool> = HashMap::new();
    let mut chosen: BTreeMap<(&'static str, PathBuf), (i64, PathBuf)> = BTreeMap::new();
    for f in found {
        let abs = &f.path;
        let name = abs.file_name().unwrap_or_default().to_string_lossy();
        if is_orca_artifact(&name) {
            continue;
        }
        let Some(p) = layout.classify(abs) else {
            sel.skipped
                .push(format!("{}: outside known roots", abs.display()));
            continue;
        };
        sel.anchors
            .entry(p.part)
            .or_default()
            .insert(p.anchor.clone());
        if !f.duplicated_by.is_empty() {
            sel.skipped.push(format!(
                "{}: also claimed by {}",
                abs.display(),
                f.duplicated_by.join(", ")
            ));
            continue;
        }
        if is_registry_hive(abs) {
            sel.registry_hives += 1;
            sel.skipped
                .push(format!("{}: wine registry hive", abs.display()));
            continue;
        }
        if p.part == HOME {
            sel.home_rels.push(p.rel.clone());
        }
        if let Some(why) = excluded(p.part, &p.anchor, &p.rel, abs, &mut autocloud) {
            sel.skipped.push(format!("{}: {why}", abs.display()));
            continue;
        }
        let Ok(mtime) = manifest::mtime_ns(abs) else {
            sel.skipped.push(format!("{}: unreadable", abs.display()));
            continue;
        };
        // The same portable key from two local anchors (a game found in two
        // prefixes): newest wins.
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
                chosen.insert(key, (mtime, abs.clone()));
            }
            None => {
                chosen.insert(key, (mtime, abs.clone()));
            }
        }
    }
    sel.files = chosen
        .into_iter()
        .map(|((part, rel), (_, abs))| SourceFile { part, rel, abs })
        .collect();
    sel
}

/// Restore's own artifacts. Excluded so a conflict copy, a moved-aside
/// original or an interrupted temp never gets backed up and fanned out to the
/// other hosts as a "save".
pub fn is_orca_artifact(name: &str) -> bool {
    name.contains(".orca-conflict-")
        || name.contains(".orca-replaced-")
        || (name.starts_with('.') && name.contains(".orca-restore-"))
}

/// A prefix's registry hive serves every game in it (Heroic's shared
/// `default`); restoring one game's copy would roll back all the others.
fn is_registry_hive(abs: &Path) -> bool {
    abs.extension().is_some_and(|e| e == "reg")
        && abs.parent().is_some_and(|d| d.join("drive_c").is_dir())
}

fn excluded(
    part: &str,
    anchor: &Path,
    rel: &Path,
    abs: &Path,
    autocloud: &mut HashMap<PathBuf, bool>,
) -> Option<&'static str> {
    if part == WINE_USER && is_local_cache(rel) {
        return Some("cache");
    }
    // Steam Cloud already syncs these between hosts; a second sync fights it
    // and surfaces as Steam cloud-conflict dialogs.
    if part == STEAM_USERDATA
        && rel
            .components()
            .nth(1)
            .is_some_and(|c| c.as_os_str() == "remote")
    {
        return Some("Steam Cloud (remote/)");
    }
    if abs.file_name().is_some_and(|n| n == STEAM_AUTOCLOUD) {
        return Some("Steam Auto-Cloud marker");
    }
    if under_autocloud(anchor, abs, autocloud) {
        return Some("Steam Auto-Cloud dir");
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

/// Whether a dir between `anchor` and `abs` holds Steam's Auto-Cloud marker.
fn under_autocloud(anchor: &Path, abs: &Path, seen: &mut HashMap<PathBuf, bool>) -> bool {
    for dir in abs.ancestors().skip(1) {
        if !dir.starts_with(anchor) {
            break;
        }
        let marked = *seen
            .entry(dir.to_path_buf())
            .or_insert_with(|| dir.join(STEAM_AUTOCLOUD).is_file());
        if marked {
            return true;
        }
    }
    false
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
    rest.pop();
    rest.iter().any(|c| LOCAL_CACHE_DIRS.contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saves::testutil::{TempDir, set_mtime_secs, write};

    fn scan(paths: &[PathBuf]) -> Vec<ScanFile> {
        paths
            .iter()
            .map(|p| ScanFile {
                path: p.clone(),
                duplicated_by: Vec::new(),
            })
            .collect()
    }

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
            a.join("AppData/Roaming/G/save.dat.orca-replaced-20261004T000000Z"),
            h.join("Games/A/user.reg"),
            b.join("AppData/Roaming/G/save.dat"),
            h.join(".config/G/s"),
        ];
        for f in &files {
            write(f, "x");
        }
        set_mtime_secs(&files[0], 100);
        set_mtime_secs(&files[7], 200);
        let mut all = files.to_vec();
        all.push(PathBuf::from("/opt/elsewhere/s"));
        let mut found = scan(&all);
        let dup = h.join(".config/Shared/s");
        write(&dup, "d");
        found.push(ScanFile {
            path: dup,
            duplicated_by: vec!["Other Game".into()],
        });
        let sel = select(&Layout::detect(h), &found);
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
                ("home".into(), ".config/G/s".into(), files[8].clone()),
                (
                    "wine-user".into(),
                    "AppData/LocalLow/G/Cache/kept.bin".into(),
                    files[3].clone()
                ),
                (
                    "wine-user".into(),
                    "AppData/Roaming/G/save.dat".into(),
                    files[7].clone()
                ),
            ]
        );
        assert_eq!(sel.skipped.len(), 6, "{:?}", sel.skipped);
        assert_eq!(sel.registry_hives, 1);
        assert_eq!(sel.anchors[WINE_USER].len(), 2);
        assert_eq!(sel.home_rels, vec![PathBuf::from(".config/G/s")]);
    }

    #[test]
    fn steam_cloud_managed_files_are_left_to_steam() {
        let t = TempDir::new();
        let h = t.path();
        let ud = h.join(".local/share/Steam/userdata/64751656");
        let u = h.join(".local/share/Steam/steamapps/compatdata/9/pfx/drive_c/users/steamuser");
        std::fs::create_dir_all(&ud).unwrap();
        let files = [
            ud.join("440/remote/cloud.sav"),
            ud.join("440/local.cfg"),
            u.join("AppData/Roaming/Auto/steam_autocloud.vdf"),
            u.join("AppData/Roaming/Auto/slot/1.sav"),
            u.join("AppData/Roaming/Plain/1.sav"),
        ];
        for f in &files {
            write(f, "x");
        }
        let sel = select(&Layout::detect(h), &scan(&files));
        let rels: Vec<String> = sel
            .files
            .iter()
            .map(|f| f.rel.display().to_string())
            .collect();
        assert_eq!(rels, vec!["440/local.cfg", "AppData/Roaming/Plain/1.sav"]);
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
            &scan(&[
                u.join("Documents/real.sav"),
                u.join("Documents/leak"),
                u.join("Documents/inner"),
            ]),
        );
        let rels: Vec<String> = sel
            .files
            .iter()
            .map(|f| f.rel.display().to_string())
            .collect();
        assert_eq!(rels, vec!["Documents/inner", "Documents/real.sav"]);
    }
}

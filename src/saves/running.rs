//! Is a game running? Restoring under a live game loses the write race (it
//! overwrites on exit) and capturing mid-save tears files, so both refuse.
//!
//! Unprivileged `/proc` scan of this user's processes. A process counts as the
//! game when, for any guard path (the game's wine prefix, install dir or save
//! dirs):
//! - its cwd or executable lies under it,
//! - its command line mentions it (wine's `Z:\...` paths included), or
//! - its `WINEPREFIX` / `STEAM_COMPAT_DATA_PATH` names it.
//!
//! Limits: a native game whose cwd, binary and arguments sit outside its save
//! dirs isn't seen; other users' processes are unreadable; a `wineserver`
//! lingering a few seconds after exit, or any other game in a shared prefix
//! (Heroic's `default`), also counts as busy.

use std::fs;
use std::path::{Path, PathBuf};

pub fn busy(guards: &[PathBuf]) -> Option<String> {
    busy_in(Path::new("/proc"), guards, std::process::id())
}

pub fn busy_in(proc_root: &Path, guards: &[PathBuf], me: u32) -> Option<String> {
    let guards: Vec<PathBuf> = guards
        .iter()
        .flat_map(|g| {
            let mut v = vec![g.clone()];
            if let Ok(c) = g.canonicalize()
                && c != *g
            {
                v.push(c);
            }
            v
        })
        .filter(|g| g.components().count() >= 3)
        .collect();
    if guards.is_empty() {
        return None;
    }
    let rd = fs::read_dir(proc_root).ok()?;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        if pid == me {
            continue;
        }
        let dir = e.path();
        if let Some(hit) = matches(&dir, &guards) {
            let comm = fs::read_to_string(dir.join("comm")).unwrap_or_default();
            return Some(format!(
                "pid {pid} ({}) uses {}",
                comm.trim(),
                hit.display()
            ));
        }
    }
    None
}

fn matches<'g>(dir: &Path, guards: &'g [PathBuf]) -> Option<&'g PathBuf> {
    for link in ["cwd", "exe"] {
        if let Ok(target) = fs::read_link(dir.join(link))
            && let Some(g) = guards.iter().find(|g| target.starts_with(g))
        {
            return Some(g);
        }
    }
    if let Ok(raw) = fs::read(dir.join("cmdline")) {
        let cmd = String::from_utf8_lossy(&raw)
            .replace('\0', " ")
            .replace('\\', "/");
        if let Some(g) = guards.iter().find(|g| cmd.contains(&*g.to_string_lossy())) {
            return Some(g);
        }
    }
    if let Ok(raw) = fs::read(dir.join("environ")) {
        for var in raw.split(|b| *b == 0) {
            let var = String::from_utf8_lossy(var);
            let Some(val) = var
                .strip_prefix("WINEPREFIX=")
                .or_else(|| var.strip_prefix("STEAM_COMPAT_DATA_PATH="))
            else {
                continue;
            };
            let val = Path::new(val.trim_end_matches('/'));
            if val.components().count() < 3 {
                continue;
            }
            if let Some(g) = guards
                .iter()
                .find(|g| val.starts_with(g) || g.starts_with(val))
            {
                return Some(g);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saves::testutil::{TempDir, write};

    #[cfg(unix)]
    #[test]
    fn detects_by_cwd_cmdline_and_wine_env() {
        let t = TempDir::new();
        let proc_root = t.path().join("proc");
        let prefix = t.path().join("home/skey/Games/Heroic/Prefixes/default");
        let compat = t
            .path()
            .join("home/skey/.local/share/Steam/steamapps/compatdata/1245620");
        let common = t
            .path()
            .join("home/skey/.local/share/Steam/steamapps/common/ELDEN RING");
        std::fs::create_dir_all(&prefix).unwrap();
        std::fs::create_dir_all(proc_root.join("1")).unwrap();
        write(&proc_root.join("1/comm"), "systemd\n");
        write(&proc_root.join("1/cmdline"), "/sbin/init\0");

        assert_eq!(busy_in(&proc_root, std::slice::from_ref(&prefix), 0), None);

        std::fs::create_dir_all(proc_root.join("42")).unwrap();
        write(&proc_root.join("42/comm"), "Hades.exe\n");
        std::os::unix::fs::symlink(prefix.join("drive_c"), proc_root.join("42/cwd")).unwrap();
        let hit = busy_in(&proc_root, std::slice::from_ref(&prefix), 0).unwrap();
        assert!(hit.starts_with("pid 42 (Hades.exe)"), "{hit}");
        assert_eq!(busy_in(&proc_root, std::slice::from_ref(&prefix), 42), None);

        write(
            &proc_root.join("77/cmdline"),
            &format!(
                "Z:{}\\eldenring.exe\0-flag\0",
                common.to_string_lossy().replace('/', "\\")
            ),
        );
        assert!(
            busy_in(&proc_root, &[common], 0)
                .unwrap()
                .starts_with("pid 77")
        );

        write(
            &proc_root.join("88/environ"),
            &format!("HOME=/x\0STEAM_COMPAT_DATA_PATH={}\0", compat.display()),
        );
        assert!(
            busy_in(&proc_root, &[compat.join("pfx")], 0)
                .unwrap()
                .starts_with("pid 88")
        );
    }
}

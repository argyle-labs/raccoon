//! Host-independent ("portable") names for save files, and the host roots
//! ludusavi is pointed at. Every path derives from `$HOME`, so the same code
//! serves Bazzite (`/var/home/<user>`) and other distros (`/home/<user>`).
//!
//! A save's portable key is `<part>:<rel>`, where `rel` is relative to the
//! part's local anchor:
//!
//! | part             | anchor                                   |
//! |------------------|------------------------------------------|
//! | `wine-user`      | `<prefix>/drive_c/users/<any user>`      |
//! | `wine-c`         | `<prefix>/drive_c` (e.g. `ProgramData`, `users/Public`) |
//! | `steam-userdata` | `<steam>/userdata/<steamid>`             |
//! | `steam-common`   | `<library>/steamapps/common`             |
//! | `home`           | `$HOME`                                  |
//!
//! The key carries no prefix name, steamid or wine user name, so the same file
//! keys identically on a host with per-game prefixes and on one with Heroic's
//! shared `default`. (Steam shortcut ids never reach a key: compatdata paths
//! classify as `wine-*`.)

use std::fs;
use std::path::{Component, Path, PathBuf};

pub const WINE_USER: &str = "wine-user";
pub const WINE_C: &str = "wine-c";
pub const STEAM_USERDATA: &str = "steam-userdata";
pub const STEAM_COMMON: &str = "steam-common";
pub const HOME: &str = "home";

/// This host's save-relevant roots.
#[derive(Debug, Clone)]
pub struct Layout {
    pub home: PathBuf,
    /// The Steam root first (if installed), then extra library folders.
    pub steam_libraries: Vec<PathBuf>,
}

/// A save file's portable placement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub part: &'static str,
    pub anchor: PathBuf,
    pub rel: PathBuf,
}

impl Layout {
    pub fn detect(home: &Path) -> Self {
        let steam_libraries = steam_root(home)
            .map(|s| steam_libraries(&s))
            .unwrap_or_default();
        Self {
            home: home.to_path_buf(),
            steam_libraries,
        }
    }

    pub fn steam_root(&self) -> Option<&Path> {
        self.steam_libraries.first().map(PathBuf::as_path)
    }

    /// Portable placement of `path`, or `None` when it lies outside every
    /// known root.
    pub fn classify(&self, path: &Path) -> Option<Placement> {
        if let Some(p) = classify_wine(path) {
            return Some(p);
        }
        for lib in &self.steam_libraries {
            for base in variants(lib) {
                if let Ok(rest) = path.strip_prefix(base.join("userdata")) {
                    let mut comps = rest.components();
                    if let Some(Component::Normal(sid)) = comps.next()
                        && is_numeric(&sid.to_string_lossy())
                    {
                        return placement(
                            STEAM_USERDATA,
                            base.join("userdata").join(sid),
                            comps.as_path(),
                        );
                    }
                }
                let common = base.join("steamapps/common");
                if let Ok(rest) = path.strip_prefix(&common) {
                    return placement(STEAM_COMMON, common, rest);
                }
            }
        }
        for base in variants(&self.home) {
            if let Ok(rest) = path.strip_prefix(&base) {
                return placement(HOME, base, rest);
            }
        }
        None
    }

    /// The local `steam/userdata/<steamid>` when exactly one Steam account has
    /// signed in on this host.
    pub fn sole_steam_user(&self) -> Option<PathBuf> {
        let userdata = self.steam_root()?.join("userdata");
        let ids: Vec<PathBuf> = sorted_dirs(&userdata)
            .into_iter()
            .filter(|(n, _)| is_numeric(n) && n != "0")
            .map(|(_, p)| p)
            .collect();
        match ids.as_slice() {
            [one] => Some(one.clone()),
            _ => None,
        }
    }

    /// The `steamapps/common` holding install dir `dir`, if installed here.
    pub fn steam_common_with(&self, dir: &str) -> Option<PathBuf> {
        self.steam_libraries
            .iter()
            .map(|l| l.join("steamapps/common"))
            .find(|c| c.join(dir).is_dir())
    }

    /// Wine prefixes under `~/Games` (Battle.net, umu, Ubisoft Connect, …)
    /// for ludusavi `otherWine` roots. ludusavi checks every game it knows
    /// against each such root, so `~/Games/Heroic` is left out: the `heroic`
    /// root already maps each Heroic game to its own prefix. A prefix is a dir
    /// with `drive_c`; umu/Proton-style `<dir>/pfx/drive_c` yields `<dir>/pfx`.
    pub fn wine_prefixes(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let games = self.home.join("Games");
        find_prefixes(&games, 3, &mut out);
        out.retain(|p| !p.starts_with(games.join("Heroic")));
        out
    }

    /// Whether Steam has app `appid` installed in any library.
    pub fn steam_app_installed(&self, appid: &str) -> bool {
        is_numeric(appid)
            && self.steam_libraries.iter().any(|l| {
                l.join(format!("steamapps/appmanifest_{appid}.acf"))
                    .is_file()
            })
    }
}

/// The `wine-c` anchor (`<prefix>/drive_c`) for a `wine-user` anchor.
pub fn drive_of_user(user_dir: &Path) -> Option<PathBuf> {
    user_dir.parent()?.parent().map(Path::to_path_buf)
}

/// The prefix's user dir under `drive`: `steamuser` (Proton/umu), else the
/// login name, else the first non-`Public` user.
pub fn user_of_drive(drive: &Path, home: &Path) -> Option<PathBuf> {
    let users = drive.join("users");
    let login = home.file_name().map(|n| n.to_string_lossy().into_owned());
    let names: Vec<String> = sorted_dirs(&users).into_iter().map(|(n, _)| n).collect();
    let pick = names
        .iter()
        .find(|n| *n == "steamuser")
        .or_else(|| names.iter().find(|n| Some(n.as_str()) == login.as_deref()))
        .or_else(|| names.iter().find(|n| *n != "Public"))?;
    Some(users.join(pick))
}

/// The prefix dir (holding `drive_c`) of an anchor inside it, plus Proton's
/// `compatdata/<appid>` when the prefix is its `pfx`.
pub fn prefix_dirs(anchor: &Path) -> Vec<PathBuf> {
    let Some(at) = anchor.ancestors().find(|a| a.ends_with("drive_c")) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = at.parent().into_iter().map(Path::to_path_buf).collect();
    if let Some(p) = out.first()
        && p.ends_with("pfx")
        && let Some(compat) = p.parent()
    {
        out.push(compat.to_path_buf());
    }
    out
}

fn classify_wine(path: &Path) -> Option<Placement> {
    let comps: Vec<Component> = path.components().collect();
    let at = comps.iter().rposition(|c| c.as_os_str() == "drive_c")?;
    let drive: PathBuf = comps[..=at].iter().collect();
    let rest: PathBuf = comps[at + 1..].iter().collect();
    let mut it = rest.components();
    if let (Some(Component::Normal(users)), Some(Component::Normal(user))) = (it.next(), it.next())
        && users == "users"
        && user != "Public"
    {
        return placement(WINE_USER, drive.join("users").join(user), it.as_path());
    }
    placement(WINE_C, drive, &rest)
}

fn placement(part: &'static str, anchor: PathBuf, rel: &Path) -> Option<Placement> {
    let normal = rel.components().all(|c| matches!(c, Component::Normal(_)));
    (normal && rel.components().next().is_some()).then(|| Placement {
        part,
        anchor,
        rel: rel.to_path_buf(),
    })
}

/// `p` as given plus its canonical form (Bazzite's `/home → /var/home`).
fn variants(p: &Path) -> Vec<PathBuf> {
    let mut v = vec![p.to_path_buf()];
    if let Ok(c) = p.canonicalize()
        && c != p
    {
        v.push(c);
    }
    v
}

fn find_prefixes(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    for (_, child) in sorted_dirs(dir) {
        if child.join("drive_c").is_dir() {
            out.push(child);
        } else if child.join("pfx/drive_c").is_dir() {
            out.push(child.join("pfx"));
        } else if depth > 0 {
            find_prefixes(&child, depth - 1, out);
        }
    }
}

pub fn steam_root(home: &Path) -> Option<PathBuf> {
    [".local/share/Steam", ".steam/steam"]
        .iter()
        .map(|r| home.join(r))
        .find(|p| p.is_dir())
}

/// The root plus every `"path"` in `libraryfolders.vdf`, deduped.
fn steam_libraries(steam: &Path) -> Vec<PathBuf> {
    let mut libs = vec![steam.to_path_buf()];
    if let Ok(vdf) = fs::read_to_string(steam.join("steamapps/libraryfolders.vdf")) {
        libs.extend(library_paths(&vdf).into_iter().map(PathBuf::from));
    }
    let mut seen = Vec::new();
    libs.retain(|p| {
        let c = p.canonicalize().unwrap_or_else(|_| p.clone());
        let fresh = !seen.contains(&c);
        seen.push(c);
        fresh
    });
    libs
}

/// `"path"  "<dir>"` values from a libraryfolders.vdf body.
fn library_paths(vdf: &str) -> Vec<String> {
    vdf.lines()
        .filter_map(|line| {
            let quoted: Vec<&str> = line.split('"').skip(1).step_by(2).collect();
            match quoted.as_slice() {
                ["path", dir] => Some(dir.replace("\\\\", "\\")),
                _ => None,
            }
        })
        .collect()
}

fn sorted_dirs(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = rd
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    out.sort();
    out
}

fn is_numeric(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saves::testutil::{TempDir, write};

    fn layout(t: &TempDir) -> Layout {
        let h = t.path();
        let steam = h.join(".local/share/Steam");
        let ext = h.join("ext-lib");
        fs::create_dir_all(steam.join("userdata/64751656")).unwrap();
        fs::create_dir_all(ext.join("steamapps/common")).unwrap();
        write(
            &steam.join("steamapps/libraryfolders.vdf"),
            &format!(
                "\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n\t\"1\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n}}\n",
                steam.display(),
                ext.display()
            ),
        );
        Layout::detect(h)
    }

    fn key(l: &Layout, p: &Path) -> Option<(String, String, String)> {
        l.classify(p).map(|c| {
            (
                c.part.to_string(),
                c.anchor
                    .strip_prefix(&l.home)
                    .unwrap()
                    .display()
                    .to_string(),
                c.rel.display().to_string(),
            )
        })
    }

    #[test]
    fn classify_maps_each_root_to_a_portable_part() {
        let t = TempDir::new();
        let l = layout(&t);
        let h = &l.home;
        assert_eq!(l.steam_libraries.len(), 2);
        let k = |p: &str| key(&l, &h.join(p));
        assert_eq!(
            k(
                ".local/share/Steam/steamapps/compatdata/2371341689/pfx/drive_c/users/steamuser/AppData/Roaming/G/s.sav"
            ),
            Some((
                "wine-user".into(),
                ".local/share/Steam/steamapps/compatdata/2371341689/pfx/drive_c/users/steamuser"
                    .into(),
                "AppData/Roaming/G/s.sav".into()
            ))
        );
        assert_eq!(
            k("Games/Heroic/Prefixes/default/drive_c/users/skey/Documents/G/a"),
            Some((
                "wine-user".into(),
                "Games/Heroic/Prefixes/default/drive_c/users/skey".into(),
                "Documents/G/a".into()
            ))
        );
        assert_eq!(
            k("Games/Battlenet/drive_c/users/Public/Documents/G/a").map(|x| (x.0, x.2)),
            Some(("wine-c".into(), "users/Public/Documents/G/a".into()))
        );
        assert_eq!(
            k(".local/share/Steam/userdata/64751656/440/remote/a").map(|x| (x.0, x.2)),
            Some(("steam-userdata".into(), "440/remote/a".into()))
        );
        assert_eq!(
            k("ext-lib/steamapps/common/Game/saves/a").map(|x| (x.0, x.1, x.2)),
            Some((
                "steam-common".into(),
                "ext-lib/steamapps/common".into(),
                "Game/saves/a".into()
            ))
        );
        assert_eq!(
            k(".config/StardewValley/Saves/F/F").map(|x| (x.0, x.2)),
            Some(("home".into(), ".config/StardewValley/Saves/F/F".into()))
        );
        assert_eq!(l.classify(Path::new("/etc/passwd")), None);
        assert_eq!(
            l.sole_steam_user(),
            Some(h.join(".local/share/Steam/userdata/64751656"))
        );
    }

    #[test]
    fn wine_prefixes_under_games() {
        let t = TempDir::new();
        let h = t.path();
        for p in [
            "Games/Battlenet/drive_c/windows",
            "Games/ubisoft-connect/drive_c/windows",
            "Games/umu/umu-default/pfx/drive_c/windows",
            "Games/Heroic/Prefixes/default/drive_c/windows",
            "Games/Heroic/Prefixes/Alan Wake 2/drive_c/windows",
            "Games/Heroic/Alan Wake 2/game.exe",
        ] {
            fs::create_dir_all(h.join(p)).unwrap();
        }
        let got: Vec<String> = Layout::detect(h)
            .wine_prefixes()
            .iter()
            .map(|p| p.strip_prefix(h).unwrap().display().to_string())
            .collect();
        assert_eq!(
            got,
            vec![
                "Games/Battlenet",
                "Games/ubisoft-connect",
                "Games/umu/umu-default/pfx",
            ]
        );
    }

    #[test]
    fn vdf_paths_parse() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"/home/u/.local/share/Steam\"\n\t\t\"label\"\t\t\"\"\n\t}\n}";
        assert_eq!(library_paths(vdf), vec!["/home/u/.local/share/Steam"]);
    }
}

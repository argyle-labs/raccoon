//! Which games have saves on this host, and where. Every path derives from
//! `$HOME`, so the same code serves Bazzite (`/var/home/<user>`) and other
//! distros (`/home/<user>`).
//!
//! A game id is host-independent: two hosts sharing one backup pool must name
//! the same game identically, because the store files backups by
//! `(kind, instance)` alone and that shared pool is the sync mechanism.
//!
//! | game id             | parts                                                         |
//! |---------------------|---------------------------------------------------------------|
//! | `steam-<appid>`     | `userdata/<steamid>` (`Steam/userdata/<steamid>/<appid>`) and `proton` (`compatdata/<appid>` prefix, any library) |
//! | `heroic-<prefix>`   | `prefix` (Heroic prefix, native or flatpak)                   |
//! | `umu-<prefix>`      | `prefix` (`~/Games/umu/<prefix>`)                             |
//! | `battlenet`         | `prefix` (`~/Games/Battlenet`)                                |
//! | `native-<dir name>` | `home/<$HOME-relative dir>` (from the `game-saves:native-paths` config row) |

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::manifest::checked_rel;
use super::{Filter, GameRoot, walk};

/// userdata appids that are Steam itself, not games: 7 = client config,
/// 760 = screenshots (often most of userdata's size).
const NON_GAME_APPIDS: &[&str] = &["7", "760"];

const PROTON_PART: &str = "proton";
const PREFIX_PART: &str = "prefix";
/// Restoring into a prefix that doesn't exist would mean fabricating a partial
/// one, which breaks Proton/Heroic's own prefix initialization on first launch.
pub const PREFIX_DEFERRED: &str = "prefix not initialized";

/// The host facts discovery needs.
#[derive(Debug, Clone)]
pub struct Env {
    pub home: PathBuf,
    /// Dirs from the `game-saves:native-paths` config row (`~/` allowed).
    pub native_paths: Vec<String>,
}

/// Every game found on this host → its parts, saves or not.
pub fn games(env: &Env) -> BTreeMap<String, Vec<GameRoot>> {
    let mut out: BTreeMap<String, Vec<GameRoot>> = BTreeMap::new();
    let mut add = |id: String, root: GameRoot| {
        let parts = out.entry(id).or_default();
        // First hit wins (e.g. two prefixes sanitizing to one id).
        if !parts.iter().any(|p| p.part == root.part) {
            parts.push(root);
        }
    };
    let home = &env.home;
    if let Some(steam) = steam_root(home) {
        for (sid, sid_dir) in numeric_dirs(&steam.join("userdata")) {
            for (appid, app_dir) in numeric_dirs(&sid_dir) {
                if !NON_GAME_APPIDS.contains(&appid.as_str()) {
                    add(
                        format!("steam-{appid}"),
                        GameRoot {
                            part: format!("userdata/{sid}"),
                            root: app_dir,
                            filter: Filter::All,
                        },
                    );
                }
            }
        }
        for (appid, user) in proton_prefixes(&steam) {
            add(
                format!("steam-{appid}"),
                GameRoot {
                    part: PROTON_PART.to_string(),
                    root: user,
                    filter: Filter::WineUser,
                },
            );
        }
    }
    for (id, user) in wine_games(home) {
        add(
            id,
            GameRoot {
                part: PREFIX_PART.to_string(),
                root: user,
                filter: Filter::WineUser,
            },
        );
    }
    for raw in &env.native_paths {
        if let Some(rel) = home_relative(home, raw) {
            let root = home.join(&rel);
            let name = rel.rsplit('/').next().unwrap_or(&rel);
            if let Some(id) = game_id("native", name)
                && root.is_dir()
            {
                add(
                    id,
                    GameRoot {
                        part: format!("home/{rel}"),
                        root,
                        filter: Filter::All,
                    },
                );
            }
        }
    }
    out
}

/// Game ids with at least one save file on this host.
pub fn instances(env: &Env) -> Vec<String> {
    games(env)
        .into_iter()
        .filter(|(_, parts)| {
            parts
                .iter()
                .any(|p| walk::has_save_files(&p.root, p.filter))
        })
        .map(|(id, _)| id)
        .collect()
}

/// The local root for `part` of game `id` — which may never have been seen on
/// this host (restoring another host's backup). `local` is [`games`]' entry for
/// `id`, if any. `Err` carries why the part is deferred here.
pub fn resolve(env: &Env, id: &str, part: &str, local: &[GameRoot]) -> Result<PathBuf, String> {
    if let Some(g) = local.iter().find(|g| g.part == part) {
        return Ok(g.root.clone());
    }
    if part == PROTON_PART || part == PREFIX_PART {
        return Err(PREFIX_DEFERRED.to_string());
    }
    if let Some(sid) = part.strip_prefix("userdata/") {
        let appid = id
            .strip_prefix("steam-")
            .filter(|a| !a.is_empty() && a.bytes().all(|b| b.is_ascii_digit()))
            .ok_or_else(|| format!("`{part}` belongs to a steam game, not `{id}`"))?;
        checked_rel(sid)?;
        let userdata = steam_root(&env.home)
            .ok_or("Steam not installed")?
            .join("userdata");
        // A missing steamid dir means a different Steam account; don't invent one.
        if !userdata.join(sid).is_dir() {
            return Err(format!("steam account {sid} not signed in on this host"));
        }
        return Ok(userdata.join(sid).join(appid));
    }
    if let Some(rel) = part.strip_prefix("home/") {
        return Ok(env.home.join(checked_rel(rel)?));
    }
    Err(format!("unknown part `{part}`"))
}

/// `<kind>-<name>` lowercased to `[a-z0-9-]`, runs of anything else collapsed
/// to one `-`. `None` when the name has no usable characters.
pub fn game_id(kind: &str, name: &str) -> Option<String> {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    (!slug.is_empty()).then(|| format!("{kind}-{slug}"))
}

fn steam_root(home: &Path) -> Option<PathBuf> {
    [".local/share/Steam", ".steam/steam"]
        .iter()
        .map(|r| home.join(r))
        .find(|p| p.is_dir())
}

/// `(appid, steamuser dir)` for every initialized Proton prefix across all
/// Steam libraries.
fn proton_prefixes(steam: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for lib in steam_libraries(steam) {
        for (appid, dir) in numeric_dirs(&lib.join("steamapps/compatdata")) {
            let user = dir.join("pfx/drive_c/users/steamuser");
            if user.is_dir() {
                out.push((appid, user));
            }
        }
    }
    out
}

/// The default library plus every `"path"` in `libraryfolders.vdf`, deduped.
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

/// `(game id, user dir)` for Heroic, umu, and Battle.net prefixes.
fn wine_games(home: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    // Heroic nests prefixes one level down (`Prefixes/default/<game>`).
    for dir in [
        home.join("Games/Heroic/Prefixes"),
        home.join(".var/app/com.heroicgameslauncher.hgl/Games/Heroic/Prefixes"),
    ] {
        for (name, user) in prefixes_under(home, &dir, 1) {
            out.extend(game_id("heroic", &name).map(|id| (id, user)));
        }
    }
    for (name, user) in prefixes_under(home, &home.join("Games/umu"), 0) {
        out.extend(game_id("umu", &name).map(|id| (id, user)));
    }
    if let Some(user) = wine_user_dir(home, &home.join("Games/Battlenet")) {
        out.push(("battlenet".to_string(), user));
    }
    out
}

/// `(prefix dir name, user dir)` for wine prefixes directly under `dir`,
/// descending up to `depth` more levels through non-prefix dirs.
fn prefixes_under(home: &Path, dir: &Path, depth: u32) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for (name, child) in sorted_dirs(dir) {
        if let Some(user) = wine_user_dir(home, &child) {
            out.push((name, user));
        } else if depth > 0 {
            out.extend(prefixes_under(home, &child, depth - 1));
        }
    }
    out
}

/// The prefix's user dir: `steamuser` (Proton/umu), else the login name, else
/// the first non-`Public` user. Handles both `<prefix>/drive_c` and the
/// Proton-style `<prefix>/pfx/drive_c`.
fn wine_user_dir(home: &Path, prefix: &Path) -> Option<PathBuf> {
    let login = home.file_name().map(|n| n.to_string_lossy().into_owned());
    let users = ["pfx/drive_c/users", "drive_c/users"]
        .iter()
        .map(|r| prefix.join(r))
        .find(|p| p.is_dir())?;
    let names: Vec<String> = sorted_dirs(&users).into_iter().map(|(n, _)| n).collect();
    let pick = names
        .iter()
        .find(|n| *n == "steamuser")
        .or_else(|| names.iter().find(|n| Some(n.as_str()) == login.as_deref()))
        .or_else(|| names.iter().find(|n| *n != "Public"))?;
    Some(users.join(pick))
}

/// `raw` (`~/x`, `$HOME/x`, or an absolute path under `$HOME`) as a
/// `$HOME`-relative path; `None` for anything outside `$HOME`.
fn home_relative(home: &Path, raw: &str) -> Option<String> {
    let rel = if let Some(r) = raw
        .strip_prefix("~/")
        .or_else(|| raw.strip_prefix("$HOME/"))
    {
        r.to_string()
    } else {
        Path::new(raw)
            .strip_prefix(home)
            .ok()?
            .to_string_lossy()
            .into_owned()
    };
    let rel = rel.trim_end_matches('/').to_string();
    checked_rel(&rel).ok().map(|_| rel)
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

fn numeric_dirs(dir: &Path) -> Vec<(String, PathBuf)> {
    sorted_dirs(dir)
        .into_iter()
        .filter(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saves::testutil::{TempDir, write};

    fn env(home: &Path) -> Env {
        Env {
            home: home.to_path_buf(),
            native_paths: Vec::new(),
        }
    }

    fn parts(env: &Env, id: &str) -> Vec<String> {
        games(env)
            .remove(id)
            .unwrap_or_default()
            .into_iter()
            .map(|g| g.part)
            .collect()
    }

    #[test]
    fn empty_home_offers_no_instances() {
        let t = TempDir::new();
        assert!(instances(&env(t.path())).is_empty());
    }

    #[test]
    fn steam_merges_userdata_and_proton_across_libraries() {
        let t = TempDir::new();
        let steam = t.path().join(".local/share/Steam");
        let ext = t.path().join("ext-lib");
        let ud = steam.join("userdata");
        write(&ud.join("111/440/remote/save"), "s");
        write(&ud.join("111/7/remote/sharedconfig.vdf"), "c");
        write(&ud.join("111/760/remote/shot.jpg"), "j");
        write(&ud.join("111/config/localconfig.vdf"), "c");
        write(
            &steam.join("steamapps/compatdata/440/pfx/drive_c/users/steamuser/AppData/Roaming/a"),
            "a",
        );
        write(
            &ext.join("steamapps/compatdata/200/pfx/drive_c/users/steamuser/Documents/b"),
            "b",
        );
        // Prefix with only cache content → no saves → not offered.
        write(
            &steam.join(
                "steamapps/compatdata/300/pfx/drive_c/users/steamuser/AppData/Local/G/Cache/x",
            ),
            "x",
        );
        write(
            &steam.join("steamapps/libraryfolders.vdf"),
            &format!(
                "\"libraryfolders\"\n{{\n\t\"0\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n\t\"1\"\n\t{{\n\t\t\"path\"\t\t\"{}\"\n\t}}\n}}\n",
                steam.display(),
                ext.display()
            ),
        );
        let e = env(t.path());
        assert_eq!(parts(&e, "steam-440"), vec!["userdata/111", "proton"]);
        assert_eq!(parts(&e, "steam-200"), vec!["proton"]);
        assert_eq!(instances(&e), vec!["steam-200", "steam-440"]);
    }

    #[test]
    fn resolve_handles_games_new_to_this_host() {
        let t = TempDir::new();
        let ud = t.path().join(".local/share/Steam/userdata");
        fs::create_dir_all(ud.join("111")).unwrap();
        let e = env(t.path());
        let local = games(&e).remove("steam-999").unwrap_or_default();
        assert!(local.is_empty());
        assert_eq!(
            resolve(&e, "steam-999", "userdata/111", &local),
            Ok(ud.join("111/999"))
        );
        assert!(resolve(&e, "steam-999", "userdata/222", &local).is_err());
        assert_eq!(
            resolve(&e, "steam-999", "proton", &local),
            Err(PREFIX_DEFERRED.to_string())
        );
        assert_eq!(
            resolve(&e, "heroic-hades", "prefix", &[]),
            Err(PREFIX_DEFERRED.to_string())
        );
        assert_eq!(
            resolve(&e, "native-factorio", "home/.factorio", &[]),
            Ok(t.path().join(".factorio"))
        );
        assert!(resolve(&e, "native-x", "home/../etc", &[]).is_err());
        assert!(resolve(&e, "heroic-x", "userdata/111", &[]).is_err());
    }

    #[test]
    fn heroic_umu_and_battlenet_prefixes() {
        let t = TempDir::new();
        let h = t.path();
        let login = h.file_name().unwrap().to_string_lossy().into_owned();
        write(
            &h.join("Games/Heroic/Prefixes/default/Hades II/drive_c/users/steamuser/Saved Games/x"),
            "x",
        );
        write(
            &h.join(format!(
                "Games/Heroic/Prefixes/Control/drive_c/users/{login}/Documents/y"
            )),
            "y",
        );
        write(
            &h.join("Games/Heroic/Prefixes/Control/drive_c/users/Public/z"),
            "z",
        );
        write(
            &h.join("Games/umu/umu-default/pfx/drive_c/users/steamuser/AppData/Roaming/r"),
            "r",
        );
        write(
            &h.join("Games/Battlenet/drive_c/users/steamuser/AppData/Roaming/b"),
            "b",
        );
        let e = env(h);
        assert_eq!(
            instances(&e),
            vec![
                "battlenet",
                "heroic-control",
                "heroic-hades-ii",
                "umu-umu-default"
            ]
        );
        let g = games(&e);
        assert!(
            g["heroic-control"][0]
                .root
                .ends_with(format!("drive_c/users/{login}"))
        );
        assert_eq!(g["battlenet"][0].part, "prefix");
    }

    #[test]
    fn native_is_config_driven_and_home_bound() {
        let t = TempDir::new();
        let h = t.path();
        write(&h.join(".local/share/factorio/saves/a.zip"), "a");
        let mut e = env(h);
        assert!(instances(&e).is_empty());
        e.native_paths = vec![
            "~/.local/share/factorio/".into(),
            format!("{}/.local/share/factorio", h.display()),
            "~/.local/share/absent".into(),
            "/etc".into(),
            "~/../escape".into(),
        ];
        assert_eq!(instances(&e), vec!["native-factorio"]);
        assert_eq!(
            parts(&e, "native-factorio"),
            vec!["home/.local/share/factorio"]
        );
    }

    #[test]
    fn game_ids_are_sanitized() {
        assert_eq!(
            game_id("heroic", "Hades II").as_deref(),
            Some("heroic-hades-ii")
        );
        assert_eq!(
            game_id("umu", "--Diablo_IV (2023)--").as_deref(),
            Some("umu-diablo-iv-2023")
        );
        assert_eq!(game_id("native", "!!!"), None);
    }

    #[test]
    fn vdf_paths_parse() {
        let vdf = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"/home/u/.local/share/Steam\"\n\t\t\"label\"\t\t\"\"\n\t}\n}";
        assert_eq!(library_paths(vdf), vec!["/home/u/.local/share/Steam"]);
    }
}

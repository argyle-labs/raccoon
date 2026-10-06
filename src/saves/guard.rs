//! Restore-side checks against a hostile or corrupted pool: payload bytes come
//! from a shared target another host (or anyone with write access to it) wrote,
//! so nothing in a manifest is trusted to pick where bytes land.

use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::manifest;

/// First path components under `$HOME` a restore never writes, matched as a
/// name prefix (`.bash` covers `.bashrc`, `.bash_profile`, …): shell startup,
/// credentials, and orca's own state.
const DENY_NAME_PREFIXES: &[&str] = &[
    ".profile",
    ".bash",
    ".zsh",
    ".zlog",
    ".zprofile",
    ".login",
    ".logout",
    ".cshrc",
    ".tcshrc",
    ".kshrc",
    ".mkshrc",
    ".xprofile",
    ".xinitrc",
    ".xsession",
    ".pam_environment",
    ".ssh",
    ".gnupg",
    ".orca",
];

/// Home-relative dirs a restore never writes under: autostart, user units,
/// session environment, `PATH` entries, and orca's own state.
const DENY_DIRS: &[&str] = &[
    ".config/autostart",
    ".config/systemd",
    ".local/share/systemd",
    ".config/environment.d",
    ".config/plasma-workspace",
    ".config/fish",
    ".config/orca",
    ".local/share/orca",
    ".local/bin",
];

/// Dirs too broad to anchor a `home:` save on: sharing only one of these with
/// a local save says nothing about where that game keeps its files.
const GENERIC_DIRS: &[&str] = &[
    "",
    ".config",
    ".local",
    ".local/share",
    ".local/state",
    ".cache",
    ".var",
    ".var/app",
    "Documents",
    "Games",
    "snap",
];

pub fn home_denied(rel: &Path) -> bool {
    let first = match rel.components().next() {
        Some(Component::Normal(s)) => s.to_string_lossy().into_owned(),
        _ => return true,
    };
    DENY_NAME_PREFIXES.iter().any(|p| first.starts_with(p))
        || DENY_DIRS.iter().any(|d| rel.starts_with(d))
}

/// Whether `rel` (home-relative) may be restored for a game whose saves on
/// this host include `local` files and whose operator config names `custom`
/// paths (both home-relative). It must sit in a save dir it shares with an
/// existing local file, or under a configured path, and never on the
/// deny-list.
pub fn home_allowed(rel: &Path, local: &[PathBuf], custom: &[PathBuf]) -> Result<(), String> {
    if home_denied(rel) {
        return Err(format!("`{}` is on the restore deny-list", rel.display()));
    }
    if custom.iter().any(|c| rel.starts_with(c)) {
        return Ok(());
    }
    let dir = rel.parent().unwrap_or(Path::new(""));
    let shares_save_dir = local.iter().any(|l| {
        let common: PathBuf = dir
            .components()
            .zip(l.parent().unwrap_or(Path::new("")).components())
            .take_while(|(a, b)| a == b)
            .map(|(a, _)| a)
            .collect();
        !GENERIC_DIRS.iter().any(|g| common == Path::new(g))
    });
    if shares_save_dir {
        Ok(())
    } else {
        Err(format!(
            "`{}` is outside every save dir this game has on this host",
            rel.display()
        ))
    }
}

/// A manifest mtime more than a day ahead would win every newest-wins
/// comparison forever.
pub fn mtime_plausible(mtime_ns: i64) -> Result<(), String> {
    let limit = manifest::to_ns(SystemTime::now() + Duration::from_secs(86_400));
    if mtime_ns > limit {
        Err("mtime is in the future".to_string())
    } else {
        Ok(())
    }
}

/// `dst` resolves inside `anchor` even through symlinks: its nearest existing
/// ancestor, canonicalized, must lie under the canonical anchor.
///
/// This is a check, not a capability: the caller writes by path afterwards,
/// so a process that can write under `anchor` (i.e. runs as this user) can
/// swap a checked directory for a symlink between the check and the write and
/// carry it elsewhere. Restore re-runs it immediately before each rename,
/// leaving only that syscall-sized window; the staged temp itself is created
/// `O_EXCL` and cannot be redirected by a symlink at its own name. Closing the
/// window entirely needs an `openat` walk with `O_NOFOLLOW` per component.
pub fn stays_inside(anchor: &Path, dst: &Path) -> Result<(), String> {
    // An anchor not created yet resolves through its nearest existing
    // ancestor; nothing below that exists to redirect a write.
    let canon_anchor = anchor
        .ancestors()
        .find_map(|a| {
            a.canonicalize()
                .ok()
                .map(|c| c.join(anchor.strip_prefix(a).unwrap_or(Path::new(""))))
        })
        .ok_or_else(|| format!("{} has no existing ancestor", anchor.display()))?;
    let mut probe = dst;
    let existing = loop {
        if let Ok(meta) = std::fs::symlink_metadata(probe) {
            // The file itself may be a symlink; it is replaced, never written
            // through, so only its directory has to resolve inside.
            if meta.file_type().is_symlink() && probe == dst {
                probe = probe.parent().unwrap_or(anchor);
                continue;
            }
            break probe
                .canonicalize()
                .map_err(|e| format!("{}: {e}", probe.display()))?;
        }
        probe = probe
            .parent()
            .ok_or_else(|| format!("{} has no existing ancestor", dst.display()))?;
    };
    // Below the anchor, or above a not-yet-created one on its own path.
    if existing.starts_with(&canon_anchor) || canon_anchor.starts_with(&existing) {
        Ok(())
    } else {
        Err(format!(
            "{} resolves outside {}",
            dst.display(),
            anchor.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saves::testutil::{TempDir, write};

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn deny_list_blocks_startup_credentials_and_orca() {
        for bad in [
            ".bashrc",
            ".bash_profile",
            ".zshrc",
            ".profile",
            ".pam_environment",
            ".ssh/authorized_keys",
            ".gnupg/gpg.conf",
            ".config/autostart/evil.desktop",
            ".config/systemd/user/evil.service",
            ".local/share/systemd/user/x",
            ".config/environment.d/x.conf",
            ".local/share/orca/raccoon/game-saves/index.json",
            ".orca/orca.db",
            ".local/bin/ls",
        ] {
            assert!(home_denied(&p(bad)), "{bad}");
            let local = [p(".config/autostart/legit.sav")];
            assert!(home_allowed(&p(bad), &local, &[p(".")]).is_err(), "{bad}");
        }
        assert!(!home_denied(&p(".config/StardewValley/Saves/F/F")));
    }

    #[test]
    fn home_paths_need_a_shared_save_dir_or_config() {
        let local = [p(".config/StardewValley/Saves/Farm_1/Farm_1")];
        assert!(home_allowed(&p(".config/StardewValley/Saves/Farm_2/Farm_2"), &local, &[]).is_ok());
        assert!(home_allowed(&p(".config/StardewValley/startup_preferences"), &local, &[]).is_ok());
        assert!(home_allowed(&p(".config/other/x"), &local, &[]).is_err());
        assert!(home_allowed(&p("Documents/x"), &[p("Documents/y")], &[]).is_err());
        assert!(home_allowed(&p(".factorio/saves/a.zip"), &[], &[]).is_err());
        assert!(home_allowed(&p(".factorio/saves/a.zip"), &[], &[p(".factorio")]).is_ok());
    }

    #[test]
    fn future_mtimes_are_rejected() {
        assert!(mtime_plausible(0).is_ok());
        let far = manifest::to_ns(SystemTime::now() + Duration::from_secs(3 * 86_400));
        assert!(mtime_plausible(far).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_dirs_cannot_carry_a_write_out_of_the_anchor() {
        let t = TempDir::new();
        let anchor = t.path().join("prefix/drive_c/users/steamuser");
        write(&anchor.join("Documents/ok.sav"), "x");
        std::fs::create_dir_all(t.path().join("outside")).unwrap();
        std::os::unix::fs::symlink(t.path().join("outside"), anchor.join("AppData")).unwrap();
        assert!(stays_inside(&anchor, &anchor.join("Documents/new/deep.sav")).is_ok());
        assert!(stays_inside(&anchor, &anchor.join("AppData/Roaming/x")).is_err());
        std::os::unix::fs::symlink(t.path().join("outside/f"), anchor.join("Documents/link"))
            .unwrap();
        assert!(stays_inside(&anchor, &anchor.join("Documents/link")).is_ok());
    }
}

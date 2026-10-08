//! Ludusavi (<https://github.com/mtkennerly/ludusavi>) as the save-discovery
//! engine: its PCGamingWiki-backed manifest knows where each game keeps its
//! saves across Steam/Proton, Heroic and plain wine prefixes. orca only drives
//! `backup --preview --api` (a read-only scan) and does the copying itself.
//!
//! Runs unprivileged out of a plugin-private dir
//! (`~/.local/share/orca/raccoon/ludusavi`) with its own `--config`, so the
//! user's own ludusavi setup is never read or touched. Only the pinned release
//! is ever run (never a `ludusavi` on PATH), verified once per process.
//!
//! Scan cost: ludusavi probes every game it knows against `$HOME` and against
//! each `otherWine` root, so roots are kept to the explicit ones below and a
//! full scan only refreshes the instance index; per-game work scans one title.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use plugin_toolkit::serde_json::{self, Value, json};
use serde::Deserialize;

use super::layout::Layout;
use crate::process::run_bounded;

pub const VERSION: &str = "0.31.0";
const LINUX_X64_URL: &str = "https://github.com/mtkennerly/ludusavi/releases/download/v0.31.0/ludusavi-v0.31.0-linux.tar.gz";
/// Upstream publishes no checksums, so the release tarball's and the extracted
/// binary's sha256 are pinned here (computed from the v0.31.0 GitHub release
/// asset).
const LINUX_X64_SHA256: &str = "7322ff45d41eae7ae064a80d8c9ecccc5b8fb6fc090a603a66369cd4b054068d";
const LINUX_X64_BIN_SHA256: &str =
    "38098e1aec77d0976fc0644ce00a265392ab58ac00b949ddff437aa9b7606d43";

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);
const TAR_TIMEOUT: Duration = Duration::from_secs(120);
/// A full scan probes every known game; a title scan only that game.
pub const FULL_SCAN_TIMEOUT: Duration = Duration::from_secs(900);
pub const TITLE_SCAN_TIMEOUT: Duration = Duration::from_secs(180);

static BINARY_VERIFIED: AtomicBool = AtomicBool::new(false);

/// `backup --preview --api` output (ludusavi's `general-output` schema), cut
/// down to what discovery reads.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    #[serde(default)]
    pub games: BTreeMap<String, PreviewGame>,
    #[serde(default)]
    pub errors: Option<PreviewErrors>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PreviewGame {
    #[serde(default)]
    pub files: BTreeMap<String, PreviewFile>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewFile {
    #[serde(default)]
    pub failed: bool,
    #[serde(default)]
    pub ignored: bool,
    /// Other games that claim the same path.
    #[serde(default)]
    pub duplicated_by: Vec<String>,
    #[serde(default)]
    pub error: Option<PreviewError>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PreviewError {
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewErrors {
    #[serde(default)]
    pub unknown_games: Option<Vec<String>>,
    #[serde(default)]
    pub some_games_failed: Option<bool>,
}

/// A file ludusavi found for a game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanFile {
    pub path: PathBuf,
    pub duplicated_by: Vec<String>,
}

impl Preview {
    pub fn parse(stdout: &str) -> Result<Self, String> {
        serde_json::from_str(stdout).map_err(|e| format!("parse ludusavi output: {e}"))
    }

    /// The files ludusavi would back up for `title`.
    pub fn files(&self, title: &str) -> Vec<ScanFile> {
        self.games
            .get(title)
            .map(|g| {
                g.files
                    .iter()
                    .filter(|(_, f)| !f.ignored && !f.failed)
                    .map(|(p, f)| ScanFile {
                        path: PathBuf::from(p),
                        duplicated_by: f.duplicated_by.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// What ludusavi reported going wrong (failed files, failed games).
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(e) = &self.errors
            && e.some_games_failed == Some(true)
        {
            out.push("ludusavi: some games failed".to_string());
        }
        for (title, g) in &self.games {
            for (path, f) in g.files.iter().filter(|(_, f)| f.failed) {
                let why = f.error.as_ref().map_or("failed", |e| e.message.as_str());
                out.push(format!("ludusavi: {title}: {path}: {why}"));
            }
        }
        out
    }
}

/// A save scan of `titles`, or of every game when empty.
pub trait Scanner {
    fn preview(&self, titles: &[&str]) -> Result<Preview, String>;
}

/// An operator-defined game (from orca config) handed to ludusavi as a custom
/// game, for saves its manifest doesn't know.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CustomGame {
    pub name: String,
    pub files: Vec<String>,
}

/// The ludusavi `config.yaml` for this host (JSON is valid YAML).
pub fn config(layout: &Layout, custom: &[CustomGame], scratch: &Path) -> Value {
    let mut roots: Vec<Value> = layout
        .steam_libraries
        .iter()
        .map(|p| json!({"store": "steam", "path": p}))
        .collect();
    for heroic in [
        layout.home.join(".config/heroic"),
        layout
            .home
            .join(".var/app/com.heroicgameslauncher.hgl/config/heroic"),
    ] {
        if heroic.is_dir() {
            roots.push(json!({"store": "heroic", "path": heroic}));
        }
    }
    for prefix in layout.wine_prefixes() {
        roots.push(json!({"store": "otherWine", "path": prefix}));
    }
    let custom: Vec<Value> = custom
        .iter()
        .map(|g| {
            let files: Vec<String> = g
                .files
                .iter()
                .map(|f| expand_home(&layout.home, f))
                .collect();
            json!({"name": g.name, "files": files, "integration": "extend"})
        })
        .collect();
    json!({
        "manifest": {"enable": true},
        "release": {"check": false},
        "roots": roots,
        "customGames": custom,
        // Preview never writes here, but ludusavi requires the paths.
        "backup": {"path": scratch, "filter": {"excludeStoreScreenshots": true}},
        "restore": {"path": scratch},
    })
}

pub fn expand_home(home: &Path, p: &str) -> String {
    match p.strip_prefix("~/").or_else(|| p.strip_prefix("$HOME/")) {
        Some(rest) => home.join(rest).to_string_lossy().into_owned(),
        None => p.to_string(),
    }
}

/// A provisioned ludusavi with this host's config written.
pub struct Ludusavi {
    bin: PathBuf,
    config_dir: PathBuf,
}

impl Ludusavi {
    /// Install (first use) and verify the pinned ludusavi, and (re)write its
    /// private config.
    pub fn prepare(layout: &Layout, custom: &[CustomGame]) -> Result<Self, String> {
        let data = data_dir(&layout.home);
        let bin = provision(&data)?;
        let config_dir = data.join("config");
        fs::create_dir_all(&config_dir)
            .map_err(|e| format!("mkdir {}: {e}", config_dir.display()))?;
        let body = serde_json::to_string_pretty(&config(layout, custom, &data.join("scratch")))
            .map_err(|e| format!("encode ludusavi config: {e}"))?;
        let path = config_dir.join("config.yaml");
        if fs::read_to_string(&path).ok().as_deref() != Some(body.as_str()) {
            super::fsx::atomic_write(&path, body.as_bytes(), super::fsx::NewMode::Private)
                .map_err(|e| format!("write {}: {e}", path.display()))?;
        }
        Ok(Self { bin, config_dir })
    }
}

impl Scanner for Ludusavi {
    fn preview(&self, titles: &[&str]) -> Result<Preview, String> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("--config").arg(&self.config_dir);
        let timeout = if titles.is_empty() {
            // Offline hosts scan with the cached manifest instead of failing.
            cmd.arg("--try-manifest-update");
            FULL_SCAN_TIMEOUT
        } else {
            // Per-game scans ride the manifest the last full scan refreshed.
            cmd.arg("--no-manifest-update");
            TITLE_SCAN_TIMEOUT
        };
        cmd.args(["backup", "--preview", "--api"]);
        if !titles.is_empty() {
            cmd.arg("--").args(titles);
        }
        let out = run_bounded(cmd, timeout)?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.trim().is_empty() {
            plugin_toolkit::tracing::warn!("[game-saves] ludusavi stderr: {}", stderr.trim());
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        // An unknown title exits 1 but still prints valid JSON.
        if stdout.trim().is_empty() {
            return Err(format!(
                "ludusavi produced no output ({}): {}",
                out.status,
                stderr.trim()
            ));
        }
        let preview = Preview::parse(&stdout)?;
        for w in preview.warnings() {
            plugin_toolkit::tracing::warn!("[game-saves] {w}");
        }
        Ok(preview)
    }
}

pub fn data_dir(home: &Path) -> PathBuf {
    home.join(".local/share/orca/raccoon/ludusavi")
}

/// The pinned ludusavi under `data`, downloading and verifying it on first
/// use and re-verifying an existing install once per process.
fn provision(data: &Path) -> Result<PathBuf, String> {
    let dir = data.join(VERSION);
    let bin = dir.join("ludusavi");
    if bin.is_file() {
        if !BINARY_VERIFIED.load(Ordering::Acquire) {
            verify_file(&bin, LINUX_X64_BIN_SHA256)?;
            BINARY_VERIFIED.store(true, Ordering::Release);
        }
        return Ok(bin);
    }
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return Err("ludusavi is only provisioned for x86_64 Linux".into());
    }
    fs::create_dir_all(data).map_err(|e| format!("mkdir {}: {e}", data.display()))?;
    let tarball =
        plugin_toolkit::reactor::block_on(plugin_toolkit::time::timeout(DOWNLOAD_TIMEOUT, async {
            let resp = plugin_toolkit::reqwest::get(LINUX_X64_URL)
                .await
                .and_then(|r| r.error_for_status())
                .map_err(|e| format!("download ludusavi: {e}"))?;
            resp.bytes()
                .await
                .map_err(|e| format!("download ludusavi: {e}"))
        }))
        .ok_or_else(|| {
            format!(
                "download ludusavi timed out after {}s",
                DOWNLOAD_TIMEOUT.as_secs()
            )
        })??;
    verify(&tarball, LINUX_X64_SHA256)?;
    install(data, &dir, &tarball, LINUX_X64_BIN_SHA256)?;
    BINARY_VERIFIED.store(true, Ordering::Release);
    Ok(bin)
}

fn verify(bytes: &[u8], want: &str) -> Result<(), String> {
    let got = plugin_toolkit::hash::sha256_hex(bytes);
    if got == want {
        Ok(())
    } else {
        Err(format!("ludusavi sha256 {got} != pinned {want}"))
    }
}

fn verify_file(path: &Path, want: &str) -> Result<(), String> {
    let got = plugin_toolkit::hash::sha256_file(path).map_err(|e| format!("{e:#}"))?;
    if got == want {
        Ok(())
    } else {
        Err(format!(
            "{} sha256 {got} != pinned {want}; delete it to reinstall",
            path.display()
        ))
    }
}

/// Unpack into a sibling staging dir, verify the binary, then rename into
/// place, so a crash or a bad archive never leaves a `dir` later runs trust.
fn install(data: &Path, dir: &Path, tarball: &[u8], bin_sha256: &str) -> Result<(), String> {
    use std::io::Write;
    let (archive, mut f) = super::fsx::create_temp(data, ".ludusavi-", super::fsx::NEW_FILE_MODE)
        .map_err(|e| format!("write archive in {}: {e}", data.display()))?;
    let written = f
        .write_all(tarball)
        .map_err(|e| format!("write {}: {e}", archive.display()));
    drop(f);
    let staging = data.join(format!(".ludusavi-{}", plugin_toolkit::mint_uuidv7()));
    let mut staging_made = false;
    let result = (|| {
        written?;
        // Not `create_dir_all`: an existing dir (or symlink) there is not ours.
        fs::create_dir(&staging).map_err(|e| format!("mkdir {}: {e}", staging.display()))?;
        staging_made = true;
        let mut tar = Command::new("tar");
        tar.arg("-xzf")
            .arg(&archive)
            .arg("-C")
            .arg(&staging)
            .arg("ludusavi");
        let out = run_bounded(tar, TAR_TIMEOUT)?;
        if !out.status.success() {
            return Err(format!(
                "tar: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        verify_file(&staging.join("ludusavi"), bin_sha256)?;
        match fs::rename(&staging, dir) {
            Ok(()) => Ok(()),
            // A concurrent run installed it first.
            Err(_) if dir.join("ludusavi").is_file() => Ok(()),
            Err(e) => Err(format!("install {}: {e}", dir.display())),
        }
    })();
    fsx_cleanup(&archive, staging_made.then_some(staging.as_path()));
    result
}

fn fsx_cleanup(archive: &Path, staging: Option<&Path>) {
    super::fsx::remove_quietly(archive);
    if let Some(staging) = staging
        && staging.is_dir()
        && let Err(e) = fs::remove_dir_all(staging)
    {
        plugin_toolkit::tracing::warn!("[game-saves] cleanup {}: {e}", staging.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saves::testutil::TempDir;

    #[test]
    fn parses_real_preview_output_and_skips_ignored() {
        let p = Preview::parse(
            r#"{
              "overall": {"totalGames": 1, "totalBytes": 4, "processedGames": 1, "processedBytes": 4,
                          "changedGames": {"new": 1, "different": 0, "same": 0}},
              "games": {"Hades": {"decision": "Processed", "change": "New",
                "files": {"/h/a.sav": {"change": "New", "bytes": 2},
                          "/h/b.sav": {"change": "New", "bytes": 2, "ignored": true}},
                "registry": {}}}
            }"#,
        )
        .unwrap();
        assert_eq!(
            p.files("Hades"),
            vec![ScanFile {
                path: PathBuf::from("/h/a.sav"),
                duplicated_by: Vec::new()
            }]
        );
        assert!(p.files("Nope").is_empty());

        let unknown = Preview::parse(
            r#"{"errors": {"unknownGames": ["Zzz"]}, "overall": {"totalGames": 0, "totalBytes": 0,
                "processedGames": 0, "processedBytes": 0, "changedGames": {"new": 0, "different": 0, "same": 0}},
                "games": {}}"#,
        )
        .unwrap();
        assert!(unknown.games.is_empty());
        assert_eq!(
            unknown.errors.unwrap().unknown_games.unwrap(),
            vec!["Zzz".to_string()]
        );
    }

    #[test]
    fn config_points_at_this_hosts_roots() {
        let t = TempDir::new();
        let h = t.path();
        for d in [
            ".local/share/Steam/userdata",
            ".config/heroic/GamesConfig",
            "Games/battlenet/drive_c",
            "Games/Heroic/Prefixes/default/drive_c",
        ] {
            fs::create_dir_all(h.join(d)).unwrap();
        }
        let custom = vec![CustomGame {
            name: "Factorio".into(),
            files: vec!["~/.factorio/saves".into()],
        }];
        let c = config(&Layout::detect(h), &custom, &h.join("scratch"));
        let roots: Vec<(String, String)> = c["roots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["store"].as_str().unwrap().to_string(),
                    Path::new(r["path"].as_str().unwrap())
                        .strip_prefix(h)
                        .unwrap()
                        .display()
                        .to_string(),
                )
            })
            .collect();
        assert_eq!(
            roots,
            vec![
                ("steam".into(), ".local/share/Steam".into()),
                ("heroic".into(), ".config/heroic".into()),
                ("otherWine".into(), "Games/battlenet".into()),
            ]
        );
        assert_eq!(
            c["customGames"][0]["files"][0].as_str().unwrap(),
            h.join(".factorio/saves").to_str().unwrap()
        );
        assert_eq!(
            c["backup"]["filter"]["excludeStoreScreenshots"],
            json!(true)
        );
    }

    #[test]
    fn surfaces_ludusavi_failures() {
        let p = Preview::parse(
            r#"{"errors": {"someGamesFailed": true}, "games": {"G": {"decision": "Processed",
                "change": "New", "files": {"/x": {"change": "New", "bytes": 1, "failed": true,
                "error": {"message": "permission denied"}}}, "registry": {}}}}"#,
        )
        .unwrap();
        assert!(p.files("G").is_empty());
        assert_eq!(
            p.warnings(),
            vec![
                "ludusavi: some games failed".to_string(),
                "ludusavi: G: /x: permission denied".to_string()
            ]
        );
    }

    #[test]
    fn install_unpacks_verifies_and_is_idempotent() {
        let t = TempDir::new();
        let src = t.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("ludusavi"), b"#!/bin/sh\necho fake\n").unwrap();
        let tgz = t.path().join("l.tar.gz");
        let out = Command::new("tar")
            .arg("-czf")
            .arg(&tgz)
            .arg("-C")
            .arg(&src)
            .arg("ludusavi")
            .output()
            .unwrap();
        assert!(out.status.success());
        let bytes = fs::read(&tgz).unwrap();
        let bin_sha = plugin_toolkit::hash::sha256_hex(b"#!/bin/sh\necho fake\n");
        let data = t.path().join("data");
        fs::create_dir_all(&data).unwrap();

        let dir = data.join("bad");
        assert!(install(&data, &dir, &bytes, &"0".repeat(64)).is_err());
        assert!(!dir.exists());

        let dir = data.join(VERSION);
        install(&data, &dir, &bytes, &bin_sha).unwrap();
        assert!(dir.join("ludusavi").is_file());
        install(&data, &dir, &bytes, &bin_sha).unwrap();
        let leftovers: Vec<_> = fs::read_dir(&data)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec![VERSION.to_string()]);
    }

    #[test]
    fn verify_rejects_a_mismatched_download() {
        assert!(verify(b"abc", &plugin_toolkit::hash::sha256_hex(b"abc")).is_ok());
        assert!(verify(b"abd", &plugin_toolkit::hash::sha256_hex(b"abc")).is_err());
        assert_eq!(LINUX_X64_SHA256.len(), 64);
        assert_eq!(LINUX_X64_BIN_SHA256.len(), 64);
        assert!(LINUX_X64_URL.contains(VERSION));
    }
}

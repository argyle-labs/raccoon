//! Ludusavi (<https://github.com/mtkennerly/ludusavi>) as the save-discovery
//! engine: its PCGamingWiki-backed manifest knows where each game keeps its
//! saves across Steam/Proton, Heroic and plain wine prefixes. orca only drives
//! `backup --preview --api` (a read-only scan) and does the copying itself.
//!
//! Runs unprivileged out of a plugin-private dir
//! (`~/.local/share/orca/raccoon/ludusavi`) with its own `--config`, so the
//! user's own ludusavi setup is never read or touched.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use plugin_toolkit::serde_json::{self, Value, json};
use serde::Deserialize;

use super::layout::Layout;

pub const VERSION: &str = "0.31.0";
const LINUX_X64_URL: &str = "https://github.com/mtkennerly/ludusavi/releases/download/v0.31.0/ludusavi-v0.31.0-linux.tar.gz";
/// Upstream publishes no checksums, so the release tarball's sha256 is pinned
/// here (computed from the v0.31.0 GitHub release asset).
const LINUX_X64_SHA256: &str = "7322ff45d41eae7ae064a80d8c9ecccc5b8fb6fc090a603a66369cd4b054068d";

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
pub struct PreviewFile {
    #[serde(default)]
    pub failed: bool,
    #[serde(default)]
    pub ignored: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewErrors {
    #[serde(default)]
    pub unknown_games: Option<Vec<String>>,
}

impl Preview {
    pub fn parse(stdout: &str) -> Result<Self, String> {
        serde_json::from_str(stdout).map_err(|e| format!("parse ludusavi output: {e}"))
    }

    /// The files ludusavi would back up for `title`.
    pub fn files(&self, title: &str) -> Vec<PathBuf> {
        self.games
            .get(title)
            .map(|g| {
                g.files
                    .iter()
                    .filter(|(_, f)| !f.ignored && !f.failed)
                    .map(|(p, _)| PathBuf::from(p))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// A save scan: all games (`None`) or one title.
pub trait Scanner {
    fn preview(&self, title: Option<&str>) -> Result<Preview, String>;
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

fn expand_home(home: &Path, p: &str) -> String {
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
    /// Find or install ludusavi and (re)write its private config.
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
            fs::write(&path, body).map_err(|e| format!("write {}: {e}", path.display()))?;
        }
        Ok(Self { bin, config_dir })
    }
}

impl Scanner for Ludusavi {
    fn preview(&self, title: Option<&str>) -> Result<Preview, String> {
        let mut cmd = Command::new(&self.bin);
        cmd.arg("--config")
            .arg(&self.config_dir)
            // Offline hosts scan with the cached manifest instead of failing.
            .arg("--try-manifest-update")
            .args(["backup", "--preview", "--api"])
            // With no titles ludusavi reads them from a non-tty stdin.
            .stdin(Stdio::null());
        if let Some(t) = title {
            cmd.arg("--").arg(t);
        }
        let out = cmd
            .output()
            .map_err(|e| format!("spawn {}: {e}", self.bin.display()))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        // An unknown title exits 1 but still prints valid JSON.
        if stdout.trim().is_empty() {
            return Err(format!(
                "ludusavi produced no output ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Preview::parse(&stdout)
    }
}

pub fn data_dir(home: &Path) -> PathBuf {
    home.join(".local/share/orca/raccoon/ludusavi")
}

/// A `ludusavi` on PATH, else the pinned release under `data`, downloading and
/// verifying it on first use.
fn provision(data: &Path) -> Result<PathBuf, String> {
    if let Some(p) = crate::checks::which("ludusavi") {
        return Ok(p);
    }
    let dir = data.join(VERSION);
    let bin = dir.join("ludusavi");
    if bin.is_file() {
        return Ok(bin);
    }
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return Err("no pinned ludusavi build for this platform; put `ludusavi` on PATH".into());
    }
    fs::create_dir_all(data).map_err(|e| format!("mkdir {}: {e}", data.display()))?;
    let tarball = plugin_toolkit::reactor::block_on(async {
        let resp = plugin_toolkit::reqwest::get(LINUX_X64_URL)
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| format!("download ludusavi: {e}"))?;
        resp.bytes()
            .await
            .map_err(|e| format!("download ludusavi: {e}"))
    })?;
    verify(&tarball, LINUX_X64_SHA256)?;
    install(data, &dir, &tarball)?;
    Ok(bin)
}

fn verify(bytes: &[u8], want: &str) -> Result<(), String> {
    let got = plugin_toolkit::hash::sha256_hex(bytes);
    if got == want {
        Ok(())
    } else {
        Err(format!("ludusavi download sha256 {got} != pinned {want}"))
    }
}

/// Unpack into a sibling staging dir, then rename into place, so a crash never
/// leaves a half-extracted `dir` that later runs would trust.
fn install(data: &Path, dir: &Path, tarball: &[u8]) -> Result<(), String> {
    let pid = std::process::id();
    let archive = data.join(format!(".ludusavi-{pid}.tar.gz"));
    let staging = data.join(format!(".ludusavi-{pid}"));
    let result = (|| {
        fs::write(&archive, tarball).map_err(|e| format!("write {}: {e}", archive.display()))?;
        fs::create_dir_all(&staging).map_err(|e| format!("mkdir {}: {e}", staging.display()))?;
        let a = archive.to_string_lossy();
        let s = staging.to_string_lossy();
        crate::checks::run("tar", &["-xzf", &a, "-C", &s, "ludusavi"])?;
        if !staging.join("ludusavi").is_file() {
            return Err("ludusavi archive has no `ludusavi` binary".to_string());
        }
        match fs::rename(&staging, dir) {
            Ok(()) => Ok(()),
            // A concurrent run installed it first.
            Err(_) if dir.join("ludusavi").is_file() => Ok(()),
            Err(e) => Err(format!("install {}: {e}", dir.display())),
        }
    })();
    for leftover in [&archive, &staging] {
        let removed = if leftover.is_dir() {
            fs::remove_dir_all(leftover)
        } else {
            fs::remove_file(leftover)
        };
        if let Err(e) = removed
            && e.kind() != std::io::ErrorKind::NotFound
        {
            plugin_toolkit::tracing::warn!("[game-saves] cleanup {}: {e}", leftover.display());
        }
    }
    result
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
        assert_eq!(p.files("Hades"), vec![PathBuf::from("/h/a.sav")]);
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
                ("otherWine".into(), "Games/Heroic/Prefixes/default".into()),
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
    fn verify_rejects_a_mismatched_download() {
        assert!(verify(b"abc", &plugin_toolkit::hash::sha256_hex(b"abc")).is_ok());
        assert!(verify(b"abd", &plugin_toolkit::hash::sha256_hex(b"abc")).is_err());
        assert_eq!(LINUX_X64_SHA256.len(), 64);
        assert!(LINUX_X64_URL.contains(VERSION));
    }
}

//! Refreshing the bundled themes, fonts and default config.
//!
//! Installing a new binary is only half an upgrade: `scripts/install.sh`
//! also copies the bundled themes and fonts into `~/.config/crt`, and a new
//! release usually brings new ones. An in-place update has to do the same,
//! or a user who updates from inside the app quietly ends up with an old
//! theme set.
//!
//! The install script has always overwritten those files unconditionally,
//! so editing a bundled theme in place loses the edit on the next install.
//! Doing that silently from inside the app would be worse, so this module
//! keeps a manifest of what it wrote: a file is only overwritten when it is
//! byte-for-byte what we last put there. Anything the user has touched is
//! left alone and reported, so they can be told rather than surprised.
//!
//! `config.toml` is never touched at all. It is the user's file; the
//! bundled copy lands beside it as `default_config.toml` for reference.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The user's own config, never written by an update.
const USER_CONFIG: &str = "config.toml";
/// Where the bundled config is kept for reference.
const DEFAULT_CONFIG: &str = "default_config.toml";

/// Record of what the updater has written into the config directory.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AssetManifest {
    /// Version that last wrote these files, for diagnostics.
    pub version: Option<String>,
    /// Relative path inside the config dir -> sha256 hex of what we wrote.
    pub files: BTreeMap<String, String>,
}

impl AssetManifest {
    /// `<config_dir>/state/assets.toml`
    pub fn path(config_dir: &Path) -> PathBuf {
        config_dir.join("state").join("assets.toml")
    }

    /// Read the manifest, treating anything unreadable as empty.
    ///
    /// An empty manifest is also what every install predating this feature
    /// has, so that path has to be safe: see [`refresh`].
    pub fn load(path: &Path) -> Self {
        let Ok(text) = fs::read_to_string(path) else {
            return Self::default();
        };
        match toml::from_str(&text) {
            Ok(manifest) => manifest,
            Err(e) => {
                log::warn!("asset manifest malformed ({}): {e}", path.display());
                Self::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, path)
    }
}

/// What a refresh did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshReport {
    /// Files written or updated.
    pub written: Vec<String>,
    /// Files left alone because the user had changed them.
    pub skipped: Vec<String>,
    /// Files already identical to the bundled copy.
    pub unchanged: usize,
}

impl RefreshReport {
    pub fn is_empty(&self) -> bool {
        self.written.is_empty() && self.skipped.is_empty()
    }

    /// One line for a toast, or `None` when nothing worth saying happened.
    pub fn summary(&self) -> Option<String> {
        if self.skipped.is_empty() {
            return None;
        }
        let mut names: Vec<&str> = self
            .skipped
            .iter()
            .map(|path| {
                Path::new(path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(path.as_str())
            })
            .collect();
        names.sort_unstable();
        Some(format!(
            "Kept your edits to {} bundled file{}: {}",
            names.len(),
            if names.len() == 1 { "" } else { "s" },
            names.join(", ")
        ))
    }
}

/// Copy the bundled assets into the config directory.
///
/// Rules per file, in order:
///
/// - not there yet: write it and remember the hash;
/// - identical to the bundled copy: nothing to do;
/// - exactly what we last wrote: overwrite, it is ours to update;
/// - anything else: the user has edited it, so leave it and report it.
///
/// The last rule is also what protects installs that predate the manifest:
/// with no record of a file, a difference is assumed to be the user's.
pub fn refresh_assets(
    bundled: &Path,
    config_dir: &Path,
    manifest: &mut AssetManifest,
    version: &str,
) -> io::Result<RefreshReport> {
    let mut report = RefreshReport::default();
    if !bundled.is_dir() {
        return Ok(report);
    }

    for source in walk(bundled)? {
        let relative = source
            .strip_prefix(bundled)
            .expect("walk returns paths under the root")
            .to_path_buf();

        // The bundled config is reference material; the live one is the
        // user's and is only ever created, never updated.
        let destination_rel = if relative == Path::new(USER_CONFIG) {
            seed_user_config(&source, config_dir)?;
            PathBuf::from(DEFAULT_CONFIG)
        } else {
            relative
        };

        let key = destination_rel.to_string_lossy().replace('\\', "/");
        let destination = config_dir.join(&destination_rel);
        let bundled_hash = hash_file(&source)?;

        match fs::read(&destination) {
            // Never written, or removed by the user: (re)create it.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                write_file(&source, &destination)?;
                manifest.files.insert(key.clone(), bundled_hash);
                report.written.push(key);
            }
            Err(e) => return Err(e),
            Ok(existing) => {
                let existing_hash = hash_bytes(&existing);
                if existing_hash == bundled_hash {
                    manifest.files.insert(key, bundled_hash);
                    report.unchanged += 1;
                } else if manifest.files.get(&key) == Some(&existing_hash) {
                    // Untouched since we wrote it, so it is ours to replace.
                    write_file(&source, &destination)?;
                    manifest.files.insert(key.clone(), bundled_hash);
                    report.written.push(key);
                } else {
                    report.skipped.push(key);
                }
            }
        }
    }

    manifest.version = Some(version.to_string());
    report.written.sort();
    report.skipped.sort();
    Ok(report)
}

/// Create `config.toml` from the bundled copy when the user has none.
fn seed_user_config(source: &Path, config_dir: &Path) -> io::Result<()> {
    let destination = config_dir.join(USER_CONFIG);
    if destination.exists() {
        return Ok(());
    }
    write_file(source, &destination)
}

fn write_file(source: &Path, destination: &Path) -> io::Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, destination)?;
    Ok(())
}

/// Every file under `root`, depth first, with directories skipped.
fn walk(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

fn hash_file(path: &Path) -> io::Result<String> {
    Ok(hash_bytes(&fs::read(path)?))
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("crt-assets-{}-{}", name, std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A bundled asset tree shaped like the one in the release tarball.
    fn bundled_assets(root: &Path, dracula: &str) -> PathBuf {
        let assets = root.join("assets");
        fs::create_dir_all(assets.join("themes")).unwrap();
        fs::create_dir_all(assets.join("fonts")).unwrap();
        fs::write(assets.join("config.toml"), "# bundled config\n").unwrap();
        fs::write(assets.join("themes").join("dracula.css"), dracula).unwrap();
        fs::write(assets.join("themes").join("matrix.css"), "matrix v1").unwrap();
        fs::write(assets.join("fonts").join("Meslo.ttf"), "font bytes").unwrap();
        assets
    }

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    #[test]
    fn a_fresh_config_directory_gets_everything() {
        let dir = TempDir::new("fresh");
        let assets = bundled_assets(dir.path(), "dracula v1");
        let config = dir.path().join("config");
        let mut manifest = AssetManifest::default();

        let report = refresh_assets(&assets, &config, &mut manifest, "0.1.6").unwrap();

        assert_eq!(report.skipped, Vec::<String>::new());
        assert!(report.written.contains(&"themes/dracula.css".to_string()));
        assert!(report.written.contains(&"fonts/Meslo.ttf".to_string()));
        // The bundled config lands as reference material...
        assert_eq!(
            read(&config.join("default_config.toml")),
            "# bundled config\n"
        );
        // ...and seeds the user's config, since they had none.
        assert_eq!(read(&config.join(USER_CONFIG)), "# bundled config\n");
        assert_eq!(manifest.version.as_deref(), Some("0.1.6"));
        // config.toml is the user's and is never tracked.
        assert!(!manifest.files.contains_key(USER_CONFIG));
    }

    #[test]
    fn an_unchanged_file_is_adopted_and_then_updated_by_the_next_release() {
        let dir = TempDir::new("adopt");
        let assets = bundled_assets(dir.path(), "dracula v1");
        let config = dir.path().join("config");
        let mut manifest = AssetManifest::default();
        refresh_assets(&assets, &config, &mut manifest, "0.1.6").unwrap();

        // A later release ships a new dracula.
        fs::write(assets.join("themes").join("dracula.css"), "dracula v2").unwrap();
        let report = refresh_assets(&assets, &config, &mut manifest, "0.1.7").unwrap();

        assert!(report.written.contains(&"themes/dracula.css".to_string()));
        assert!(report.skipped.is_empty());
        assert_eq!(
            read(&config.join("themes").join("dracula.css")),
            "dracula v2"
        );
    }

    #[test]
    fn a_file_the_user_edited_is_kept_and_reported() {
        let dir = TempDir::new("edited");
        let assets = bundled_assets(dir.path(), "dracula v1");
        let config = dir.path().join("config");
        let mut manifest = AssetManifest::default();
        refresh_assets(&assets, &config, &mut manifest, "0.1.6").unwrap();

        // The user tweaks their copy...
        fs::write(
            config.join("themes").join("dracula.css"),
            "dracula v1 /* my colours */",
        )
        .unwrap();
        // ...and a later release changes the same file.
        fs::write(assets.join("themes").join("dracula.css"), "dracula v2").unwrap();

        let report = refresh_assets(&assets, &config, &mut manifest, "0.1.7").unwrap();

        assert_eq!(report.skipped, vec!["themes/dracula.css".to_string()]);
        assert_eq!(
            read(&config.join("themes").join("dracula.css")),
            "dracula v1 /* my colours */"
        );
        let summary = report.summary().expect("something to say");
        assert!(summary.contains("dracula"), "{summary}");
    }

    #[test]
    fn an_install_predating_the_manifest_keeps_its_edits() {
        // Every existing install lands here: files on disk, no manifest.
        let dir = TempDir::new("nomanifest");
        let assets = bundled_assets(dir.path(), "dracula v2");
        let config = dir.path().join("config");
        fs::create_dir_all(config.join("themes")).unwrap();
        fs::write(config.join("themes").join("dracula.css"), "my own dracula").unwrap();
        // matrix.css was never touched and matches the bundled copy.
        fs::write(config.join("themes").join("matrix.css"), "matrix v1").unwrap();

        let mut manifest = AssetManifest::default();
        let report = refresh_assets(&assets, &config, &mut manifest, "0.1.6").unwrap();

        assert_eq!(report.skipped, vec!["themes/dracula.css".to_string()]);
        assert_eq!(
            read(&config.join("themes").join("dracula.css")),
            "my own dracula"
        );
        // The untouched one is adopted, so the next release can update it.
        assert_eq!(
            manifest.files.get("themes/matrix.css").map(String::as_str),
            Some(hash_bytes(b"matrix v1").as_str())
        );
    }

    #[test]
    fn an_existing_user_config_is_never_overwritten() {
        let dir = TempDir::new("userconfig");
        let assets = bundled_assets(dir.path(), "dracula v1");
        let config = dir.path().join("config");
        fs::create_dir_all(&config).unwrap();
        fs::write(config.join(USER_CONFIG), "[font]\nsize = 18\n").unwrap();

        let mut manifest = AssetManifest::default();
        refresh_assets(&assets, &config, &mut manifest, "0.1.6").unwrap();

        assert_eq!(read(&config.join(USER_CONFIG)), "[font]\nsize = 18\n");
        assert_eq!(read(&config.join(DEFAULT_CONFIG)), "# bundled config\n");
    }

    #[test]
    fn a_deleted_file_comes_back() {
        let dir = TempDir::new("deleted");
        let assets = bundled_assets(dir.path(), "dracula v1");
        let config = dir.path().join("config");
        let mut manifest = AssetManifest::default();
        refresh_assets(&assets, &config, &mut manifest, "0.1.6").unwrap();

        fs::remove_file(config.join("themes").join("matrix.css")).unwrap();
        let report = refresh_assets(&assets, &config, &mut manifest, "0.1.6").unwrap();

        assert!(report.written.contains(&"themes/matrix.css".to_string()));
        assert_eq!(read(&config.join("themes").join("matrix.css")), "matrix v1");
    }

    #[test]
    fn a_new_theme_in_a_later_release_is_added() {
        let dir = TempDir::new("newtheme");
        let assets = bundled_assets(dir.path(), "dracula v1");
        let config = dir.path().join("config");
        let mut manifest = AssetManifest::default();
        refresh_assets(&assets, &config, &mut manifest, "0.1.6").unwrap();

        fs::write(assets.join("themes").join("solarized.css"), "solarized").unwrap();
        let report = refresh_assets(&assets, &config, &mut manifest, "0.1.7").unwrap();

        assert_eq!(report.written, vec!["themes/solarized.css".to_string()]);
        assert!(config.join("themes").join("solarized.css").is_file());
    }

    #[test]
    fn a_missing_bundle_directory_is_not_an_error() {
        let dir = TempDir::new("missing");
        let mut manifest = AssetManifest::default();
        let report = refresh_assets(
            &dir.path().join("nowhere"),
            &dir.path().join("config"),
            &mut manifest,
            "0.1.6",
        )
        .unwrap();
        assert!(report.is_empty());
    }

    #[test]
    fn the_manifest_round_trips_and_survives_corruption() {
        let dir = TempDir::new("manifest");
        let path = AssetManifest::path(dir.path());
        assert_eq!(AssetManifest::load(&path), AssetManifest::default());

        let manifest = AssetManifest {
            version: Some("0.1.6".into()),
            files: BTreeMap::from([("themes/a.css".to_string(), "abc".to_string())]),
        };
        manifest.save(&path).unwrap();
        assert_eq!(AssetManifest::load(&path), manifest);

        fs::write(&path, "not toml {{{").unwrap();
        assert_eq!(AssetManifest::load(&path), AssetManifest::default());
    }

    #[test]
    fn the_summary_only_speaks_up_about_skipped_files() {
        let mut report = RefreshReport::default();
        assert_eq!(report.summary(), None);
        report.written.push("themes/a.css".into());
        assert_eq!(report.summary(), None, "writing files is not news");

        report.skipped.push("themes/dracula.css".into());
        let one = report.summary().unwrap();
        assert!(one.contains("1 bundled file:"), "{one}");
        assert!(one.contains("dracula"), "{one}");

        report.skipped.push("themes/matrix.css".into());
        let two = report.summary().unwrap();
        assert!(two.contains("2 bundled files:"), "{two}");
    }
}

//! Parsing of the `SHA256SUMS` file published with every release.
//!
//! The file is plain coreutils output, one line per release asset:
//!
//! ```text
//! d638933a…cdbf  crt-0.1.6-linux-x86_64.tar.gz
//! 635267c0…79a4  crt-0.1.6-macos-aarch64.tar.gz
//! ```
//!
//! It is fetched from `releases/latest/download/SHA256SUMS`, which GitHub
//! redirects to the newest non-prerelease tag. The version is read from the
//! filenames rather than the tag, so one request answers "what is current?"
//! and "what should this download hash to?" at once.
//!
//! Anything unexpected is an error rather than a guess: a release we cannot
//! read is treated as no release at all, which leaves the user on a working
//! install.

use std::collections::BTreeMap;

use semver::Version;
use thiserror::Error;

/// Where the update check looks. `latest` resolves to the newest release
/// that is not a prerelease, which is why prerelease channels are out of
/// scope for this crate.
pub const LATEST_SUMS_URL: &str =
    "https://github.com/colliery-io/crt/releases/latest/download/SHA256SUMS";

/// Base for per-tag asset downloads.
const RELEASE_DOWNLOAD_BASE: &str = "https://github.com/colliery-io/crt/releases/download";

/// Where to look for the release description.
///
/// Debug builds honour `CRT_UPDATE_URL`, which is how the download, verify
/// and swap path gets exercised end to end against a local release (curl
/// takes `file://` URLs) without publishing one. Release builds always use
/// GitHub: an environment variable that redirects the updater would be a way
/// to feed it someone else's bytes.
pub fn sums_url() -> String {
    #[cfg(debug_assertions)]
    if let Ok(url) = std::env::var("CRT_UPDATE_URL")
        && !url.is_empty()
    {
        return url;
    }
    LATEST_SUMS_URL.to_string()
}

/// Release assets are named `crt-<version>-<os>-<arch>.tar.gz`.
const ASSET_PREFIX: &str = "crt-";
const ASSET_SUFFIX: &str = ".tar.gz";

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ManifestError {
    #[error("SHA256SUMS listed no release assets")]
    NoAssets,

    #[error("SHA256SUMS line {line}: {reason}")]
    Malformed { line: usize, reason: String },

    #[error("SHA256SUMS mixes versions {first} and {second}")]
    MixedVersions { first: Version, second: Version },

    #[error("could not parse version {value:?}: {reason}")]
    Version { value: String, reason: String },
}

/// One downloadable release asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    /// Bare filename, e.g. `crt-0.1.6-macos-aarch64.tar.gz`.
    pub name: String,
    /// `macos` or `linux`, as spelled in the filename.
    pub os: String,
    /// `x86_64` or `aarch64`, as spelled in the filename.
    pub arch: String,
    /// Expected SHA-256 of the tarball.
    pub sha256: [u8; 32],
    /// Full download URL for the release this asset belongs to.
    pub download_url: String,
}

/// Every asset of one release, plus the version they agree on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseManifest {
    pub version: Version,
    /// Keyed by asset filename, so iteration order is stable.
    pub assets: BTreeMap<String, Asset>,
}

impl ReleaseManifest {
    /// Parse the contents of a `SHA256SUMS` file.
    ///
    /// Accepts blank lines, `#` comments and the `*` binary marker some
    /// `sha256sum` modes emit. Rejects a file whose entries disagree about
    /// the version: that means the release was assembled wrongly, and
    /// picking one of the versions would be a guess.
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        let mut version: Option<Version> = None;
        let mut assets = BTreeMap::new();

        for (index, raw_line) in text.lines().enumerate() {
            let line_no = index + 1;
            // `trim` also takes care of the \r in CRLF files.
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let (sha256, name) = parse_line(line, line_no)?;
            let (asset_version, os, arch) = parse_asset_name(&name, line_no)?;

            match &version {
                None => version = Some(asset_version.clone()),
                Some(seen) if *seen != asset_version => {
                    return Err(ManifestError::MixedVersions {
                        first: seen.clone(),
                        second: asset_version,
                    });
                }
                Some(_) => {}
            }

            let download_url = format!("{RELEASE_DOWNLOAD_BASE}/v{}/{}", asset_version, name);
            assets.insert(
                name.clone(),
                Asset {
                    name,
                    os,
                    arch,
                    sha256,
                    download_url,
                },
            );
        }

        match version {
            Some(version) => Ok(ReleaseManifest { version, assets }),
            None => Err(ManifestError::NoAssets),
        }
    }

    /// The asset built for a given platform, if the release has one.
    pub fn asset_for(&self, os: &str, arch: &str) -> Option<&Asset> {
        self.assets
            .values()
            .find(|asset| asset.os == os && asset.arch == arch)
    }

    /// The asset for the platform this binary was built for.
    pub fn asset_for_current_platform(&self) -> Option<&Asset> {
        let (os, arch) = current_platform();
        self.asset_for(os, arch)
    }
}

/// Split one line into its hash and filename.
fn parse_line(line: &str, line_no: usize) -> Result<([u8; 32], String), ManifestError> {
    let mut fields = line.split_whitespace();
    let malformed = |reason: &str| ManifestError::Malformed {
        line: line_no,
        reason: reason.to_string(),
    };

    let hash = fields.next().ok_or_else(|| malformed("empty line"))?;
    let name = fields
        .next()
        .ok_or_else(|| malformed("no filename after the hash"))?;
    if fields.next().is_some() {
        // Asset names never contain spaces, so a third field means the line
        // is not what we think it is.
        return Err(malformed("unexpected extra fields"));
    }

    let sha256 = parse_sha256(hash)
        .ok_or_else(|| malformed(&format!("{hash:?} is not a 64-character hex digest")))?;
    // Tolerate the binary marker: "<hash> *<name>".
    let name = name.strip_prefix('*').unwrap_or(name);
    Ok((sha256, name.to_string()))
}

fn parse_sha256(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Pull the version, OS and architecture out of `crt-<ver>-<os>-<arch>.tar.gz`.
///
/// Split from the right: the version may itself contain hyphens, as in
/// `0.2.0-rc.1`, while the OS and architecture never do.
fn parse_asset_name(
    name: &str,
    line_no: usize,
) -> Result<(Version, String, String), ManifestError> {
    let malformed = |reason: String| ManifestError::Malformed {
        line: line_no,
        reason,
    };

    let stem = name
        .strip_prefix(ASSET_PREFIX)
        .and_then(|rest| rest.strip_suffix(ASSET_SUFFIX))
        .ok_or_else(|| {
            malformed(format!(
                "{name:?} is not named crt-<version>-<os>-<arch>{ASSET_SUFFIX}"
            ))
        })?;

    let (rest, arch) = stem
        .rsplit_once('-')
        .ok_or_else(|| malformed(format!("{name:?} has no architecture")))?;
    let (version, os) = rest
        .rsplit_once('-')
        .ok_or_else(|| malformed(format!("{name:?} has no operating system")))?;

    if version.is_empty() || os.is_empty() || arch.is_empty() {
        return Err(malformed(format!("{name:?} has an empty name component")));
    }

    let version = parse_version(version).map_err(|e| malformed(e.to_string()))?;
    Ok((version, os.to_string(), arch.to_string()))
}

/// Parse a semver string, tolerating a leading `v` as used by git tags.
pub fn parse_version(value: &str) -> Result<Version, ManifestError> {
    let trimmed = value.trim();
    let bare = trimmed.strip_prefix('v').unwrap_or(trimmed);
    Version::parse(bare).map_err(|e| ManifestError::Version {
        value: value.to_string(),
        reason: e.to_string(),
    })
}

/// How the running version relates to the latest release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    UpToDate,
    Newer(Version),
    /// The running build is ahead of the latest release: a local build, or a
    /// release that was pulled. Never offer to "update" backwards.
    RunningIsNewer,
}

impl UpdateStatus {
    pub fn available_version(&self) -> Option<&Version> {
        match self {
            UpdateStatus::Newer(version) => Some(version),
            _ => None,
        }
    }
}

/// Compare the running version against a release.
///
/// An unparseable running version is an error rather than a panic: the app
/// should log it and leave the user alone, not crash on startup.
pub fn compare(running: &str, latest: &Version) -> Result<UpdateStatus, ManifestError> {
    let running = parse_version(running)?;
    Ok(match running.cmp(latest) {
        std::cmp::Ordering::Less => UpdateStatus::Newer(latest.clone()),
        std::cmp::Ordering::Equal => UpdateStatus::UpToDate,
        std::cmp::Ordering::Greater => UpdateStatus::RunningIsNewer,
    })
}

/// The `(os, arch)` pair naming the assets this build can install, spelled as
/// the release filenames spell them.
pub const fn current_platform() -> (&'static str, &'static str) {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    (os, arch)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped exactly like the file the release workflow writes.
    const REAL: &str = "\
a58bcb0750a280d603c76d394668b5c77ad201c442ae8e2819d5547da6ac1ee7  crt-0.1.6-linux-aarch64.tar.gz
d638933a4e6f32518a86553ed529cd4c0f9d7f6e170fd69393317610a592cdbf  crt-0.1.6-linux-x86_64.tar.gz
635267c098980f9c89d415dfe883e280ebb04600fd1d62bda0c9823bf1c679a4  crt-0.1.6-macos-aarch64.tar.gz
30912c8fbcf9567dc05dc2d89234e844d60bb815e13debc7a0d7e514eae6a6e7  crt-0.1.6-macos-x86_64.tar.gz
";

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn parses_a_real_release_file() {
        let m = ReleaseManifest::parse(REAL).expect("parses");
        assert_eq!(m.version, v("0.1.6"));
        assert_eq!(m.assets.len(), 4);

        let asset = m.asset_for("macos", "aarch64").expect("macos arm asset");
        assert_eq!(asset.name, "crt-0.1.6-macos-aarch64.tar.gz");
        assert_eq!(asset.sha256[0], 0x63);
        assert_eq!(asset.sha256[31], 0xa4);
        assert_eq!(
            asset.download_url,
            "https://github.com/colliery-io/crt/releases/download/v0.1.6/crt-0.1.6-macos-aarch64.tar.gz"
        );
    }

    #[test]
    fn selects_every_platform_and_reports_a_missing_one() {
        let m = ReleaseManifest::parse(REAL).unwrap();
        for (os, arch) in [
            ("linux", "x86_64"),
            ("linux", "aarch64"),
            ("macos", "x86_64"),
            ("macos", "aarch64"),
        ] {
            let asset = m.asset_for(os, arch).expect("asset present");
            assert_eq!((asset.os.as_str(), asset.arch.as_str()), (os, arch));
        }
        assert!(m.asset_for("windows", "x86_64").is_none());
        assert!(m.asset_for("linux", "riscv64").is_none());
    }

    #[test]
    fn the_current_platform_has_an_asset_in_a_full_release() {
        let m = ReleaseManifest::parse(REAL).unwrap();
        assert!(m.asset_for_current_platform().is_some());
    }

    #[test]
    fn tolerates_comments_blank_lines_crlf_and_the_binary_marker() {
        let text = "# generated by release.yml\r\n\r\n\
d638933a4e6f32518a86553ed529cd4c0f9d7f6e170fd69393317610a592cdbf *crt-0.1.6-linux-x86_64.tar.gz\r\n";
        let m = ReleaseManifest::parse(text).expect("parses");
        assert_eq!(m.version, v("0.1.6"));
        assert_eq!(m.assets.len(), 1);
        assert!(m.assets.contains_key("crt-0.1.6-linux-x86_64.tar.gz"));
    }

    #[test]
    fn rejects_a_release_that_mixes_versions() {
        let text = "\
d638933a4e6f32518a86553ed529cd4c0f9d7f6e170fd69393317610a592cdbf  crt-0.1.6-linux-x86_64.tar.gz
a58bcb0750a280d603c76d394668b5c77ad201c442ae8e2819d5547da6ac1ee7  crt-0.1.7-linux-aarch64.tar.gz
";
        assert_eq!(
            ReleaseManifest::parse(text),
            Err(ManifestError::MixedVersions {
                first: v("0.1.6"),
                second: v("0.1.7"),
            })
        );
    }

    #[test]
    fn rejects_bad_hashes() {
        for hash in [
            "deadbeef",                                                           // too short
            "d638933a4e6f32518a86553ed529cd4c0f9d7f6e170fd69393317610a592cdbfff", // too long
            "z638933a4e6f32518a86553ed529cd4c0f9d7f6e170fd69393317610a592cdbf",   // not hex
        ] {
            let text = format!("{hash}  crt-0.1.6-linux-x86_64.tar.gz\n");
            let err = ReleaseManifest::parse(&text).unwrap_err();
            assert!(
                matches!(err, ManifestError::Malformed { line: 1, .. }),
                "{hash} gave {err:?}"
            );
        }
    }

    #[test]
    fn rejects_unexpected_filenames_and_line_shapes() {
        for case in [
            "d638933a4e6f32518a86553ed529cd4c0f9d7f6e170fd69393317610a592cdbf  crt-0.1.6-linux-x86_64.zip",
            "d638933a4e6f32518a86553ed529cd4c0f9d7f6e170fd69393317610a592cdbf  something-else.tar.gz",
            "d638933a4e6f32518a86553ed529cd4c0f9d7f6e170fd69393317610a592cdbf  crt-0.1.6-linux.tar.gz",
            "d638933a4e6f32518a86553ed529cd4c0f9d7f6e170fd69393317610a592cdbf  crt-nope-linux-x86_64.tar.gz",
            "d638933a4e6f32518a86553ed529cd4c0f9d7f6e170fd69393317610a592cdbf",
            "d638933a4e6f32518a86553ed529cd4c0f9d7f6e170fd69393317610a592cdbf  a.tar.gz  b.tar.gz",
        ] {
            assert!(ReleaseManifest::parse(case).is_err(), "accepted {case:?}");
        }
    }

    #[test]
    fn an_empty_or_comment_only_file_has_no_assets() {
        assert_eq!(ReleaseManifest::parse(""), Err(ManifestError::NoAssets));
        assert_eq!(
            ReleaseManifest::parse("# nothing here\n\n"),
            Err(ManifestError::NoAssets)
        );
    }

    #[test]
    fn keeps_a_prerelease_version_in_the_filename() {
        let text = "d638933a4e6f32518a86553ed529cd4c0f9d7f6e170fd69393317610a592cdbf  crt-0.2.0-rc.1-linux-x86_64.tar.gz\n";
        let m = ReleaseManifest::parse(text).expect("parses");
        assert_eq!(m.version, v("0.2.0-rc.1"));
        assert_eq!(m.asset_for("linux", "x86_64").unwrap().arch, "x86_64");
    }

    #[test]
    fn compares_versions_in_both_directions() {
        assert_eq!(
            compare("0.1.5", &v("0.1.6")).unwrap(),
            UpdateStatus::Newer(v("0.1.6"))
        );
        assert_eq!(
            compare("0.1.6", &v("0.1.6")).unwrap(),
            UpdateStatus::UpToDate
        );
        assert_eq!(
            compare("0.2.0", &v("0.1.6")).unwrap(),
            UpdateStatus::RunningIsNewer
        );
    }

    #[test]
    fn compare_tolerates_a_v_prefix_and_orders_prereleases_below_releases() {
        assert_eq!(
            compare("v0.1.5", &v("0.1.6")).unwrap(),
            UpdateStatus::Newer(v("0.1.6"))
        );
        // semver: 0.2.0-rc.1 precedes 0.2.0, so a release supersedes its rc.
        assert_eq!(
            compare("0.2.0-rc.1", &v("0.2.0")).unwrap(),
            UpdateStatus::Newer(v("0.2.0"))
        );
        assert_eq!(
            compare("0.2.0", &v("0.2.0-rc.1")).unwrap(),
            UpdateStatus::RunningIsNewer
        );
    }

    #[test]
    fn an_unparseable_running_version_is_an_error_not_a_panic() {
        let err = compare("not-a-version", &v("0.1.6")).unwrap_err();
        assert!(matches!(err, ManifestError::Version { .. }), "{err:?}");
    }

    #[test]
    fn available_version_is_only_set_when_newer() {
        assert_eq!(
            UpdateStatus::Newer(v("1.0.0")).available_version(),
            Some(&v("1.0.0"))
        );
        assert_eq!(UpdateStatus::UpToDate.available_version(), None);
        assert_eq!(UpdateStatus::RunningIsNewer.available_version(), None);
    }

    #[test]
    fn current_platform_is_one_we_publish() {
        let (os, arch) = current_platform();
        assert!(matches!(os, "macos" | "linux"), "{os}");
        assert!(matches!(arch, "x86_64" | "aarch64"), "{arch}");
    }
}

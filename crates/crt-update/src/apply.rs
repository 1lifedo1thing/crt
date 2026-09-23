//! Replacing an install with a newer one.
//!
//! The shape of the operation is fixed by one requirement: at no point may
//! the user be left without a working CRT. So the new version is assembled
//! completely, beside the old one and on the same filesystem, and only then
//! swapped in with two renames. A crash anywhere before the first rename
//! leaves the old install untouched; between the two renames, the old
//! install is still there under `.old` and is put back.
//!
//! The previous version is deliberately kept after a successful swap. It is
//! removed on the next launch, once the new binary has proved it can start
//! (CRT-T-0216); until then it is the recovery path.
//!
//! The process doing the swap keeps running from the old inode, so an update
//! never disturbs the shells in the window that started it.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use semver::Version;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::assets::{AssetManifest, RefreshReport, refresh_assets};
use crate::fetch::{Fetch, FetchError};
use crate::install_kind::InstallKind;
use crate::manifest::Asset;

/// How long a download may take. Generous: the tarballs are ~17 MB and some
/// connections are slow, but not unbounded.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// Directory name used for staging, hidden so it does not show up in Finder
/// or a file listing next to the binary.
const STAGING_DIR: &str = ".crt-update";

/// Suffix given to the outgoing version.
const PREVIOUS_EXT: &str = "old";

/// Where the outgoing version is moved to.
///
/// The suffix is appended rather than replacing an extension: a bundle is
/// `crt.app`, and `with_extension` would turn that into `crt.old` and lose
/// the `.app`, which macOS treats as a different kind of thing entirely.
fn previous_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(PREVIOUS_EXT);
    target.with_file_name(name)
}

/// What the updater is doing, for progress reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Downloading,
    Verifying,
    Unpacking,
    Installing,
}

impl Stage {
    /// Progress text. The version is optional because the caller does not
    /// always know it yet: the release is read as part of the update.
    pub fn message(&self, version: Option<&Version>) -> String {
        match self {
            Stage::Downloading => match version {
                Some(version) => format!("Downloading v{version}…"),
                None => "Downloading update…".to_string(),
            },
            Stage::Verifying => "Verifying download…".to_string(),
            Stage::Unpacking => "Unpacking…".to_string(),
            Stage::Installing => "Installing…".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ApplyError {
    #[error("this install cannot be updated in place")]
    NotSelfReplaceable,

    #[error("another update is already running (pid {pid})")]
    InProgress { pid: u32 },

    #[error("{0}")]
    Fetch(#[from] FetchError),

    #[error("the download did not match the published checksum")]
    ChecksumMismatch { expected: String, actual: String },

    #[error("the archive contains an unsafe path: {entry}")]
    UnsafeArchive { entry: String },

    #[error("could not unpack the download: {reason}")]
    Unpack { reason: String },

    #[error("the archive did not contain {expected}")]
    MissingPayload { expected: String },

    #[error("could not read the latest release: {reason}")]
    ReleaseUnreadable { reason: String },

    #[error("could not replace {path} (permission denied)")]
    PermissionDenied { path: PathBuf },

    #[error("could not install the new version: {reason}")]
    Swap { reason: String },

    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
}

impl ApplyError {
    /// What to show the user. Where there is something they can do about it,
    /// the message says so.
    pub fn user_message(&self) -> String {
        match self {
            ApplyError::PermissionDenied { path } => format!(
                "Could not replace {} (permission denied). Re-run the install script to update.",
                path.display()
            ),
            ApplyError::ChecksumMismatch { .. } => {
                "The download did not match the published checksum, so nothing was changed."
                    .to_string()
            }
            ApplyError::InProgress { .. } => "An update is already in progress.".to_string(),
            ApplyError::NotSelfReplaceable => {
                "This install is managed elsewhere and cannot update itself.".to_string()
            }
            other => format!("Update failed: {other}"),
        }
    }
}

fn io_err(path: &Path, e: io::Error) -> ApplyError {
    if e.kind() == io::ErrorKind::PermissionDenied {
        ApplyError::PermissionDenied {
            path: path.to_path_buf(),
        }
    } else {
        ApplyError::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        }
    }
}

/// Everything needed to perform one update.
#[derive(Debug, Clone)]
pub struct UpdatePlan {
    pub kind: InstallKind,
    pub asset: Asset,
    pub version: Version,
    /// Config directory to refresh bundled themes and fonts into. `None`
    /// installs the binary only.
    pub config_dir: Option<PathBuf>,
}

/// The result of a successful update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub version: Version,
    /// Where the outgoing version was moved to. Kept until the new version
    /// has started once.
    pub previous: PathBuf,
    /// What the bundled asset refresh did.
    pub assets: RefreshReport,
}

/// Which layout is being replaced. The pipeline is identical; only what we
/// look for inside the archive, and what gets renamed, differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// A single executable file.
    SingleBinary,
    /// A macOS `.app` directory.
    AppBundle,
}

impl Layout {
    fn of(kind: &InstallKind) -> Option<Self> {
        match kind {
            InstallKind::UserBinary { .. } => Some(Layout::SingleBinary),
            InstallKind::AppBundle { .. } => Some(Layout::AppBundle),
            _ => None,
        }
    }
}

/// Hold the update lock for as long as this value lives.
struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Take the lock in `staging`, or report who holds it.
///
/// A lock whose process is gone is stale - the previous run was killed - and
/// is taken over rather than blocking updates forever.
fn acquire_lock(staging: &Path) -> Result<LockGuard, ApplyError> {
    let path = staging.join("lock");
    for attempt in 0..2 {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use io::Write;
                let _ = write!(file, "{}", std::process::id());
                return Ok(LockGuard { path });
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt == 0 => {
                let holder = fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| text.trim().parse::<u32>().ok());
                match holder {
                    Some(pid) if process_is_alive(pid) => {
                        return Err(ApplyError::InProgress { pid });
                    }
                    _ => {
                        log::info!("removing a stale update lock at {}", path.display());
                        let _ = fs::remove_file(&path);
                    }
                }
            }
            Err(e) => return Err(io_err(&path, e)),
        }
    }
    Err(ApplyError::InProgress { pid: 0 })
}

/// Whether a pid belongs to a live process.
///
/// `kill -0` rather than a libc dependency: this runs at most once per
/// update, only when a lock file is already in the way.
fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Download, verify, unpack and swap in a new version.
///
/// `progress` is called once per stage, on the calling thread.
pub fn apply(
    plan: &UpdatePlan,
    fetch: &dyn Fetch,
    progress: &mut dyn FnMut(Stage),
) -> Result<Applied, ApplyError> {
    let layout = Layout::of(&plan.kind).ok_or(ApplyError::NotSelfReplaceable)?;
    let target = plan
        .kind
        .swap_target()
        .ok_or(ApplyError::NotSelfReplaceable)?
        .to_path_buf();
    let container = target
        .parent()
        .ok_or_else(|| ApplyError::Swap {
            reason: format!("{} has no parent directory", target.display()),
        })?
        .to_path_buf();

    // Staging sits beside the target so the final rename stays within one
    // filesystem, which is what makes it atomic.
    let staging = container.join(STAGING_DIR);
    fs::create_dir_all(&staging).map_err(|e| io_err(&staging, e))?;
    let lock = acquire_lock(&staging)?;

    // Anything left by an earlier run is rubbish now that we hold the lock.
    clean_staging(&staging);

    let result = run_pipeline(plan, layout, fetch, progress, &staging, &target);

    // Leave nothing behind whether or not it worked. The lock file has to go
    // before the directory can, so drop the guard first.
    clean_staging(&staging);
    drop(lock);
    let _ = fs::remove_dir(&staging);

    result
}

/// Remove staging contents, keeping the lock file we hold.
fn clean_staging(staging: &Path) {
    let Ok(entries) = fs::read_dir(staging) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name() == "lock" {
            continue;
        }
        let path = entry.path();
        let _ = if path.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
    }
}

fn run_pipeline(
    plan: &UpdatePlan,
    layout: Layout,
    fetch: &dyn Fetch,
    progress: &mut dyn FnMut(Stage),
    staging: &Path,
    target: &Path,
) -> Result<Applied, ApplyError> {
    let archive = staging.join(&plan.asset.name);

    progress(Stage::Downloading);
    fetch.download(&plan.asset.download_url, &archive, DOWNLOAD_TIMEOUT)?;

    progress(Stage::Verifying);
    let actual = sha256_file(&archive)?;
    if actual != plan.asset.sha256 {
        // Never unpack bytes we cannot vouch for.
        let _ = fs::remove_file(&archive);
        return Err(ApplyError::ChecksumMismatch {
            expected: hex(&plan.asset.sha256),
            actual: hex(&actual),
        });
    }

    progress(Stage::Unpacking);
    let unpacked = staging.join("unpacked");
    unpack(&archive, &unpacked)?;
    let staged = locate_payload(&unpacked, layout, target)?;

    progress(Stage::Installing);
    strip_quarantine(&staged);
    let previous = swap(&staged, target)?;

    // Installing the binary is only half an upgrade; the release also
    // carries the themes and fonts that belong with it. This happens after
    // the swap, so a failed install never disturbs the config directory.
    let assets = refresh_bundled_assets(plan, layout, &unpacked, target);

    Ok(Applied {
        version: plan.version.clone(),
        previous,
        assets,
    })
}

/// Copy the release's bundled assets into the config directory.
///
/// Where they are depends on what was just moved: a bare binary leaves the
/// rest of the archive in staging, while a bundle takes its `Resources`
/// with it to the install location.
fn refresh_bundled_assets(
    plan: &UpdatePlan,
    layout: Layout,
    unpacked: &Path,
    target: &Path,
) -> RefreshReport {
    let Some(config_dir) = plan.config_dir.as_deref() else {
        return RefreshReport::default();
    };

    let bundled = match layout {
        Layout::SingleBinary => {
            // Same asymmetry as the payload: the Linux archive has assets at
            // its root, the macOS one carries them inside the bundle.
            let at_root = unpacked.join("assets");
            if at_root.is_dir() {
                at_root
            } else {
                bundle_resources(unpacked).join("assets")
            }
        }
        Layout::AppBundle => target.join("Contents").join("Resources").join("assets"),
    };

    let manifest_path = AssetManifest::path(config_dir);
    let mut manifest = AssetManifest::load(&manifest_path);
    let version = plan.version.to_string();

    match refresh_assets(&bundled, config_dir, &mut manifest, &version) {
        Ok(report) => {
            if let Err(e) = manifest.save(&manifest_path) {
                log::warn!("could not save the asset manifest: {e}");
            }
            report
        }
        Err(e) => {
            // The binary is already installed and working; stale themes are
            // not worth reporting as a failed update.
            log::warn!("could not refresh bundled assets: {e}");
            RefreshReport::default()
        }
    }
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_file(path: &Path) -> Result<[u8; 32], ApplyError> {
    let mut file = fs::File::open(path).map_err(|e| io_err(path, e))?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher).map_err(|e| io_err(path, e))?;
    Ok(hasher.finalize().into())
}

/// Extract a `.tar.gz` into `dest`.
///
/// Entries are checked before anything is written: an archive that tries to
/// escape its destination is refused outright rather than partially applied.
fn unpack(archive: &Path, dest: &Path) -> Result<(), ApplyError> {
    for pass in 0..2 {
        let file = fs::File::open(archive).map_err(|e| io_err(archive, e))?;
        let decoder = flate2::read::GzDecoder::new(file);
        let mut tar = tar::Archive::new(decoder);
        let entries = tar.entries().map_err(|e| ApplyError::Unpack {
            reason: e.to_string(),
        })?;

        if pass == 0 {
            // First pass: look at every path, write nothing.
            for entry in entries {
                let entry = entry.map_err(|e| ApplyError::Unpack {
                    reason: e.to_string(),
                })?;
                let path = entry.path().map_err(|e| ApplyError::Unpack {
                    reason: e.to_string(),
                })?;
                check_safe_path(&path)?;
            }
            continue;
        }

        fs::create_dir_all(dest).map_err(|e| io_err(dest, e))?;
        let file = fs::File::open(archive).map_err(|e| io_err(archive, e))?;
        let decoder = flate2::read::GzDecoder::new(file);
        let mut tar = tar::Archive::new(decoder);
        tar.set_preserve_permissions(true);
        tar.unpack(dest).map_err(|e| ApplyError::Unpack {
            reason: e.to_string(),
        })?;
    }
    Ok(())
}

/// Reject absolute paths and any `..` component.
fn check_safe_path(path: &Path) -> Result<(), ApplyError> {
    let unsafe_entry = path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    });
    if unsafe_entry {
        return Err(ApplyError::UnsafeArchive {
            entry: path.display().to_string(),
        });
    }
    Ok(())
}

/// `<unpacked>/crt.app/Contents/MacOS/crt`, the executable inside a bundle
/// the archive happens to carry.
fn bundled_executable(unpacked: &Path) -> PathBuf {
    bundle_contents(unpacked).join("MacOS").join("crt")
}

/// `<unpacked>/crt.app/Contents/Resources`.
fn bundle_resources(unpacked: &Path) -> PathBuf {
    bundle_contents(unpacked).join("Resources")
}

fn bundle_contents(unpacked: &Path) -> PathBuf {
    unpacked.join("crt.app").join("Contents")
}

/// Find the thing to install inside the unpacked archive.
fn locate_payload(unpacked: &Path, layout: Layout, target: &Path) -> Result<PathBuf, ApplyError> {
    match layout {
        Layout::SingleBinary => {
            // The Linux tarball puts the binary at the root, named as it is
            // installed ("crt"). Fall back to the target's own name so a
            // renamed install still finds its payload.
            let name = target
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("crt"));
            for candidate in [
                unpacked.join("crt"),
                unpacked.join(&name),
                // macOS ships an app bundle even to someone running a bare
                // binary — a developer who copied their own build into
                // ~/.local/bin, say. Take the executable out of the bundle
                // rather than failing after a 16 MB download.
                bundled_executable(unpacked),
            ] {
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
            Err(ApplyError::MissingPayload {
                expected: "crt".to_string(),
            })
        }
        Layout::AppBundle => {
            // Check the bundle is whole before it replaces a working one: a
            // directory named crt.app that macOS will not launch is worse
            // than no update.
            let bundle = unpacked.join("crt.app");
            let contents = bundle.join("Contents");
            for required in [
                contents.join("MacOS").join("crt"),
                contents.join("Info.plist"),
            ] {
                if !required.is_file() {
                    return Err(ApplyError::MissingPayload {
                        expected: required
                            .strip_prefix(unpacked)
                            .unwrap_or(&required)
                            .display()
                            .to_string(),
                    });
                }
            }
            Ok(bundle)
        }
    }
}

/// Remove the quarantine attribute from what we are about to install.
///
/// Defensive: `curl` does not set it and the app has no
/// `LSFileQuarantineEnabled`, so it should never be present on something we
/// downloaded ourselves. It costs one process to be sure, and a bundle that
/// is quarantined is one Gatekeeper refuses to launch. Never `sudo`: these
/// are files this user just wrote.
fn strip_quarantine(path: &Path) {
    if !cfg!(target_os = "macos") {
        return;
    }
    match std::process::Command::new("xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        // A non-zero status here just means there was nothing to remove.
        Ok(status) => log::debug!("xattr -dr on {} exited {status}", path.display()),
        Err(e) => log::debug!("could not run xattr on {}: {e}", path.display()),
    }
}

/// Put `staged` where `target` is, keeping the outgoing version as `.old`.
///
/// Two renames within one directory. If the second fails, the first is undone
/// so the user is never left without an install.
fn swap(staged: &Path, target: &Path) -> Result<PathBuf, ApplyError> {
    let previous = previous_path(target);

    // A leftover .old from an update whose first launch never happened.
    if previous.exists() {
        let removed = if previous.is_dir() {
            fs::remove_dir_all(&previous)
        } else {
            fs::remove_file(&previous)
        };
        removed.map_err(|e| io_err(&previous, e))?;
    }

    fs::rename(target, &previous).map_err(|e| io_err(target, e))?;

    if let Err(e) = fs::rename(staged, target) {
        // Put the old version back before reporting.
        let restored = fs::rename(&previous, target);
        let reason = if restored.is_ok() {
            format!("{e}; the previous version was restored")
        } else {
            format!(
                "{e}; the previous version is at {} and must be renamed back by hand",
                previous.display()
            )
        };
        return Err(
            if e.kind() == io::ErrorKind::PermissionDenied && restored.is_ok() {
                ApplyError::PermissionDenied {
                    path: target.to_path_buf(),
                }
            } else {
                ApplyError::Swap { reason }
            },
        );
    }

    Ok(previous)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::MemoryFetch;
    use crate::install_kind::ManagedHint;
    use std::io::Write;

    /// A scratch directory that cleans itself up.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            // Keep the name free of shell metacharacters: one test runs a
            // script that mentions this path. `ThreadId(1)` would not do.
            let thread = format!("{:?}", std::thread::current().id())
                .chars()
                .filter(|c| c.is_ascii_digit())
                .collect::<String>();
            let path = std::env::temp_dir().join(format!(
                "crt-apply-{}-{}-{}",
                name,
                std::process::id(),
                thread
            ));
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

    /// Build a `.tar.gz` holding `files`, returning its bytes.
    ///
    /// Names are written into the header directly rather than through
    /// `append_data`, because the tar crate refuses to *create* an entry
    /// containing `..` - which is exactly the archive the escape test needs.
    fn make_tarball(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, contents) in files {
            let mut header = tar::Header::new_gnu();
            let name_bytes = name.as_bytes();
            assert!(
                name_bytes.len() < 100,
                "test name too long for a tar header"
            );
            header.as_old_mut().name[..name_bytes.len()].copy_from_slice(name_bytes);
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_cksum();
            builder
                .append(&header, std::io::Cursor::new(contents))
                .unwrap();
        }
        let tar_bytes = builder.into_inner().unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&tar_bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn sha256(bytes: &[u8]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hasher.finalize().into()
    }

    /// An install tree with a "current" binary that prints `version`.
    fn install(dir: &Path, version: &str) -> PathBuf {
        let bin = dir.join("crt");
        fs::write(&bin, format!("#!/bin/sh\necho {version}\n")).unwrap();
        set_executable(&bin);
        bin
    }

    fn set_executable(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).unwrap();
        }
        #[cfg(not(unix))]
        let _ = path;
    }

    fn plan_for(bin: &Path, body: &[u8]) -> (UpdatePlan, MemoryFetch) {
        let tarball = make_tarball(&[("crt", body)]);
        let asset = Asset {
            name: "crt-0.1.6-linux-x86_64.tar.gz".to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            sha256: sha256(&tarball),
            download_url: "https://example.invalid/crt-0.1.6-linux-x86_64.tar.gz".to_string(),
        };
        let fetch = MemoryFetch::new().serving("crt-0.1.6", tarball);
        let plan = UpdatePlan {
            kind: InstallKind::UserBinary {
                path: bin.to_path_buf(),
            },
            asset,
            version: Version::parse("0.1.6").unwrap(),
            config_dir: None,
        };
        (plan, fetch)
    }

    fn no_progress() -> impl FnMut(Stage) {
        |_| {}
    }

    #[test]
    fn a_successful_update_swaps_the_binary_and_keeps_the_previous_one() {
        let dir = TempDir::new("success");
        let bin = install(dir.path(), "0.1.5");
        let (plan, fetch) = plan_for(&bin, b"#!/bin/sh\necho 0.1.6\n");

        let mut stages = Vec::new();
        let applied = apply(&plan, &fetch, &mut |stage| stages.push(stage)).expect("applies");

        assert_eq!(applied.version, Version::parse("0.1.6").unwrap());
        assert_eq!(applied.previous, dir.path().join("crt.old"));
        assert!(fs::read_to_string(&bin).unwrap().contains("0.1.6"));
        // The outgoing version stays until the new one has started once.
        assert!(
            fs::read_to_string(dir.path().join("crt.old"))
                .unwrap()
                .contains("0.1.5")
        );
        assert_eq!(
            stages,
            vec![
                Stage::Downloading,
                Stage::Verifying,
                Stage::Unpacking,
                Stage::Installing
            ]
        );
        // Nothing is left behind.
        assert!(!dir.path().join(STAGING_DIR).exists());
    }

    #[cfg(unix)]
    #[test]
    fn the_installed_binary_is_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("exec");
        let bin = install(dir.path(), "0.1.5");
        let (plan, fetch) = plan_for(&bin, b"#!/bin/sh\necho 0.1.6\n");

        apply(&plan, &fetch, &mut no_progress()).expect("applies");
        let mode = fs::metadata(&bin).unwrap().permissions().mode();
        assert_ne!(
            mode & 0o111,
            0,
            "installed binary is not executable: {mode:o}"
        );
    }

    #[test]
    fn a_checksum_mismatch_changes_nothing() {
        let dir = TempDir::new("checksum");
        let bin = install(dir.path(), "0.1.5");
        let (mut plan, fetch) = plan_for(&bin, b"#!/bin/sh\necho 0.1.6\n");
        plan.asset.sha256 = [0u8; 32];

        let err = apply(&plan, &fetch, &mut no_progress()).unwrap_err();
        assert!(
            matches!(err, ApplyError::ChecksumMismatch { .. }),
            "{err:?}"
        );
        assert!(fs::read_to_string(&bin).unwrap().contains("0.1.5"));
        assert!(!dir.path().join("crt.old").exists());
        assert!(!dir.path().join(STAGING_DIR).exists());
    }

    #[test]
    fn a_download_failure_changes_nothing() {
        let dir = TempDir::new("offline");
        let bin = install(dir.path(), "0.1.5");
        let (plan, _) = plan_for(&bin, b"new");
        let fetch = MemoryFetch::new().failing("crt-0.1.6", FetchError::Offline);

        let err = apply(&plan, &fetch, &mut no_progress()).unwrap_err();
        assert_eq!(err, ApplyError::Fetch(FetchError::Offline));
        assert!(fs::read_to_string(&bin).unwrap().contains("0.1.5"));
        assert!(!dir.path().join("crt.old").exists());
    }

    #[test]
    fn an_archive_that_escapes_its_destination_is_refused() {
        let dir = TempDir::new("escape");
        let bin = install(dir.path(), "0.1.5");
        let tarball = make_tarball(&[("../evil", b"pwned"), ("crt", b"new")]);
        let asset = Asset {
            name: "crt-0.1.6-linux-x86_64.tar.gz".to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            sha256: sha256(&tarball),
            download_url: "https://example.invalid/crt-0.1.6-linux-x86_64.tar.gz".to_string(),
        };
        let fetch = MemoryFetch::new().serving("crt-0.1.6", tarball);
        let plan = UpdatePlan {
            kind: InstallKind::UserBinary { path: bin.clone() },
            asset,
            version: Version::parse("0.1.6").unwrap(),
            config_dir: None,
        };

        let err = apply(&plan, &fetch, &mut no_progress()).unwrap_err();
        assert!(matches!(err, ApplyError::UnsafeArchive { .. }), "{err:?}");
        // Checked before anything is written, so the sibling was never created.
        assert!(!dir.path().join("evil").exists());
        assert!(fs::read_to_string(&bin).unwrap().contains("0.1.5"));
    }

    #[test]
    fn an_archive_without_the_binary_is_refused() {
        let dir = TempDir::new("payload");
        let bin = install(dir.path(), "0.1.5");
        let tarball = make_tarball(&[("README", b"nothing useful")]);
        let asset = Asset {
            name: "crt-0.1.6-linux-x86_64.tar.gz".to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            sha256: sha256(&tarball),
            download_url: "https://example.invalid/crt-0.1.6-linux-x86_64.tar.gz".to_string(),
        };
        let fetch = MemoryFetch::new().serving("crt-0.1.6", tarball);
        let plan = UpdatePlan {
            kind: InstallKind::UserBinary { path: bin.clone() },
            asset,
            version: Version::parse("0.1.6").unwrap(),
            config_dir: None,
        };

        let err = apply(&plan, &fetch, &mut no_progress()).unwrap_err();
        assert!(matches!(err, ApplyError::MissingPayload { .. }), "{err:?}");
        assert!(fs::read_to_string(&bin).unwrap().contains("0.1.5"));
    }

    #[test]
    fn a_lock_held_by_a_live_process_refuses_the_update() {
        let dir = TempDir::new("locked");
        let bin = install(dir.path(), "0.1.5");
        let (plan, fetch) = plan_for(&bin, b"new");

        // Our own pid is certainly alive.
        let staging = dir.path().join(STAGING_DIR);
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("lock"), std::process::id().to_string()).unwrap();

        let err = apply(&plan, &fetch, &mut no_progress()).unwrap_err();
        assert_eq!(
            err,
            ApplyError::InProgress {
                pid: std::process::id()
            }
        );
        assert!(fs::read_to_string(&bin).unwrap().contains("0.1.5"));
    }

    #[test]
    fn a_stale_lock_is_taken_over() {
        let dir = TempDir::new("stale");
        let bin = install(dir.path(), "0.1.5");
        let (plan, fetch) = plan_for(&bin, b"#!/bin/sh\necho 0.1.6\n");

        // pid 0 is never a real process; neither is a very high unused one.
        let staging = dir.path().join(STAGING_DIR);
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("lock"), "0").unwrap();

        apply(&plan, &fetch, &mut no_progress()).expect("takes over the stale lock");
        assert!(fs::read_to_string(&bin).unwrap().contains("0.1.6"));
    }

    #[test]
    fn leftovers_from_a_crashed_run_do_not_block_the_next_one() {
        let dir = TempDir::new("leftover");
        let bin = install(dir.path(), "0.1.5");
        let (plan, fetch) = plan_for(&bin, b"#!/bin/sh\necho 0.1.6\n");

        let staging = dir.path().join(STAGING_DIR);
        fs::create_dir_all(staging.join("unpacked")).unwrap();
        fs::write(staging.join("unpacked").join("crt"), "junk").unwrap();
        fs::write(staging.join("crt-0.1.6-linux-x86_64.tar.gz"), "partial").unwrap();

        apply(&plan, &fetch, &mut no_progress()).expect("applies");
        assert!(fs::read_to_string(&bin).unwrap().contains("0.1.6"));
    }

    #[test]
    fn an_existing_old_file_is_replaced_rather_than_blocking_the_swap() {
        let dir = TempDir::new("oldfile");
        let bin = install(dir.path(), "0.1.5");
        fs::write(dir.path().join("crt.old"), "0.1.4").unwrap();
        let (plan, fetch) = plan_for(&bin, b"#!/bin/sh\necho 0.1.6\n");

        apply(&plan, &fetch, &mut no_progress()).expect("applies");
        assert!(
            fs::read_to_string(dir.path().join("crt.old"))
                .unwrap()
                .contains("0.1.5")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_read_only_install_directory_fails_without_touching_anything() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("readonly");
        let install_dir = dir.path().join("bin");
        fs::create_dir_all(&install_dir).unwrap();
        let bin = install(&install_dir, "0.1.5");
        let (plan, fetch) = plan_for(&bin, b"new");

        let mut perms = fs::metadata(&install_dir).unwrap().permissions();
        perms.set_mode(0o555);
        fs::set_permissions(&install_dir, perms).unwrap();

        let err = apply(&plan, &fetch, &mut no_progress()).unwrap_err();

        // Restore before asserting so the directory can be cleaned up.
        let mut perms = fs::metadata(&install_dir).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&install_dir, perms).unwrap();

        assert!(
            matches!(err, ApplyError::PermissionDenied { .. }),
            "{err:?}"
        );
        assert!(err.user_message().contains("install script"));
        assert!(fs::read_to_string(&bin).unwrap().contains("0.1.5"));
        assert!(!install_dir.join("crt.old").exists());
    }

    #[cfg(unix)]
    #[test]
    fn the_running_binary_keeps_working_across_the_swap() {
        // The process holds the old inode, so replacing the file underneath
        // it must not disturb it. This is what lets an update happen without
        // closing the user's shells.
        use std::process::{Command, Stdio};
        let dir = TempDir::new("running");
        let bin = dir.path().join("crt");
        // The script announces that it is running before it sleeps: spawn()
        // returns before the child has exec'd, and swapping the file in that
        // window would test nothing (the child would just run the new one).
        let started = dir.path().join("started");
        fs::write(
            &bin,
            format!(
                "#!/bin/sh\ntouch '{}'\nsleep 2\necho 0.1.5\n",
                started.display()
            ),
        )
        .unwrap();
        set_executable(&bin);

        let child = Command::new(&bin)
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawns");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !started.exists() {
            assert!(std::time::Instant::now() < deadline, "child never started");
            std::thread::sleep(Duration::from_millis(10));
        }

        let (plan, fetch) = plan_for(&bin, b"#!/bin/sh\necho 0.1.6\n");
        apply(&plan, &fetch, &mut no_progress()).expect("applies");

        let output = child.wait_with_output().expect("waits");
        assert!(output.status.success(), "the running process was disturbed");
        assert!(String::from_utf8_lossy(&output.stdout).contains("0.1.5"));
        assert!(fs::read_to_string(&bin).unwrap().contains("0.1.6"));
    }

    #[test]
    fn installs_we_do_not_own_are_refused_before_any_download() {
        let dir = TempDir::new("managed");
        let bin = install(dir.path(), "0.1.5");
        let (mut plan, _) = plan_for(&bin, b"new");
        // A fetch that would fail loudly if it were ever called.
        let fetch = MemoryFetch::new().failing("crt-0.1.6", FetchError::Offline);

        for kind in [
            InstallKind::Managed {
                hint: ManagedHint::Pacman,
            },
            InstallKind::Dev,
        ] {
            plan.kind = kind;
            assert_eq!(
                apply(&plan, &fetch, &mut no_progress()).unwrap_err(),
                ApplyError::NotSelfReplaceable
            );
        }
        assert!(!dir.path().join(STAGING_DIR).exists());
    }

    // ---- macOS app bundle layout -------------------------------------
    //
    // These run on Linux too: nothing here needs a real bundle, and the
    // layout rules should not silently rot on the platform CI mostly uses.

    /// An installed `crt.app` whose executable prints `version`.
    fn install_bundle(dir: &Path, version: &str) -> PathBuf {
        let bundle = dir.join("crt.app");
        let macos = bundle.join("Contents").join("MacOS");
        fs::create_dir_all(&macos).unwrap();
        fs::write(bundle.join("Contents").join("Info.plist"), "<plist/>").unwrap();
        let exe = macos.join("crt");
        fs::write(&exe, format!("#!/bin/sh\necho {version}\n")).unwrap();
        set_executable(&exe);
        bundle
    }

    /// A release tarball containing a whole `crt.app`.
    fn bundle_tarball(version: &str, with_plist: bool) -> Vec<u8> {
        let mut files: Vec<(&str, Vec<u8>)> = vec![(
            "crt.app/Contents/MacOS/crt",
            format!("#!/bin/sh\necho {version}\n").into_bytes(),
        )];
        if with_plist {
            files.push(("crt.app/Contents/Info.plist", b"<plist/>".to_vec()));
        }
        let borrowed: Vec<(&str, &[u8])> = files.iter().map(|(n, c)| (*n, c.as_slice())).collect();
        make_tarball(&borrowed)
    }

    fn bundle_plan(bundle: &Path, tarball: Vec<u8>) -> (UpdatePlan, MemoryFetch) {
        let asset = Asset {
            name: "crt-0.1.6-macos-aarch64.tar.gz".to_string(),
            os: "macos".to_string(),
            arch: "aarch64".to_string(),
            sha256: sha256(&tarball),
            download_url: "https://example.invalid/crt-0.1.6-macos-aarch64.tar.gz".to_string(),
        };
        let fetch = MemoryFetch::new().serving("crt-0.1.6", tarball);
        let plan = UpdatePlan {
            kind: InstallKind::AppBundle {
                bundle_root: bundle.to_path_buf(),
            },
            asset,
            version: Version::parse("0.1.6").unwrap(),
            config_dir: None,
        };
        (plan, fetch)
    }

    #[test]
    fn a_bundle_is_replaced_whole_and_keeps_its_app_extension() {
        let dir = TempDir::new("bundle");
        let bundle = install_bundle(dir.path(), "0.1.5");
        let (plan, fetch) = bundle_plan(&bundle, bundle_tarball("0.1.6", true));

        let applied = apply(&plan, &fetch, &mut no_progress()).expect("applies");

        // Regression: with_extension() would have made this "crt.old" and
        // dropped the .app, which macOS treats as something else entirely.
        assert_eq!(applied.previous, dir.path().join("crt.app.old"));
        assert!(applied.previous.is_dir());

        let exe = bundle.join("Contents").join("MacOS").join("crt");
        assert!(fs::read_to_string(&exe).unwrap().contains("0.1.6"));
        assert!(bundle.join("Contents").join("Info.plist").is_file());
        assert!(
            fs::read_to_string(applied.previous.join("Contents").join("MacOS").join("crt"))
                .unwrap()
                .contains("0.1.5")
        );
        assert!(!dir.path().join(STAGING_DIR).exists());
    }

    #[test]
    fn a_bundle_without_its_plist_is_refused() {
        let dir = TempDir::new("noplist");
        let bundle = install_bundle(dir.path(), "0.1.5");
        let (plan, fetch) = bundle_plan(&bundle, bundle_tarball("0.1.6", false));

        let err = apply(&plan, &fetch, &mut no_progress()).unwrap_err();
        assert!(
            matches!(&err, ApplyError::MissingPayload { expected } if expected.contains("Info.plist")),
            "{err:?}"
        );
        // The working bundle is untouched.
        let exe = bundle.join("Contents").join("MacOS").join("crt");
        assert!(fs::read_to_string(&exe).unwrap().contains("0.1.5"));
        assert!(!dir.path().join("crt.app.old").exists());
    }

    #[test]
    fn an_old_bundle_from_a_previous_update_is_replaced() {
        let dir = TempDir::new("oldbundle");
        let bundle = install_bundle(dir.path(), "0.1.5");
        // A .old directory left because a first launch never happened.
        let stale = dir.path().join("crt.app.old");
        fs::create_dir_all(stale.join("Contents")).unwrap();
        fs::write(stale.join("Contents").join("marker"), "0.1.4").unwrap();

        let (plan, fetch) = bundle_plan(&bundle, bundle_tarball("0.1.6", true));
        apply(&plan, &fetch, &mut no_progress()).expect("applies");

        assert!(!stale.join("Contents").join("marker").exists());
        assert!(
            fs::read_to_string(stale.join("Contents").join("MacOS").join("crt"))
                .unwrap()
                .contains("0.1.5")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_bundle_in_an_unwritable_directory_fails_without_touching_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("roapps");
        // Stand-in for /Applications on a non-admin account.
        let applications = dir.path().join("Applications");
        fs::create_dir_all(&applications).unwrap();
        let bundle = install_bundle(&applications, "0.1.5");
        let (plan, fetch) = bundle_plan(&bundle, bundle_tarball("0.1.6", true));

        let mut perms = fs::metadata(&applications).unwrap().permissions();
        perms.set_mode(0o555);
        fs::set_permissions(&applications, perms).unwrap();

        let err = apply(&plan, &fetch, &mut no_progress()).unwrap_err();

        let mut perms = fs::metadata(&applications).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&applications, perms).unwrap();

        assert!(
            matches!(err, ApplyError::PermissionDenied { .. }),
            "{err:?}"
        );
        assert!(err.user_message().contains("install script"));
        let exe = bundle.join("Contents").join("MacOS").join("crt");
        assert!(fs::read_to_string(&exe).unwrap().contains("0.1.5"));
        assert!(!applications.join("crt.app.old").exists());
    }

    #[test]
    fn the_previous_path_appends_rather_than_replacing_an_extension() {
        assert_eq!(
            previous_path(Path::new("/home/dev/.local/bin/crt")),
            PathBuf::from("/home/dev/.local/bin/crt.old")
        );
        assert_eq!(
            previous_path(Path::new("/Applications/crt.app")),
            PathBuf::from("/Applications/crt.app.old")
        );
    }

    #[test]
    fn an_update_also_brings_the_releases_themes_but_keeps_edited_ones() {
        let dir = TempDir::new("assets");
        let bin = install(dir.path(), "0.1.5");
        let config = dir.path().join("config");
        fs::create_dir_all(config.join("themes")).unwrap();
        // One theme the user has made their own, one they have not touched.
        fs::write(config.join("themes").join("dracula.css"), "my dracula").unwrap();

        let tarball = make_tarball(&[
            ("crt", b"#!/bin/sh\necho 0.1.6\n"),
            ("assets/themes/dracula.css", b"bundled dracula v2"),
            ("assets/themes/solarized.css", b"new in this release"),
        ]);
        let asset = Asset {
            name: "crt-0.1.6-linux-x86_64.tar.gz".to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            sha256: sha256(&tarball),
            download_url: "https://example.invalid/crt-0.1.6-linux-x86_64.tar.gz".to_string(),
        };
        let fetch = MemoryFetch::new().serving("crt-0.1.6", tarball);
        let plan = UpdatePlan {
            kind: InstallKind::UserBinary { path: bin.clone() },
            asset,
            version: Version::parse("0.1.6").unwrap(),
            config_dir: Some(config.clone()),
        };

        let applied = apply(&plan, &fetch, &mut no_progress()).expect("applies");

        // The binary was replaced...
        assert!(fs::read_to_string(&bin).unwrap().contains("0.1.6"));
        // ...the new theme arrived...
        assert_eq!(
            fs::read_to_string(config.join("themes").join("solarized.css")).unwrap(),
            "new in this release"
        );
        // ...and the edited one was left alone and reported.
        assert_eq!(
            fs::read_to_string(config.join("themes").join("dracula.css")).unwrap(),
            "my dracula"
        );
        assert_eq!(
            applied.assets.skipped,
            vec!["themes/dracula.css".to_string()]
        );
        assert!(
            applied
                .assets
                .written
                .contains(&"themes/solarized.css".to_string())
        );
        // The manifest is saved so the next release can update what it wrote.
        assert!(crate::assets::AssetManifest::path(&config).is_file());
    }

    /// Regression, found by updating a bare binary from the real v0.1.6
    /// release: on macOS every release asset is an app bundle, so a bare
    /// binary install downloaded 16 MB and then failed with "the archive did
    /// not contain crt". Take the executable and the assets out of the
    /// bundle instead.
    #[test]
    fn a_bare_binary_can_be_updated_from_an_archive_containing_a_bundle() {
        let dir = TempDir::new("frombundle");
        let bin = install(dir.path(), "0.1.5");
        let config = dir.path().join("config");

        let tarball = make_tarball(&[
            ("crt.app/Contents/MacOS/crt", b"#!/bin/sh\necho 0.1.6\n"),
            ("crt.app/Contents/Info.plist", b"<plist/>"),
            (
                "crt.app/Contents/Resources/assets/themes/solarized.css",
                b"new theme",
            ),
        ]);
        let asset = Asset {
            name: "crt-0.1.6-macos-aarch64.tar.gz".to_string(),
            os: "macos".to_string(),
            arch: "aarch64".to_string(),
            sha256: sha256(&tarball),
            download_url: "https://example.invalid/crt-0.1.6-macos-aarch64.tar.gz".to_string(),
        };
        let fetch = MemoryFetch::new().serving("crt-0.1.6", tarball);
        let plan = UpdatePlan {
            kind: InstallKind::UserBinary { path: bin.clone() },
            asset,
            version: Version::parse("0.1.6").unwrap(),
            config_dir: Some(config.clone()),
        };

        let applied = apply(&plan, &fetch, &mut no_progress()).expect("applies");

        // The binary from inside the bundle is now the install...
        assert!(fs::read_to_string(&bin).unwrap().contains("0.1.6"));
        assert_eq!(applied.previous, dir.path().join("crt.old"));
        // ...and its themes came from the bundle's Resources.
        assert_eq!(
            fs::read_to_string(config.join("themes").join("solarized.css")).unwrap(),
            "new theme"
        );
    }

    #[test]
    fn an_update_without_a_config_dir_installs_the_binary_only() {
        let dir = TempDir::new("noconfig");
        let bin = install(dir.path(), "0.1.5");
        let (plan, fetch) = plan_for(&bin, b"#!/bin/sh\necho 0.1.6\n");
        assert!(plan.config_dir.is_none());

        let applied = apply(&plan, &fetch, &mut no_progress()).expect("applies");
        assert!(applied.assets.is_empty());
    }

    #[test]
    fn unsafe_paths_are_recognised_in_every_shape() {
        assert!(check_safe_path(Path::new("crt")).is_ok());
        assert!(check_safe_path(Path::new("icons/crt.png")).is_ok());
        assert!(check_safe_path(Path::new("./crt")).is_ok());
        assert!(check_safe_path(Path::new("../evil")).is_err());
        assert!(check_safe_path(Path::new("a/../../evil")).is_err());
        assert!(check_safe_path(Path::new("/etc/passwd")).is_err());
    }

    #[test]
    fn stage_messages_name_the_version_being_installed() {
        let version = Version::parse("0.1.6").unwrap();
        assert!(Stage::Downloading.message(Some(&version)).contains("0.1.6"));
        for stage in [Stage::Verifying, Stage::Unpacking, Stage::Installing] {
            assert!(!stage.message(Some(&version)).is_empty());
        }
        // Without a version the download stage still says something useful.
        let plain = Stage::Downloading.message(None);
        assert!(
            plain.contains("Downloading") && !plain.contains("v"),
            "{plain}"
        );
    }

    #[test]
    fn a_dead_pid_is_not_mistaken_for_a_live_one() {
        assert!(process_is_alive(std::process::id()));
        assert!(!process_is_alive(0));
    }
}

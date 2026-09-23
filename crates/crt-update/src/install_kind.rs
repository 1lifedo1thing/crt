//! Working out where the running binary came from.
//!
//! The updater may only replace an install it owns: one put there by
//! `scripts/install.sh`, as a bare binary or as a macOS `.app` bundle.
//! Anything else — a package manager's copy, a `cargo install`, a build tree
//! — is left alone and the user is told which command upgrades it.
//!
//! Classification is deliberately pessimistic. Every fact about the
//! filesystem comes through [`FsProbe`], so the rules are unit-testable
//! everywhere, and anything that does not clearly match a self-replaceable
//! layout ends up as [`InstallKind::Managed`]. Getting this wrong in the
//! permissive direction would mean overwriting a file another tool believes
//! it owns; getting it wrong in the strict direction only costs the user a
//! manual upgrade.

use std::path::{Component, Path, PathBuf};

/// The filesystem facts classification needs.
///
/// A trait rather than direct calls so the rules can be exercised for macOS
/// layouts on Linux CI and vice versa.
pub trait FsProbe {
    /// Resolve symlinks and `..`. Should return the input unchanged when the
    /// path cannot be resolved, so classification still gets something to
    /// look at.
    fn canonicalize(&self, path: &Path) -> PathBuf;

    /// Whether a *directory* can be written to by this process. Used on the
    /// directory holding whatever would be renamed, because that is the
    /// permission an in-place swap actually needs.
    fn is_writable(&self, dir: &Path) -> bool;

    fn home_dir(&self) -> Option<PathBuf>;

    /// True for a debug build. Debug builds are never self-replaced.
    fn is_debug_build(&self) -> bool;

    /// Homebrew's prefix (`/opt/homebrew`, `/usr/local`), when it is in use.
    fn homebrew_prefix(&self) -> Option<PathBuf> {
        None
    }

    /// `ID` from `/etc/os-release`, used only to name the right package
    /// manager in the upgrade hint.
    fn os_release_id(&self) -> Option<String> {
        None
    }
}

/// Which package manager owns a [`InstallKind::Managed`] install, as far as
/// we can tell. Only used to print the right upgrade command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedHint {
    Pacman,
    Cargo,
    Homebrew,
    /// Somewhere under a system prefix, or a directory we cannot write to.
    SystemPrefix,
}

impl ManagedHint {
    /// The command that upgrades this install.
    pub fn command(&self) -> &'static str {
        match self {
            ManagedHint::Pacman => "yay -Syu crt",
            ManagedHint::Cargo => "cargo install crt",
            ManagedHint::Homebrew => "brew upgrade crt",
            ManagedHint::SystemPrefix => "your system package manager",
        }
    }

    /// How the install is described in a notification.
    pub fn installed_via(&self) -> &'static str {
        match self {
            ManagedHint::Pacman => "pacman",
            ManagedHint::Cargo => "cargo",
            ManagedHint::Homebrew => "Homebrew",
            ManagedHint::SystemPrefix => "a package manager",
        }
    }
}

/// Where the running executable lives, and whether we may replace it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    /// A macOS `.app` bundle we can swap wholesale.
    AppBundle { bundle_root: PathBuf },
    /// A bare binary in a user-writable directory.
    UserBinary { path: PathBuf },
    /// Owned by something else. Notify only.
    Managed { hint: ManagedHint },
    /// Running out of a build tree.
    Dev,
}

impl InstallKind {
    /// Only these two layouts may be replaced in place.
    pub fn can_self_replace(&self) -> bool {
        matches!(
            self,
            InstallKind::AppBundle { .. } | InstallKind::UserBinary { .. }
        )
    }

    /// The path an update would rename: the bundle, or the binary itself.
    pub fn swap_target(&self) -> Option<&Path> {
        match self {
            InstallKind::AppBundle { bundle_root } => Some(bundle_root),
            InstallKind::UserBinary { path } => Some(path),
            _ => None,
        }
    }

    /// What to tell a user who cannot update in place.
    pub fn upgrade_hint(&self) -> Option<&'static str> {
        match self {
            InstallKind::Managed { hint } => Some(hint.command()),
            InstallKind::Dev => Some("git pull && cargo build --release"),
            _ => None,
        }
    }

    /// Short description for `crt --version`.
    pub fn label(&self) -> &'static str {
        match self {
            InstallKind::AppBundle { .. } => "app bundle",
            InstallKind::UserBinary { .. } => "user install",
            InstallKind::Managed { .. } => "package manager install",
            InstallKind::Dev => "development build",
        }
    }
}

/// Classify the executable at `exe`.
///
/// Order matters, and it is the order the checks appear in:
///
/// 1. canonicalise, so a symlink into `~/.cargo/bin` is judged by its target;
/// 2. a debug build, or a path inside a `target/` directory, is [`Dev`];
/// 3. a well-known package-manager prefix is [`Managed`];
/// 4. whatever would be renamed — bundle or binary — must sit in a writable
///    directory, otherwise it is [`Managed`] too;
/// 5. what is left is an app bundle or a plain user binary.
///
/// [`Dev`]: InstallKind::Dev
/// [`Managed`]: InstallKind::Managed
pub fn classify(exe: &Path, probe: &dyn FsProbe) -> InstallKind {
    let exe = probe.canonicalize(exe);

    if probe.is_debug_build() || has_component(&exe, "target") {
        return InstallKind::Dev;
    }

    if let Some(hint) = managed_prefix_hint(&exe, probe) {
        return InstallKind::Managed { hint };
    }

    // A bundle is replaced whole, so it is the bundle's parent that has to be
    // writable, not `Contents/MacOS`.
    let bundle_root = bundle_root_of(&exe);
    let swap_target = bundle_root.as_deref().unwrap_or(&exe);
    let container = match swap_target.parent() {
        Some(parent) => parent,
        // A path with no parent is nothing we can rename next to.
        None => {
            return InstallKind::Managed {
                hint: ManagedHint::SystemPrefix,
            };
        }
    };

    if !probe.is_writable(container) {
        return InstallKind::Managed {
            hint: ManagedHint::SystemPrefix,
        };
    }

    match bundle_root {
        Some(bundle_root) => InstallKind::AppBundle { bundle_root },
        None => InstallKind::UserBinary { path: exe },
    }
}

/// Match the `<name>.app/Contents/MacOS/<binary>` layout and return the
/// `.app` directory.
fn bundle_root_of(exe: &Path) -> Option<PathBuf> {
    let macos_dir = exe.parent()?;
    if macos_dir.file_name()? != "MacOS" {
        return None;
    }
    let contents = macos_dir.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    let bundle = contents.parent()?;
    if bundle.extension()? == "app" {
        Some(bundle.to_path_buf())
    } else {
        None
    }
}

/// Package-manager prefixes, checked before anything else on disk.
fn managed_prefix_hint(exe: &Path, probe: &dyn FsProbe) -> Option<ManagedHint> {
    if let Some(home) = probe.home_dir()
        && exe.starts_with(home.join(".cargo").join("bin"))
    {
        return Some(ManagedHint::Cargo);
    }

    if let Some(prefix) = probe.homebrew_prefix()
        && exe.starts_with(&prefix)
    {
        return Some(ManagedHint::Homebrew);
    }

    if exe.starts_with("/usr") || exe.starts_with("/opt") {
        // Only the hint differs; either way we will not touch it.
        let arch_like = probe.os_release_id().is_some_and(|id| {
            matches!(id.as_str(), "arch" | "manjaro" | "endeavouros" | "cachyos")
        });
        return Some(if arch_like {
            ManagedHint::Pacman
        } else {
            ManagedHint::SystemPrefix
        });
    }

    None
}

fn has_component(path: &Path, name: &str) -> bool {
    path.components()
        .any(|c| matches!(c, Component::Normal(n) if n == name))
}

/// [`FsProbe`] backed by the real filesystem.
pub struct RealFs;

impl FsProbe for RealFs {
    fn canonicalize(&self, path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    fn is_writable(&self, dir: &Path) -> bool {
        // std has no `access(2)`, and directory permission bits do not answer
        // the question for other users or read-only mounts. Ask the
        // filesystem directly with a uniquely named file, removed at once.
        let probe = dir.join(format!(".crt-write-probe-{}", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
        {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
                true
            }
            Err(_) => false,
        }
    }

    fn home_dir(&self) -> Option<PathBuf> {
        dirs_home()
    }

    fn is_debug_build(&self) -> bool {
        cfg!(debug_assertions)
    }

    fn homebrew_prefix(&self) -> Option<PathBuf> {
        if let Some(prefix) = std::env::var_os("HOMEBREW_PREFIX") {
            return Some(PathBuf::from(prefix));
        }
        // Defaults: Apple silicon, then Intel/Linuxbrew.
        for candidate in ["/opt/homebrew", "/usr/local/Homebrew"] {
            let path = Path::new(candidate);
            if path.is_dir() {
                return Some(path.to_path_buf());
            }
        }
        None
    }

    fn os_release_id(&self) -> Option<String> {
        let text = std::fs::read_to_string("/etc/os-release").ok()?;
        text.lines()
            .find_map(|line| line.strip_prefix("ID="))
            .map(|id| id.trim_matches('"').to_ascii_lowercase())
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Filesystem facts stated outright, so a macOS layout can be classified
    /// on Linux CI and the other way round.
    #[derive(Default)]
    struct FakeFs {
        links: Vec<(PathBuf, PathBuf)>,
        unwritable: HashSet<PathBuf>,
        home: Option<PathBuf>,
        debug: bool,
        homebrew: Option<PathBuf>,
        os_release: Option<String>,
    }

    impl FakeFs {
        fn new() -> Self {
            FakeFs {
                home: Some(PathBuf::from("/home/dev")),
                ..Default::default()
            }
        }
        fn home(mut self, home: &str) -> Self {
            self.home = Some(PathBuf::from(home));
            self
        }
        fn link(mut self, from: &str, to: &str) -> Self {
            self.links.push((PathBuf::from(from), PathBuf::from(to)));
            self
        }
        fn unwritable(mut self, dir: &str) -> Self {
            self.unwritable.insert(PathBuf::from(dir));
            self
        }
        fn debug(mut self) -> Self {
            self.debug = true;
            self
        }
        fn homebrew(mut self, prefix: &str) -> Self {
            self.homebrew = Some(PathBuf::from(prefix));
            self
        }
        fn os_release(mut self, id: &str) -> Self {
            self.os_release = Some(id.to_string());
            self
        }
    }

    impl FsProbe for FakeFs {
        fn canonicalize(&self, path: &Path) -> PathBuf {
            for (from, to) in &self.links {
                if path == from {
                    return to.clone();
                }
            }
            path.to_path_buf()
        }
        fn is_writable(&self, dir: &Path) -> bool {
            !self.unwritable.contains(dir)
        }
        fn home_dir(&self) -> Option<PathBuf> {
            self.home.clone()
        }
        fn is_debug_build(&self) -> bool {
            self.debug
        }
        fn homebrew_prefix(&self) -> Option<PathBuf> {
            self.homebrew.clone()
        }
        fn os_release_id(&self) -> Option<String> {
            self.os_release.clone()
        }
    }

    fn classify_str(exe: &str, fs: &FakeFs) -> InstallKind {
        classify(Path::new(exe), fs)
    }

    #[test]
    fn a_script_installed_binary_is_replaceable() {
        let kind = classify_str("/home/dev/.local/bin/crt", &FakeFs::new());
        assert_eq!(
            kind,
            InstallKind::UserBinary {
                path: PathBuf::from("/home/dev/.local/bin/crt")
            }
        );
        assert!(kind.can_self_replace());
        assert_eq!(kind.upgrade_hint(), None);
    }

    #[test]
    fn a_user_owned_app_bundle_is_replaceable_and_swaps_the_bundle() {
        let kind = classify_str("/Applications/crt.app/Contents/MacOS/crt", &FakeFs::new());
        assert_eq!(
            kind,
            InstallKind::AppBundle {
                bundle_root: PathBuf::from("/Applications/crt.app")
            }
        );
        assert!(kind.can_self_replace());
        // The bundle is renamed, not the executable inside it.
        assert_eq!(kind.swap_target(), Some(Path::new("/Applications/crt.app")));
    }

    #[test]
    fn an_app_bundle_in_an_unwritable_applications_is_managed() {
        // A non-admin account on macOS: /Applications is root:admin 775.
        let fs = FakeFs::new().unwritable("/Applications");
        assert_eq!(
            classify_str("/Applications/crt.app/Contents/MacOS/crt", &fs),
            InstallKind::Managed {
                hint: ManagedHint::SystemPrefix
            }
        );
    }

    #[test]
    fn an_unwritable_bin_directory_is_managed() {
        let fs = FakeFs::new().unwritable("/home/dev/.local/bin");
        assert_eq!(
            classify_str("/home/dev/.local/bin/crt", &fs),
            InstallKind::Managed {
                hint: ManagedHint::SystemPrefix
            }
        );
    }

    #[test]
    fn a_symlink_into_cargo_bin_is_judged_by_its_target() {
        // ~/.cargo/bin is writable, so only canonicalising keeps us from
        // replacing a binary cargo believes it owns.
        let fs = FakeFs::new().link("/home/dev/.local/bin/crt", "/home/dev/.cargo/bin/crt");
        let kind = classify_str("/home/dev/.local/bin/crt", &fs);
        assert_eq!(
            kind,
            InstallKind::Managed {
                hint: ManagedHint::Cargo
            }
        );
        assert!(!kind.can_self_replace());
        assert_eq!(kind.upgrade_hint(), Some("cargo install crt"));
    }

    #[test]
    fn cargo_install_is_managed() {
        assert_eq!(
            classify_str("/home/dev/.cargo/bin/crt", &FakeFs::new()),
            InstallKind::Managed {
                hint: ManagedHint::Cargo
            }
        );
    }

    #[test]
    fn cargo_bin_is_found_relative_to_the_real_home() {
        // The same path is only "cargo's" when it is under *this* user's
        // home; another user's ~/.cargo/bin is just an unwritable directory.
        let fs = FakeFs::new().home("/Users/dev");
        assert_eq!(
            classify_str("/Users/dev/.cargo/bin/crt", &fs),
            InstallKind::Managed {
                hint: ManagedHint::Cargo
            }
        );
        assert!(matches!(
            classify_str("/Users/other/.cargo/bin/crt", &fs),
            InstallKind::UserBinary { .. }
        ));
        assert_eq!(
            classify_str(
                "/Users/other/.cargo/bin/crt",
                &FakeFs::new()
                    .home("/Users/dev")
                    .unwritable("/Users/other/.cargo/bin")
            ),
            InstallKind::Managed {
                hint: ManagedHint::SystemPrefix
            }
        );
    }

    #[test]
    fn a_release_build_run_from_the_target_dir_is_dev() {
        // cfg!(debug_assertions) is false for `cargo run --release`, so the
        // path component is what catches it.
        let fs = FakeFs::new();
        assert_eq!(
            classify_str("/home/dev/src/crt/target/release/crt", &fs),
            InstallKind::Dev
        );
        assert_eq!(
            classify_str("/home/dev/src/crt/target/debug/crt", &fs),
            InstallKind::Dev
        );
    }

    #[test]
    fn any_debug_build_is_dev_wherever_it_sits() {
        let fs = FakeFs::new().debug();
        assert_eq!(
            classify_str("/home/dev/.local/bin/crt", &fs),
            InstallKind::Dev
        );
        let kind = classify_str("/Applications/crt.app/Contents/MacOS/crt", &fs);
        assert_eq!(kind, InstallKind::Dev);
        assert!(!kind.can_self_replace());
        assert_eq!(
            kind.upgrade_hint(),
            Some("git pull && cargo build --release")
        );
    }

    #[test]
    fn a_directory_merely_called_target_elsewhere_still_counts_as_dev() {
        // Conservative on purpose: refusing to update a real install costs a
        // manual upgrade, updating a build tree costs a confusing mess.
        assert_eq!(
            classify_str("/home/dev/target/crt", &FakeFs::new()),
            InstallKind::Dev
        );
    }

    #[test]
    fn system_prefixes_are_managed() {
        let fs = FakeFs::new();
        for exe in ["/usr/bin/crt", "/usr/local/bin/crt", "/opt/crt/bin/crt"] {
            assert_eq!(
                classify_str(exe, &fs),
                InstallKind::Managed {
                    hint: ManagedHint::SystemPrefix
                },
                "{exe}"
            );
        }
    }

    #[test]
    fn the_aur_package_gets_the_pacman_hint() {
        let fs = FakeFs::new().os_release("arch");
        let kind = classify_str("/usr/bin/crt", &fs);
        assert_eq!(
            kind,
            InstallKind::Managed {
                hint: ManagedHint::Pacman
            }
        );
        assert_eq!(kind.upgrade_hint(), Some("yay -Syu crt"));
        assert_eq!(
            classify_str("/usr/bin/crt", &FakeFs::new().os_release("ubuntu")),
            InstallKind::Managed {
                hint: ManagedHint::SystemPrefix
            }
        );
    }

    #[test]
    fn homebrew_is_managed_even_on_apple_silicon_prefixes() {
        let fs = FakeFs::new().homebrew("/opt/homebrew");
        let kind = classify_str("/opt/homebrew/bin/crt", &fs);
        assert_eq!(
            kind,
            InstallKind::Managed {
                hint: ManagedHint::Homebrew
            }
        );
        assert_eq!(kind.upgrade_hint(), Some("brew upgrade crt"));
    }

    #[test]
    fn a_bundle_shaped_path_that_is_not_a_bundle_is_a_plain_binary() {
        let fs = FakeFs::new();
        // Right directory names, no .app extension.
        assert!(matches!(
            classify_str("/home/dev/crt/Contents/MacOS/crt", &fs),
            InstallKind::UserBinary { .. }
        ));
        // .app at the top but the binary is not under Contents/MacOS.
        assert!(matches!(
            classify_str("/home/dev/crt.app/crt", &fs),
            InstallKind::UserBinary { .. }
        ));
    }

    #[test]
    fn hints_and_labels_exist_for_every_kind() {
        let kinds = [
            InstallKind::AppBundle {
                bundle_root: PathBuf::from("/Applications/crt.app"),
            },
            InstallKind::UserBinary {
                path: PathBuf::from("/home/dev/.local/bin/crt"),
            },
            InstallKind::Managed {
                hint: ManagedHint::Pacman,
            },
            InstallKind::Dev,
        ];
        for kind in kinds {
            assert!(!kind.label().is_empty());
            assert_eq!(kind.upgrade_hint().is_some(), !kind.can_self_replace());
        }
        for hint in [
            ManagedHint::Pacman,
            ManagedHint::Cargo,
            ManagedHint::Homebrew,
            ManagedHint::SystemPrefix,
        ] {
            assert!(!hint.command().is_empty());
            assert!(!hint.installed_via().is_empty());
        }
    }
}

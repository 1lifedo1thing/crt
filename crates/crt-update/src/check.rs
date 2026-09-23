//! Deciding whether to check for an update, and what to say about it.
//!
//! The rules here are the whole anti-nag design, so they live in one pure
//! function with tests rather than being spread across the event loop:
//!
//! - a launch check runs at most once per `interval`, and not at all when the
//!   user turned checks off;
//! - the same version is announced once, not on every launch;
//! - a check the user asked for runs regardless, and says something even when
//!   there is nothing to report;
//! - a failed check still counts as a check, so a machine with no network
//!   does not retry on every launch.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::fetch::{Fetch, FetchError};
use crate::install_kind::InstallKind;
use crate::manifest::{self, ReleaseManifest, UpdateStatus};

/// How long to wait for the check. Short: it runs on every launch and nobody
/// is waiting for its answer.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(15);

/// What the app remembers between launches.
///
/// Kept apart from `config.toml`, which belongs to the user; this file is
/// ours to rewrite.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateState {
    /// Unix seconds of the last completed check, successful or not.
    pub last_check: Option<u64>,
    /// Version the user was last told about, so we only say it once.
    pub last_notified_version: Option<String>,
    /// Version whose first-launch chores have been done (CRT-T-0216).
    pub last_finished_version: Option<String>,
}

impl UpdateState {
    /// `<config_dir>/state/update.toml`
    pub fn path(config_dir: &Path) -> PathBuf {
        config_dir.join("state").join("update.toml")
    }

    /// Read state, treating anything unreadable as empty: a corrupt state
    /// file should cost one extra check, not break startup.
    pub fn load(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                log::warn!("update state unreadable ({}): {e}", path.display());
                return Self::default();
            }
        };
        match toml::from_str(&text) {
            Ok(state) => state,
            Err(e) => {
                log::warn!("update state malformed ({}): {e}", path.display());
                Self::default()
            }
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        // Write and rename so a crash cannot leave a truncated file that the
        // next launch would log a warning about.
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    }

    fn elapsed_since_check(&self, now: SystemTime) -> Option<Duration> {
        let last = self.last_check?;
        let now = now.duration_since(UNIX_EPOCH).ok()?.as_secs();
        // A check timestamped in the future means the clock moved backwards
        // (or the state file came from another machine). Report it as
        // unknown, which checks now, rather than as "just checked", which
        // would block checks until real time caught up.
        now.checked_sub(last).map(Duration::from_secs)
    }
}

/// Everything the check needs from configuration and the running build.
#[derive(Debug, Clone)]
pub struct CheckConfig {
    /// `[updates] check` - governs the automatic launch check only.
    pub enabled: bool,
    /// `[updates] interval_hours`.
    pub interval: Duration,
    /// The user picked "Check for Updates…": ignore both of the above, and
    /// report even when there is nothing new.
    pub user_requested: bool,
    /// Version of the running build.
    pub running_version: String,
}

/// What a check concluded. `None` from [`run_check`] means it did not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateEvent {
    /// A newer release exists. `notify` is false when this exact version has
    /// already been announced and the user did not ask again.
    Available {
        version: Version,
        notify: bool,
    },
    /// Running the latest, or something newer than the latest.
    UpToDate {
        running: String,
    },
    Failed {
        error: FetchError,
    },
    /// The release exists but we could not make sense of it.
    Unreadable {
        reason: String,
    },
}

/// Run the update check, subject to the rules in the module docs.
///
/// `state` is updated in place; the caller persists it.
pub fn run_check(
    fetch: &dyn Fetch,
    state: &mut UpdateState,
    now: SystemTime,
    config: &CheckConfig,
) -> Option<UpdateEvent> {
    if !config.user_requested {
        if !config.enabled {
            return None;
        }
        if let Some(elapsed) = state.elapsed_since_check(now)
            && elapsed < config.interval
        {
            return None;
        }
    }

    // Count the attempt before it can fail, so an offline machine backs off
    // for the whole interval instead of retrying on every launch.
    if let Ok(unix) = now.duration_since(UNIX_EPOCH) {
        state.last_check = Some(unix.as_secs());
    }

    let text = match fetch.get_text(&manifest::sums_url(), CHECK_TIMEOUT) {
        Ok(text) => text,
        Err(error) => return Some(UpdateEvent::Failed { error }),
    };

    let manifest = match ReleaseManifest::parse(&text) {
        Ok(manifest) => manifest,
        Err(e) => {
            return Some(UpdateEvent::Unreadable {
                reason: e.to_string(),
            });
        }
    };

    let status = match manifest::compare(&config.running_version, &manifest.version) {
        Ok(status) => status,
        Err(e) => {
            return Some(UpdateEvent::Unreadable {
                reason: e.to_string(),
            });
        }
    };

    Some(match status {
        UpdateStatus::Newer(version) => {
            let already_told = state.last_notified_version.as_deref() == Some(&version.to_string());
            let notify = config.user_requested || !already_told;
            if notify {
                state.last_notified_version = Some(version.to_string());
            }
            UpdateEvent::Available { version, notify }
        }
        UpdateStatus::UpToDate | UpdateStatus::RunningIsNewer => UpdateEvent::UpToDate {
            running: config.running_version.clone(),
        },
    })
}

/// The line shown when a newer version exists. What follows the version
/// depends on whether we can act on it.
pub fn availability_message(version: &Version, kind: &InstallKind) -> String {
    match kind {
        InstallKind::AppBundle { .. } | InstallKind::UserBinary { .. } => {
            format!("CRT v{version} is available — update from the menu")
        }
        InstallKind::Managed { hint } => format!(
            "CRT v{version} is available — installed via {}, run `{}`",
            hint.installed_via(),
            hint.command()
        ),
        InstallKind::Dev => {
            format!("CRT v{version} is available — rebuild from source to update")
        }
    }
}

/// The line shown when a user-requested check finds nothing.
pub fn up_to_date_message(running: &str) -> String {
    format!("CRT v{running} is the latest release")
}

/// The line shown when a check fails.
pub fn failure_message(error: &FetchError) -> String {
    match error {
        FetchError::Offline => "Could not check for updates: no network connection".to_string(),
        FetchError::Timeout => "Could not check for updates: the server timed out".to_string(),
        FetchError::CurlMissing(_) => {
            "Could not check for updates: curl is not installed".to_string()
        }
        other => format!("Could not check for updates: {other}"),
    }
}

/// The label for the update entry in a menu.
pub fn menu_label(available: Option<&Version>) -> String {
    match available {
        Some(version) => format!("Update to v{version}…"),
        None => "Check for Updates…".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::MemoryFetch;
    use crate::install_kind::ManagedHint;

    const SUMS_0_1_6: &str = "\
a58bcb0750a280d603c76d394668b5c77ad201c442ae8e2819d5547da6ac1ee7  crt-0.1.6-linux-aarch64.tar.gz
d638933a4e6f32518a86553ed529cd4c0f9d7f6e170fd69393317610a592cdbf  crt-0.1.6-linux-x86_64.tar.gz
635267c098980f9c89d415dfe883e280ebb04600fd1d62bda0c9823bf1c679a4  crt-0.1.6-macos-aarch64.tar.gz
30912c8fbcf9567dc05dc2d89234e844d60bb815e13debc7a0d7e514eae6a6e7  crt-0.1.6-macos-x86_64.tar.gz
";

    fn fetch_ok() -> MemoryFetch {
        MemoryFetch::new().serving("SHA256SUMS", SUMS_0_1_6)
    }

    fn config(running: &str) -> CheckConfig {
        CheckConfig {
            enabled: true,
            interval: Duration::from_secs(24 * 3600),
            user_requested: false,
            running_version: running.to_string(),
        }
    }

    fn at(unix: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(unix)
    }

    const DAY: u64 = 24 * 3600;

    #[test]
    fn finds_a_newer_release() {
        let mut state = UpdateState::default();
        let event = run_check(&fetch_ok(), &mut state, at(1_000_000), &config("0.1.5"));
        assert_eq!(
            event,
            Some(UpdateEvent::Available {
                version: Version::parse("0.1.6").unwrap(),
                notify: true,
            })
        );
        assert_eq!(state.last_notified_version.as_deref(), Some("0.1.6"));
        assert_eq!(state.last_check, Some(1_000_000));
    }

    #[test]
    fn the_same_version_is_announced_only_once() {
        let mut state = UpdateState::default();
        run_check(&fetch_ok(), &mut state, at(0), &config("0.1.5"));
        // A day later the release is still 0.1.6: found again, but quietly.
        let event = run_check(&fetch_ok(), &mut state, at(DAY), &config("0.1.5"));
        assert_eq!(
            event,
            Some(UpdateEvent::Available {
                version: Version::parse("0.1.6").unwrap(),
                notify: false,
            })
        );
    }

    #[test]
    fn a_newer_release_than_the_one_announced_speaks_up_again() {
        let mut state = UpdateState {
            last_notified_version: Some("0.1.6".to_string()),
            ..Default::default()
        };
        let sums = SUMS_0_1_6.replace("0.1.6", "0.1.7");
        let fetch = MemoryFetch::new().serving("SHA256SUMS", sums);
        let event = run_check(&fetch, &mut state, at(DAY), &config("0.1.5"));
        assert_eq!(
            event,
            Some(UpdateEvent::Available {
                version: Version::parse("0.1.7").unwrap(),
                notify: true,
            })
        );
    }

    #[test]
    fn nothing_happens_before_the_interval_has_passed() {
        let mut state = UpdateState {
            last_check: Some(1_000_000),
            ..Default::default()
        };
        let before = state.clone();
        assert_eq!(
            run_check(
                &fetch_ok(),
                &mut state,
                at(1_000_000 + DAY - 1),
                &config("0.1.5")
            ),
            None
        );
        assert_eq!(state, before, "a skipped check must not touch state");

        // One second later the interval has elapsed.
        assert!(
            run_check(
                &fetch_ok(),
                &mut state,
                at(1_000_000 + DAY),
                &config("0.1.5")
            )
            .is_some()
        );
    }

    #[test]
    fn disabled_checks_never_touch_the_network() {
        let mut state = UpdateState::default();
        let mut cfg = config("0.1.5");
        cfg.enabled = false;
        // Any fetch would be an error; None proves none was attempted.
        let fetch = MemoryFetch::new().failing("SHA256SUMS", FetchError::Offline);
        assert_eq!(run_check(&fetch, &mut state, at(DAY), &cfg), None);
        assert_eq!(state, UpdateState::default());
    }

    #[test]
    fn a_user_requested_check_ignores_both_the_switch_and_the_interval() {
        let mut state = UpdateState {
            last_check: Some(1_000_000),
            last_notified_version: Some("0.1.6".to_string()),
            ..Default::default()
        };
        let mut cfg = config("0.1.5");
        cfg.enabled = false;
        cfg.user_requested = true;

        let event = run_check(&fetch_ok(), &mut state, at(1_000_001), &cfg);
        assert_eq!(
            event,
            Some(UpdateEvent::Available {
                version: Version::parse("0.1.6").unwrap(),
                // Asked directly, so say it again even though it is known.
                notify: true,
            })
        );
    }

    #[test]
    fn being_up_to_date_is_reported_to_whoever_asked() {
        let mut state = UpdateState::default();
        assert_eq!(
            run_check(&fetch_ok(), &mut state, at(DAY), &config("0.1.6")),
            Some(UpdateEvent::UpToDate {
                running: "0.1.6".to_string()
            })
        );
    }

    #[test]
    fn a_local_build_ahead_of_the_release_is_not_offered_a_downgrade() {
        let mut state = UpdateState::default();
        assert_eq!(
            run_check(&fetch_ok(), &mut state, at(DAY), &config("0.2.0")),
            Some(UpdateEvent::UpToDate {
                running: "0.2.0".to_string()
            })
        );
    }

    #[test]
    fn a_failed_check_still_counts_as_a_check() {
        let mut state = UpdateState::default();
        let fetch = MemoryFetch::new().failing("SHA256SUMS", FetchError::Offline);
        assert_eq!(
            run_check(&fetch, &mut state, at(1_000_000), &config("0.1.5")),
            Some(UpdateEvent::Failed {
                error: FetchError::Offline
            })
        );
        assert_eq!(state.last_check, Some(1_000_000));
        // So the next launch a minute later does not try again.
        assert_eq!(
            run_check(&fetch, &mut state, at(1_000_060), &config("0.1.5")),
            None
        );
    }

    #[test]
    fn an_unreadable_release_is_reported_separately_from_a_network_failure() {
        let mut state = UpdateState::default();
        let fetch = MemoryFetch::new().serving("SHA256SUMS", "not a checksums file");
        assert!(matches!(
            run_check(&fetch, &mut state, at(DAY), &config("0.1.5")),
            Some(UpdateEvent::Unreadable { .. })
        ));

        let mut state = UpdateState::default();
        assert!(matches!(
            run_check(&fetch_ok(), &mut state, at(DAY), &config("not-a-version")),
            Some(UpdateEvent::Unreadable { .. })
        ));
    }

    #[test]
    fn a_clock_that_went_backwards_still_allows_a_check() {
        let mut state = UpdateState {
            last_check: Some(2_000_000),
            ..Default::default()
        };
        assert!(run_check(&fetch_ok(), &mut state, at(1_000_000), &config("0.1.5")).is_some());
    }

    #[test]
    fn state_round_trips_through_disk_and_survives_corruption() {
        let dir = std::env::temp_dir().join(format!("crt-update-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = UpdateState::path(&dir);

        assert_eq!(UpdateState::load(&path), UpdateState::default());

        let state = UpdateState {
            last_check: Some(1_700_000_000),
            last_notified_version: Some("0.1.6".to_string()),
            last_finished_version: Some("0.1.5".to_string()),
        };
        state.save(&path).expect("saves");
        assert_eq!(UpdateState::load(&path), state);

        std::fs::write(&path, "this is not toml {{{").unwrap();
        assert_eq!(UpdateState::load(&path), UpdateState::default());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn messages_name_the_version_and_the_way_out() {
        let version = Version::parse("0.1.6").unwrap();

        let bundle = InstallKind::AppBundle {
            bundle_root: "/Applications/crt.app".into(),
        };
        let text = availability_message(&version, &bundle);
        assert!(text.contains("0.1.6") && text.contains("menu"), "{text}");

        let managed = InstallKind::Managed {
            hint: ManagedHint::Pacman,
        };
        let text = availability_message(&version, &managed);
        assert!(
            text.contains("pacman") && text.contains("yay -Syu crt"),
            "{text}"
        );

        let text = availability_message(&version, &InstallKind::Dev);
        assert!(text.contains("rebuild from source"), "{text}");

        assert!(up_to_date_message("0.1.6").contains("0.1.6"));
        assert!(failure_message(&FetchError::Offline).contains("no network"));
        assert!(failure_message(&FetchError::CurlMissing("for updates")).contains("curl"));
    }

    #[test]
    fn the_menu_label_switches_when_something_is_available() {
        assert_eq!(menu_label(None), "Check for Updates…");
        assert_eq!(
            menu_label(Some(&Version::parse("0.1.6").unwrap())),
            "Update to v0.1.6…"
        );
    }
}

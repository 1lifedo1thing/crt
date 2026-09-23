//! Update checking, from the app's side.
//!
//! The decisions live in `crt-update`; this is the plumbing around them. A
//! check runs on a worker thread so the first frame is never delayed by DNS,
//! and its result comes back through the same waker the PTY readers use.
//!
//! Nothing here installs anything. The check reports what exists, the menu
//! entry changes its label, and applying is a separate, user-initiated step
//! (CRT-T-0214 / CRT-T-0215).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::SystemTime;

use crt_update::{
    CheckConfig, CurlFetch, InstallKind, RealFs, UpdateEvent, UpdateState, Version,
    availability_message, classify, failure_message, menu_label, run_check, up_to_date_message,
};

use crate::config::UpdatesConfig;

use super::{WakeReason, Waker};

/// The version this build reports.
///
/// Debug builds honour `CRT_FAKE_VERSION` so the whole flow can be exercised
/// against a real release without cutting one.
pub(crate) fn running_version() -> String {
    #[cfg(debug_assertions)]
    if let Ok(forced) = std::env::var("CRT_FAKE_VERSION")
        && !forced.is_empty()
    {
        return forced;
    }
    env!("CARGO_PKG_VERSION").to_string()
}

/// Where the running binary came from, decided once per launch.
pub(crate) fn current_install_kind() -> InstallKind {
    match std::env::current_exe() {
        Ok(exe) => classify(&exe, &RealFs),
        Err(e) => {
            // Without a path there is nothing we could safely replace.
            log::warn!("could not locate the running executable ({e}); updates are notify-only");
            InstallKind::Dev
        }
    }
}

/// What to do with a check's outcome, decided away from the event loop.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UpdateOutcome {
    /// Show this toast.
    Toast(String),
    /// Nothing worth saying: a repeat of a version already announced, or a
    /// background failure the user did not ask about.
    Silent,
}

/// Update state owned by the app.
pub(crate) struct Updates {
    pub(crate) kind: InstallKind,
    /// Newest release seen, when it is newer than the running build.
    pub(crate) available: Option<Version>,
    rx: Option<Receiver<UpdateEvent>>,
    /// A worker is running; do not start a second one.
    in_flight: bool,
    /// The automatic launch check has been started (or deliberately skipped).
    launch_check_done: bool,
}

impl Updates {
    pub(crate) fn new() -> Self {
        let kind = current_install_kind();
        log::debug!("install kind: {} ({kind:?})", kind.label());
        Self {
            kind,
            available: None,
            rx: None,
            in_flight: false,
            launch_check_done: false,
        }
    }

    /// Label for the update entry in any menu.
    pub(crate) fn menu_label(&self) -> String {
        menu_label(self.available.as_ref())
    }

    /// Start the once-per-launch check, if it has not run yet.
    ///
    /// Called after the first frame is on screen: a check that resolves DNS
    /// before the window is drawn would be felt as slow startup.
    pub(crate) fn start_launch_check(
        &mut self,
        waker: &Arc<Waker>,
        config: &UpdatesConfig,
        state_path: Option<PathBuf>,
    ) {
        if self.launch_check_done {
            return;
        }
        self.launch_check_done = true;
        if !config.check {
            log::debug!("update checks are disabled; no network access");
            return;
        }
        self.spawn(waker, config, state_path, false);
    }

    /// Run a check because the user asked for one.
    pub(crate) fn start_user_check(
        &mut self,
        waker: &Arc<Waker>,
        config: &UpdatesConfig,
        state_path: Option<PathBuf>,
    ) {
        self.spawn(waker, config, state_path, true);
    }

    fn spawn(
        &mut self,
        waker: &Arc<Waker>,
        config: &UpdatesConfig,
        state_path: Option<PathBuf>,
        user_requested: bool,
    ) {
        if self.in_flight {
            log::debug!("an update check is already running");
            return;
        }
        let Some(state_path) = state_path else {
            log::warn!("no config directory; cannot remember update checks");
            return;
        };

        let check_config = CheckConfig {
            enabled: config.check,
            interval: config.interval(),
            user_requested,
            running_version: running_version(),
        };
        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.in_flight = true;

        let waker = waker.clone();
        std::thread::spawn(move || {
            let mut state = UpdateState::load(&state_path);
            let fetch = CurlFetch::new(&check_config.running_version);
            let event = run_check(&fetch, &mut state, SystemTime::now(), &check_config);

            if event.is_some()
                && let Err(e) = state.save(&state_path)
            {
                log::warn!(
                    "could not save update state ({}): {e}",
                    state_path.display()
                );
            }

            // Always send, even when the check was skipped, so the main
            // thread can clear `in_flight`.
            let _ = tx.send(match event {
                Some(event) => event,
                None => UpdateEvent::UpToDate {
                    running: check_config.running_version.clone(),
                },
            });
            waker.wake(WakeReason::Update);
        });
    }

    /// Collect whatever the worker has finished, and decide what to show.
    ///
    /// Returns one outcome per event; the caller turns `Toast` into a toast
    /// on whichever window the user is looking at.
    pub(crate) fn poll(&mut self, was_user_requested: bool) -> Vec<UpdateOutcome> {
        let mut events = Vec::new();
        let mut disconnected = false;
        if let Some(rx) = self.rx.as_ref() {
            loop {
                match rx.try_recv() {
                    Ok(event) => events.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }

        // The worker sends exactly one event and exits, so either of these
        // means it is finished and another check may start.
        if disconnected {
            self.rx = None;
        }
        if !events.is_empty() || disconnected {
            self.in_flight = false;
        }

        events
            .into_iter()
            .map(|event| self.apply(event, was_user_requested))
            .collect()
    }

    fn apply(&mut self, event: UpdateEvent, was_user_requested: bool) -> UpdateOutcome {
        match event {
            UpdateEvent::Available { version, notify } => {
                let message = availability_message(&version, &self.kind);
                // Remember it either way: the menu entry should offer the
                // update even on a launch where the toast stays quiet.
                self.available = Some(version);
                if notify {
                    UpdateOutcome::Toast(message)
                } else {
                    UpdateOutcome::Silent
                }
            }
            UpdateEvent::UpToDate { running } => {
                self.available = None;
                if was_user_requested {
                    UpdateOutcome::Toast(up_to_date_message(&running))
                } else {
                    UpdateOutcome::Silent
                }
            }
            UpdateEvent::Failed { error } => {
                log::warn!("update check failed: {error}");
                if was_user_requested {
                    UpdateOutcome::Toast(failure_message(&error))
                } else {
                    // A laptop that woke up on a train should not be nagged
                    // about it; the log has the detail.
                    UpdateOutcome::Silent
                }
            }
            UpdateEvent::Unreadable { reason } => {
                log::warn!("could not read the latest release: {reason}");
                if was_user_requested {
                    UpdateOutcome::Toast("Could not read the latest release".to_string())
                } else {
                    UpdateOutcome::Silent
                }
            }
        }
    }
}

impl super::App {
    /// `<config dir>/state/update.toml`, when there is a config dir at all.
    pub(crate) fn update_state_path() -> Option<PathBuf> {
        crate::config::Config::config_dir().map(|dir| UpdateState::path(&dir))
    }

    /// Run a check because the user picked the menu entry.
    pub(crate) fn request_update_check(&mut self) {
        // A self-replaceable install with a known update: this is where
        // applying will hook in (CRT-T-0214 / CRT-T-0215). Until then, say
        // so plainly rather than pretending the entry does nothing.
        if self.updates.kind.can_self_replace()
            && let Some(version) = self.updates.available.clone()
        {
            self.show_update_toast(format!(
                "v{version} is available. In-place update lands in a later release; \
                 re-run the install script for now."
            ));
            return;
        }

        // Managed and dev installs cannot be updated from here; open the
        // release page so the user can read what changed.
        if !self.updates.kind.can_self_replace()
            && self.updates.available.is_some()
            && let Err(e) = open::that(RELEASES_PAGE)
        {
            log::warn!("could not open the releases page: {e}");
        }

        self.update_check_requested = true;
        let config = self.config.updates.clone();
        let state_path = Self::update_state_path();
        self.updates
            .start_user_check(&self.waker, &config, state_path);
    }

    /// Turn finished checks into toasts and refresh the menu entry.
    pub(crate) fn drain_update_events(&mut self) {
        let requested = self.update_check_requested;
        let outcomes = self.updates.poll(requested);
        if outcomes.is_empty() {
            return;
        }
        self.update_check_requested = false;

        let label = self.updates.menu_label();
        for state in self.windows.values_mut() {
            state.ui.context_menu.set_update_label(label.clone());
        }
        #[cfg(target_os = "macos")]
        if let Some(ids) = self.menu_ids.as_ref() {
            ids.check_for_updates_item.set_text(&label);
        }

        for outcome in outcomes {
            if let UpdateOutcome::Toast(message) = outcome {
                self.show_update_toast(message);
            }
        }
    }

    /// Show an update message on whichever window the user is looking at.
    fn show_update_toast(&mut self, message: String) {
        let focused = self
            .focused_window
            .filter(|id| self.windows.contains_key(id));
        let target = match focused {
            Some(id) => self.windows.get_mut(&id),
            None => self.windows.values_mut().next(),
        };
        match target {
            Some(state) => state.ui.toast.show(message, crate::window::ToastType::Info),
            // No window yet: the log is the only place left to say it.
            None => log::info!("{message}"),
        }
    }
}

/// Where a user without an in-place update is sent to read about the release.
const RELEASES_PAGE: &str = "https://github.com/colliery-io/crt/releases/latest";

#[cfg(test)]
mod tests {
    use super::*;
    use crt_update::ManagedHint;

    fn updates(kind: InstallKind) -> Updates {
        Updates {
            kind,
            available: None,
            rx: None,
            in_flight: false,
            launch_check_done: false,
        }
    }

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn an_available_update_is_remembered_even_when_it_is_not_announced() {
        let mut u = updates(InstallKind::UserBinary {
            path: "/home/dev/.local/bin/crt".into(),
        });

        let outcome = u.apply(
            UpdateEvent::Available {
                version: v("0.1.6"),
                notify: false,
            },
            false,
        );
        // Quiet, but the menu still offers it.
        assert_eq!(outcome, UpdateOutcome::Silent);
        assert_eq!(u.available, Some(v("0.1.6")));
        assert_eq!(u.menu_label(), "Update to v0.1.6…");
    }

    #[test]
    fn a_first_sighting_is_announced_with_the_right_advice() {
        let mut u = updates(InstallKind::Managed {
            hint: ManagedHint::Pacman,
        });
        let outcome = u.apply(
            UpdateEvent::Available {
                version: v("0.1.6"),
                notify: true,
            },
            false,
        );
        match outcome {
            UpdateOutcome::Toast(text) => {
                assert!(text.contains("0.1.6"), "{text}");
                assert!(text.contains("yay -Syu crt"), "{text}");
            }
            other => panic!("expected a toast, got {other:?}"),
        }
    }

    #[test]
    fn background_non_news_stays_quiet_but_a_requested_check_answers() {
        let mut u = updates(InstallKind::Dev);

        assert_eq!(
            u.apply(
                UpdateEvent::UpToDate {
                    running: "0.1.6".into()
                },
                false
            ),
            UpdateOutcome::Silent
        );
        assert!(matches!(
            u.apply(
                UpdateEvent::UpToDate {
                    running: "0.1.6".into()
                },
                true
            ),
            UpdateOutcome::Toast(_)
        ));
    }

    #[test]
    fn a_background_failure_is_logged_not_shown() {
        let mut u = updates(InstallKind::Dev);
        let offline = || UpdateEvent::Failed {
            error: crt_update::FetchError::Offline,
        };
        assert_eq!(u.apply(offline(), false), UpdateOutcome::Silent);
        match u.apply(offline(), true) {
            UpdateOutcome::Toast(text) => assert!(text.contains("no network"), "{text}"),
            other => panic!("expected a toast, got {other:?}"),
        }
    }

    #[test]
    fn an_unreadable_release_never_claims_an_update_exists() {
        let mut u = updates(InstallKind::Dev);
        u.available = Some(v("0.1.6"));
        let outcome = u.apply(
            UpdateEvent::Unreadable {
                reason: "mixed versions".into(),
            },
            true,
        );
        assert!(matches!(outcome, UpdateOutcome::Toast(_)));
        // The stale "available" reading is left alone rather than invented anew.
        assert_eq!(u.available, Some(v("0.1.6")));
    }

    #[test]
    fn going_up_to_date_clears_the_menu_entry() {
        let mut u = updates(InstallKind::UserBinary {
            path: "/home/dev/.local/bin/crt".into(),
        });
        u.available = Some(v("0.1.6"));
        u.apply(
            UpdateEvent::UpToDate {
                running: "0.1.6".into(),
            },
            false,
        );
        assert_eq!(u.available, None);
        assert_eq!(u.menu_label(), "Check for Updates…");
    }

    #[test]
    fn the_running_version_is_a_real_semver() {
        assert!(crt_update::manifest::parse_version(&running_version()).is_ok());
    }
}

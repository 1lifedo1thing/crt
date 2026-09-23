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
    Applied, ApplyError, CheckConfig, CurlFetch, Fetch, InstallKind, RealFs, ReleaseManifest,
    Stage, UpdateEvent, UpdatePlan, UpdateState, Version, apply, availability_message, classify,
    failure_message, manifest, menu_label, run_check, up_to_date_message,
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

/// What an in-progress update reports back to the main thread.
#[derive(Debug)]
pub(crate) enum ApplyEvent {
    Stage(Stage),
    Done(Applied),
    Failed(ApplyError),
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
    /// Receives progress from an in-progress update.
    apply_rx: Option<Receiver<ApplyEvent>>,
    /// An update is being installed.
    applying: bool,
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
            apply_rx: None,
            applying: false,
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

    /// Download and install the newest release, reporting progress.
    ///
    /// The release is read again here rather than cached from the check: the
    /// asset list and its hashes belong together, they are a few hundred
    /// bytes, and fetching them now means the download is verified against
    /// what the release says at the moment it is installed.
    pub(crate) fn start_apply(&mut self, waker: &Arc<Waker>) {
        if self.applying {
            log::debug!("an update is already being installed");
            return;
        }
        if !self.kind.can_self_replace() {
            return;
        }

        let (tx, rx) = channel();
        self.apply_rx = Some(rx);
        self.applying = true;

        let kind = self.kind.clone();
        let waker = waker.clone();
        let running = running_version();
        std::thread::spawn(move || {
            let fetch = CurlFetch::new(&running);
            let send = |event| {
                let _ = tx.send(event);
                waker.wake(WakeReason::Update);
            };

            let outcome = plan_update(&fetch, &kind)
                .and_then(|plan| apply(&plan, &fetch, &mut |stage| send(ApplyEvent::Stage(stage))));

            send(match outcome {
                Ok(applied) => ApplyEvent::Done(applied),
                Err(error) => ApplyEvent::Failed(error),
            });
        });
    }

    /// Collect progress from an in-progress update.
    pub(crate) fn poll_apply(&mut self) -> Vec<ApplyEvent> {
        let mut events = Vec::new();
        let mut disconnected = false;
        if let Some(rx) = self.apply_rx.as_ref() {
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

        if events
            .iter()
            .any(|e| matches!(e, ApplyEvent::Done(_) | ApplyEvent::Failed(_)))
        {
            self.applying = false;
        }
        if disconnected {
            self.apply_rx = None;
            self.applying = false;
        }

        // An update that succeeded is no longer available to install.
        if events.iter().any(|e| matches!(e, ApplyEvent::Done(_))) {
            self.available = None;
        }
        events
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
        // A self-replaceable install with an update waiting: install it.
        if self.updates.kind.can_self_replace() && self.updates.available.is_some() {
            self.updates.start_apply(&self.waker);
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

    /// Turn update progress into toasts.
    pub(crate) fn drain_apply_events(&mut self) {
        for event in self.updates.poll_apply() {
            match event {
                ApplyEvent::Stage(stage) => {
                    let version = self.updates.available.clone();
                    self.show_update_toast(stage.message(version.as_ref()));
                }
                ApplyEvent::Done(applied) => {
                    log::info!(
                        "installed v{}; previous version kept at {}",
                        applied.version,
                        applied.previous.display()
                    );
                    self.show_update_toast(format!(
                        "v{} installed. It takes effect the next time you open CRT.",
                        applied.version
                    ));
                    self.refresh_update_menu_label();
                }
                ApplyEvent::Failed(error) => {
                    log::warn!("update failed: {error}");
                    self.show_update_toast(error.user_message());
                }
            }
        }
    }

    /// Put the current label on the update entry in every menu.
    fn refresh_update_menu_label(&mut self) {
        let label = self.updates.menu_label();
        for state in self.windows.values_mut() {
            state.ui.context_menu.set_update_label(label.clone());
        }
        #[cfg(target_os = "macos")]
        if let Some(ids) = self.menu_ids.as_ref() {
            ids.check_for_updates_item.set_text(&label);
        }
    }

    /// Turn finished checks into toasts and refresh the menu entry.
    pub(crate) fn drain_update_events(&mut self) {
        self.drain_apply_events();

        let requested = self.update_check_requested;
        let outcomes = self.updates.poll(requested);
        if outcomes.is_empty() {
            return;
        }
        self.update_check_requested = false;

        self.refresh_update_menu_label();

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

/// Read the current release and build the plan for installing it.
fn plan_update(fetch: &dyn Fetch, kind: &InstallKind) -> Result<UpdatePlan, ApplyError> {
    let text = fetch.get_text(&manifest::sums_url(), crt_update::CHECK_TIMEOUT)?;
    let release = ReleaseManifest::parse(&text).map_err(|e| ApplyError::ReleaseUnreadable {
        reason: e.to_string(),
    })?;
    let asset = release
        .asset_for_current_platform()
        .ok_or_else(|| {
            let (os, arch) = manifest::current_platform();
            ApplyError::ReleaseUnreadable {
                reason: format!("v{} has no {os}-{arch} build", release.version),
            }
        })?
        .clone();

    Ok(UpdatePlan {
        kind: kind.clone(),
        version: release.version,
        asset,
    })
}

/// Run `crt update` without a window.
///
/// Exit codes are meant for scripts: 0 did something, 2 nothing to do,
/// 1 failed.
pub(crate) fn run_cli(check_only: bool) -> i32 {
    let running = running_version();
    let kind = current_install_kind();
    println!("crt {running} ({})", kind.label());

    let fetch = CurlFetch::new(&running);
    let text = match fetch.get_text(&manifest::sums_url(), crt_update::CHECK_TIMEOUT) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("{}", failure_message(&e));
            return 1;
        }
    };
    let release = match ReleaseManifest::parse(&text) {
        Ok(release) => release,
        Err(e) => {
            eprintln!("Could not read the latest release: {e}");
            return 1;
        }
    };

    let status = match manifest::compare(&running, &release.version) {
        Ok(status) => status,
        Err(e) => {
            eprintln!("Could not compare versions: {e}");
            return 1;
        }
    };
    let Some(version) = status.available_version().cloned() else {
        println!("{}", up_to_date_message(&running));
        return 2;
    };

    println!("v{version} is available");
    if check_only {
        return 0;
    }

    if !kind.can_self_replace() {
        match kind.upgrade_hint() {
            Some(hint) => println!("This install is managed elsewhere; update it with: {hint}"),
            None => println!("This install cannot be updated in place"),
        }
        return 2;
    }

    let plan = match plan_update(&fetch, &kind) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("{}", e.user_message());
            return 1;
        }
    };

    match apply(&plan, &fetch, &mut |stage| {
        println!("{}", stage.message(Some(&plan.version)));
    }) {
        Ok(applied) => {
            println!(
                "v{} installed. The previous version is at {}.",
                applied.version,
                applied.previous.display()
            );
            0
        }
        Err(e) => {
            eprintln!("{}", e.user_message());
            1
        }
    }
}

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
            apply_rx: None,
            applying: false,
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

//! Configuration file watcher for hot-reloading
//!
//! Watches `~/.config/crt/config.toml` and `~/.config/crt/themes/*.css`.
//!
//! Events are debounced on the *trailing* edge: a save that arrives as a
//! burst of filesystem events (truncate, write, rename) produces exactly one
//! `ConfigEvent`, emitted once the burst has been quiet for `DEBOUNCE`, so the
//! reload always reads the completed file. The watcher thread also calls the
//! supplied wake function so the event loop does not have to poll.

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use crate::config::Config;
use crt_core::WakeFn;

/// Quiet period after the last filesystem event before a change is reported.
pub const DEBOUNCE: Duration = Duration::from_millis(100);

/// Events from the config watcher
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigEvent {
    /// config.toml was modified
    ConfigChanged,
    /// A theme CSS file was modified (path of the file)
    ThemeChanged(PathBuf),
}

/// Raw filesystem notification, before debouncing
enum RawEvent {
    Config,
    Theme(PathBuf),
}

/// Watches config directory for changes
pub struct ConfigWatcher {
    _watcher: RecommendedWatcher,
    receiver: Receiver<RawEvent>,
    /// Pending config change and the time of its most recent event
    pending_config: Option<Instant>,
    /// Pending theme changes keyed by path, with the time of the most recent event
    pending_themes: Vec<(PathBuf, Instant)>,
}

impl ConfigWatcher {
    /// Create a new config watcher. `wake` is called from the watcher thread
    /// whenever a relevant file changes so the event loop can poll promptly.
    pub fn new(wake: Option<WakeFn>) -> Option<Self> {
        let config_dir = Config::config_dir()?;
        // The directory may not exist yet on first run; create it so the
        // watch succeeds and hot-reload works once the user saves a config.
        let _ = std::fs::create_dir_all(&config_dir);
        let themes_dir = config_dir.join("themes");
        let _ = std::fs::create_dir_all(&themes_dir);

        // Canonicalize paths to handle symlinks (e.g., /tmp -> /private/tmp on macOS)
        let config_dir = config_dir.canonicalize().unwrap_or(config_dir);
        let themes_dir = themes_dir.canonicalize().unwrap_or(themes_dir);
        let config_path = config_dir.join("config.toml");

        let (tx, rx) = channel();

        let config_path_clone = config_path.clone();
        let themes_dir_clone = themes_dir.clone();

        let mut watcher = notify::recommended_watcher(move |res: Result<Event, _>| {
            let event = match res {
                Ok(event) => event,
                Err(e) => {
                    log::error!("Watcher error: {:?}", e);
                    return;
                }
            };
            // Ignore access/metadata-only events
            if event.kind.is_access() || event.kind.is_other() {
                return;
            }
            let mut relevant = false;
            for path in &event.paths {
                if path == &config_path_clone {
                    log::debug!("Config file changed");
                    let _ = tx.send(RawEvent::Config);
                    relevant = true;
                } else if path.starts_with(&themes_dir_clone)
                    && path.extension().is_some_and(|e| e == "css")
                {
                    log::debug!("Theme file changed: {:?}", path);
                    let _ = tx.send(RawEvent::Theme(path.clone()));
                    relevant = true;
                }
            }
            if relevant && let Some(wake) = &wake {
                wake();
            }
        })
        .ok()?;

        // Watch only what we care about: the config directory non-recursively
        // (for config.toml) and the themes directory. This keeps profiler
        // logs and other files in the config dir from generating events.
        watcher
            .watch(&config_dir, RecursiveMode::NonRecursive)
            .ok()?;
        if let Err(e) = watcher.watch(&themes_dir, RecursiveMode::NonRecursive) {
            log::warn!("Could not watch themes directory {:?}: {}", themes_dir, e);
        }

        log::info!("Watching {:?} for config changes", config_dir);

        Some(Self {
            _watcher: watcher,
            receiver: rx,
            pending_config: None,
            pending_themes: Vec::new(),
        })
    }

    /// Absorb raw events into the pending set (non-blocking).
    fn drain_raw(&mut self) {
        let now = Instant::now();
        while let Ok(event) = self.receiver.try_recv() {
            match event {
                RawEvent::Config => self.pending_config = Some(now),
                RawEvent::Theme(path) => {
                    if let Some(entry) = self.pending_themes.iter_mut().find(|(p, _)| *p == path) {
                        entry.1 = now;
                    } else {
                        self.pending_themes.push((path, now));
                    }
                }
            }
        }
    }

    /// Poll for a debounced config event (non-blocking).
    ///
    /// Returns the next change whose burst has been quiet for `DEBOUNCE`.
    pub fn poll(&mut self) -> Option<ConfigEvent> {
        self.drain_raw();
        let now = Instant::now();

        if let Some(last) = self.pending_config
            && now.duration_since(last) >= DEBOUNCE
        {
            self.pending_config = None;
            return Some(ConfigEvent::ConfigChanged);
        }

        if let Some(idx) = self
            .pending_themes
            .iter()
            .position(|(_, last)| now.duration_since(*last) >= DEBOUNCE)
        {
            let (path, _) = self.pending_themes.swap_remove(idx);
            return Some(ConfigEvent::ThemeChanged(path));
        }

        None
    }

    /// When the next pending event becomes due, if any. The event loop uses
    /// this to schedule a wake-up instead of polling.
    pub fn next_due(&mut self) -> Option<Instant> {
        self.drain_raw();
        let config_due = self.pending_config.map(|t| t + DEBOUNCE);
        let theme_due = self.pending_themes.iter().map(|(_, t)| *t + DEBOUNCE).min();
        match (config_due, theme_due) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Whether `path` is a theme file this watcher would report.
    #[allow(dead_code)]
    pub fn is_theme_path(path: &Path) -> bool {
        path.extension().is_some_and(|e| e == "css")
    }
}

//! Update discovery for CRT.
//!
//! Two questions have to be answered before anything is downloaded:
//!
//! - **What is the latest release?** [`manifest`] parses the `SHA256SUMS`
//!   file published with every release (see `.github/workflows/release.yml`).
//!   That one file carries both the version, in its filenames, and the hashes
//!   used to verify a download, so discovery needs no GitHub API call and no
//!   JSON.
//! - **May we replace the running binary?** [`install_kind`] classifies where
//!   the executable came from. A binary owned by a package manager, or one
//!   sitting in a build tree, is never touched: the user is told how to
//!   upgrade it instead.
//!
//! Both modules are pure. `manifest` is a parser over a string; `install_kind`
//! takes its filesystem facts from an [`FsProbe`] so every branch is testable
//! on any platform. The I/O that acts on their output lives elsewhere.

pub mod apply;
pub mod check;
pub mod fetch;
pub mod install_kind;
pub mod manifest;

pub use apply::{Applied, ApplyError, Stage, UpdatePlan, apply};
pub use check::{
    CHECK_TIMEOUT, CheckConfig, UpdateEvent, UpdateState, availability_message, failure_message,
    menu_label, run_check, up_to_date_message,
};
pub use fetch::{CurlFetch, Fetch, FetchError, MemoryFetch};
pub use install_kind::{FsProbe, InstallKind, ManagedHint, RealFs, classify};
pub use manifest::{
    Asset, LATEST_SUMS_URL, ManifestError, ReleaseManifest, UpdateStatus, compare,
    current_platform, sums_url,
};
pub use semver::Version;

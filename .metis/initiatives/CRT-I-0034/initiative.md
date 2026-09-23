---
id: in-place-self-update-with-update
level: initiative
title: "In-place self-update with update notifications"
short_code: "CRT-I-0034"
created_at: 2026-09-22T16:19:44.650467+00:00
updated_at: 2026-09-22T16:37:17.128618+00:00
parent: CRT-V-0001
blocked_by: []
archived: false

tags:
  - "#initiative"
  - "#phase/decompose"


exit_criteria_met: false
estimated_complexity: M
initiative_id: in-place-self-update-with-update
---

# In-place self-update with update notifications Initiative

## Context

Upgrading CRT today means re-running the install script (`curl … install.sh | sh`) or, on Arch, the package manager. Nothing tells a user that a newer release exists, and the v0.1.4 → v0.1.5 cycle showed how much a point release can matter. Users find out about fixes only by visiting GitHub.

How installs work today (as of v0.1.5), verified in the repo, which the design has to respect:

- **Release assets** (`release.yml`, tag push): four tarballs on the GitHub release, `crt-<ver>-{macos,linux}-{x86_64,aarch64}.tar.gz`. No checksums file, no signatures. `publish-aur.sh` recomputes sha256 sums itself.
- **macOS**: the tarball holds an unsigned, un-notarised `crt.app`; `install.sh` copies it to `/Applications/crt.app` as the user (so the user owns the bundle) and runs `sudo xattr -rd com.apple.quarantine` on it. That xattr step is vestigial: `curl` does not set the quarantine attribute, and `Info.plist` has no `LSFileQuarantineEnabled`, so nothing CRT itself downloads gets it either. `/Applications` is `root:admin 775`: admin users can rename inside it, non-admin users cannot.
- **Linux**: the tarball holds a bare `crt` binary plus icons; `install.sh` copies it to `${CRT_INSTALL_DIR:-~/.local/bin}` and installs a `.desktop` entry and icons.
- **AUR**: a PKGBUILD pointing at the same tarballs. The package manager owns these installs; the app must never overwrite them. `cargo install` (`~/.cargo/bin`) is the same situation with a writable path.
- **Config side of install**: `install.sh` copies bundled `themes/*.css`, `fonts/*` and `default_config.toml` into `~/.config/crt`. It never overwrites the user's `config.toml`, but it overwrites the bundled theme files unconditionally (`install.sh:227`), so in-place edits to a bundled theme are lost on every install today.
- **In the app**: no HTTP client dependency; `open` crate is present; `Toast` (info/warning/error, 5 s) and the native About menu (shows `CARGO_PKG_VERSION`) exist; the event loop sleeps and is woken through `Waker`/`WakeReason`, so a background check needs a wake reason to surface its result. Restarting the app kills every shell in every tab.

## Goals & Non-Goals

**Goals:**
- Tell the user, inside the emulator, when a newer release is available, without being nagging or blocking.
- Let the user upgrade from inside the emulator, on macOS and script-installed Linux, without re-running the install script, including the bundled themes/fonts/default config refresh the script does, and without losing their edits.
- Do it safely: verify what is downloaded before it replaces anything, never leave a half-replaced install, keep the previous version until the new one has run.
- Respect installs the app does not own (AUR, `cargo install`, Homebrew if ever added, dev builds): notify only, and say how to upgrade.
- Let the user turn checks off, and never contact the network when they have.

**Non-Goals:**
- Applying updates without the user asking (checks are automatic; the upgrade is always user-initiated).
- Restarting the app for the user. An installed update takes effect the next time CRT is opened.
- Pre-release channels. `releases/latest` never includes pre-releases and that is the only source.
- Delta/differential updates.
- Code signing and notarisation on macOS: a separate release-pipeline initiative. This one works with unsigned bundles and must not need changes once signing exists.
- An update server. GitHub Releases is the only source.
- Windows.

## Requirements

### User Requirements
- **User Characteristics**: developers running a terminal emulator; comfortable with a one-line install command but unlikely to check GitHub for releases.
- **System Functionality**: an unobtrusive "v0.1.6 is available" notice, a one-action upgrade that leaves running shells alone, clear failure messages.
- **User Interfaces**: a toast on check, a menu entry (native menu on macOS, context menu elsewhere) "Check for Updates…" / "Update to vX.Y.Z", a `[updates]` config section, and `crt --version` / `crt update` for scripts.

### System Requirements
- **Functional Requirements**
  - REQ-001 (discovery): On launch (rate-limited, default once per 24 h) and on demand, fetch `https://github.com/colliery-io/crt/releases/latest/download/SHA256SUMS` off the main thread. The file lists `<sha256>  crt-<ver>-<os>-<arch>.tar.gz` for every asset; the version is parsed from the filenames with `semver` and compared with `CARGO_PKG_VERSION`. One plain HTTPS request, no GitHub API, no rate limit. A file that does not parse, or lists more than one version, is ignored with a log line.
  - REQ-002 (notify): Surface a newer version as an info toast naming the version, and retitle the update menu entry "Update to vX.Y.Z…". Show the toast at most once per newer version per day; the menu entry stays until the update is applied.
  - REQ-003 (install kind): Classify `fs::canonicalize(current_exe())` into: `AppBundle { bundle_root }` (path contains `.app/Contents/MacOS/`), `Managed { hint }` (under `/usr`, `/opt`, a Homebrew prefix, `~/.cargo/bin`, or not writable by the current user), `Dev` (`cfg!(debug_assertions)` or path contains `/target/`), else `UserBinary { path }`. Only `AppBundle` and `UserBinary` may self-replace. Order matters: `Managed` and `Dev` are tested before `UserBinary`.
  - REQ-004 (apply): For a self-replaceable install: take a lock file in the staging dir (fail with "another update is in progress" if held); download the asset for the running `(target_os, target_arch)` into a hidden staging dir on the same filesystem as the install target (`/Applications/.crt-update/` or `<install_dir>/.crt-update/`); verify its sha256 against `SHA256SUMS`; unpack; strip `com.apple.quarantine` recursively without sudo (defensive; see Context); then swap with two renames: current → `<name>.old`, staged → current. Remove the staging dir. The running process keeps working on the old inode.
  - REQ-005 (after apply): Toast "v0.1.6 installed. It takes effect the next time you open CRT." No restart prompt. The menu entry reverts to "Check for Updates…" and `--version` of the running process still reports the old version.
  - REQ-006 (first launch after upgrade): When the running version differs from `state.last_finished_version`: refresh bundled assets into the config dir under a manifest (see REQ-011), refresh Linux icons/`.desktop`, delete the `.old` sibling, record the version. Reaching the first presented frame counts as success.
  - REQ-007 (managed/dev): Notification plus a per-kind hint (`yay -Syu crt`, `cargo install crt`, `git pull && cargo build --release`), never a download; the menu entry opens the release page via `open`.
  - REQ-008 (config): `[updates] check = true|false` (default true), `interval_hours = 24`. `check = false` means no network access at all, including on launch; "Check for Updates…" in the menu is still allowed because the user asked for it there.
  - REQ-009 (CLI): `crt --version` prints version and install kind; `crt update` runs discovery + apply headlessly and exits non-zero on failure; `crt update --check` only reports. Uses the same lock as the GUI.
  - REQ-010 (failure): Every failure (offline, 404, checksum mismatch, permission denied, lock held, swap failure) ends in a toast with a specific message and a `warn` log line, the staging dir removed, and the install untouched. The permission-denied message names the path and says to re-run the install script.
  - REQ-011 (asset manifest): Every file the updater (or `crt update --finish-install`, which `install.sh` will call) writes into the config dir is recorded in `~/.config/crt/state/assets.toml` with its sha256. On refresh, a file is overwritten only when its current hash equals the recorded one (unmodified) or it does not exist; modified files are skipped and listed in one info toast ("3 bundled themes skipped because you edited them"). `config.toml` is never touched.
  - REQ-012 (rollback): The previous version stays at `crt.app.old` / `crt.old` until REQ-006 completes. Recovery when the new version does not start is a documented manual rename; there is no `--rollback` flag, because it would depend on the new binary running.
- **Non-Functional Requirements**
  - NFR-001: The check never blocks the UI thread and adds no startup latency: spawned after the first presented frame, result delivered through `WakeReason::Update`. Network timeouts are short (connect 5 s, total 15 s for the check) and never retried within the interval.
  - NFR-002: Integrity: `release.yml` publishes `SHA256SUMS` alongside the tarballs; `publish-aur.sh` reads sums from it. The app verifies every download against it before unpacking. Authenticity (signing) is the codesigning initiative's job; until then the trust anchor is TLS to `github.com`, and the docs say so.
  - NFR-003: The swap is atomic per rename; a crash at any point leaves the old or the new install complete, plus at most a staging dir the next run cleans up.
  - NFR-004: Never `sudo`. If the target is not writable, fail per REQ-010.
  - NFR-005: No new heavyweight dependencies. HTTPS via a `curl` subprocess (`install.sh` already requires it; macOS ships it; honours `HTTPS_PROXY` and the system trust store) behind a `Fetch` trait so tests substitute it; `semver`, `sha2`, `tar`, `flate2` only.
  - NFR-006: Binary size and the startup path are unchanged when `updates.check = false`.
  - NFR-007: Privacy: the check sends one request to github.com with the default `curl` user agent and nothing else; documented in `docs/how-to/update.md` next to `check = false`.

## Use Cases

### Use Case 1: Passive notification
- **Actor**: any user, default config
- **Scenario**: launches CRT 3 days after v0.1.6 shipped. After the first frame the check fetches `SHA256SUMS`, finds v0.1.6 > v0.1.5, shows an info toast "CRT v0.1.6 is available — Update from the menu". The menu entry reads "Update to v0.1.6…".
- **Expected Outcome**: the toast fades as usual; nothing else changes; no toast again for 24 h; the menu entry persists.

### Use Case 2: In-place upgrade on macOS
- **Actor**: admin user who installed with `install.sh`
- **Scenario**: picks "Update to v0.1.6…". Toasts show progress ("Downloading v0.1.6…", "Verifying", "Installing"). The bundle is downloaded to `/Applications/.crt-update/`, verified, unpacked, renamed over the old one (old kept as `crt.app.old`). Toast: "v0.1.6 installed. It takes effect the next time you open CRT."
- **Expected Outcome**: shells keep running. Next launch: About shows v0.1.6, themes/fonts refreshed except the user's edited `dracula.css` (one toast lists it), `crt.app.old` is gone.

### Use Case 3: Package-manager install
- **Actor**: Arch user on the AUR package, or anyone who used `cargo install`
- **Scenario**: same check; toast "CRT v0.1.6 is available — installed via pacman, run `yay -Syu crt`". Menu entry opens the release page.
- **Expected Outcome**: nothing on disk changes.

### Use Case 4: Failure
- **Actor**: non-admin macOS user
- **Scenario**: download and verification succeed; renaming inside `/Applications` fails with EACCES.
- **Expected Outcome**: staging dir removed, old bundle untouched, error toast "Could not replace /Applications/crt.app (permission denied). Re-run the install script to update.", `warn` log line.

## Architecture

### Overview
New crate `crates/crt-update` with three layers, plus thin wiring in the app:

1. **`manifest` (pure)**: parse `SHA256SUMS` into `ReleaseManifest { version: semver::Version, assets: BTreeMap<String, [u8; 32]> }`; select the asset for `(os, arch)`; compare with the running version. Fixture-tested.
2. **`install_kind` (pure over a probe)**: `classify(exe: &Path, probe: &dyn FsProbe) -> InstallKind` with `FsProbe { canonicalize, is_writable, home_dir, is_debug_build }` so every branch is unit-tested on all platforms.
3. **`apply` (I/O, worker thread)**: `Fetch` trait (curl-backed in production, in-memory in tests) → `download`, `verify`, `unpack`, `swap`, `finish_first_launch`. Reports `UpdateEvent { Available(Version), Progress(Stage), Installed(Version), Failed(UpdateError), UpToDate }` through a channel; the app side maps them to toasts/menu state and wakes the loop with `WakeReason::Update`.
4. **App wiring**: `[updates]` config, `state/update.toml` (last check time, last notified version, last finished version), menu entries, CLI subcommand, first-launch hook after the first presented frame.

### Sequence (upgrade)
`menu Update` → `App` spawns worker (lock) → `fetch SHA256SUMS` → `download asset` (progress events) → `verify` → `unpack` → `xattr strip` → `swap` → `Installed` event → toast → worker exits, lock released. Next launch: `finish_first_launch` → assets manifest refresh → delete `.old` → record version.

### Release pipeline
`release.yml`: after all builds, write `SHA256SUMS` over the four tarballs and attach it; `publish-aur.sh` reads sums from it. Both are independent of the app work and ship first.

## Detailed Design

Fixed during discovery (see Decisions); the design phase fills in the module APIs and the state file schema.

- **Version source**: filenames in `SHA256SUMS` (`crt-0.1.6-macos-aarch64.tar.gz`); `v` prefix tolerated. All entries must agree on one version.
- **Rate limiting / state**: `~/.config/crt/state/update.toml` with `last_check`, `last_notified_version`, `last_finished_version`; separate from `config.toml`. Last-writer-wins between processes is acceptable.
- **Locking**: `<staging>/lock` created with `O_EXCL`, containing the pid; stale if the pid is gone.
- **Same-filesystem staging**: hidden sibling dir of the target so the final rename is atomic; removed after success or failure; leftovers removed on the next launch.
- **Restart**: none. `open -n` / re-exec is out of scope.
- **Asset refresh**: `crt update --finish-install <assets_dir>` is the one implementation; `install.sh` calls it after copying the binary (fallback to its current copy logic if the binary cannot run, for the first install of a version that has it).
- **`Dev` hint**: `git pull && cargo build --release`; `Managed` hints keyed by which prefix matched.
- **Tests**: `manifest` and `install_kind` are pure; `apply` is integration-tested against an in-memory `Fetch` and a temp install tree (binary and fake `.app` layout) on both CI platforms; the macOS bundle swap gets one manual check on an unsigned bundle before release.

## Alternatives Considered

- **GitHub REST API for discovery**: 60 unauthenticated requests/hour per IP; an office NAT hits 403s. Would allow a pre-release channel. Rejected for `SHA256SUMS` via `releases/latest/download`, which also removes JSON parsing.
- **`ureq` + `rustls` in-process HTTP**: self-contained, easy progress reporting; adds ~1.5 MB and a CA bundle that ignores corporate roots and proxies. Rejected for a `curl` subprocess behind a trait.
- **`self_update` crate**: single-binary model only, pulls `reqwest`. Rejected.
- **Sparkle / AppImageUpdate**: platform-native but two frameworks, an appcast to host, and Sparkle needs a signed bundle. Revisit with the codesigning initiative.
- **Restart prompt after install**: rejected; restarting a terminal emulator closes every running shell. The update takes effect on the next launch.
- **Overwrite bundled themes as `install.sh` does**: rejected in favour of a hash manifest so in-place edits survive; costs one small state file.
- **Auto-apply in the background**: rejected; a terminal replacing itself under a session without consent is a bad surprise, more so with unsigned bundles.
- **Homebrew tap**: worth having as a managed macOS path, but distribution work outside this initiative.

## Implementation Plan

Each phase ships value on its own; decompose into one task per line item.

1. **Release pipeline**: `SHA256SUMS` attached to releases; `publish-aur.sh` consumes it. No app change; ships with the next release so the app has something to fetch.
2. **`crt-update` crate, pure layers**: `manifest` parsing/selection/comparison; `install_kind` classification with `FsProbe`; fixtures and tests on both CI platforms.
3. **Check + notify in the app**: `[updates]` config; `state/update.toml`; `Fetch` trait with the curl backend; background check after the first frame via `WakeReason::Update`; toast + menu entry (native on macOS, context menu elsewhere) with the managed/dev hints; `crt --version` prints the install kind.
4. **Apply, Linux user binary**: lock, staging, download with progress, verify, unpack, swap, cleanup, failure toasts; `crt update` / `--check`; integration tests on a temp install tree.
5. **Apply, macOS bundle**: bundle-shaped unpack and swap, quarantine strip, `.old` bundle; the same integration tests on a fake `.app` tree plus one manual check on an unsigned bundle.
6. **First-launch finish + asset manifest**: `assets.toml`, refresh with modified-file skipping and its toast, icons/`.desktop` on Linux, `.old` deletion, `crt update --finish-install` used by `install.sh`; `docs/how-to/update.md` (how it works, `check = false`, privacy note, manual rollback).

## Decisions (2026-09-22)

1. **Structure**: new crate `crates/crt-update`; the app and the `crt update` CLI both call it.
2. **Consent**: checks on by default, once per 24 h; `[updates] check = false` disables all network access. Applying is always user-initiated.
3. **Signing / notarisation**: separate initiative; this one ships with `SHA256SUMS` + TLS as the trust anchor.
4. **Scope of in-place apply**: macOS app bundle and Linux user-dir binary. AUR, `cargo install` and any future Homebrew tap are notify-only. No Homebrew tap here.
5. **No restart prompt**: an installed update takes effect on the next launch; shells are never closed by the updater.
6. **Asset refresh uses a hash manifest**: bundled files the user edited in place are skipped and listed, not overwritten.
7. **Discovery via `releases/latest/download/SHA256SUMS`**, not the GitHub API; no pre-release channel.
8. **HTTPS via a `curl` subprocess** behind a `Fetch` trait; no in-process TLS stack.

Corrections folded in from the review: `~/.cargo/bin` and unwritable paths are `Managed`; symlinks are canonicalised before classification; `Dev` covers release builds run from `target/`; the quarantine step is defensive only and never uses sudo; staging is a hidden same-filesystem dir with a lock file; rollback is the `.old` sibling plus a documented rename, not a flag; Linux icons/`.desktop` are refreshed with the assets.

## Status Updates

### 2026-09-23 - all six tasks complete, ready for review

Commits on `feat/self-update`: `89d9a78` (SHA256SUMS), `5c134d0` (crate, pure layers), `a3ab352` (check + notify), `019110e` (apply + CLI), `c752e67` (macOS bundle), `b44b08b` (assets, first launch, docs).

865 workspace tests (up from 848 before this initiative; 81 in `crt-update`), `cargo clippy --all-targets` clean, `cargo fmt` applied.

**Deviations from the plan above, all recorded on their tasks:**

- **REQ-006**: bundled assets are refreshed at *install* time, not on first launch. The tarball carries `assets/` and the updater has it unpacked in hand; first launch keeps the other half of the requirement (removing the retired copy, which is the moment the new binary has proved it starts). The `.desktop`/icons half of REQ-006 was dropped: `scripts/install.sh` never installed them - `installer/linux/install.sh` does - so the assumption behind it was wrong, and silently rewriting a user's `.desktop` file is worse than leaving it.
- **`Fetch`** has `get_text` and `download` rather than one method with a destination enum; the two transfers want different error handling.
- **`UpdateEvent`** gained `Unreadable` alongside `Failed`: a release we cannot parse is not a network problem and must not read as "up to date".
- **Background failures are silent** (logged only); only a check the user asked for reports a failure.
- **`CRT_UPDATE_URL`** (debug builds, or release builds with the `crt-update/dev-update-url` feature) points the updater at a local release so the whole download-verify-swap path can be exercised over curl's `file://` support. A shipped build ignores it.

**Bugs this work found in its own design:** `with_extension("old")` would have turned `crt.app` into `crt.old`, making the documented recovery path a directory macOS will not launch. Fixed and pinned by a test before it could ship.

**Verified end to end on macOS** with an optimised binary and a local release: check, install, asset refresh keeping an edited theme, `.old` kept, first launch removing it and recording the version. The failure paths were verified against the real network too (no release carries `SHA256SUMS` yet, so the 404 path is exercised for real, and `check = false` makes no request at all).

**Left for a human before this ships:**

1. **The first release with `SHA256SUMS` closes the last gap.** No published release has the file yet, so the "an update is available" toast and the retitled menu entry have never run against a real GitHub release. Everything up to that point is covered.
2. **Manual macOS checks**: launching a `release.sh` bundle through Finder/LaunchServices after an update, and the non-admin `/Applications` case (covered by a read-only-directory test, not by a real account).
3. **Manual Linux check**: a real `install.sh` install updating itself. Same code path as the verified macOS single-binary case.
4. The initiative is left in `decompose` for review rather than transitioned.
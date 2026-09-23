# Updating CRT

CRT can tell you when a new release exists and, for installs it owns,
install it for you. Nothing is ever installed without you asking.

## What happens on launch

Once the first frame is on screen, CRT asks GitHub what the latest release
is. If it is newer than what you are running, you get a toast naming the
version, and the update entry in the menu changes to **Update to vX.Y.Z…**.

The check runs at most once a day, and a version is announced once. The
menu entry stays until the update is installed, so a notice you missed is
never lost.

```toml
[updates]
# Check on launch. With false, CRT makes no network requests of its own;
# "Check for Updates" in the menu still works, because that is you asking.
check = true

# Minimum hours between automatic checks (minimum 1).
interval_hours = 24
```

### What is sent

One HTTPS request to `github.com`, for the release's checksum file, with
`crt/<version>` as the user agent. Nothing else: no identifiers, no
telemetry, no request at all when `check = false`.

## Installing an update

Pick **Update to vX.Y.Z…** from the menu (the app menu on macOS, the
right-click menu elsewhere), or run:

```sh
crt update            # install the latest release
crt update --check    # only report what is available
```

The exit codes suit scripts: `0` did something, `2` nothing to do, `1`
failed.

CRT downloads the release, checks it against the checksum the release
publishes, unpacks it beside the current install, and swaps it in with a
rename. **Your shells keep running** — the update does not restart
anything. It takes effect the next time you open CRT.

Bundled themes and fonts are refreshed at the same time, because a release
usually brings new ones. A bundled file you have edited yourself is kept,
not overwritten, and CRT tells you which ones it kept. Your `config.toml`
is never touched; the bundled one lands beside it as `default_config.toml`.

## Installs CRT will not touch

If CRT was installed by something else, it tells you and stops there:

| Install | What you get |
|---|---|
| `install.sh` (binary or `crt.app`) | Updated in place |
| AUR / pacman | `yay -Syu crt` |
| `cargo install` | `cargo install crt` |
| Homebrew | `brew upgrade crt` |
| Built from source | `git pull && cargo build --release` |

`crt --version` prints which of these you have:

```
crt 0.1.6 (app bundle)
```

## If an update goes wrong

The previous version is kept next to the new one until the new one has
started once. If the new version will not start, put the old one back by
hand:

```sh
# macOS app bundle
rm -rf /Applications/crt.app && mv /Applications/crt.app.old /Applications/crt.app

# binary install
rm ~/.local/bin/crt && mv ~/.local/bin/crt.old ~/.local/bin/crt
```

There is no `--rollback` flag on purpose: it would live in the new binary,
which is the thing that is not starting.

Everything else fails safely. A download that does not match its checksum
is discarded, a half-finished update leaves the old version in place, and
an install directory CRT cannot write to (a `/Applications` owned by
another admin, say) is reported with the path so you can re-run the install
script instead.

## What the updater trusts

Downloads are fetched over HTTPS from `github.com` and checked against the
`SHA256SUMS` file published with each release. That file is what tells CRT
both which version is current and what the download should hash to.

Releases are not yet code-signed or notarised, so the trust anchor today is
TLS to github.com. Signing is tracked separately; when it lands, this page
will say so.

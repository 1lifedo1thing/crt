//! Fetching over HTTPS, by way of `curl`.
//!
//! The updater needs exactly two transfers: a few hundred bytes of
//! `SHA256SUMS`, and one tarball. Linking a TLS stack for that would add
//! roughly a megabyte and a second certificate store that knows nothing about
//! a corporate proxy or an internal root. `curl` is already required by
//! `scripts/install.sh`, ships with macOS, and honours `HTTPS_PROXY`,
//! `CURL_CA_BUNDLE` and the system trust store.
//!
//! Everything goes through the [`Fetch`] trait so tests can serve bytes from
//! memory and never touch the network.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FetchError {
    #[error("curl is not installed, so {0} cannot be checked")]
    CurlMissing(&'static str),

    #[error("no network connection")]
    Offline,

    #[error("the server took too long to respond")]
    Timeout,

    #[error("the server returned an error for {url}")]
    Http { url: String },

    #[error("download failed: {message}")]
    Transport { message: String },

    #[error("could not write {path}: {message}")]
    Io { path: String, message: String },
}

impl FetchError {
    /// Whether retrying later might work. Used to decide how loudly to
    /// complain: a laptop with the lid shut is not an error worth a red toast.
    pub fn is_transient(&self) -> bool {
        matches!(self, FetchError::Offline | FetchError::Timeout)
    }
}

pub trait Fetch {
    /// Retrieve a small document into memory.
    fn get_text(&self, url: &str, timeout: Duration) -> Result<String, FetchError>;

    /// Download to `dest`. Implementations write to a temporary sibling and
    /// rename, so `dest` never exists in a half-written state.
    fn download(&self, url: &str, dest: &Path, timeout: Duration) -> Result<(), FetchError>;
}

/// [`Fetch`] backed by the `curl` binary.
pub struct CurlFetch {
    user_agent: String,
}

impl CurlFetch {
    pub fn new(version: &str) -> Self {
        Self {
            user_agent: format!("crt/{version}"),
        }
    }

    /// Arguments common to both transfers.
    ///
    /// `-f` turns an HTTP error status into a non-zero exit instead of a
    /// body full of HTML; `-L` follows the redirect from
    /// `releases/latest/download/…` to the tagged asset.
    fn base_args(&self, timeout: Duration) -> Vec<String> {
        vec![
            "-fsSL".to_string(),
            "--connect-timeout".to_string(),
            "5".to_string(),
            "--max-time".to_string(),
            timeout.as_secs().max(1).to_string(),
            "-A".to_string(),
            self.user_agent.clone(),
        ]
    }

    fn run(&self, args: Vec<String>, url: &str, what: &'static str) -> Result<Vec<u8>, FetchError> {
        let output = Command::new("curl").args(&args).arg(url).output();

        let output = match output {
            Ok(output) => output,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(FetchError::CurlMissing(what));
            }
            Err(e) => {
                return Err(FetchError::Transport {
                    message: e.to_string(),
                });
            }
        };

        if output.status.success() {
            return Ok(output.stdout);
        }

        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(map_curl_exit(output.status.code(), url, stderr))
    }
}

/// Translate curl's exit code into something worth showing a user.
fn map_curl_exit(code: Option<i32>, url: &str, stderr: String) -> FetchError {
    match code {
        // 6: could not resolve host, 7: could not connect
        Some(6) | Some(7) => FetchError::Offline,
        // 28: operation timed out
        Some(28) => FetchError::Timeout,
        // 22: HTTP status >= 400, because of -f
        Some(22) => FetchError::Http {
            url: url.to_string(),
        },
        _ => FetchError::Transport {
            message: if stderr.is_empty() {
                format!("curl exited with {code:?}")
            } else {
                stderr
            },
        },
    }
}

impl Fetch for CurlFetch {
    fn get_text(&self, url: &str, timeout: Duration) -> Result<String, FetchError> {
        let bytes = self.run(self.base_args(timeout), url, "for updates")?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn download(&self, url: &str, dest: &Path, timeout: Duration) -> Result<(), FetchError> {
        // Download beside the destination and rename, so an interrupted
        // transfer can never be mistaken for a complete file.
        let part = dest.with_extension("part");
        if let Some(parent) = part.parent() {
            std::fs::create_dir_all(parent).map_err(|e| FetchError::Io {
                path: parent.display().to_string(),
                message: e.to_string(),
            })?;
        }
        let _ = std::fs::remove_file(&part);

        let mut args = self.base_args(timeout);
        args.push("-o".to_string());
        args.push(part.display().to_string());

        match self.run(args, url, "the download") {
            Ok(_) => {}
            Err(e) => {
                let _ = std::fs::remove_file(&part);
                return Err(e);
            }
        }

        std::fs::rename(&part, dest).map_err(|e| {
            let _ = std::fs::remove_file(&part);
            FetchError::Io {
                path: dest.display().to_string(),
                message: e.to_string(),
            }
        })
    }
}

/// [`Fetch`] that serves canned responses. Used by tests so nothing in the
/// update path needs a network or a real release.
pub struct MemoryFetch {
    responses: Vec<(String, Result<Vec<u8>, FetchError>)>,
}

impl MemoryFetch {
    pub fn new() -> Self {
        Self {
            responses: Vec::new(),
        }
    }

    /// Serve `body` for any URL containing `url_fragment`.
    pub fn serving(mut self, url_fragment: &str, body: impl Into<Vec<u8>>) -> Self {
        self.responses
            .push((url_fragment.to_string(), Ok(body.into())));
        self
    }

    /// Fail with `error` for any URL containing `url_fragment`.
    pub fn failing(mut self, url_fragment: &str, error: FetchError) -> Self {
        self.responses.push((url_fragment.to_string(), Err(error)));
        self
    }

    fn lookup(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        self.responses
            .iter()
            .find(|(fragment, _)| url.contains(fragment.as_str()))
            .map(|(_, response)| response.clone())
            .unwrap_or_else(|| {
                Err(FetchError::Http {
                    url: url.to_string(),
                })
            })
    }
}

impl Default for MemoryFetch {
    fn default() -> Self {
        Self::new()
    }
}

impl Fetch for MemoryFetch {
    fn get_text(&self, url: &str, _timeout: Duration) -> Result<String, FetchError> {
        let bytes = self.lookup(url)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn download(&self, url: &str, dest: &Path, _timeout: Duration) -> Result<(), FetchError> {
        let bytes = self.lookup(url)?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| FetchError::Io {
                path: parent.display().to_string(),
                message: e.to_string(),
            })?;
        }
        std::fs::write(dest, bytes).map_err(|e| FetchError::Io {
            path: dest.display().to_string(),
            message: e.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Duration = Duration::from_secs(5);

    #[test]
    fn memory_fetch_serves_and_fails_by_url_fragment() {
        let fetch = MemoryFetch::new()
            .serving("SHA256SUMS", "sums here")
            .failing("tar.gz", FetchError::Offline);

        assert_eq!(
            fetch
                .get_text("https://example.invalid/SHA256SUMS", T)
                .unwrap(),
            "sums here"
        );
        assert_eq!(
            fetch.get_text("https://example.invalid/crt.tar.gz", T),
            Err(FetchError::Offline)
        );
        // Anything not set up reads as a server error, not a silent success.
        assert!(matches!(
            fetch.get_text("https://example.invalid/other", T),
            Err(FetchError::Http { .. })
        ));
    }

    #[test]
    fn memory_fetch_downloads_to_a_file() {
        let dir = std::env::temp_dir().join(format!("crt-fetch-test-{}", std::process::id()));
        let dest = dir.join("asset.tar.gz");
        let fetch = MemoryFetch::new().serving("asset", b"payload".to_vec());

        fetch
            .download("https://example.invalid/asset.tar.gz", &dest, T)
            .expect("downloads");
        assert_eq!(std::fs::read(&dest).unwrap(), b"payload");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn curl_exit_codes_map_to_useful_errors() {
        let url = "https://example.invalid/x";
        assert_eq!(
            map_curl_exit(Some(6), url, String::new()),
            FetchError::Offline
        );
        assert_eq!(
            map_curl_exit(Some(7), url, String::new()),
            FetchError::Offline
        );
        assert_eq!(
            map_curl_exit(Some(28), url, String::new()),
            FetchError::Timeout
        );
        assert_eq!(
            map_curl_exit(Some(22), url, String::new()),
            FetchError::Http {
                url: url.to_string()
            }
        );
        // Anything else keeps curl's own words, which beats inventing some.
        assert_eq!(
            map_curl_exit(Some(60), url, "certificate problem".to_string()),
            FetchError::Transport {
                message: "certificate problem".to_string()
            }
        );
        assert!(matches!(
            map_curl_exit(None, url, String::new()),
            FetchError::Transport { .. }
        ));
    }

    #[test]
    fn only_network_failures_are_transient() {
        assert!(FetchError::Offline.is_transient());
        assert!(FetchError::Timeout.is_transient());
        assert!(!FetchError::Http { url: "u".into() }.is_transient());
        assert!(!FetchError::CurlMissing("for updates").is_transient());
    }

    #[test]
    fn base_args_carry_the_version_and_timeout() {
        let args = CurlFetch::new("0.1.6").base_args(Duration::from_secs(15));
        assert!(args.contains(&"-fsSL".to_string()));
        assert!(args.contains(&"crt/0.1.6".to_string()));
        assert!(args.contains(&"15".to_string()));
        // A sub-second timeout must not round down to "0" (no limit).
        let args = CurlFetch::new("0.1.6").base_args(Duration::from_millis(1));
        assert!(args.contains(&"1".to_string()));
    }
}

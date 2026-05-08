//! Background update checker.
//!
//! Polls GitHub's `releases/latest` for the daemon repo
//! (`core-deck/core-deck`) and the firmware repo (`core-deck/firmware`)
//! once on startup and again every 24 hours. When a tag exceeds the
//! current daemon version (`CARGO_PKG_VERSION`) or the device's
//! reported firmware version, the result lands on a `TrayUpdate` so
//! the tray menu can surface a "Update available" row that opens the
//! release page on click.
//!
//! No autoupdate, no signature verification — Homebrew handles macOS
//! upgrades for the daemon, and the user reflashes the device by hand.
//! This module's job is just "let the user know."
//!
//! Failures (offline, GitHub rate limit, etc.) log at debug and try
//! again on the next tick. There's no opt-out toggle yet; add when
//! someone asks.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::time::interval;
use tracing::{debug, info, warn};

use crate::state::{TrayUpdate, UpdateInfo};
use crate::DaemonState;

const DAEMON_REPO: &str = "core-deck/core-deck";
const FIRMWARE_REPO: &str = "core-deck/firmware";
const POLL_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Wait before the first check so we don't dog-pile GitHub during
/// daemon startup (when other tasks are spawning, hooks installing,
/// the tray icon coming up, etc.).
const INITIAL_DELAY: Duration = Duration::from_secs(60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
struct GithubRelease {
    /// Tag like `v0.2.3` or `0.2.3` — we strip the leading `v` before
    /// comparing.
    tag_name: String,
    html_url: String,
}

pub async fn run_update_checker(state: Arc<DaemonState>) {
    tokio::time::sleep(INITIAL_DELAY).await;

    let mut tick = interval(POLL_INTERVAL);
    // First tick fires immediately — that's our startup check, after
    // INITIAL_DELAY above.
    loop {
        tick.tick().await;
        check_once(&state).await;
    }
}

async fn check_once(state: &Arc<DaemonState>) {
    let client = match build_client() {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "update check: failed to build HTTP client");
            return;
        }
    };

    // Daemon side: compare release tag to the version baked into our
    // binary at compile time.
    let daemon_current = env!("CARGO_PKG_VERSION");
    let daemon = match fetch_latest_release(&client, DAEMON_REPO).await {
        Some(r) if is_newer(&r.tag_name, daemon_current) => {
            info!(
                latest = %r.tag_name,
                current = daemon_current,
                "daemon update available",
            );
            Some(UpdateInfo {
                latest_version: clean_version(&r.tag_name).to_string(),
                html_url: r.html_url,
            })
        }
        _ => None,
    };

    // Firmware side: skip when no device is connected or when the
    // firmware never reported a parseable version. The device's
    // GetVersion HID command returns a string that the daemon stashes
    // on `device_status.firmware_version`.
    let firmware_current = state.device_status.read().await.firmware_version.clone();
    let firmware = match firmware_current {
        Some(current) => match fetch_latest_release(&client, FIRMWARE_REPO).await {
            Some(r) if is_newer(&r.tag_name, &current) => {
                info!(
                    latest = %r.tag_name,
                    current = %current,
                    "firmware update available",
                );
                Some(UpdateInfo {
                    latest_version: clean_version(&r.tag_name).to_string(),
                    html_url: r.html_url,
                })
            }
            _ => None,
        },
        None => {
            debug!("update check: no firmware version yet, skipping firmware repo");
            None
        }
    };

    state.send_tray_update(TrayUpdate::UpdatesAvailable { daemon, firmware });
}

fn build_client() -> Result<reqwest::Client, reqwest::Error> {
    let user_agent = format!("coredeck-daemon/{}", env!("CARGO_PKG_VERSION"));
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent(user_agent)
        .build()
}

async fn fetch_latest_release(client: &reqwest::Client, repo: &str) -> Option<GithubRelease> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    match client.get(&url).send().await {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                // 404 is normal when the repo has no releases yet
                // (e.g. a fresh firmware fork before any tag). Log
                // at debug so the daemon log doesn't grow scary.
                debug!(repo, %status, "release fetch returned non-success");
                return None;
            }
            match resp.json::<GithubRelease>().await {
                Ok(r) => Some(r),
                Err(e) => {
                    warn!(repo, error = %e, "release JSON parse failed");
                    None
                }
            }
        }
        Err(e) => {
            // Offline / DNS failures land here — debug, not warn.
            debug!(repo, error = %e, "release fetch failed");
            None
        }
    }
}

/// Strip a leading `v` so `v0.2.3` and `0.2.3` are equivalent inputs
/// to the semver parser.
fn clean_version(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

/// True when `latest_tag` parses as a higher semver than `current`.
/// Returns false on any parse failure — we'd rather be silent than
/// nag the user about a "fix.broken.tag" or pre-release garbage.
fn is_newer(latest_tag: &str, current: &str) -> bool {
    let latest = match semver::Version::parse(clean_version(latest_tag)) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let current = match semver::Version::parse(clean_version(current)) {
        Ok(v) => v,
        Err(_) => return false,
    };
    latest > current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_strips_v() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.2.0", "v0.1.0"));
    }

    #[test]
    fn same_version_not_newer() {
        assert!(!is_newer("v0.1.0", "0.1.0"));
    }

    #[test]
    fn older_not_newer() {
        assert!(!is_newer("v0.1.0", "0.2.0"));
    }

    #[test]
    fn unparseable_returns_false() {
        assert!(!is_newer("garbage", "0.1.0"));
        assert!(!is_newer("0.1.0", "garbage"));
    }

    #[test]
    fn patch_and_minor() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("0.2.0", "0.1.99"));
    }
}

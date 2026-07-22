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

/// Latest release seen for one repo, kept so the tray rows can be
/// re-evaluated without another network round-trip.
#[derive(Debug, Clone)]
pub struct CachedRelease {
    pub tag: String,
    pub html_url: String,
}

/// Last successfully fetched releases. Lives on `DaemonState` so the
/// device-connect path can re-run the comparison the moment the
/// firmware reports its version — otherwise a stale "Update available:
/// firmware vX" row sticks around for up to 24h after the user
/// reflashes (the row was computed against the pre-flash version and
/// nothing revisited it until the next poll tick).
#[derive(Debug, Default)]
pub struct UpdateCache {
    pub daemon: Option<CachedRelease>,
    pub firmware: Option<CachedRelease>,
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

    // Fetch both repos and stash whatever succeeded. A failed fetch
    // (offline, rate limit) keeps the previous cache entry rather than
    // wiping it — stale-but-real beats empty.
    if let Some(r) = fetch_latest_release(&client, DAEMON_REPO).await {
        state.update_cache.lock().await.daemon = Some(CachedRelease {
            tag: r.tag_name,
            html_url: r.html_url,
        });
    }
    if let Some(r) = fetch_latest_release(&client, FIRMWARE_REPO).await {
        state.update_cache.lock().await.firmware = Some(CachedRelease {
            tag: r.tag_name,
            html_url: r.html_url,
        });
    }

    reevaluate(state).await;
}

/// Recompute both "Update available" tray rows from the cached release
/// tags and the *current* versions, and push the result. No network.
///
/// Called from `check_once` after each poll, and from the HID
/// connect/disconnect paths so the firmware row tracks the version the
/// device is reporting right now (clearing immediately after a reflash
/// instead of lingering until the next 24h tick).
pub async fn reevaluate(state: &Arc<DaemonState>) {
    let firmware_current = state.device_status.read().await.firmware_version.clone();

    let (daemon, firmware) = {
        let cache = state.update_cache.lock().await;
        compute_updates(
            &cache,
            env!("CARGO_PKG_VERSION"),
            firmware_current.as_deref(),
        )
    };

    if let Some(ref d) = daemon {
        info!(latest = %d.latest_version, current = env!("CARGO_PKG_VERSION"), "daemon update available");
    }
    match (&firmware, &firmware_current) {
        (Some(f), Some(cur)) => {
            info!(latest = %f.latest_version, current = %cur, "firmware update available")
        }
        (None, None) => debug!("update check: no firmware version yet, skipping firmware row"),
        _ => {}
    }

    state.send_tray_update(TrayUpdate::UpdatesAvailable { daemon, firmware });
}

/// Pure comparison core: which tray rows should show, given the cached
/// latest releases and the current daemon/firmware versions.
fn compute_updates(
    cache: &UpdateCache,
    daemon_current: &str,
    firmware_current: Option<&str>,
) -> (Option<UpdateInfo>, Option<UpdateInfo>) {
    let daemon = cache
        .daemon
        .as_ref()
        .filter(|r| is_newer(&r.tag, daemon_current))
        .map(|r| UpdateInfo {
            latest_version: clean_version(&r.tag).to_string(),
            html_url: r.html_url.clone(),
        });

    // No reported firmware version (device disconnected / never
    // reported one) → no firmware row. Matches the documented "row only
    // appears once the device has reported a parseable version".
    let firmware = firmware_current.and_then(|current| {
        cache
            .firmware
            .as_ref()
            .filter(|r| is_newer(&r.tag, current))
            .map(|r| UpdateInfo {
                latest_version: clean_version(&r.tag).to_string(),
                html_url: r.html_url.clone(),
            })
    });

    (daemon, firmware)
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

    fn cache(daemon_tag: Option<&str>, firmware_tag: Option<&str>) -> UpdateCache {
        let rel = |t: &str| CachedRelease {
            tag: t.to_string(),
            html_url: "https://example.invalid/release".to_string(),
        };
        UpdateCache {
            daemon: daemon_tag.map(rel),
            firmware: firmware_tag.map(rel),
        }
    }

    #[test]
    fn firmware_row_clears_when_device_catches_up() {
        // The reflash scenario: cache says latest is v2.2.0; the device
        // now reports 2.2.0 → no row (this was the stale-row bug).
        let c = cache(None, Some("v2.2.0"));
        let (_, fw) = compute_updates(&c, "0.2.0", Some("2.2.0"));
        assert!(fw.is_none());

        // Pre-flash it must still show.
        let (_, fw) = compute_updates(&c, "0.2.0", Some("2.1.0"));
        assert_eq!(fw.unwrap().latest_version, "2.2.0");
    }

    #[test]
    fn no_firmware_version_means_no_row() {
        let c = cache(None, Some("v2.2.0"));
        let (_, fw) = compute_updates(&c, "0.2.0", None);
        assert!(fw.is_none());
    }

    #[test]
    fn daemon_row_from_cache() {
        let c = cache(Some("v0.3.0"), None);
        let (daemon, _) = compute_updates(&c, "0.2.0", None);
        assert_eq!(daemon.unwrap().latest_version, "0.3.0");
        // Same version → no row.
        let (daemon, _) = compute_updates(&c, "0.3.0", None);
        assert!(daemon.is_none());
    }

    #[test]
    fn empty_cache_yields_no_rows() {
        let c = UpdateCache::default();
        let (daemon, fw) = compute_updates(&c, "0.2.0", Some("2.1.0"));
        assert!(daemon.is_none());
        assert!(fw.is_none());
    }
}

//! The live Roblox client version, for the `User-Agent` the launcher checks.
//!
//! `gamejoin` refuses a join with status 12 — "You are not allowed to play this
//! game", or "Game 211" — when the `User-Agent` does not name a client version
//! it recognises, so the version in it has to track the real deployment rather
//! than a pinned constant. Rove's own client learned this the hard way
//! (`client_version.rs`, "*gamejoin rejects unknown agents with status 12*"),
//! and this is that module ported.
//!
//! The version comes from Roblox's own client-settings service, cached for the
//! process and refreshed when a join is refused. Everything here is
//! best-effort: any failure falls back to the compiled-in version, so a join
//! never depends on the fetch succeeding, and an Android phone that cannot
//! reach `clientsettings.roblox.com` still joins with the fallback.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

/// The platform the `User-Agent` claims. This client speaks the desktop
/// client's wire protocol, and Windows is the reference deployment of it.
const PLATFORM: &str = "WindowsPlayer";
const ENDPOINT_HOST: &str = "clientsettings.roblox.com";

/// Used until a live fetch succeeds, and on every later failure. Rove's value
/// at the time this was ported; a mismatch is exactly what the retry below is
/// for, and a stale version still admits a join on most days.
pub const FALLBACK_VERSION: &str = "0.748.0.7481526";

/// How long a fetched version stays good before another attempt.
const TTL: Duration = Duration::from_secs(6 * 60 * 60);
/// A failed fetch is retried sooner than a good one expires.
const FAILURE_TTL: Duration = Duration::from_secs(60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(6);

struct Cached {
    version: String,
    fetched: Instant,
    ok: bool,
}

fn cache() -> &'static Mutex<Option<Cached>> {
    static CACHE: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// A plausible `MAJOR.MINOR.PATCH.BUILD` version, bounded.
pub fn valid_version(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 32
        && text.split('.').count() == 4
        && text
            .split('.')
            .all(|part| !part.is_empty() && part.len() <= 9 && part.bytes().all(|b| b.is_ascii_digit()))
}

/// The `User-Agent` a desktop client sends.
pub fn user_agent() -> String {
    format!(
        "Roblox/WinInet RobloxApp/{} ({})",
        current_version(),
        PLATFORM
    )
}

/// The current best-known version: live if fetched, else the fallback.
pub fn current_version() -> String {
    let fresh = {
        let guard = cache().lock().unwrap_or_else(|e| e.into_inner());
        guard
            .as_ref()
            .filter(|entry| entry.fetched.elapsed() < if entry.ok { TTL } else { FAILURE_TTL })
            .map(|entry| entry.version.clone())
    };
    if let Some(version) = fresh {
        return version;
    }
    // Refreshing here keeps `user_agent()` synchronous at the call site.
    let fetched = fetch();
    let version = fetched.clone().unwrap_or_else(|| FALLBACK_VERSION.to_owned());
    let mut guard = cache().lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(Cached {
        version: version.clone(),
        fetched: Instant::now(),
        ok: fetched.is_some(),
    });
    version
}

/// Drop the cached version, so the next call refetches. A refused join calls
/// this once: the deployment may have moved since the version was read.
pub fn invalidate() {
    *cache().lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Seed the cache directly (tests, or a version learned elsewhere).
pub fn seed(version: &str) {
    if !valid_version(version) {
        return;
    }
    *cache().lock().unwrap_or_else(|e| e.into_inner()) = Some(Cached {
        version: version.to_owned(),
        fetched: Instant::now(),
        ok: true,
    });
}

fn fetch() -> Option<String> {
    let url = format!("https://{ENDPOINT_HOST}/v2/client-version/{PLATFORM}");
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(FETCH_TIMEOUT))
        .build();
    let agent: ureq::Agent = config.into();
    let mut response = agent.get(&url).call().ok()?;
    if response.status().as_u16() != 200 {
        return None;
    }
    let body: Value = response.body_mut().read_json().ok()?;
    let version = body.get("version").and_then(Value::as_str)?.to_owned();
    valid_version(&version).then_some(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_shape_is_strict() {
        assert!(valid_version("0.748.0.7481526"));
        assert!(!valid_version("0.748"));
        assert!(!valid_version("0.748.0."));
        assert!(!valid_version(""));
        assert!(!valid_version("0.748.0.7481526\n"));
        assert!(!valid_version(&"1".repeat(40)));
        assert!(!valid_version("0.748.x.1"));
    }

    #[test]
    fn seed_overrides_and_the_user_agent_names_the_version() {
        seed("0.999.0.9990001");
        assert_eq!(
            user_agent(),
            "Roblox/WinInet RobloxApp/0.999.0.9990001 (WindowsPlayer)"
        );
    }
}

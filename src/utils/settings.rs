//! Admin-managed runtime settings, and the cache in front of them.
//!
//! One setting pair today — the NFC face-verification gate — read on the hot
//! path by `routes::nfc_card` and written by `routes::settings`.
//!
//! WHY THERE IS A CACHE AT ALL
//! These used to be environment variables, which cost nothing to read. Moving
//! them into the database is what makes them changeable without a redeploy,
//! and it would also put a query on every card save. The cache buys that back;
//! the TTL and the explicit invalidation are what keep it from becoming a
//! different kind of "needs a restart".
//!
//! WHAT THE CACHE GUARANTEES, and what it does not:
//!
//!   * In THIS process, a write through `PUT /admin-api/settings/...` calls
//!     [`invalidate`], so the very next request reads the new value. No stale
//!     read at all.
//!   * Across OTHER processes — a second instance behind the proxy, or
//!     duerp-api if it ever reads these — the TTL bounds it. Worst case is
//!     [`TTL`] seconds of staleness, not a redeploy.
//!
//! That trade is deliberate: a shared cache (Redis) or `LISTEN/NOTIFY` would
//! make cross-process invalidation immediate, and neither exists in this
//! service today. If a second instance is ever deployed, this is the comment
//! to come back to.

use std::sync::{LazyLock, RwLock};
use std::time::{Duration, Instant};

use sqlx::PgPool;

/// How long a cached read may be reused.
///
/// Short enough that an operator flipping the switch on another instance sees
/// it take effect while they are still looking at the screen; long enough that
/// a busy card desk is not one query per save.
pub const TTL: Duration = Duration::from_secs(30);

/// The face gate's configuration, as the handler wants it.
#[derive(Clone, Debug, PartialEq)]
pub struct FaceVerify {
    /// Should a save compare the card photo with the selfie at all?
    pub enabled: bool,
    /// Where to send the pair. `None` when nothing is configured anywhere.
    pub url: Option<String>,
    /// Did this come from the settings table, or from the environment?
    /// Recorded in the step log, because "the admin turned it off" and "nobody
    /// ever configured it" look identical in the response otherwise.
    pub managed: bool,
}

static CACHE: LazyLock<RwLock<Option<(Instant, FaceVerify)>>> =
    LazyLock::new(|| RwLock::new(None));

/// Drop the cached value. Called by the settings API after every successful
/// write, so the next read in this process is fresh.
pub fn invalidate() {
    if let Ok(mut guard) = CACHE.write() {
        *guard = None;
    }
}

/// `NFC_FACE_VERIFY_URL` from the environment — the fallback, and what this
/// service used exclusively before the settings table existed.
fn env_url() -> Option<String> {
    std::env::var("NFC_FACE_VERIFY_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Turn one row-pair into the effective configuration.
///
/// THE RULE THAT PREVENTS A SILENT REGRESSION: the seeded row says `OFF`, and
/// `OFF` means saves proceed without any face check. If that seed were taken
/// at face value, applying the migration would quietly disable the gate on
/// every deployment that had it working through `.env`.
///
/// So the table only decides once an admin has actually set it — which is what
/// `updated_by` records. Until then the old rule applies: a URL in the
/// environment means verify, no URL means refuse (fail closed).
pub fn resolve(row_value: Option<&str>, row_url: Option<&str>, managed: bool) -> FaceVerify {
    resolve_with(row_value, row_url, managed, env_url())
}

/// [`resolve`], with the environment passed in rather than read.
///
/// Split out so the rule can be tested without depending on what
/// `NFC_FACE_VERIFY_URL` happens to be in the shell that ran `cargo test` —
/// which is exactly the kind of test that passes on a laptop and fails in CI,
/// or the other way round.
fn resolve_with(
    row_value: Option<&str>,
    row_url: Option<&str>,
    managed: bool,
    env_url: Option<String>,
) -> FaceVerify {
    let stored_url = row_url
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let url = stored_url.or(env_url);

    let enabled = if managed {
        row_value
            .map(|v| v.trim().eq_ignore_ascii_case("ON"))
            .unwrap_or(false)
    } else {
        // Pre-settings behaviour, preserved exactly: configured means on.
        url.is_some()
    };

    FaceVerify { enabled, url, managed }
}

/// The current configuration, from cache when it is fresh.
///
/// Falls back to the environment on a database error rather than failing the
/// request: the caller is in the middle of a card save, and this function
/// deciding "no answer" would turn a database blip into a refused save. The
/// gate's own fail-closed rule still applies to what comes back.
pub async fn face_verify(db: &PgPool) -> FaceVerify {
    if let Ok(guard) = CACHE.read() {
        if let Some((at, cached)) = guard.as_ref() {
            if at.elapsed() < TTL {
                return cached.clone();
            }
        }
    }

    let row = sqlx::query_as::<_, (Option<String>, Option<String>, bool)>(
        "SELECT (SELECT value FROM attendance.system_settings WHERE key = 'NFC_FACE_VERIFY'),
                (SELECT value FROM attendance.system_settings WHERE key = 'NFC_FACE_VERIFY_URL'),
                EXISTS (SELECT 1 FROM attendance.system_settings
                         WHERE key = 'NFC_FACE_VERIFY' AND updated_by IS NOT NULL)",
    )
    .fetch_one(db)
    .await;

    let settings = match row {
        Ok((value, url, managed)) => resolve(value.as_deref(), url.as_deref(), managed),
        Err(e) => {
            eprintln!("settings: falling back to the environment ({e})");
            resolve(None, None, false)
        }
    };

    if let Ok(mut guard) = CACHE.write() {
        *guard = Some((Instant::now(), settings.clone()));
    }
    settings
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENV: &str = "http://10.224.224.101:8089/verify";

    #[test]
    fn an_unmanaged_setting_keeps_the_old_environment_behaviour() {
        // The seeded row says OFF. Taking that at face value would disable the
        // face gate on every deployment that had it working through `.env` —
        // silently, on deploy day. `updated_by IS NULL` means "nobody has
        // decided", so the environment still decides.
        let configured = resolve_with(Some("OFF"), Some(""), false, Some(ENV.into()));
        assert!(configured.enabled, "a URL in the environment still means verify");
        assert_eq!(configured.url.as_deref(), Some(ENV));
        assert!(!configured.managed);

        // And with nothing configured anywhere, the old fail-closed state.
        let nothing = resolve_with(Some("OFF"), Some(""), false, None);
        assert_eq!(nothing.url, None);
        assert!(!nothing.enabled);
    }

    #[test]
    fn a_managed_setting_is_obeyed_even_with_a_url_present() {
        // The whole point of the toggle: an admin can turn the gate off while
        // leaving the endpoint configured, and turn it back on without
        // re-entering it.
        let off = resolve_with(Some("OFF"), Some("https://face.du.ac.bd/verify"), true, Some(ENV.into()));
        assert!(!off.enabled);
        assert_eq!(off.url.as_deref(), Some("https://face.du.ac.bd/verify"));

        let on = resolve_with(Some("ON"), Some("https://face.du.ac.bd/verify"), true, None);
        assert!(on.enabled);
        assert!(on.managed);
    }

    #[test]
    fn a_managed_setting_reads_on_case_insensitively_and_nothing_else() {
        let url = Some("https://x.du.ac.bd");
        assert!(resolve_with(Some("on"), url, true, None).enabled);
        assert!(resolve_with(Some(" ON "), url, true, None).enabled);
        // Anything that is not ON is off. A garbled value must not enable a
        // gate — or disable one — by accident; it reads as OFF, which verifies
        // nothing, and the API cannot store it anyway.
        assert!(!resolve_with(Some("enabled"), url, true, None).enabled);
        assert!(!resolve_with(None, url, true, None).enabled);
    }

    #[test]
    fn the_stored_url_wins_over_the_environment() {
        // Otherwise an operator would change the URL on the screen, see it
        // saved, and watch the service keep calling the old one.
        let s = resolve_with(Some("ON"), Some("https://stored.du.ac.bd/verify"), true, Some(ENV.into()));
        assert_eq!(s.url.as_deref(), Some("https://stored.du.ac.bd/verify"));
    }

    #[test]
    fn invalidate_is_safe_to_call_when_nothing_is_cached() {
        invalidate();
        invalidate();
    }
}

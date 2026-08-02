//! Launch-time self-update: keep OVM itself current the same way products are,
//! but with the atomic-swap safety the direct self-updater already provides.
//!
//! Policy (`self.autoUpdate`, default `on`) drives three behaviors:
//!   - **on** — a background refresh stages the newer release (download +
//!     verify + immutable install) without touching the active version; the
//!     next invocation activates it atomically via [`activate_pending_on_startup`]
//!     and prints a single `↑ OVM <new> (was <old>)` line. The staging never runs
//!     on the launch foreground, so the hot path stays network-free.
//!   - **notify** — [`maybe_notify_self_on_launch`] reads the cached latest and,
//!     if newer, prompts (interactive) or prints one deduplicated notice.
//!   - **off** — nothing happens on launch.
//!
//! A dev snapshot (`dev-<hash>`) is always exempt: those installs are
//! developer-controlled. Every step is fail-open — a failed check, download, or
//! activation must never break or delay a launch.

use crate::autoupdate::{self, NotifyChoice, UpdateAction};
use crate::config::{AutoUpdatePolicy, OvmConfig, OvmDirs, SelfChannel};
use crate::error::Result;
use crate::self_manager::{is_self_management_command, SelfManager, SELF_CHILD_ENV};
use crate::update_cache::now_secs;
use console::style;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::self_update::{self, resolved_latest_version, SelfUpdateChannel};

/// Set on child processes that must never attempt a pending activation — namely
/// the activation probe, which runs the freshly swapped control plane while the
/// self operation lock is held, and the detached background refresh, which
/// stages but must leave activation to a user-facing foreground invocation.
pub const SKIP_SELF_AUTOUPDATE_ENV: &str = "OVM_SKIP_SELF_AUTOUPDATE";

/// Subject key for the shared notify snooze cache.
const SELF_SUBJECT: &str = "self";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingSelfUpdate {
    version: String,
}

/// Base wait before retrying a failed staging attempt, doubled per consecutive
/// failure up to [`BACKOFF_MAX_SHIFT`] and capped at the check interval.
const BACKOFF_BASE_SECS: u64 = 5 * 60;
/// Bounds the doubling so the shift can never overflow.
const BACKOFF_MAX_SHIFT: u32 = 16;

/// The last failed staging attempt, so a release that can never be staged is
/// retried on a decaying schedule instead of on every invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StagingAttempt {
    version: String,
    last_attempt_at: u64,
    failures: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SelfLatestCache {
    version: String,
    channel: String,
    fetched_at: u64,
}

fn pending_path(base: &Path) -> PathBuf {
    base.join("self").join("pending-update.json")
}

fn staging_attempt_path(base: &Path) -> PathBuf {
    base.join("self").join("staging-attempt.json")
}

fn latest_cache_path(base: &Path) -> PathBuf {
    base.join("cache").join("self-update").join("latest.json")
}

fn is_dev_snapshot(version: &str) -> bool {
    version.starts_with("dev-")
}

/// Strictly-newer comparison over release identifiers. Non-semver identifiers
/// (dev snapshots) never compare as newer.
fn semver_newer(candidate: &str, current: &str) -> bool {
    match (Version::parse(candidate), Version::parse(current)) {
        (Ok(candidate), Ok(current)) => candidate > current,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Startup activation (foreground, every invocation)
// ---------------------------------------------------------------------------

/// Activate a self-update staged by an earlier launch. Runs at the very start
/// of every invocation but is a single stat in the common (no-pending) case.
/// Entirely fail-open: any error leaves the active version untouched.
pub fn activate_pending_on_startup(args: &[String]) {
    if std::env::var_os(SKIP_SELF_AUTOUPDATE_ENV).is_some() {
        return;
    }
    // A self-managed child is the exec'd versioned binary, not the control
    // plane; it must not re-activate.
    if std::env::var_os(SELF_CHILD_ENV).is_some() {
        return;
    }
    // Never interpose on the user's own `ovm self …` commands.
    if is_self_management_command(args.get(1).map(String::as_str)) {
        return;
    }
    let Ok(dirs) = OvmDirs::new() else {
        return;
    };
    // Cheap gate: no staged update means nothing to do.
    if !pending_path(&dirs.base).exists() {
        return;
    }
    let _ = try_activate_pending(&dirs);
}

fn try_activate_pending(dirs: &OvmDirs) -> Result<()> {
    let path = pending_path(&dirs.base);
    let Some(pending) = read_pending(&path)? else {
        return Ok(());
    };
    let manager = SelfManager::at(dirs.clone());

    // Only the standalone control plane owns the swap. When invoked any other
    // way (cargo/brew/dev binary) there is no control plane to refresh.
    let is_control_plane = std::env::current_exe()
        .map(|exe| manager.is_control_plane_executable(&exe))
        .unwrap_or(false);
    if !is_control_plane {
        return Ok(());
    }

    let current = manager.current_version()?;
    let stale = match current.as_deref() {
        None => true,
        Some(current) => {
            is_dev_snapshot(current)
                || current == pending.version
                || !manager.is_complete(&pending.version)
                || !semver_newer(&pending.version, current)
        }
    };
    if stale {
        // The staged version no longer applies (already active, superseded, a
        // dev snapshot took over, or the bundle vanished). Drop the marker so we
        // don't reconsider it every launch.
        let _ = clear_pending(&path);
        return Ok(());
    }
    let old = current.expect("checked above");

    let operation = manager.acquire_operation_lock()?;
    let result = self_update::activate_release(&manager, &pending.version);
    if result.is_ok() {
        self_update::direct::apply_retention(&manager, &operation);
    }
    drop(operation);
    // Clear the marker regardless: a success is applied, and a failure already
    // rolled back — retrying it every launch would just delay them.
    let _ = clear_pending(&path);
    match result {
        Ok(()) => {
            clear_staging_failure(&dirs.base);
            eprintln!(
                "{} OVM {} (was {})",
                style("↑").green(),
                pending.version,
                old
            );
        }
        Err(_) => {
            // A release can install cleanly and still fail its activation
            // probe. Clearing the marker alone would leave nothing staged, so
            // the next invocation restages and re-activates the same broken
            // release — repeating a foreground activate-and-roll-back plus a
            // download on every command. Count it as a failed attempt so the
            // same backoff applies.
            let _ = record_staging_attempt(&dirs.base, &pending.version);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Background staging (detached refresh)
// ---------------------------------------------------------------------------

/// Whether the launcher should spawn a background refresh for the self channel:
/// the policy allows it and the cached latest check is stale.
pub fn self_check_due(base: &Path, config: &OvmConfig) -> bool {
    if config.self_.auto_update == AutoUpdatePolicy::Off {
        return false;
    }
    match read_latest_cache(base) {
        Some(cache) => {
            if cache.channel != config.self_.channel.label()
                || !cache_is_fresh(&cache, config.update_check_interval)
            {
                return true;
            }
            staging_outstanding(base, config, &cache.version)
        }
        None => true,
    }
}

/// A fresh cache normally silences the background check — but when the cached
/// latest is newer than the active version and no matching update is staged,
/// an earlier staging attempt failed (transient network, killed child). That
/// work must retry on the next invocation, not wait out the check window: the
/// cache lookup is served locally, so the retry costs nothing unless the
/// download itself is still needed.
fn staging_outstanding(base: &Path, config: &OvmConfig, cached_latest: &str) -> bool {
    if config.self_.auto_update != AutoUpdatePolicy::On {
        return false;
    }
    let manager = SelfManager::at(OvmDirs::at(base.to_path_buf()));
    let Ok(Some(current)) = manager.current_version() else {
        return false;
    };
    if is_dev_snapshot(&current) || !semver_newer(cached_latest, &current) {
        return false;
    }
    let staged_matches = match read_pending(&pending_path(base)) {
        Ok(Some(pending)) => pending.version == cached_latest,
        Ok(None) | Err(_) => false,
    };
    if staged_matches {
        return false;
    }
    // Outstanding work, but a release can be *permanently* unstageable — a
    // missing platform asset, an archive that never verifies. Without a
    // backoff every invocation would spawn another child to fail the same way,
    // and since the refresh is now armed from every command that is unbounded
    // churn, not a retry.
    staging_retry_due(base, cached_latest, config.update_check_interval)
}

/// Whether enough time has passed to retry staging `version`.
///
/// The first retry is immediate — that is the transient case this exists for,
/// a download killed mid-flight. Each consecutive failure then doubles the
/// wait, capped at the normal check interval, so a release that can never be
/// staged costs no more than the ordinary polling cadence.
fn staging_retry_due(base: &Path, version: &str, interval_hours: u64) -> bool {
    let Some(attempt) = read_staging_attempt(base) else {
        return true;
    };
    if attempt.version != version {
        return true;
    }
    let now = now_secs();
    if attempt.last_attempt_at > now {
        // The clock moved backwards (or the record was written under a skewed
        // clock). A future timestamp would otherwise suppress every retry until
        // real time caught up — days, potentially. Treat it as due; the next
        // attempt re-stamps it under the current clock.
        return true;
    }
    let cap = interval_hours.saturating_mul(3600).max(BACKOFF_BASE_SECS);
    let backoff = BACKOFF_BASE_SECS
        .saturating_mul(1u64 << attempt.failures.min(BACKOFF_MAX_SHIFT))
        .min(cap);
    now.saturating_sub(attempt.last_attempt_at) >= backoff
}

/// Record that `version` was attempted, returning whether the record persisted.
///
/// Written *before* the attempt runs, so every way it can fail — a failed
/// download, a marker that cannot be written, a killed process, a release that
/// installs but never activates — leaves evidence. Recording only on a
/// specific error path means the paths it does not cover retry forever.
///
/// The caller must not proceed when this returns `false`: an attempt that
/// cannot be recorded is one the backoff cannot see, and an unrecorded
/// permanent failure is retried by every subsequent invocation.
#[must_use]
fn record_staging_attempt(base: &Path, version: &str) -> bool {
    let failures = match read_staging_attempt(base) {
        Some(previous) if previous.version == version => previous.failures.saturating_add(1),
        _ => 0,
    };
    let attempt = StagingAttempt {
        version: version.to_string(),
        last_attempt_at: now_secs(),
        failures,
    };
    let path = staging_attempt_path(base);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(serialized) = serde_json::to_string(&attempt) else {
        return false;
    };
    std::fs::write(&path, serialized).is_ok()
}

/// Clear the attempt record.
///
/// Called only when a release actually *activates*. Clearing on a successful
/// staging instead would reset the counter for a release that stages fine and
/// then fails its activation probe every time: it would restage, clear, fail,
/// re-record at zero, and never reach a meaningful backoff. Activation is the
/// only outcome that proves the candidate is good.
pub(crate) fn clear_staging_failure(base: &Path) {
    let _ = std::fs::remove_file(staging_attempt_path(base));
}

fn read_staging_attempt(base: &Path) -> Option<StagingAttempt> {
    let raw = std::fs::read_to_string(staging_attempt_path(base)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Background entry point (runs in the detached `__refresh-cache` child):
/// refresh the cached latest self version and, under policy `on`, stage a newer
/// release for the next invocation to activate. Fail-open.
pub fn refresh_self_if_due(dirs: &OvmDirs, config: &OvmConfig) {
    let _ = try_refresh_self(dirs, config, |manager, channel| {
        self_update::stage_latest(manager, channel.into())
    });
}

/// The staging step is injected so tests can reach the code after a *successful*
/// stage. The real stager downloads a release, so without this seam the only
/// reachable paths were the early returns, and the rule this function exists to
/// enforce — that a successful stage must NOT clear the attempt record — was
/// untestable and could be silently reverted.
fn try_refresh_self<S>(dirs: &OvmDirs, config: &OvmConfig, stage: S) -> Result<()>
where
    S: FnOnce(&SelfManager, SelfChannel) -> Result<Option<String>>,
{
    let policy = config.self_.auto_update;
    if policy == AutoUpdatePolicy::Off {
        return Ok(());
    }
    let manager = SelfManager::at(dirs.clone());
    let Some(current) = manager.current_version()? else {
        // Not a direct install; there is no control plane to self-update.
        return Ok(());
    };
    if is_dev_snapshot(&current) {
        return Ok(());
    }

    let channel = config.self_.channel;
    let latest = latest_self_version(&dirs.base, channel, config.update_check_interval)?;
    if !semver_newer(&latest, &current) {
        return Ok(());
    }
    if policy != AutoUpdatePolicy::On {
        // notify reads the refreshed cache on the foreground; nothing to stage.
        return Ok(());
    }

    // Skip the download when the newer release is already staged.
    let path = pending_path(&dirs.base);
    match read_pending(&path) {
        Ok(Some(pending)) if pending.version == latest => return Ok(()),
        Ok(_) => {}
        Err(_) => {
            // An unreadable marker (corrupt, or a directory) would otherwise
            // return early on every invocation without leaving a record, so
            // every command would keep spawning a child to fail here again.
            let _ = record_staging_attempt(&dirs.base, &latest);
            return Ok(());
        }
    }
    // Back off here, not only in `self_check_due`: a short or zero
    // `updateCheckInterval` keeps the version cache permanently stale, so the
    // check is always due and would otherwise reach this staging path on every
    // invocation regardless of how often it has already failed.
    if !staging_retry_due(&dirs.base, &latest, config.update_check_interval) {
        return Ok(());
    }
    // Stamp the attempt first so that every failure below — the download, the
    // marker write, or the process being killed mid-flight — leaves a record
    // to back off from. If the stamp itself cannot be persisted, do not stage:
    // the backoff would be blind to the outcome and a permanent failure would
    // be retried by every invocation.
    if !record_staging_attempt(&dirs.base, &latest) {
        return Ok(());
    }
    if let Some(staged) = stage(&manager, channel)? {
        write_pending(&path, &staged)?;
    }
    // Deliberately NOT cleared here. Staging succeeding says nothing about
    // whether the release can actually run; only activation does, and that is
    // where the record is cleared. Clearing on a successful stage is what let a
    // probe-failing release reset its counter every cycle.
    Ok(())
}

/// The channel's latest self version, served from the daily cache when fresh
/// and otherwise fetched from GitHub and cached.
fn latest_self_version(base: &Path, channel: SelfChannel, interval_hours: u64) -> Result<String> {
    if let Some(cache) = read_latest_cache(base) {
        if cache.channel == channel.label() && cache_is_fresh(&cache, interval_hours) {
            return Ok(cache.version);
        }
    }
    let version = resolved_latest_version(SelfUpdateChannel::from(channel))?;
    write_latest_cache(base, channel, &version);
    Ok(version)
}

// ---------------------------------------------------------------------------
// Notify (foreground launch path)
// ---------------------------------------------------------------------------

/// Launch-time notify for OVM itself. Under policy `notify`, read the cached
/// latest and, when it is newer, prompt the user (interactive) or print one
/// deduplicated notice (non-interactive). Reads only local state, so it adds no
/// network to the hot path. Fail-open.
pub fn maybe_notify_self_on_launch(dirs: &OvmDirs, config: &OvmConfig) {
    let _ = try_notify_self(dirs, config);
}

fn try_notify_self(dirs: &OvmDirs, config: &OvmConfig) -> Result<()> {
    // `checkForUpdates: false` turns off update checking, and the cached self
    // latest is the product of one. It outlives the setting being turned off,
    // so without this a launch would still announce (and offer to install) a
    // newer OVM to someone who asked for no update checks.
    if !config.check_for_updates {
        return Ok(());
    }
    // `on` is handled by background staging + startup activation; `off` is
    // silent. Only `notify` announces on the foreground.
    if config.self_.auto_update != AutoUpdatePolicy::Notify {
        return Ok(());
    }
    let manager = SelfManager::at(dirs.clone());
    let Some(current) = manager.current_version()? else {
        return Ok(());
    };
    if is_dev_snapshot(&current) {
        return Ok(());
    }
    let Some(cache) = read_latest_cache(&dirs.base) else {
        return Ok(());
    };
    if cache.channel != config.self_.channel.label() {
        return Ok(());
    }
    let latest = cache.version;
    let newer = semver_newer(&latest, &current);
    let is_tty = console::Term::stderr().is_term();
    let snoozed = autoupdate::is_snoozed(&dirs.base, SELF_SUBJECT, &latest);
    let label = format!("OVM {latest} available");

    match autoupdate::decide_action(AutoUpdatePolicy::Notify, newer, is_tty, snoozed) {
        UpdateAction::Prompt => match autoupdate::prompt_notify(&label) {
            NotifyChoice::Install => install_self_now(&manager, config, &current),
            NotifyChoice::Snooze => {
                autoupdate::record_snooze(&dirs.base, SELF_SUBJECT, &latest);
                Ok(())
            }
        },
        UpdateAction::Notice => {
            eprintln!("{label} — run `ovm self update`");
            autoupdate::record_snooze(&dirs.base, SELF_SUBJECT, &latest);
            Ok(())
        }
        UpdateAction::Apply | UpdateAction::Idle => Ok(()),
    }
}

/// Install-now from a notify prompt: stage and activate immediately (the user
/// asked, so the download latency is expected), then announce the swap.
fn install_self_now(manager: &SelfManager, config: &OvmConfig, old: &str) -> Result<()> {
    let channel = SelfUpdateChannel::from(config.self_.channel);
    let Some(version) = self_update::stage_latest(manager, channel)? else {
        return Ok(());
    };
    let operation = manager.acquire_operation_lock()?;
    let result = self_update::activate_release(manager, &version);
    if result.is_ok() {
        self_update::direct::apply_retention(manager, &operation);
    }
    drop(operation);
    // A pending marker from a prior background stage is now moot either way.
    let _ = clear_pending(&pending_path(&manager.ovm_dirs.base));
    result?;
    eprintln!("{} OVM {} (was {})", style("↑").green(), version, old);
    Ok(())
}

// ---------------------------------------------------------------------------
// Pending-marker and latest-cache persistence
// ---------------------------------------------------------------------------

fn read_pending(path: &Path) -> Result<Option<PendingSelfUpdate>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(serde_json::from_str(&contents).ok()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_pending(path: &Path, version: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let payload = serde_json::to_string_pretty(&PendingSelfUpdate {
        version: version.to_string(),
    })?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, payload)?;
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error.into());
    }
    Ok(())
}

fn clear_pending(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn read_latest_cache(base: &Path) -> Option<SelfLatestCache> {
    let raw = std::fs::read_to_string(latest_cache_path(base)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_latest_cache(base: &Path, channel: SelfChannel, version: &str) {
    let path = latest_cache_path(base);
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let cache = SelfLatestCache {
        version: version.to_string(),
        channel: channel.label().to_string(),
        fetched_at: now_secs(),
    };
    let Ok(payload) = serde_json::to_string_pretty(&cache) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, payload).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn cache_is_fresh(cache: &SelfLatestCache, interval_hours: u64) -> bool {
    if interval_hours == 0 {
        return false;
    }
    let ttl = interval_hours.saturating_mul(60).saturating_mul(60);
    now_secs().saturating_sub(cache.fetched_at) <= ttl
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn pending_marker_round_trips() {
        let dir = tempdir().unwrap();
        let path = pending_path(dir.path());
        assert!(read_pending(&path).unwrap().is_none());

        write_pending(&path, "0.0.4").unwrap();
        assert_eq!(read_pending(&path).unwrap().unwrap().version, "0.0.4");

        clear_pending(&path).unwrap();
        assert!(read_pending(&path).unwrap().is_none());
        // Clearing an absent marker is a no-op, not an error.
        clear_pending(&path).unwrap();
    }

    #[test]
    fn dev_snapshots_are_never_newer() {
        assert!(is_dev_snapshot("dev-abc123"));
        assert!(!is_dev_snapshot("0.0.4"));
        assert!(!semver_newer("dev-abc123", "0.0.3"));
        assert!(semver_newer("0.0.4", "0.0.3"));
        assert!(!semver_newer("0.0.3", "0.0.3"));
        assert!(!semver_newer("0.0.2", "0.0.3"));
    }

    #[test]
    fn latest_cache_freshness_respects_channel_and_ttl() {
        let dir = tempdir().unwrap();
        write_latest_cache(dir.path(), SelfChannel::Stable, "0.0.4");
        let cache = read_latest_cache(dir.path()).expect("cache written");
        assert_eq!(cache.version, "0.0.4");
        assert_eq!(cache.channel, "stable");
        assert!(cache_is_fresh(&cache, 24));

        let stale = SelfLatestCache {
            version: "0.0.4".into(),
            channel: "stable".into(),
            fetched_at: now_secs().saturating_sub(48 * 60 * 60),
        };
        assert!(!cache_is_fresh(&stale, 24));
        // A zero interval always forces a refresh.
        assert!(!cache_is_fresh(&cache, 0));
    }

    #[test]
    fn refresh_self_is_a_noop_without_a_direct_install() {
        let dir = tempdir().unwrap();
        let dirs = OvmDirs::at(dir.path().join(".ovm"));
        let mut config = OvmConfig::default();
        // Policy on, but no self install: current_version is None, so we return
        // before any network call and write no cache.
        refresh_self_if_due(&dirs, &config);
        assert!(!latest_cache_path(&dirs.base).exists());
        // Off is inert too.
        config.self_.auto_update = AutoUpdatePolicy::Off;
        refresh_self_if_due(&dirs, &config);
        assert!(!latest_cache_path(&dirs.base).exists());
    }

    #[test]
    fn pending_activation_is_skipped_off_the_control_plane() {
        let dir = tempdir().unwrap();
        let dirs = OvmDirs::at(dir.path().join(".ovm"));
        let path = pending_path(&dirs.base);
        write_pending(&path, "9.9.9").unwrap();
        // The test binary is not the installed control plane, so activation is
        // skipped and the staged marker is preserved for a real control plane.
        try_activate_pending(&dirs).unwrap();
        assert_eq!(read_pending(&path).unwrap().unwrap().version, "9.9.9");
    }

    #[test]
    fn self_check_due_when_cache_missing_or_off() {
        let dir = tempdir().unwrap();
        let mut config = OvmConfig::default();
        // On, no cache -> due.
        assert!(self_check_due(dir.path(), &config));

        write_latest_cache(dir.path(), SelfChannel::Stable, "0.0.4");
        assert!(!self_check_due(dir.path(), &config));

        // A channel switch invalidates the cache.
        config.self_.channel = SelfChannel::Alpha;
        assert!(self_check_due(dir.path(), &config));

        // Off is never due.
        config.self_.auto_update = AutoUpdatePolicy::Off;
        config.self_.channel = SelfChannel::Stable;
        assert!(!self_check_due(dir.path(), &config));
    }

    #[cfg(unix)]
    #[test]
    fn failed_staging_re_arms_the_due_check() {
        let dir = tempdir().unwrap();
        let dirs = OvmDirs::at(dir.path().to_path_buf());
        let config = OvmConfig::default();

        // Active version 0.0.3, fresh cache saying 0.0.4 is out.
        let self_dirs = crate::self_manager::SelfDirs::at(&dirs.base);
        let version_dir = self_dirs.versions.join("0.0.3");
        std::fs::create_dir_all(&version_dir).unwrap();
        std::os::unix::fs::symlink(&version_dir, &self_dirs.current).unwrap();
        write_latest_cache(dir.path(), SelfChannel::Stable, "0.0.4");

        // Fresh cache + newer latest + nothing staged: an earlier staging
        // attempt failed, so the check must stay due instead of sleeping
        // out the interval.
        assert!(self_check_due(dir.path(), &config));

        // Once the matching version is staged, the check goes quiet.
        write_pending(&pending_path(dir.path()), "0.0.4").unwrap();
        assert!(!self_check_due(dir.path(), &config));

        // A stale pending from a superseded candidate re-arms it again.
        write_pending(&pending_path(dir.path()), "0.0.3-alpha.1").unwrap();
        assert!(self_check_due(dir.path(), &config));
    }

    #[cfg(unix)]
    #[test]
    fn permanently_unstageable_release_backs_off_instead_of_retrying_forever() {
        let dir = tempdir().unwrap();
        let dirs = OvmDirs::at(dir.path().to_path_buf());
        let config = OvmConfig::default();

        let self_dirs = crate::self_manager::SelfDirs::at(&dirs.base);
        let version_dir = self_dirs.versions.join("0.0.3");
        std::fs::create_dir_all(&version_dir).unwrap();
        std::os::unix::fs::symlink(&version_dir, &self_dirs.current).unwrap();
        write_latest_cache(dir.path(), SelfChannel::Stable, "0.0.4");

        // Nothing staged and no prior attempt: retry immediately. That is the
        // transient case — a download killed mid-flight.
        assert!(self_check_due(dir.path(), &config));

        // After a failure the check goes quiet rather than spawning a child on
        // every invocation, which is the churn this guards against.
        assert!(record_staging_attempt(&dirs.base, "0.0.4"));
        assert!(!self_check_due(dir.path(), &config));

        // Consecutive failures widen the wait.
        let first = read_staging_attempt(&dirs.base).unwrap();
        assert!(record_staging_attempt(&dirs.base, "0.0.4"));
        let second = read_staging_attempt(&dirs.base).unwrap();
        assert_eq!(first.failures, 0);
        assert_eq!(second.failures, 1);

        // Once the backoff elapses the retry is allowed again.
        let elapsed = StagingAttempt {
            version: "0.0.4".into(),
            last_attempt_at: now_secs().saturating_sub(24 * 60 * 60),
            failures: 3,
        };
        std::fs::write(
            staging_attempt_path(&dirs.base),
            serde_json::to_string(&elapsed).unwrap(),
        )
        .unwrap();
        assert!(self_check_due(dir.path(), &config));

        // A newer candidate is never held back by an older version's failures.
        assert!(record_staging_attempt(&dirs.base, "0.0.4"));
        assert!(!self_check_due(dir.path(), &config));
        write_latest_cache(dir.path(), SelfChannel::Stable, "0.0.5");
        assert!(self_check_due(dir.path(), &config));

        // A success clears the record.
        assert!(record_staging_attempt(&dirs.base, "0.0.5"));
        assert!(!self_check_due(dir.path(), &config));
        clear_staging_failure(&dirs.base);
        assert!(self_check_due(dir.path(), &config));
    }

    #[test]
    fn staging_backoff_holds_when_the_check_interval_cannot_gate_it() {
        // `updateCheckInterval: 0` keeps the version cache permanently stale, so
        // the due check always fires. The staging path must still back off, or
        // that supported configuration recreates the retry storm.
        let dir = tempdir().unwrap();
        let config = OvmConfig {
            update_check_interval: 0,
            ..OvmConfig::default()
        };
        assert!(staging_retry_due(
            dir.path(),
            "0.0.4",
            config.update_check_interval
        ));
        assert!(record_staging_attempt(dir.path(), "0.0.4"));
        assert!(!staging_retry_due(
            dir.path(),
            "0.0.4",
            config.update_check_interval
        ));
    }

    #[test]
    fn a_clock_moving_backwards_does_not_suppress_retries() {
        let dir = tempdir().unwrap();
        let future = StagingAttempt {
            version: "0.0.4".into(),
            last_attempt_at: now_secs().saturating_add(7 * 24 * 60 * 60),
            failures: 2,
        };
        std::fs::create_dir_all(staging_attempt_path(dir.path()).parent().unwrap()).unwrap();
        std::fs::write(
            staging_attempt_path(dir.path()),
            serde_json::to_string(&future).unwrap(),
        )
        .unwrap();
        // Without the guard this stays suppressed until real time catches up.
        assert!(staging_retry_due(dir.path(), "0.0.4", 4));
    }

    #[test]
    fn every_staging_failure_path_is_recorded_not_just_the_download() {
        // The attempt is stamped before staging runs, so a failure after a
        // successful download — a marker that cannot be written, a killed
        // process — backs off exactly like a failed download.
        let dir = tempdir().unwrap();
        assert!(read_staging_attempt(dir.path()).is_none());

        assert!(record_staging_attempt(dir.path(), "0.0.4"));
        let recorded = read_staging_attempt(dir.path()).expect("attempt stamped before staging");
        assert_eq!(recorded.version, "0.0.4");
        assert!(!staging_retry_due(dir.path(), "0.0.4", 4));

        // Only a completed staging clears it.
        clear_staging_failure(dir.path());
        assert!(read_staging_attempt(dir.path()).is_none());
        assert!(staging_retry_due(dir.path(), "0.0.4", 4));
    }

    #[test]
    fn attempt_counts_accumulate_across_repeated_stage_and_activate_cycles() {
        // Bookkeeping only: proves the counter accumulates across the
        // stage-then-failed-activation cycle rather than resetting, which is
        // what makes the backoff widen for a release that stages fine but
        // never activates.
        //
        // This exercises the record functions directly; the production staging
        // path is covered by
        // `successful_staging_does_not_clear_the_attempt_record`.
        let dir = tempdir().unwrap();

        for cycle in 0..4 {
            // Staging succeeds — which must NOT clear the record. Each cycle
            // stamps twice (stage, then the failed activation), so the count
            // entering cycle N is 2N.
            assert!(record_staging_attempt(dir.path(), "0.0.4"));
            let after_stage = read_staging_attempt(dir.path()).expect("record kept");
            assert_eq!(after_stage.failures, cycle * 2);

            // Activation then fails and records another attempt.
            assert!(record_staging_attempt(dir.path(), "0.0.4"));
        }

        let record = read_staging_attempt(dir.path()).expect("record kept");
        assert!(
            record.failures >= 4,
            "failures must accumulate across stage/activate cycles, got {}",
            record.failures
        );
    }

    /// Build a tempdir that looks enough like a direct install for
    /// `try_refresh_self` to run past its early returns: a `self/current`
    /// pointer so `current_version` is `Some`, and a fresh latest-version cache
    /// so resolving the latest release needs no network.
    fn staged_install(current: &str, latest: &str) -> (tempfile::TempDir, OvmDirs) {
        let dir = tempdir().unwrap();
        let dirs = OvmDirs::at(dir.path().join(".ovm"));
        let version_dir = dirs.base.join("self").join("versions").join(current);
        std::fs::create_dir_all(&version_dir).unwrap();
        crate::symlink::switch_symlink(&dirs.base.join("self").join("current"), &version_dir)
            .unwrap();
        write_latest_cache(&dirs.base, SelfChannel::Stable, latest);
        (dir, dirs)
    }

    #[test]
    fn successful_staging_does_not_clear_the_attempt_record() {
        // The livelock this guards: a release that stages fine but cannot run
        // will fail activation forever. If staging clears the attempt record,
        // the counter resets every cycle, the backoff never widens, and every
        // ovm invocation re-downloads it. Only activation proves the candidate
        // is good, so only activation may clear.
        //
        // Injecting the stager is what makes this reachable — the real one
        // downloads a release.
        let (_dir, dirs) = staged_install("0.0.1", "9.9.9");
        let mut config = OvmConfig::default();
        config.self_.auto_update = AutoUpdatePolicy::On;

        let mut staged_calls = 0;
        try_refresh_self(&dirs, &config, |_, _| {
            staged_calls += 1;
            Ok(Some("9.9.9".to_string()))
        })
        .unwrap();

        assert_eq!(staged_calls, 1, "the staging path must have been reached");
        assert_eq!(
            read_pending(&pending_path(&dirs.base))
                .unwrap()
                .expect("staged marker written")
                .version,
            "9.9.9"
        );
        let record = read_staging_attempt(&dirs.base).expect(
            "a successful stage must leave the attempt record in place; \
             clearing here is the livelock bug",
        );
        assert_eq!(record.version, "9.9.9");
    }

    #[test]
    fn an_already_staged_release_is_not_downloaded_again() {
        let (_dir, dirs) = staged_install("0.0.1", "9.9.9");
        let mut config = OvmConfig::default();
        config.self_.auto_update = AutoUpdatePolicy::On;
        write_pending(&pending_path(&dirs.base), "9.9.9").unwrap();

        let mut staged_calls = 0;
        try_refresh_self(&dirs, &config, |_, _| {
            staged_calls += 1;
            Ok(Some("9.9.9".to_string()))
        })
        .unwrap();

        assert_eq!(staged_calls, 0, "the pending marker must short-circuit");
    }

    #[test]
    fn staging_backs_off_instead_of_retrying_every_invocation() {
        // Second call in the same window must not reach the network again.
        let (_dir, dirs) = staged_install("0.0.1", "9.9.9");
        let mut config = OvmConfig::default();
        config.self_.auto_update = AutoUpdatePolicy::On;

        let mut staged_calls = 0;
        for _ in 0..3 {
            try_refresh_self(&dirs, &config, |_, _| {
                staged_calls += 1;
                // Staging fails, so no pending marker is written and the next
                // invocation would otherwise take this path again.
                Ok(None)
            })
            .unwrap();
        }

        assert_eq!(
            staged_calls, 1,
            "only the first attempt is due; the rest must be held by the backoff"
        );
    }

    #[test]
    fn an_unwritable_attempt_record_is_reported_so_staging_can_be_skipped() {
        let dir = tempdir().unwrap();
        // A directory where the record file belongs: the write cannot succeed.
        std::fs::create_dir_all(staging_attempt_path(dir.path())).unwrap();
        assert!(
            !record_staging_attempt(dir.path(), "0.0.4"),
            "an unpersisted attempt must be reported, not silently ignored"
        );
    }

    #[test]
    fn a_corrupt_attempt_record_retries_rather_than_wedging() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(staging_attempt_path(dir.path()).parent().unwrap()).unwrap();
        std::fs::write(staging_attempt_path(dir.path()), b"{ not json").unwrap();
        assert!(read_staging_attempt(dir.path()).is_none());
        assert!(staging_retry_due(dir.path(), "0.0.4", 4));
    }
}

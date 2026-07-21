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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SelfLatestCache {
    version: String,
    channel: String,
    fetched_at: u64,
}

fn pending_path(base: &Path) -> PathBuf {
    base.join("self").join("pending-update.json")
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
    drop(operation);
    // Clear the marker regardless: a success is applied, and a failure already
    // rolled back — retrying it every launch would just delay them.
    let _ = clear_pending(&path);
    if result.is_ok() {
        eprintln!(
            "{} OVM {} (was {})",
            style("↑").green(),
            pending.version,
            old
        );
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
            cache.channel != config.self_.channel.label()
                || !cache_is_fresh(&cache, config.update_check_interval)
        }
        None => true,
    }
}

/// Background entry point (runs in the detached `__refresh-cache` child):
/// refresh the cached latest self version and, under policy `on`, stage a newer
/// release for the next invocation to activate. Fail-open.
pub fn refresh_self_if_due(dirs: &OvmDirs, config: &OvmConfig) {
    let _ = try_refresh_self(dirs, config);
}

fn try_refresh_self(dirs: &OvmDirs, config: &OvmConfig) -> Result<()> {
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
    if let Some(pending) = read_pending(&path)? {
        if pending.version == latest {
            return Ok(());
        }
    }
    if let Some(staged) = self_update::stage_latest(&manager, channel.into())? {
        write_pending(&path, &staged)?;
    }
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
}

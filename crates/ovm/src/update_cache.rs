//! Cache of known upstream versions per product.
//!
//! A small aggregate snapshot supplies each product's stable latest release.
//! Detached workers conditionally validate it at most once a minute with an
//! ETag, then refresh a full per-product index only when that product's summary
//! changed. Both snapshots have a 24-hour read TTL; failed probes retain the
//! last validated answer and retry with bounded exponential backoff.

use crate::error::Result;
use crate::product::Product;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const TTL_SECS: u64 = 60 * 60 * 24;
const LATEST_PROBE_COOLDOWN_SECS: u64 = 60;
const LATEST_PROBE_MAX_BACKOFF_SECS: u64 = 60 * 60;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VersionIndex {
    #[serde(default)]
    pub versions: Vec<String>,
    #[serde(default)]
    pub dates: HashMap<String, String>,
    pub fetched_at: u64,
}

/// The tiny per-product facts published in `api/registry.json`.
///
/// Equality is deliberately broader than `latest`: prerelease additions,
/// retirements, and metadata-only republishes should refresh that product's
/// full index even when its stable latest did not move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RegistryProductSummary {
    pub latest: String,
    pub version_count: u64,
    pub retired_count: u64,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LatestProbeState {
    #[serde(default)]
    etag: Option<String>,
    #[serde(default)]
    summaries: HashMap<String, RegistryProductSummary>,
    #[serde(default)]
    pending_indexes: Vec<String>,
    #[serde(default)]
    validated_at: u64,
    #[serde(default)]
    attempted_at: u64,
    #[serde(default)]
    consecutive_failures: u32,
}

impl VersionIndex {
    pub fn new(versions: Vec<String>, dates: HashMap<String, String>) -> Self {
        Self {
            versions,
            dates,
            fetched_at: now_secs(),
        }
    }

    pub fn is_fresh(&self) -> bool {
        now_secs().saturating_sub(self.fetched_at) <= TTL_SECS
    }

    pub fn latest(&self, product: Product) -> Option<&str> {
        self.versions
            .iter()
            .filter(|version| {
                product.is_official_remote_version(version) && product.is_release_version(version)
            })
            .max_by(|left, right| product.compare_version_strings(left, right))
            .map(String::as_str)
    }

    fn retain_official_versions(&mut self, product: Product) {
        self.versions
            .retain(|version| product.is_official_remote_version(version));
        self.dates
            .retain(|version, _| product.is_official_remote_version(version));
    }

    pub fn into_parts(self) -> (Vec<String>, HashMap<String, String>) {
        (self.versions, self.dates)
    }
}

pub fn version_index_path(base: &Path, product: Product) -> PathBuf {
    base.join("cache")
        .join("registry")
        .join(format!("{}.json", product.canonical_name()))
}

pub fn load_version_index(base: &Path, product: Product) -> Option<VersionIndex> {
    let path = version_index_path(base, product);
    let mut index: VersionIndex = std::fs::read_to_string(path)
        .ok()
        .and_then(|value| serde_json::from_str(&value).ok())?;
    index.retain_official_versions(product);
    Some(index)
}

pub fn load_fresh_version_index(base: &Path, product: Product) -> Option<VersionIndex> {
    let index = load_version_index(base, product)?;
    if probe_marks_index_pending(base, product) {
        return None;
    }
    if index.is_fresh() || probe_confirms_index(base, product, now_secs()) {
        Some(index)
    } else {
        None
    }
}

pub fn save_version_index(base: &Path, product: Product, index: &VersionIndex) -> Result<()> {
    let path = version_index_path(base, product);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Atomic write: serialize to <path>.tmp, then rename. rename(2) is atomic on
    // the same filesystem, so concurrent readers either see the previous index
    // or the new one — never a half-written file.
    let tmp = path.with_extension("json.tmp");
    let payload = serde_json::to_string_pretty(index)?;
    std::fs::write(&tmp, payload)?;
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }

    Ok(())
}

pub fn fresh_latest(base: &Path, product: Product) -> Option<String> {
    load_fresh_version_index(base, product)
        .and_then(|index| index.latest(product).map(str::to_string))
}

fn latest_probe_path(base: &Path) -> PathBuf {
    base.join("cache")
        .join("registry")
        .join("latest-probe.json")
}

fn load_latest_probe_state(base: &Path) -> Option<LatestProbeState> {
    std::fs::read_to_string(latest_probe_path(base))
        .ok()
        .and_then(|value| serde_json::from_str(&value).ok())
}

fn save_latest_probe_state(base: &Path, state: &LatestProbeState) -> Result<()> {
    let path = latest_probe_path(base);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(state)?)?;
    if let Err(error) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error.into());
    }
    Ok(())
}

fn latest_probe_retry_delay(failures: u32) -> u64 {
    let shift = failures.saturating_sub(1).min(6);
    LATEST_PROBE_COOLDOWN_SECS
        .saturating_mul(1_u64 << shift)
        .min(LATEST_PROBE_MAX_BACKOFF_SECS)
}

fn latest_probe_due_at(base: &Path, now: u64) -> bool {
    let Some(state) = load_latest_probe_state(base) else {
        return true;
    };
    if now < state.attempted_at {
        return true;
    }
    let delay = latest_probe_retry_delay(state.consecutive_failures);
    now.saturating_sub(state.attempted_at) >= delay
}

pub(crate) fn latest_probe_due(base: &Path) -> bool {
    latest_probe_due_at(base, now_secs())
}

pub(crate) fn latest_probe_etag(base: &Path) -> Option<String> {
    load_latest_probe_state(base).and_then(|state| state.etag)
}

/// Record a changed aggregate registry and return the managed products whose
/// summary changed. The background worker uses that list to refresh only those
/// full indexes; launches can consume `latest` immediately even if a full-index
/// refresh fails.
pub(crate) fn record_latest_probe_modified(
    base: &Path,
    etag: Option<String>,
    summaries: HashMap<Product, RegistryProductSummary>,
    now: u64,
) -> Result<Vec<Product>> {
    let previous = load_latest_probe_state(base).unwrap_or_default();
    let mut pending_indexes = previous.pending_indexes;
    for product in Product::ALL {
        let name = product.canonical_name();
        if summaries
            .get(&product)
            .is_some_and(|summary| previous.summaries.get(name) != Some(summary))
            && !pending_indexes.iter().any(|pending| pending == name)
        {
            pending_indexes.push(name.to_string());
        }
    }

    let summaries = summaries
        .into_iter()
        .map(|(product, summary)| (product.canonical_name().to_string(), summary))
        .collect();
    save_latest_probe_state(
        base,
        &LatestProbeState {
            etag,
            summaries,
            pending_indexes: pending_indexes.clone(),
            validated_at: now,
            attempted_at: now,
            consecutive_failures: 0,
        },
    )?;
    Ok(products_from_names(&pending_indexes))
}

pub(crate) fn record_latest_probe_not_modified(base: &Path, now: u64) -> Result<Vec<Product>> {
    let mut state = load_latest_probe_state(base).unwrap_or_default();
    state.validated_at = now;
    state.attempted_at = now;
    state.consecutive_failures = 0;
    let pending = products_from_names(&state.pending_indexes);
    save_latest_probe_state(base, &state)?;
    Ok(pending)
}

pub(crate) fn record_latest_probe_failure(base: &Path, now: u64) -> Result<()> {
    let mut state = load_latest_probe_state(base).unwrap_or_default();
    state.attempted_at = now;
    state.consecutive_failures = state.consecutive_failures.saturating_add(1);
    save_latest_probe_state(base, &state)
}

pub(crate) fn record_index_refresh_success(base: &Path, product: Product) -> Result<()> {
    let Some(mut state) = load_latest_probe_state(base) else {
        return Ok(());
    };
    state
        .pending_indexes
        .retain(|pending| pending != product.canonical_name());
    save_latest_probe_state(base, &state)
}

fn products_from_names(names: &[String]) -> Vec<Product> {
    Product::ALL
        .into_iter()
        .filter(|product| names.iter().any(|name| name == product.canonical_name()))
        .collect()
}

fn probe_confirms_index(base: &Path, product: Product, now: u64) -> bool {
    let Some(state) = load_latest_probe_state(base) else {
        return false;
    };
    now.saturating_sub(state.validated_at) <= TTL_SECS
        && state.summaries.contains_key(product.canonical_name())
        && !state
            .pending_indexes
            .iter()
            .any(|pending| pending == product.canonical_name())
}

fn probe_marks_index_pending(base: &Path, product: Product) -> bool {
    load_latest_probe_state(base).is_some_and(|state| {
        state
            .pending_indexes
            .iter()
            .any(|pending| pending == product.canonical_name())
    })
}

fn fresh_probed_latest_at(base: &Path, product: Product, now: u64) -> Option<String> {
    let state = load_latest_probe_state(base)?;
    if now.saturating_sub(state.validated_at) > TTL_SECS {
        return None;
    }
    let latest = &state.summaries.get(product.canonical_name())?.latest;
    if !product.is_official_remote_version(latest) || !product.is_release_version(latest) {
        return None;
    }
    Some(latest.clone())
}

pub(crate) fn fresh_probed_latest(base: &Path, product: Product) -> Option<String> {
    fresh_probed_latest_at(base, product, now_secs())
}

/// Seconds since the Unix epoch, saturating to 0 if the clock is before it.
/// Shared so callers can compare against `VersionIndex::fetched_at`.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn version_index_round_trips_and_reports_latest() {
        let dir = tempdir().unwrap();
        let index = VersionIndex::new(
            vec!["rust-v0.129.0".into(), "rust-v0.130.0".into()],
            HashMap::from([("rust-v0.130.0".into(), "2026-05-13".into())]),
        );

        save_version_index(dir.path(), Product::Codex, &index).expect("save");

        let loaded = load_fresh_version_index(dir.path(), Product::Codex).expect("fresh index");
        assert_eq!(loaded.latest(Product::Codex), Some("rust-v0.130.0"));
        assert_eq!(
            fresh_latest(dir.path(), Product::Codex).as_deref(),
            Some("rust-v0.130.0")
        );
    }

    #[test]
    fn version_index_latest_ignores_prerelease_versions() {
        let index = VersionIndex::new(
            vec![
                "rust-v0.130.0".into(),
                "rust-v0.131.0-alpha.16".into(),
                "rust-v0.129.0".into(),
            ],
            HashMap::new(),
        );

        assert_eq!(index.latest(Product::Codex), Some("rust-v0.130.0"));
    }

    #[test]
    fn save_version_index_is_atomic_under_partial_writes() {
        // Simulate a previous interrupted write that left a .tmp file behind.
        // save_version_index must overwrite it cleanly and never leave the
        // primary index file in a half-written state.
        let dir = tempdir().unwrap();
        let path = version_index_path(dir.path(), Product::Claude);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path.with_extension("json.tmp"), "garbage").unwrap();

        let index = VersionIndex::new(
            vec!["2.1.141".into()],
            HashMap::from([("2.1.141".into(), "2026-05-14".into())]),
        );
        save_version_index(dir.path(), Product::Claude, &index).expect("save");

        // Primary file present and parseable.
        let loaded = load_version_index(dir.path(), Product::Claude).expect("loaded");
        let (versions, _) = loaded.into_parts();
        assert_eq!(versions, vec!["2.1.141"]);

        // Temp scratch file was renamed away (or never republished as junk).
        assert!(
            !path.with_extension("json.tmp").exists(),
            "tmp file should be renamed into place, not left behind"
        );
    }

    fn summary(version: &str, updated_at: &str) -> RegistryProductSummary {
        RegistryProductSummary {
            latest: version.into(),
            version_count: 10,
            retired_count: 0,
            updated_at: updated_at.into(),
        }
    }

    #[test]
    fn aggregate_probe_uses_a_short_success_cooldown() {
        let dir = tempdir().unwrap();
        let checked_at = 1_000_000;
        let summaries = HashMap::from([(
            Product::Codex,
            summary("rust-v0.148.0", "2026-08-19T01:22:51Z"),
        )]);

        record_latest_probe_modified(
            dir.path(),
            Some("\"registry-v1\"".into()),
            summaries,
            checked_at,
        )
        .expect("record modified probe");

        assert!(!latest_probe_due_at(dir.path(), checked_at + 59));
        assert!(latest_probe_due_at(dir.path(), checked_at + 60));
        assert_eq!(
            latest_probe_etag(dir.path()).as_deref(),
            Some("\"registry-v1\"")
        );
        assert_eq!(
            fresh_probed_latest_at(dir.path(), Product::Codex, checked_at + 60).as_deref(),
            Some("rust-v0.148.0")
        );
    }

    #[test]
    fn aggregate_probe_retries_immediately_after_the_clock_moves_backwards() {
        let dir = tempdir().unwrap();
        record_latest_probe_failure(dir.path(), 1_000_000).expect("record failed probe");

        assert!(latest_probe_due_at(dir.path(), 999_999));
    }

    #[test]
    fn aggregate_probe_failure_backoff_is_capped_at_one_hour() {
        assert_eq!(latest_probe_retry_delay(1), 60);
        assert_eq!(latest_probe_retry_delay(7), 60 * 60);
        assert_eq!(latest_probe_retry_delay(u32::MAX), 60 * 60);
    }

    #[test]
    fn aggregate_probe_failure_backs_off_without_discarding_known_latest() {
        let dir = tempdir().unwrap();
        let checked_at = 1_000_000;
        let summaries = HashMap::from([(
            Product::Codex,
            summary("rust-v0.148.0", "2026-08-19T01:22:51Z"),
        )]);
        record_latest_probe_modified(dir.path(), None, summaries, checked_at)
            .expect("seed successful probe");

        record_latest_probe_failure(dir.path(), checked_at + 60).expect("first failure");
        record_latest_probe_failure(dir.path(), checked_at + 120).expect("second failure");

        assert!(!latest_probe_due_at(dir.path(), checked_at + 239));
        assert!(latest_probe_due_at(dir.path(), checked_at + 240));
        assert_eq!(
            fresh_probed_latest_at(dir.path(), Product::Codex, checked_at + 240).as_deref(),
            Some("rust-v0.148.0"),
            "a transient probe failure must not erase the last validated latest"
        );

        record_latest_probe_not_modified(dir.path(), checked_at + 240)
            .expect("304 revalidates probe state");
        assert!(!latest_probe_due_at(dir.path(), checked_at + 299));
        assert!(latest_probe_due_at(dir.path(), checked_at + 300));
        assert_eq!(
            fresh_probed_latest_at(dir.path(), Product::Codex, checked_at + 240 + TTL_SECS)
                .as_deref(),
            Some("rust-v0.148.0"),
            "304 must extend the validated lifetime and reset failure backoff"
        );
    }

    #[test]
    fn fresh_probe_rejects_a_tampered_prerelease_latest() {
        let dir = tempdir().unwrap();
        let checked_at = 1_000_000;
        let summaries = HashMap::from([(
            Product::Codex,
            summary("rust-v0.149.0-alpha.1", "2026-08-19T01:22:51Z"),
        )]);
        record_latest_probe_modified(dir.path(), None, summaries, checked_at)
            .expect("seed invalid probe state");

        assert_eq!(
            fresh_probed_latest_at(dir.path(), Product::Codex, checked_at + 1),
            None
        );
    }

    #[test]
    fn modified_probe_identifies_only_products_whose_summary_changed() {
        let dir = tempdir().unwrap();
        let first = HashMap::from([
            (Product::Claude, summary("2.1.235", "2026-08-19T01:22:51Z")),
            (
                Product::Codex,
                summary("rust-v0.147.0", "2026-08-19T01:22:51Z"),
            ),
        ]);
        record_latest_probe_modified(dir.path(), None, first, 1_000_000).expect("seed first probe");
        record_index_refresh_success(dir.path(), Product::Claude).expect("refresh Claude index");
        record_index_refresh_success(dir.path(), Product::Codex).expect("refresh Codex index");

        let second = HashMap::from([
            (Product::Claude, summary("2.1.235", "2026-08-19T01:22:51Z")),
            (
                Product::Codex,
                summary("rust-v0.148.0", "2026-08-19T02:02:24Z"),
            ),
        ]);
        let changed = record_latest_probe_modified(
            dir.path(),
            Some("\"registry-v2\"".into()),
            second,
            1_000_060,
        )
        .expect("record second probe");

        assert_eq!(changed, vec![Product::Codex]);
    }

    #[test]
    fn unchanged_probe_keeps_retrying_a_failed_full_index_refresh() {
        let dir = tempdir().unwrap();
        let summaries = HashMap::from([(
            Product::Codex,
            summary("rust-v0.148.0", "2026-08-19T01:22:51Z"),
        )]);
        let changed = record_latest_probe_modified(
            dir.path(),
            Some("\"registry-v2\"".into()),
            summaries,
            1_000_000,
        )
        .expect("record changed aggregate");
        assert_eq!(changed, vec![Product::Codex]);

        let retry = record_latest_probe_not_modified(dir.path(), 1_000_060)
            .expect("record unchanged aggregate");
        assert_eq!(retry, vec![Product::Codex]);

        record_index_refresh_success(dir.path(), Product::Codex).expect("refresh Codex index");
        let settled = record_latest_probe_not_modified(dir.path(), 1_000_120)
            .expect("record second unchanged aggregate");
        assert!(settled.is_empty());
    }

    #[test]
    fn pending_summary_invalidates_an_old_index_until_refresh_succeeds() {
        let dir = tempdir().unwrap();
        let now = now_secs();
        let index = VersionIndex {
            versions: vec!["rust-v0.147.0".into()],
            dates: HashMap::new(),
            fetched_at: now.saturating_sub(TTL_SECS + 1),
        };
        save_version_index(dir.path(), Product::Codex, &index).expect("seed stale index");
        let summaries = HashMap::from([(
            Product::Codex,
            summary("rust-v0.148.0", "2026-08-19T01:22:51Z"),
        )]);
        record_latest_probe_modified(dir.path(), None, summaries, now)
            .expect("record changed aggregate");

        assert!(load_fresh_version_index(dir.path(), Product::Codex).is_none());

        let refreshed = VersionIndex {
            versions: vec!["rust-v0.147.0".into(), "rust-v0.148.0".into()],
            dates: HashMap::new(),
            fetched_at: now.saturating_sub(TTL_SECS + 1),
        };
        save_version_index(dir.path(), Product::Codex, &refreshed).expect("save refreshed index");
        record_index_refresh_success(dir.path(), Product::Codex).expect("mark refresh complete");

        assert_eq!(
            load_fresh_version_index(dir.path(), Product::Codex)
                .and_then(|index| index.latest(Product::Codex).map(str::to_string))
                .as_deref(),
            Some("rust-v0.148.0"),
            "a validated unchanged aggregate extends the matching full index"
        );
    }
}

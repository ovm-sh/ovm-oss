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

/// Where the served Codex skew-evidence document is cached for the
/// `ovm-codex-skew` companion. Lives beside the registry indexes so the
/// companion finds it from the one directory `ovm` names in its env contract
/// (`OVM_REGISTRY_CACHE`).
pub fn codex_skew_evidence_path(base: &Path) -> PathBuf {
    registry_cache_dir(base).join("codex-skew.json")
}

/// The directory holding cached registry documents.
pub fn registry_cache_dir(base: &Path) -> PathBuf {
    base.join("cache").join("registry")
}

/// After a failed evidence fetch, wait this long before trying again. Keeps a
/// dead endpoint from being hit on every background refresh of an active
/// terminal while still recovering well inside the read TTL.
const CODEX_SKEW_EVIDENCE_RETRY_SECS: u64 = 5 * 60;

/// Marker left beside the cached evidence when a fetch was wanted but failed
/// (or when Codex's index moved and the refetch did not land). Its presence
/// keeps the evidence due — an old document must not masquerade as fresh for
/// 24h just because the one refetch that should have replaced it timed out —
/// and its mtime paces retries.
fn codex_skew_evidence_pending_path(base: &Path) -> PathBuf {
    registry_cache_dir(base).join("codex-skew.pending")
}

/// True when the evidence should be (re)fetched now: the cached document is
/// missing, older than the read TTL, or a refetch is pending — and the last
/// failed attempt is old enough to retry. The observatory republishes at most
/// a few times a day, so the same 24-hour horizon the version indexes use is
/// plenty.
pub(crate) fn codex_skew_evidence_due(base: &Path) -> bool {
    codex_skew_evidence_due_at(base, SystemTime::now())
}

fn codex_skew_evidence_due_at(base: &Path, now: SystemTime) -> bool {
    let age_secs = |path: PathBuf| -> Option<u64> {
        let modified = std::fs::metadata(path).ok()?.modified().ok()?;
        // A future mtime (clock rollback, restored cache) counts as ancient:
        // refreshing rewrites it to now, which is the only way it self-heals.
        Some(
            now.duration_since(modified)
                .map(|age| age.as_secs())
                .unwrap_or(u64::MAX),
        )
    };
    let pending = age_secs(codex_skew_evidence_pending_path(base));
    if pending.is_some_and(|age| age < CODEX_SKEW_EVIDENCE_RETRY_SECS) {
        return false;
    }
    if pending.is_some() {
        return true;
    }
    age_secs(codex_skew_evidence_path(base)).is_none_or(|age| age > TTL_SECS)
}

/// Record that an evidence fetch was wanted and did not land; paces the retry.
pub(crate) fn mark_codex_skew_evidence_pending(base: &Path) -> Result<()> {
    let path = codex_skew_evidence_pending_path(base);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, format!("{}\n", now_secs()))?;
    Ok(())
}

/// How long "knowledge only advances" protects a cached document from being
/// replaced by one with a smaller key. Bounded so a lagging replica or a
/// partial publish is shrugged off, but a poisoned or simply wrong document
/// can never hold the cache for longer than this.
const CODEX_SKEW_EVIDENCE_MONOTONE_SECS: u64 = 7 * 24 * 60 * 60;

/// When the cached document was last REPLACED (not restamped). Kept beside it
/// so the TTL restamp on a landed-but-not-newer fetch does not extend the
/// monotone protection indefinitely.
fn codex_skew_evidence_saved_at_path(base: &Path) -> PathBuf {
    registry_cache_dir(base).join("codex-skew.saved-at")
}

/// Digest of a document's text, as recorded in the saved-at sidecar: the
/// sidecar protects exactly the document it was written for, not whatever
/// happens to be at the path.
fn codex_skew_evidence_digest(text: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// True while the cached document is still within its monotone window — i.e.
/// a fetched document with a smaller key should NOT replace it yet. Only a
/// sidecar whose digest matches the cached document counts: a sidecar written
/// for a document whose rename never landed protects nothing.
pub(crate) fn codex_skew_evidence_is_protected(base: &Path) -> bool {
    let Some(sidecar) = std::fs::read_to_string(codex_skew_evidence_saved_at_path(base)).ok()
    else {
        return false;
    };
    let mut fields = sidecar.split_whitespace();
    let Some(saved_at) = fields.next().and_then(|text| text.parse::<u64>().ok()) else {
        return false;
    };
    let Some(digest) = fields.next() else {
        return false;
    };
    let matches_document = std::fs::read_to_string(codex_skew_evidence_path(base))
        .ok()
        .is_some_and(|text| codex_skew_evidence_digest(&text) == digest);
    // A saved-at in the future (clock rollback, restored cache) does not
    // protect anything: it would otherwise protect until that time plus a week.
    let now = now_secs();
    matches_document && saved_at <= now && now - saved_at <= CODEX_SKEW_EVIDENCE_MONOTONE_SECS
}

/// A fetch landed but carried nothing newer than the cached document: clear
/// the pending marker and restamp the cached document as validated now, so
/// it is neither retried nor treated as expired on the next cycle.
pub(crate) fn touch_codex_skew_evidence(base: &Path) {
    let _ = std::fs::remove_file(codex_skew_evidence_pending_path(base));
    if let Ok(file) = std::fs::OpenOptions::new()
        .append(true)
        .open(codex_skew_evidence_path(base))
    {
        let _ = file.set_modified(SystemTime::now());
    }
}

/// The cached evidence document's text, if any.
pub(crate) fn load_codex_skew_evidence_text(base: &Path) -> Option<String> {
    std::fs::read_to_string(codex_skew_evidence_path(base)).ok()
}

/// Atomically replace the cached evidence document and clear any pending
/// marker. Callers pass text the registry client already validated; a
/// half-written file is never visible.
///
/// Order matters, the other way from a first guess: the DOCUMENT is renamed
/// into place first, then the sidecar is written (itself atomically). A rename
/// failure — the common failure, and the one that used to strip the still-
/// active old document of its protection — now returns before the sidecar is
/// touched, so the previous document AND its sidecar stay intact. A crash
/// after the rename but before the sidecar lands leaves the new (correct)
/// document active but briefly unprotected: it re-protects on the next fetch,
/// which is the mild direction. `is_protected` requires the sidecar's digest
/// to match the active document, so the stale pre-rename sidecar protects
/// nothing until the new one is in place.
pub(crate) fn save_codex_skew_evidence(base: &Path, text: &str) -> Result<()> {
    let path = codex_skew_evidence_path(base);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    if let Err(error) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error.into());
    }
    let _ = std::fs::remove_file(codex_skew_evidence_pending_path(base));
    // Best-effort, atomic, bound to the document now in place. A partial write
    // can never be observed (write to tmp, then rename), and a failure here
    // leaves the document usable and merely unprotected, not corrupt.
    let sidecar = codex_skew_evidence_saved_at_path(base);
    let sidecar_tmp = registry_cache_dir(base).join("codex-skew.saved-at.tmp");
    let payload = format!("{} {}\n", now_secs(), codex_skew_evidence_digest(text));
    if std::fs::write(&sidecar_tmp, payload).is_ok()
        && std::fs::rename(&sidecar_tmp, &sidecar).is_err()
    {
        let _ = std::fs::remove_file(&sidecar_tmp);
    }
    Ok(())
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

#[cfg(test)]
mod codex_skew_evidence_tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn evidence_is_due_when_missing_or_older_than_the_ttl_and_saved_atomically() {
        let dir = tempdir().unwrap();
        assert!(codex_skew_evidence_due(dir.path()), "nothing cached yet");

        save_codex_skew_evidence(dir.path(), "{\"schema_version\":1}").expect("save");
        let path = codex_skew_evidence_path(dir.path());
        assert_eq!(path, registry_cache_dir(dir.path()).join("codex-skew.json"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"schema_version\":1}"
        );
        assert!(
            !path.with_extension("json.tmp").exists(),
            "no temp file left behind"
        );

        let now = SystemTime::now();
        assert!(!codex_skew_evidence_due_at(dir.path(), now));
        assert!(!codex_skew_evidence_due_at(
            dir.path(),
            now + Duration::from_secs(TTL_SECS - 60)
        ));
        assert!(codex_skew_evidence_due_at(
            dir.path(),
            now + Duration::from_secs(TTL_SECS + 60)
        ));
        // A future-dated cache (clock rollback) is due, not trusted.
        assert!(codex_skew_evidence_due_at(
            dir.path(),
            now - Duration::from_secs(60 * 60)
        ));
    }

    #[test]
    fn a_saved_document_is_protected_for_a_week_then_replaceable() {
        let dir = tempdir().unwrap();
        assert!(
            !codex_skew_evidence_is_protected(dir.path()),
            "nothing saved"
        );
        save_codex_skew_evidence(dir.path(), "{}").expect("save");
        assert!(codex_skew_evidence_is_protected(dir.path()));
        // A restamp does not extend the protection window.
        touch_codex_skew_evidence(dir.path());
        let sidecar =
            std::fs::read_to_string(codex_skew_evidence_saved_at_path(dir.path())).unwrap();
        let (saved_at, digest) = sidecar.trim().split_once(' ').unwrap();
        assert!(now_secs().saturating_sub(saved_at.parse::<u64>().unwrap()) < 60);
        // Backdate the save past the window: no longer protected.
        std::fs::write(
            codex_skew_evidence_saved_at_path(dir.path()),
            format!(
                "{} {digest}\n",
                now_secs() - CODEX_SKEW_EVIDENCE_MONOTONE_SECS - 1
            ),
        )
        .unwrap();
        assert!(!codex_skew_evidence_is_protected(dir.path()));
        // A future-dated save (clock rollback) protects nothing either.
        std::fs::write(
            codex_skew_evidence_saved_at_path(dir.path()),
            format!("{} {digest}\n", now_secs() + 3600),
        )
        .unwrap();
        assert!(!codex_skew_evidence_is_protected(dir.path()));
        // A sidecar for a document that never landed protects nothing: the
        // cached text no longer matches its digest.
        std::fs::write(
            codex_skew_evidence_saved_at_path(dir.path()),
            format!("{} {digest}\n", now_secs()),
        )
        .unwrap();
        assert!(codex_skew_evidence_is_protected(dir.path()));
        std::fs::write(codex_skew_evidence_path(dir.path()), "{\"other\":1}").unwrap();
        assert!(!codex_skew_evidence_is_protected(dir.path()));
    }

    /// A wanted refetch that failed must keep the evidence due — paced, not
    /// forgotten — until a fetch lands, even though the old document is still
    /// inside its TTL.
    #[test]
    fn a_pending_refetch_keeps_fresh_evidence_due_and_paces_retries() {
        let dir = tempdir().unwrap();
        save_codex_skew_evidence(dir.path(), "{}").expect("save");
        let now = SystemTime::now();
        assert!(!codex_skew_evidence_due_at(dir.path(), now));

        mark_codex_skew_evidence_pending(dir.path()).expect("mark");
        // Just failed: back off. (`now` is taken after the marker is written —
        // a marker from the future would count as ancient, by design.)
        let now = SystemTime::now();
        assert!(!codex_skew_evidence_due_at(dir.path(), now));
        // Past the retry window: due again although the document is fresh.
        assert!(codex_skew_evidence_due_at(
            dir.path(),
            now + Duration::from_secs(CODEX_SKEW_EVIDENCE_RETRY_SECS + 1)
        ));
        // A successful save clears the marker.
        save_codex_skew_evidence(dir.path(), "{}").expect("save again");
        assert!(!codex_skew_evidence_due_at(
            dir.path(),
            now + Duration::from_secs(CODEX_SKEW_EVIDENCE_RETRY_SECS + 1)
        ));
        assert!(!codex_skew_evidence_pending_path(dir.path()).exists());

        // A landed-but-not-newer fetch restamps an expired document.
        let old = now - Duration::from_secs(TTL_SECS + 600);
        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(codex_skew_evidence_path(dir.path()))
            .unwrap();
        file.set_modified(old).unwrap();
        drop(file);
        assert!(codex_skew_evidence_due_at(dir.path(), now));
        mark_codex_skew_evidence_pending(dir.path()).expect("mark");
        touch_codex_skew_evidence(dir.path());
        assert!(!codex_skew_evidence_pending_path(dir.path()).exists());
        assert!(!codex_skew_evidence_due_at(dir.path(), SystemTime::now()));

        // Missing cache + recent failure: also backs off, then retries.
        std::fs::remove_file(codex_skew_evidence_path(dir.path())).unwrap();
        mark_codex_skew_evidence_pending(dir.path()).expect("mark");
        let now = SystemTime::now();
        assert!(!codex_skew_evidence_due_at(dir.path(), now));
        assert!(codex_skew_evidence_due_at(
            dir.path(),
            now + Duration::from_secs(CODEX_SKEW_EVIDENCE_RETRY_SECS + 1)
        ));
    }
}

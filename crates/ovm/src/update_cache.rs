//! Cache of known upstream versions per product.
//!
//! Full version indexes are populated whenever OVM fetches registry data for a
//! product. TTL is 24 hours — entries older than that are treated as stale and
//! ignored by freshness-aware readers.

use crate::error::Result;
use crate::product::Product;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const TTL_SECS: u64 = 60 * 60 * 24;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VersionIndex {
    #[serde(default)]
    pub versions: Vec<String>,
    #[serde(default)]
    pub dates: HashMap<String, String>,
    pub fetched_at: u64,
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

    pub fn is_fresh_for(&self, ttl_secs: u64) -> bool {
        now_secs().saturating_sub(self.fetched_at) <= ttl_secs
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
    load_version_index(base, product).filter(VersionIndex::is_fresh)
}

pub fn version_index_due(base: &Path, product: Product, interval_hours: u64) -> bool {
    if interval_hours == 0 {
        return true;
    }

    let ttl_secs = interval_hours.saturating_mul(60).saturating_mul(60);
    load_version_index(base, product)
        .map(|index| !index.is_fresh_for(ttl_secs))
        .unwrap_or(true)
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
    fn version_index_due_respects_custom_interval() {
        let dir = tempdir().unwrap();
        let index = VersionIndex {
            versions: vec!["2.1.91".into()],
            dates: HashMap::new(),
            fetched_at: now_secs().saturating_sub(2 * 60 * 60),
        };

        save_version_index(dir.path(), Product::Claude, &index).expect("save");

        assert!(version_index_due(dir.path(), Product::Claude, 1));
        assert!(!version_index_due(dir.path(), Product::Claude, 3));
    }

    #[test]
    fn zero_hour_interval_is_always_due() {
        let dir = tempdir().unwrap();
        let index = VersionIndex::new(vec!["2.1.91".into()], HashMap::new());

        save_version_index(dir.path(), Product::Claude, &index).expect("save");

        assert!(version_index_due(dir.path(), Product::Claude, 0));
    }

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
}

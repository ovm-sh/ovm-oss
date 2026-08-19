use crate::config::{OvmConfig, OvmDirs};
use crate::error::Result;
use crate::product::Product;
use crate::sources::registry::{self, LatestProbe};
use crate::update_cache::{self, VersionIndex};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

const LOCK_STALE_AFTER: Duration = Duration::from_secs(10 * 60);

pub fn spawn_if_due(dirs: &OvmDirs, config: &OvmConfig) {
    if !config.check_for_updates {
        return;
    }
    if std::env::var("OVM_DISABLE_BACKGROUND_REFRESH").is_ok_and(|value| value != "0") {
        return;
    }
    let latest_probe_due = update_cache::latest_probe_due(&dirs.base);
    let self_due = super::self_autoupdate::self_check_due(&dirs.base, config);
    if !latest_probe_due && !self_due {
        return;
    }

    // Resolve through any launcher symlink (e.g. the `pi` owned launcher) so
    // the detached child runs as the real `ovm` binary. This keeps the refresh
    // off the name-based launcher path even if the sentinel dispatch in `main`
    // ever regresses — belt-and-suspenders against the fork-storm failure mode.
    let Ok(exe) = std::env::current_exe().and_then(std::fs::canonicalize) else {
        return;
    };

    let _ = Command::new(exe)
        .arg("__refresh-cache")
        .env("OVM_BACKGROUND_REFRESH", "1")
        // The detached refresh may stage a self-update, but activation must be
        // left to a user-facing foreground invocation (so the `↑ OVM` line is
        // seen), never performed silently here.
        .env(super::self_autoupdate::SKIP_SELF_AUTOUPDATE_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

pub fn run_hidden() -> Result<()> {
    let dirs = OvmDirs::new()?;
    let config = OvmConfig::load(&dirs.config_file)?;
    if !config.check_for_updates {
        return Ok(());
    }

    dirs.ensure_base_dirs()?;
    let Some(_lock) = RefreshLock::acquire(&dirs.base)? else {
        return Ok(());
    };

    if update_cache::latest_probe_due(&dirs.base) {
        let _ = refresh_latest_probe(&dirs.base);
    }

    // Keep OVM itself current: refresh the cached latest self version and, under
    // policy `on`, stage a newer release for the next invocation to activate.
    super::self_autoupdate::refresh_self_if_due(&dirs, &config);

    Ok(())
}

fn refresh_latest_probe(base: &Path) -> Result<()> {
    let etag = update_cache::latest_probe_etag(base);
    let now = update_cache::now_secs();
    let pending = match registry::probe_latest_from_registry(etag.as_deref()) {
        Some(LatestProbe::NotModified) => {
            update_cache::record_latest_probe_not_modified(base, now)?
        }
        Some(LatestProbe::Modified { etag, summaries }) => {
            update_cache::record_latest_probe_modified(base, etag, summaries, now)?
        }
        None => {
            return update_cache::record_latest_probe_failure(base, now);
        }
    };
    for product in pending {
        if refresh_product_from_registry(base, product).unwrap_or(false) {
            let _ = update_cache::record_index_refresh_success(base, product);
        }
    }
    Ok(())
}

fn refresh_product_from_registry(base: &Path, product: Product) -> Result<bool> {
    let Some((versions, dates)) = registry::list_versions_from_registry(product) else {
        return Ok(false);
    };

    let index = VersionIndex::new(versions, dates);
    update_cache::save_version_index(base, product, &index)?;
    Ok(true)
}

struct RefreshLock {
    path: PathBuf,
}

impl RefreshLock {
    fn acquire(base: &Path) -> Result<Option<Self>> {
        let path = base.join("cache").join("refresh.lock");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        for _ in 0..2 {
            match create_lock_file(&path) {
                Ok(mut file) => {
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(Some(Self { path }));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if lock_is_stale(&path) {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    return Ok(None);
                }
                Err(error) => return Err(error.into()),
            }
        }

        Ok(None)
    }
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn create_lock_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn lock_is_stale(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return true;
    };
    let Ok(modified) = metadata.modified() else {
        return true;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|age| age > LOCK_STALE_AFTER)
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_blocks_concurrent_refreshes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = RefreshLock::acquire(dir.path())
            .expect("first lock")
            .expect("acquired");
        let second = RefreshLock::acquire(dir.path()).expect("second lock");

        assert!(second.is_none());

        drop(first);
        let third = RefreshLock::acquire(dir.path()).expect("third lock");
        assert!(third.is_some());
    }
}

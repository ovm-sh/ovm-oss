use crate::config::{
    install_source_is_complete, OvmConfig, OvmDirs, ProductDirs, VersionSource, COMPLETE_MARKER,
    INSTALLING_MARKER,
};
use crate::dev_metadata::{DevInstallMetadata, DevInstallMode};
use crate::error::{OvmError, Result};
use crate::hooks::{self, Hook};
use crate::product::Product;
use crate::sources::{codex, gcs, npm, pi, qm, registry};
use crate::symlink;
use crate::util::{create_new_file, make_handle_executable, write_new_file};
use console::style;
use fs4::{FileExt, TryLockError};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

struct InstallLock {
    _file: File,
    waited: bool,
}

#[derive(Debug)]
struct InstallSourcePaths {
    root: PathBuf,
    destination: PathBuf,
    legacy_metadata: Option<PathBuf>,
}

impl InstallSourcePaths {
    fn installing_marker(&self) -> PathBuf {
        self.root.join(INSTALLING_MARKER)
    }

    fn complete_marker(&self) -> PathBuf {
        self.root.join(COMPLETE_MARKER)
    }

    fn quarantine_path(&self) -> Result<PathBuf> {
        let parent = self.root.parent().ok_or_else(|| {
            OvmError::Config(format!("No parent directory for {}", self.root.display()))
        })?;
        let name = self.root.file_name().ok_or_else(|| {
            OvmError::Config(format!("No source name for {}", self.root.display()))
        })?;
        Ok(parent.join(format!(".{}.incomplete", name.to_string_lossy())))
    }

    /// Marker-aware completeness with compatibility for installs created before
    /// markers existed. An `.installing` marker always wins: a process may have
    /// exposed the binary before it crashed, so that source must be recovered.
    fn is_complete(&self) -> bool {
        install_source_is_complete(
            &self.root,
            &self.destination,
            self.legacy_metadata.as_deref(),
        )
    }
}

/// An installed version that retention pruning has selected, with the bytes it
/// occupies. Sizes are read before anything is removed, so the user can be told
/// what is about to go instead of what already went.
#[derive(Debug, Clone)]
pub struct PruneCandidate {
    pub version: String,
    pub bytes: u64,
}

pub struct VersionManager {
    pub dirs: OvmDirs,
    pub product_dirs: ProductDirs,
    pub config: OvmConfig,
}

pub enum InstallRequest {
    Standard {
        use_npm: bool,
        version: String,
    },
    Dev {
        label: String,
        source: DevInstallSource,
        link: bool,
    },
    /// Publish a binary already on this machine as the managed install for
    /// `version`, with no download. Used by `ovm adopt`.
    Import {
        version: String,
        binary: PathBuf,
    },
}

pub enum DevInstallSource {
    Binary(PathBuf),
    Bundle(PathBuf),
}

impl DevInstallSource {
    /// The single regular file a dev install reads its bytes from.
    ///
    /// `--bundle` is a convenience for "the directory my build landed in", not
    /// a second install shape: it names the same one executable `--binary`
    /// would have named. Only Codex supports dev installs
    /// ([`Product::supports_dev_installs`]), and a Codex install *is* one
    /// binary — nothing else in the bundle is copied, linked or read. That is
    /// what lets [`VersionManager::install_dev`] bind either source to a single
    /// open descriptor; a product whose dev install had to carry a whole tree
    /// could not be defended the same way and would need its own reasoning.
    fn resolve_binary(&self, product: Product) -> PathBuf {
        match self {
            Self::Binary(path) => path.clone(),
            Self::Bundle(path) => path.join(product.binary_name()),
        }
    }
}

impl VersionManager {
    pub fn new(product: Product) -> Result<Self> {
        let dirs = OvmDirs::new()?;
        let config = OvmConfig::load(&dirs.config_file)?;
        Ok(Self::with(dirs, config, product))
    }

    /// Build a manager from directories and a config the caller already has.
    ///
    /// [`Self::new`] re-reads and re-parses `config.json` per manager, which is
    /// pure waste for a caller that builds one manager per product from a single
    /// load (`ovm cleanup`, the launch-path survey).
    pub fn with(dirs: OvmDirs, config: OvmConfig, product: Product) -> Self {
        let product_dirs = dirs.product_dirs(product);
        Self {
            dirs,
            product_dirs,
            config,
        }
    }

    pub fn product(&self) -> Product {
        self.product_dirs.product
    }

    /// "<Product> <version> is not installed" — the same refusal `use`,
    /// `uninstall` and `archive` each owe a caller who named a version the
    /// store does not have.
    fn not_installed_error(&self, version: &str) -> OvmError {
        OvmError::Message(format!(
            "{} {version} is not installed. Run: {}",
            self.product().display_name(),
            self.product().install_example(version)
        ))
    }

    /// The refusal for a destructive `action` ("uninstall", "archive") aimed at
    /// the version that is currently selected.
    fn cannot_touch_active_version(&self, action: &str, version: &str) -> OvmError {
        OvmError::Message(format!(
            "Cannot {action} active {} version {version}. Switch first: {}",
            self.product().canonical_name(),
            self.product().use_example("other-version")
        ))
    }

    pub fn list_installed(&self) -> Result<Vec<String>> {
        Ok(self
            .list_installed_with_sources()?
            .into_iter()
            .map(|(version, _)| version)
            .collect())
    }

    /// Every installed version with the sources that make it installed.
    ///
    /// Deciding whether a version counts as installed *is* the source scan, so
    /// callers that need the sources afterwards (retention planning, `ovm list`)
    /// take them from here instead of paying for a second scan of the same
    /// directories.
    pub fn list_installed_with_sources(&self) -> Result<Vec<(String, Vec<VersionSource>)>> {
        let mut versions = Vec::new();

        if !self.product_dirs.versions.exists() {
            return Ok(versions);
        }

        for entry in fs::read_dir(&self.product_dirs.versions)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if fs::read_dir(entry.path())?.next().transpose()?.is_none() {
                    continue;
                }
                let file_name = entry.file_name();
                if let Some(name) = file_name.to_str() {
                    // A killed install leaves a non-empty tree with an
                    // `.installing` marker and no complete source; listing it
                    // as installed presents phantom state.
                    let sources = self.product_dirs.version_sources(name);
                    if sources.is_empty() {
                        continue;
                    }
                    versions.push((name.to_string(), sources));
                }
            }
        }

        let product = self.product();
        versions.sort_by(|left, right| product.compare_version_strings(&left.0, &right.0));
        Ok(versions)
    }

    pub fn list_remote_versions(&self) -> Result<Vec<String>> {
        let (versions, _) = self.list_remote_versions_with_dates()?;
        Ok(versions)
    }

    pub fn list_remote_versions_with_dates(
        &self,
    ) -> Result<(Vec<String>, HashMap<String, String>)> {
        // Fast path: a fresh registry cache wins outright.
        if let Some(index) =
            crate::update_cache::load_fresh_version_index(&self.dirs.base, self.product())
        {
            let (mut versions, dates) = index.into_parts();
            self.product().sort_versions(&mut versions);
            return Ok((versions, dates));
        }

        // Registry next — single fetch, all products covered.
        if let Some((versions, dates)) = registry::list_versions_from_registry(self.product()) {
            let mut versions = versions;
            self.product().sort_versions(&mut versions);
            let index = crate::update_cache::VersionIndex::new(versions.clone(), dates.clone());
            let _ =
                crate::update_cache::save_version_index(&self.dirs.base, self.product(), &index);
            return Ok((versions, dates));
        }

        // Upstream APIs (npm / GitHub / Pi releases). Slowest but freshest.
        match self.fetch_upstream_versions() {
            Ok((mut versions, dates)) => {
                self.product().sort_versions(&mut versions);
                let index = crate::update_cache::VersionIndex::new(versions.clone(), dates.clone());
                let _ = crate::update_cache::save_version_index(
                    &self.dirs.base,
                    self.product(),
                    &index,
                );
                Ok((versions, dates))
            }
            Err(upstream_err) => {
                // Last resort: a stale cache beats failing the command outright.
                if let Some(index) =
                    crate::update_cache::load_version_index(&self.dirs.base, self.product())
                {
                    eprintln!(
                        "  {} Upstream unreachable, falling back to cached versions ({})",
                        style("!").yellow(),
                        style(format!("error: {upstream_err}")).dim()
                    );
                    let (mut versions, dates) = index.into_parts();
                    self.product().sort_versions(&mut versions);
                    return Ok((versions, dates));
                }
                Err(upstream_err)
            }
        }
    }

    fn fetch_upstream_versions(&self) -> Result<(Vec<String>, HashMap<String, String>)> {
        let versions = match self.product() {
            Product::Claude => npm::list_remote_versions()?
                .into_iter()
                .map(|version| version.to_string())
                .collect(),
            Product::Codex => codex::list_remote_versions()?,
            Product::Pi => pi::list_remote_versions()?,
            Product::Qm => qm::list_remote_versions()?,
        };
        Ok((versions, HashMap::new()))
    }

    pub fn current_version(&self) -> Result<Option<String>> {
        symlink::read_current_version(&self.product_dirs.current)
    }

    pub fn active_binary_path(&self, version: &str) -> PathBuf {
        self.product_dirs.resolved_binary(version)
    }

    pub fn standard_install_is_complete(&self, version: &str) -> bool {
        if self.product().is_bundle() {
            self.product_dirs.bundle_install_is_complete(version)
        } else {
            self.standard_source_paths(version, false).is_complete()
        }
    }

    /// Reject a launch-supplied version that could escape the version store.
    /// The launch path accepts any installed version (dev, pinned, official),
    /// so it can't use the stricter `validate_storage_version_component`, but
    /// it must still block path separators / traversal before the string
    /// becomes a filesystem path handed to `exec` (`active_binary_path`).
    pub fn reject_version_traversal(&self, version: &str) -> Result<()> {
        if has_path_separator_or_traversal(version) {
            return Err(OvmError::Message(
                "Versions cannot contain path separators or traversal components.".into(),
            ));
        }
        Ok(())
    }

    pub fn install_is_complete(&self, version: &str) -> bool {
        if version.starts_with("dev:") {
            return self.dev_source_paths(version).is_complete();
        }

        match self.product() {
            Product::Claude => {
                self.standard_source_paths(version, false).is_complete()
                    || self.standard_source_paths(version, true).is_complete()
            }
            Product::Codex => self.standard_source_paths(version, false).is_complete(),
            Product::Pi | Product::Qm => self.product_dirs.bundle_install_is_complete(version),
        }
    }

    pub fn version_sources(&self, version: &str) -> Vec<VersionSource> {
        self.product_dirs.version_sources(version)
    }

    pub fn dev_install_metadata(&self, version: &str) -> Result<Option<DevInstallMetadata>> {
        DevInstallMetadata::read(&self.product_dirs.dev_meta_path(version))
    }

    pub fn version_exists(&self, version: &str) -> bool {
        self.product_dirs.version_dir(version).exists()
    }

    pub fn use_version(&self, version: &str) -> Result<()> {
        let follow_latest = version == "latest";
        let version = if follow_latest {
            self.latest_installed_release()?.ok_or_else(|| {
                OvmError::Message(format!(
                    "No installed release versions found for {}. Run: {}",
                    self.product().display_name(),
                    self.product().install_example("latest")
                ))
            })?
        } else {
            self.product().normalize_version(version)
        };
        validate_storage_version_component(self.product(), &version)?;

        if !self.version_exists(&version) {
            return Err(self.not_installed_error(&version));
        }

        let binary = self.active_binary_path(&version);
        if !binary.exists() {
            return Err(OvmError::Message(format!(
                "{} {version} is archived. Reinstall with: {}",
                self.product().display_name(),
                self.product().install_example(&version)
            )));
        }
        if !self.install_is_complete(&version) {
            return Err(OvmError::Message(format!(
                "{} {version} has an incomplete install. Retry with: {}",
                self.product().display_name(),
                self.product().install_example(&version)
            )));
        }

        hooks::run_hook(&self.dirs.hooks, Hook::PreSwitch, &version);
        self.ensure_dirs()?;

        symlink::switch_symlink(
            &self.product_dirs.current,
            &self.product_dirs.version_dir(&version),
        )?;
        // Point the `~/.ovm/bin/<product>` launcher at OVM's stable entrypoint so
        // launching a managed product routes through multi-call dispatch and runs
        // `maybe_auto_update` first. Direct self-managed installs must target the
        // control plane rather than pinning one immutable OVM version; Homebrew,
        // Cargo, and checkout binaries continue to target the running executable.
        // Claude stays OVM-owned through this same path — the `~/.local/bin/claude`
        // probe still targets `~/.ovm/bin/claude`.
        let active_executable = std::fs::canonicalize(std::env::current_exe()?)?;
        let self_manager = crate::self_manager::SelfManager::at(self.dirs.clone());
        let active_launcher = if self_manager.is_managed_version_executable(&active_executable) {
            self_manager.control_plane_path()
        } else {
            active_executable
        };
        symlink::switch_symlink(&self.product_dirs.active_bin, &active_launcher)?;

        // Record this as a deliberate pin: a plain launch under auto-update `on`
        // must not silently replace a version the user chose. `use latest` and the
        // other follow-latest paths (`ovm <product> latest`, launch-time
        // auto-update) clear it instead, so "no pin file" means "track latest".
        // Best-effort: a failed write only means the next launch may auto-update,
        // never a broken switch.
        if follow_latest {
            self.clear_pin();
        } else {
            let _ = std::fs::write(&self.product_dirs.pin, format!("{version}\n"));
        }

        hooks::run_hook(&self.dirs.hooks, Hook::PostSwitch, &version);
        Ok(())
    }

    /// The version the user explicitly switched to, when the active selection is
    /// a deliberate pin rather than latest-tracking. `None` means "track latest"
    /// (auto-update `on` may advance the active version freely).
    pub fn read_pin(&self) -> Option<String> {
        std::fs::read_to_string(&self.product_dirs.pin)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    }

    /// Drop the pin so plain launches track latest again. Called by the
    /// follow-latest paths (`ovm <product> latest`, auto-update apply).
    pub fn clear_pin(&self) {
        let _ = std::fs::remove_file(&self.product_dirs.pin);
    }

    fn latest_installed_release(&self) -> Result<Option<String>> {
        let versions = self.list_installed()?;
        Ok(versions.into_iter().rev().find(|version| {
            self.product().parsed_release_version(version).is_some()
                && self.install_is_complete(version)
        }))
    }

    pub fn install(&self, request: InstallRequest) -> Result<String> {
        match request {
            InstallRequest::Standard { use_npm, version } => {
                self.install_standard(&self.product().normalize_version(&version), use_npm)
            }
            InstallRequest::Dev {
                label,
                source,
                link,
            } => self.install_dev(&label, source, link),
            InstallRequest::Import { version, binary } => {
                self.install_import(&self.product().normalize_version(&version), &binary)
            }
        }
    }

    pub fn uninstall(&self, version: &str) -> Result<()> {
        let version = self.product().normalize_version(version);
        validate_storage_version_component(self.product(), &version)?;

        if !self.version_exists(&version) {
            return Err(self.not_installed_error(&version));
        }

        if let Some(current) = self.current_version()? {
            if current == version {
                return Err(self.cannot_touch_active_version("uninstall", &version));
            }
        }

        hooks::run_hook(&self.dirs.hooks, Hook::PreUninstall, &version);
        fs::remove_dir_all(self.product_dirs.version_dir(&version))?;
        hooks::run_hook(&self.dirs.hooks, Hook::PostUninstall, &version);
        Ok(())
    }

    pub fn clean(&self, version: &str) -> Result<u64> {
        let version = self.product().normalize_version(version);
        validate_storage_version_component(self.product(), &version)?;
        let version_dir = self.product_dirs.version_dir(&version);
        let mut freed = 0;

        for path in [
            version_dir.join("raw"),
            version_dir.join("npm").join("raw"),
            version_dir.join("release").join("raw"),
        ] {
            if path.exists() {
                freed += dir_size(&path)?;
                fs::remove_dir_all(path)?;
            }
        }

        Ok(freed)
    }

    pub fn archive(&self, version: &str) -> Result<u64> {
        let version = self.product().normalize_version(version);
        validate_storage_version_component(self.product(), &version)?;
        let version_dir = self.product_dirs.version_dir(&version);
        if !version_dir.exists() {
            return Err(self.not_installed_error(&version));
        }

        if let Some(current) = self.current_version()? {
            if current == version {
                return Err(self.cannot_touch_active_version("archive", &version));
            }
        }

        self.archive_version_dirs(&version)
    }

    fn archive_version_dirs(&self, version: &str) -> Result<u64> {
        let mut freed = 0;
        for path in self.archivable_paths(version) {
            if path.exists() {
                freed += dir_size(&path)?;
                fs::remove_dir_all(path)?;
            }
        }
        Ok(freed)
    }

    pub fn archive_below(&self, min_version: &str) -> Result<(u64, usize)> {
        let min_version = self.product().normalize_version(min_version);
        validate_storage_version_component(self.product(), &min_version)?;
        let min = self
            .product()
            .parsed_release_version(&min_version)
            .ok_or_else(|| OvmError::VersionNotFound(min_version.clone()))?;
        let current = self.current_version()?;
        let mut total_freed = 0;
        let mut count = 0;

        for version in self.list_installed()? {
            if current.as_deref() == Some(version.as_str()) {
                continue;
            }
            let Some(candidate) = self.product().parsed_release_version(&version) else {
                continue;
            };

            if candidate < min {
                let freed = self.archive_version_dirs(&version)?;
                if freed > 0 {
                    count += 1;
                    total_freed += freed;
                }
            }
        }

        Ok((total_freed, count))
    }

    /// Installed versions that retention pruning is eligible to act on: not the
    /// active version, not a dev build, not already archived, and untouched for
    /// at least `days`.
    ///
    /// Inspection only — nothing is removed and no install tree is measured.
    /// It is not free, though: every installed version is stat-walked for its
    /// sources and its mtime, which on a large store is thousands of syscalls.
    /// The launch path therefore asks at most once per update interval (see
    /// [`crate::commands::cleanup::prune_all_products`]); the explicit `ovm
    /// cleanup` commands ask every time.
    pub fn plan_inactive_installs_older_than(&self, days: u64) -> Result<Vec<String>> {
        let Some(current) = self.current_version()? else {
            return Ok(Vec::new());
        };
        let cutoff = Duration::from_secs(days.saturating_mul(24 * 60 * 60));
        let now = SystemTime::now();
        let mut planned = Vec::new();

        // Sources come from the listing that already computed them — asking
        // `version_sources` again here re-walks every version directory.
        for (version, sources) in self.list_installed_with_sources()? {
            if version == current || version.starts_with("dev:") {
                continue;
            }
            if sources.contains(&VersionSource::Archived) || sources.contains(&VersionSource::Dev) {
                continue;
            }

            let version_dir = self.product_dirs.version_dir(&version);
            if !version_dir_is_older_than(&version_dir, cutoff, now) {
                continue;
            }

            planned.push(version);
        }

        Ok(planned)
    }

    /// Measure what each planned version currently occupies, so callers can say
    /// what will go *before* it goes. An unreadable directory measures as zero
    /// rather than aborting the report.
    pub fn measure_versions(&self, versions: &[String]) -> Vec<PruneCandidate> {
        versions
            .iter()
            .map(|version| PruneCandidate {
                version: version.clone(),
                bytes: dir_size(&self.product_dirs.version_dir(version)).unwrap_or(0),
            })
            .collect()
    }

    /// Permanently delete the given versions. Irreversible — reserved for paths
    /// the user asked for explicitly.
    pub fn remove_versions(&self, versions: &[String]) -> Result<(u64, usize)> {
        let mut total_freed = 0;
        let mut count = 0;

        for version in versions {
            if !self.version_exists(version) {
                continue;
            }
            // Re-read `current` immediately before removing THIS version, not
            // once for the whole batch. The plan was built earlier and the user
            // may have run `ovm use <version>` in another terminal since; acting
            // on a stale plan would leave `current` pointing at a version we
            // just deleted, and the next launch would fail on a version that had
            // only just been selected.
            if self.current_version()?.as_deref() == Some(version.as_str()) {
                continue;
            }
            let freed = dir_size(&self.product_dirs.version_dir(version))?;
            self.uninstall(version)?;
            total_freed += freed;
            count += 1;
        }

        Ok((total_freed, count))
    }

    pub fn clean_all(&self) -> Result<u64> {
        let mut total = 0;
        for version in self.list_installed()? {
            total += self.clean(&version)?;
        }
        Ok(total)
    }

    fn install_standard(&self, version: &str, use_npm: bool) -> Result<String> {
        if use_npm && !self.product().supports_npm() {
            return Err(OvmError::Message(format!(
                "{} does not support npm installs.",
                self.product().display_name()
            )));
        }

        let (version, resolved_from_latest) = if version == "latest" {
            eprintln!("  {} Resolving latest version...", style("→").dim());
            (self.resolve_latest(use_npm)?, true)
        } else {
            (version.to_string(), false)
        };
        validate_storage_version_component(self.product(), &version)?;

        let install_lock = self.acquire_install_lock(&version)?;
        let source = self.standard_source_paths(&version, use_npm);

        let source_is_complete = if self.product().is_bundle() {
            self.product_dirs.bundle_install_is_complete(&version)
        } else {
            source.is_complete()
        };
        if source_is_complete {
            if install_lock.waited {
                self.report_reused_install(&version);
                return Ok(version);
            }
            if resolved_from_latest {
                eprintln!(
                    "  {} {} {} already installed",
                    style("✓").green(),
                    self.product().display_name(),
                    style(&version).green().bold()
                );
                return Ok(version);
            }
            return Err(OvmError::VersionAlreadyInstalled(version));
        }

        if install_lock.waited {
            self.report_taking_over_install(&version);
        }
        let result = self.run_install_transaction(&version, &source, || {
            match (self.product(), use_npm) {
                (Product::Claude, false) => self.install_claude_native(&version)?,
                (Product::Claude, true) => self.install_claude_npm(&version)?,
                (Product::Codex, false) => self.install_codex_release(&version)?,
                (Product::Codex, true) => unreachable!("checked above"),
                (Product::Pi, false) => self.install_pi_release(&version)?,
                (Product::Pi, true) => unreachable!("checked above"),
                (Product::Qm, false) => self.install_qm_release(&version)?,
                (Product::Qm, true) => unreachable!("checked above"),
            }
            Ok(version.clone())
        });
        drop(install_lock);
        result
    }

    /// Publish `binary` as the managed install for `version` — the adoption
    /// path, where the bytes come off the local disk instead of the network.
    ///
    /// Everything *except* where the bytes come from is the download install's
    /// transaction, deliberately: the per-version install lock, quarantining
    /// any pre-existing source tree instead of writing into it, `.installing` →
    /// contents → `.complete`, and a cleanup that can only remove what this
    /// call created. An import that rolled its own protocol could publish
    /// `.complete` over a tree a concurrent `ovm install` was still writing —
    /// activating a mixed binary — and its error path could delete that other
    /// process's live install root.
    ///
    /// The bytes themselves are staged and proven before anything is published;
    /// see [`Self::stage_verify_and_publish_import`].
    ///
    /// Validation here is bound to an open file handle rather than to a path.
    /// A check that resolves `binary` and then hands the *path* on leaves the
    /// window every later step re-traverses: the pre-lock refusal below can
    /// accept a genuinely foreign target, the process can then sit in
    /// `acquire_install_lock` for as long as another OVM holds it, and a
    /// symlink re-pointed during that wait would have the copy read something
    /// else entirely — including this version's own managed tree, which the
    /// transaction deletes before the copy runs. So once the lock is held the
    /// source is resolved and opened *once* ([`BoundSource`]), the
    /// store-containment refusal is re-decided on that resolved path, and the
    /// copy reads the handle. See [`BoundSource`] for what the binding
    /// does and does not guarantee.
    ///
    /// Containment is decided on a *name*, though, and a name is only ever a
    /// claim about the moment it was read — so the last check before the
    /// transaction is one that cannot be raced at all:
    /// [`Self::reject_import_of_a_file_this_install_deletes`] asks whether the
    /// open handle *is* one of the files the transaction would remove.
    fn install_import(&self, version: &str, binary: &Path) -> Result<String> {
        if self.product().is_bundle() {
            // A single copied file is not a bundle install; the bundle layout would
            // be missing everything but the executable.
            return Err(OvmError::Message(format!(
                "{} installs are bundles and cannot be imported from a single binary.",
                self.product().display_name()
            )));
        }
        validate_storage_version_component(self.product(), version)?;
        if !binary.is_file() {
            return Err(OvmError::Message(format!(
                "Cannot import {}: not a file",
                binary.display()
            )));
        }
        self.reject_import_from_the_store(binary)?;

        let install_lock = self.acquire_install_lock(version)?;
        let source = self.standard_source_paths(version, false);

        // Unlike an explicit `ovm install`, an already-complete version is the
        // outcome adoption wanted, not an error — including when another
        // process published it while we waited on the lock.
        if source.is_complete() {
            if install_lock.waited {
                self.report_reused_install(version);
            }
            return Ok(version.to_string());
        }

        // Bind the source to one open handle, and re-decide containment on the
        // path that handle came from. The pre-lock refusal above described the
        // path as it was *before* the wait on the install lock; this one
        // describes the file the transaction is actually about to read, and it
        // still happens before `prepare_install_source` can remove anything.
        let bound = BoundSource::open(binary, BoundSourceUse::Import)?;
        self.reject_import_from_the_store(bound.resolved())?;
        self.reject_import_of_a_file_this_install_deletes(version, &bound, &source)?;

        if install_lock.waited {
            self.report_taking_over_install(version);
        }
        let result = self.run_install_transaction(version, &source, || {
            self.stage_verify_and_publish_import(&bound, &source, version)?;
            Ok(version.to_string())
        });
        drop(install_lock);
        result
    }

    /// Refuse an import whose bytes come from inside `~/.ovm` — before the
    /// transaction is entered, so nothing is locked, moved or removed.
    ///
    /// The transaction quarantines and removes this version's existing source
    /// tree *before* the import closure copies anything
    /// ([`Self::prepare_install_source`]). A source path under that tree is
    /// therefore deleted while it is still needed: the copy fails with ENOENT
    /// and the user's file is simply gone — after which `ovm adopt` would have
    /// printed "Original install left untouched". An incomplete managed install
    /// (binary present, no `.complete`) is exactly the shape that reaches this.
    ///
    /// `ovm adopt` rejects such a path at the CLI with a message naming the
    /// command to run instead; this is the floor under that check, so no other
    /// current or future caller can reproduce the deletion. Symlinks cannot
    /// smuggle a path past it: both sides are resolved before they are compared.
    ///
    /// [`Self::install_import`] asks twice. The first ask is the courtesy: it
    /// refuses before the process waits on the install lock, so the user gets
    /// the error immediately and nothing is locked, moved or removed. The
    /// second ask is the one that is load-bearing — it runs after the lock is
    /// held, on the path an already-open handle resolved from, so a link
    /// re-pointed during the wait is judged as what it now is rather than as
    /// what it was.
    fn reject_import_from_the_store(&self, binary: &Path) -> Result<()> {
        if !path_is_inside(&self.dirs.base, binary) {
            return Ok(());
        }

        Err(OvmError::Message(format!(
            "Cannot import {} as managed {}: it is already inside OVM's store ({}). \
             A managed install is repaired with `ovm install {name} <version>` and selected \
             with `ovm use {name} <version>` — importing it would delete it.",
            binary.display(),
            self.product().display_name(),
            self.dirs.base.display(),
            name = self.product().canonical_name(),
        )))
    }

    /// Refuse an import whose open handle **is** a file this install is about
    /// to delete.
    ///
    /// [`Self::reject_import_from_the_store`] is a judgement about a path, and
    /// a path is re-resolved every time it is looked at. `O_NOFOLLOW` binds the
    /// final component only: an attacker who swaps an *intermediate* directory
    /// of the user's path for a link into the store during the canonicalize →
    /// open window gets a handle on a managed file that every name-based check
    /// still describes as foreign — the resolution saw the real directory, the
    /// open saw the link, and restoring the directory afterwards makes the
    /// containment re-check agree that the path is outside the store. The
    /// transaction then deletes this version's source tree
    /// ([`Self::prepare_install_source`]) with the import still holding it.
    /// The same divergence needs no race at all to reach: a hard link outside
    /// the store names a managed inode truthfully, and canonicalization has
    /// nothing to unwind.
    ///
    /// So this asks a question no path game can change the answer to: `fstat`
    /// the descriptor OVM already holds, walk the files this transaction may
    /// remove ([`file_this_install_deletes`]), and refuse if the handle is one
    /// of them. Whatever the name says, the handle cannot be a file inside the
    /// deletion set and outside it at the same time.
    ///
    /// What this does not claim: it defends the destructive outcome, not the
    /// tidiness of the path. A handle bound to a managed file *outside* the
    /// deletion set — another version's tree — still gets past here, and is
    /// refused by the containment check whenever the name is inside the store.
    /// The residue is the narrow case of a foreign name whose bytes are some
    /// other version's managed file: that import copies bytes the user could
    /// have copied themselves, deletes nothing, and is published only if it
    /// passes the signature and `--version` proofs like any other source.
    fn reject_import_of_a_file_this_install_deletes(
        &self,
        version: &str,
        bound: &BoundSource,
        source: &InstallSourcePaths,
    ) -> Result<()> {
        let Some(doomed) = file_this_install_deletes(bound, source)? else {
            return Ok(());
        };

        Err(OvmError::Message(format!(
            "Cannot import {} as managed {} {version}: it is the same file as {}, which this \
             install removes before it copies anything — the import would delete the bytes it \
             was about to read. A managed install is repaired with `ovm install {name} \
             {version}` and selected with `ovm use {name} {version}`. Nothing was installed and \
             nothing was removed.",
            bound.requested().display(),
            self.product().display_name(),
            doomed.display(),
            name = self.product().canonical_name(),
        )))
    }

    /// Capture `binary` into the transaction's staging area, prove the captured
    /// copy, and only then publish it under the name the store hands out.
    ///
    /// Adoption labels a file the user already has with a version that file
    /// *reported*. Reading the version at the original path and copying that
    /// path later leaves a window where the two are not the same bytes: an
    /// updater (`brew upgrade`, `npm i -g`, the product's own self-update)
    /// landing between the two steps would have OVM publish the new build under
    /// the old build's version. So the bytes are captured once, and every check
    /// runs on the capture:
    ///
    ///   * the publisher's macOS code signature, through the same
    ///     [`crate::sources::verify_product_binary`] a download goes through —
    ///     so "managed by OVM" means one thing however the bytes arrived, and
    ///     it stays a no-op off macOS and for products that ship unsigned,
    ///     exactly as for downloads;
    ///   * `--version` again, on the staged copy, which must still report the
    ///     version being installed;
    ///   * the source file's identity — size, mtime, and on unix the
    ///     device/inode pair — read from the open handle immediately before the
    ///     copy and again once it has finished, which must be unchanged.
    ///
    /// That last check exists because the version re-check is not by itself a
    /// consistency proof. Off macOS there is no signature to fall back on, and
    /// an updater rewriting the executable *in place* while the copy reads it
    /// can yield a mixed old/new byte stream that still answers `--version`
    /// plausibly. Be honest about its reach now that the source is a handle
    /// rather than a name ([`BoundSource`]): a *swap* — replace-by-rename,
    /// or a re-pointed symlink — can no longer affect the copy at all, because
    /// the handle keeps reading the file that was resolved, opened and checked
    /// for containment. What is left for the before/after comparison is the
    /// in-place rewrite, which it catches when the rewrite finishes during — or
    /// continues past — the copy window. It cannot catch a writer that begins
    /// and ends entirely between the first `fstat` and the first byte read, nor
    /// a same-size rewrite landing inside a single mtime tick; perfect
    /// atomicity against in-place writers is not reachable from userspace. The
    /// `--version` re-check and, on macOS, the publisher signature remain the
    /// deeper checks — this narrows the window they sit in.
    ///
    /// Publishing is a rename *within* the staging directory, so the file that
    /// was verified is the file that appears — the user's path is never
    /// traversed again after it was opened, and nothing crosses a filesystem
    /// boundary at that step (the one copy that may, the handle → staging,
    /// happens before any check).
    ///
    /// This defends the version label, not the machine. Someone who already
    /// controls the user's disk can hand OVM a matching binary and OVM will
    /// believe it; what this removes is the far likelier accident, an ordinary
    /// upgrade racing an adopt, and it closes the gap where an imported Claude
    /// or Codex skipped the signature check its downloaded twin must pass.
    fn stage_verify_and_publish_import(
        &self,
        binary: &BoundSource,
        source: &InstallSourcePaths,
        version: &str,
    ) -> Result<()> {
        let staged = staged_import_path(&source.destination)?;
        crate::util::ensure_parent_dir(&staged)?;
        // Every read below goes through the handle opened before the
        // transaction started: `fstat` for the identities, the same descriptor
        // for the bytes. Nothing here resolves a name, so re-pointing the
        // user's path mid-copy is not merely detected, it is inert.
        let before = binary.identity()?;
        binary.copy_to_new_file(&staged)?;
        let after = binary.identity()?;
        refuse_if_source_changed_during_copy(self.product(), binary.requested(), &before, &after)?;

        crate::sources::verify_product_binary(self.product(), &staged)?;

        let reported = crate::commands::adopt::reported_store_version(self.product(), &staged)?;
        if reported != version {
            return Err(OvmError::Message(format!(
                "{} at {} reports {reported}, not the {version} it was about to be installed as. \
                 The file changed between being read and being copied (an upgrade landing \
                 mid-adopt looks exactly like this); nothing was installed. Re-run the adopt.",
                self.product().display_name(),
                binary.requested().display(),
            )));
        }

        // `rename` is the one publish step that needs no `O_EXCL` twin: it
        // operates on names, not on what they point at. POSIX (and both Linux
        // and macOS `rename(2)`) says a destination that exists is unlinked,
        // and that a symbolic link at either operand is itself the thing
        // renamed or replaced — never followed. So a link planted at
        // `source.destination` between prepare and here is destroyed by the
        // publish rather than written through, and its target is untouched.
        // `a_symlink_at_the_publish_target_is_replaced_not_followed` pins that
        // claim to this platform.
        fs::rename(&staged, &source.destination)?;
        Ok(())
    }

    fn resolve_latest_or_installed(
        &self,
        resolve_remote: impl FnOnce() -> Result<String>,
    ) -> Result<String> {
        match resolve_remote() {
            Ok(version) => Ok(version),
            Err(error) => {
                if let Some(version) = self.latest_installed_release()? {
                    eprintln!(
                        "  {} Could not reach update service; using latest installed {} {}",
                        style("!").yellow(),
                        self.product().display_name(),
                        style(&version).green().bold()
                    );
                    return Ok(version);
                }

                Err(OvmError::Message(format!(
                    "Could not resolve latest {} version and no installed release is available. Last error: {error}",
                    self.product().display_name()
                )))
            }
        }
    }

    pub fn latest_available_version(&self) -> Result<String> {
        self.resolve_latest(false)
    }

    /// Dispatch table for `install <product> latest`.
    fn resolve_latest(&self, use_npm: bool) -> Result<String> {
        match (self.product(), use_npm) {
            (Product::Claude, true) => self.resolve_latest_or_installed(npm::get_latest_version),
            (Product::Claude, false) => self.resolve_latest_or_installed(gcs::get_latest_version),
            (Product::Codex, false) => {
                self.resolve_latest_or_installed(|| self.resolve_codex_latest())
            }
            (Product::Pi, false) => self.resolve_latest_or_installed(pi::get_latest_version),
            (Product::Qm, false) => self.resolve_latest_or_installed(qm::get_latest_version),
            (Product::Codex, true) | (Product::Pi, true) | (Product::Qm, true) => {
                Err(OvmError::Message(format!(
                    "{} does not support npm latest resolution.",
                    self.product().display_name()
                )))
            }
        }
    }

    /// Codex publishes a stable npm `latest` tag plus platform tarballs, while
    /// GitHub's unauthenticated release API is easy to rate-limit. Prefer npm
    /// for explicit `latest`, then fall back to the OVM registry and GitHub.
    fn resolve_codex_latest(&self) -> Result<String> {
        let npm_error = match codex::get_latest_npm_release_version() {
            Ok(latest) => return Ok(latest),
            Err(error) => error,
        };

        if let Some((mut versions, dates)) = registry::list_versions_from_registry(Product::Codex) {
            self.product().sort_versions(&mut versions);
            let index = crate::update_cache::VersionIndex::new(versions, dates);
            let latest = index.latest(Product::Codex).map(str::to_string);
            let _ =
                crate::update_cache::save_version_index(&self.dirs.base, Product::Codex, &index);
            if let Some(latest) = latest {
                return Ok(latest);
            }
        }

        codex::get_latest_version().map_err(|github_error| {
            OvmError::Message(format!(
                "Could not resolve latest Codex from npm ({npm_error}) or GitHub ({github_error})"
            ))
        })
    }

    /// Publish a local build as `dev:<label>` — copied into the store, or
    /// linked to where the developer built it.
    ///
    /// Like an import ([`Self::install_import`]) this takes a path the user
    /// controls and hands it to a transaction that *deletes before it reads*:
    /// [`Self::prepare_install_source`] removes any incomplete `dev` tree for
    /// this label before the closure copies or links. So the source is bound to
    /// one open handle up front ([`BoundSource`]) and every later step is about
    /// that handle rather than about the name — the refusals below, the copy,
    /// and the target the `--link` symlink is published with. A name checked
    /// here and re-resolved inside the transaction is the shape that turns
    /// "install my build" into "delete the old install, then fail to find the
    /// new one" — or, in link mode, into a `.complete` version pointing at a
    /// link the user can later aim back at the destination itself.
    ///
    /// [`DevInstallSource::resolve_binary`] is what makes one descriptor
    /// enough: `--bundle` names the same single executable `--binary` does.
    fn install_dev(&self, label: &str, source: DevInstallSource, link: bool) -> Result<String> {
        if !self.product().supports_dev_installs() {
            return Err(OvmError::Message(format!(
                "{} does not support dev installs.",
                self.product().display_name()
            )));
        }

        validate_dev_label(label)?;
        let version = format!("dev:{label}");
        let source_binary = source.resolve_binary(self.product());
        if !source_binary.exists() {
            return Err(OvmError::Message(format!(
                "Dev source binary not found at {}",
                source_binary.display()
            )));
        }

        let install_lock = self.acquire_install_lock(&version)?;
        let install_paths = self.dev_source_paths(&version);
        if install_paths.is_complete() {
            if install_lock.waited {
                let requested_mode = if link {
                    DevInstallMode::Link
                } else {
                    DevInstallMode::Copy
                };
                let existing =
                    DevInstallMetadata::read(&self.product_dirs.dev_meta_path(&version))?
                        .ok_or_else(|| {
                            OvmError::Message(format!(
                                "{} {version} completed without dev metadata",
                                self.product().display_name()
                            ))
                        })?;
                // Compared against the path that was *asked for*, which is what
                // the closure records (see the metadata write below), so the
                // same command re-run recognizes the install another process
                // just published instead of refusing it.
                if existing.source != source_binary || existing.mode != requested_mode {
                    return Err(OvmError::Message(format!(
                        "{} {version} was installed by another OVM process from {} in {} mode; requested {} in {} mode",
                        self.product().display_name(),
                        existing.source.display(),
                        existing.mode.label(),
                        source_binary.display(),
                        requested_mode.label()
                    )));
                }
                self.report_reused_install(&version);
                return Ok(version);
            }
            return Err(OvmError::VersionAlreadyInstalled(version));
        }

        if install_lock.waited {
            self.report_taking_over_install(&version);
        }

        // Bind the source to one open handle before anything is decided about
        // it, exactly as an import does and for the same reason: everything
        // below happens *after* the wait on the install lock and either side of
        // a transaction whose prepare step deletes this version's incomplete
        // dev tree, so a name re-resolved down there can mean a different file
        // — or no file at all. See [`BoundSource`].
        let bound = BoundSource::open(&source_binary, BoundSourceUse::DevInstall)?;

        // Same shape as an import: the prepare step deletes any incomplete
        // source root before the closure copies (or links) — so a source
        // pointing INTO that root names a file about to be removed. Re-running
        // a dev install against the previously installed copy of an interrupted
        // install looks exactly like this. A complete install never gets here
        // (the short-circuit above answers first); refuse rather than delete
        // what we were asked to install.
        //
        // Decided on the *resolved* path rather than on the name that was
        // typed. `path_is_inside` resolves either side, so this is not about
        // a symlink reaching in — that was always caught. It is about which
        // file the refusal is a statement about: the typed name is re-resolved
        // at the instant it is read, while the bound resolution is the file the
        // handle is on and stays true through the copy. A source whose target
        // is in the doomed tree is the same self-deletion however it is
        // spelled; in link mode it would publish a destination that dangles the
        // moment prepare runs.
        let doomed = [install_paths.root.clone(), install_paths.quarantine_path()?];
        if doomed
            .iter()
            .any(|root| path_is_inside(root, bound.resolved()))
        {
            return Err(OvmError::Message(format!(
                "Dev source {}{} is inside the incomplete {version} install this command \
                 would replace, and would be deleted before it could be read. Point \
                 --binary/--bundle at your build output instead.",
                source_binary.display(),
                resolved_via(&source_binary, bound.resolved()),
            )));
        }

        // And the question a name cannot answer at all: is the handle *itself*
        // one of the files this transaction removes? A hard link into the dev
        // tree, or an intermediate directory swapped for a link into it during
        // the resolve → open window, is outside that tree by name and inside it
        // by identity.
        if let Some(doomed) = file_this_install_deletes(&bound, &install_paths)? {
            return Err(OvmError::Message(format!(
                "Dev source {} is the same file as {}, which this install removes before it \
                 copies anything — the install would delete the bytes it was about to read. \
                 Point --binary/--bundle at your build output instead. Nothing was installed \
                 and nothing was removed.",
                source_binary.display(),
                doomed.display(),
            )));
        }

        let result = self.run_install_transaction(&version, &install_paths, || {
            let destination = &install_paths.destination;
            let destination_parent = destination.parent().ok_or_else(|| {
                OvmError::Config(format!("No parent directory for {}", destination.display()))
            })?;
            fs::create_dir_all(destination_parent)?;

            if link {
                // Point the managed destination at the resolved file, not at
                // the name the user typed. A link-to-a-link would leave what
                // `~/.ovm/.../dev/bin/<binary>` means in the hands of a path
                // OVM checked once and never sees again — including the case
                // where that path is later retargeted at the destination
                // itself, closing a symlink cycle. Linking to the resolved
                // target ends the chain at the file the refusals above judged.
                symlink::switch_symlink(destination, bound.resolved())?;
            } else {
                // Copy from the handle, never from the name: `fs::copy` would
                // re-follow the path here, on the far side of the prepare step,
                // which is the window the refusals above cannot cover.
                //
                // Writing the published path directly (rather than a staged
                // sibling, as an import does) is safe: `prepare_install_source`
                // has just recreated `install_paths.root` empty, so nothing is
                // there to overwrite; this process holds the per-version
                // install lock; and a copy that fails never reaches the
                // `.complete` write, so the transaction's cleanup removes the
                // partial file along with the root. "Nothing is there to
                // overwrite" is enforced rather than assumed — the copy opens
                // the destination `O_EXCL` and refuses an entry it did not
                // create, so a link standing here is never written through to
                // its target.
                bound.copy_to_new_file(destination)?;
            }

            // Metadata records the path the user asked for, not the resolved
            // one. It is what the lock-contention reuse check above compares
            // against — so re-running the *same* command must produce the same
            // value — and what `ovm list` shows and the git-provenance lookup
            // walks up from. What the destination actually points at in link
            // mode is recorded by the symlink itself.
            let metadata = DevInstallMetadata::collect(
                source_binary,
                if link {
                    DevInstallMode::Link
                } else {
                    DevInstallMode::Copy
                },
            );
            write_new_file(
                &self.product_dirs.dev_meta_path(&version),
                serde_json::to_string_pretty(&metadata)?.as_bytes(),
            )?;

            eprintln!(
                "  {} Installed {} {} {}",
                style("✓").green(),
                self.product().display_name(),
                style(&version).green().bold(),
                style(if link { "(dev link)" } else { "(dev copy)" }).dim()
            );

            Ok(version.clone())
        });
        drop(install_lock);
        result
    }

    fn acquire_install_lock(&self, version: &str) -> Result<InstallLock> {
        let lock_dir = self
            .dirs
            .base
            .join("locks")
            .join("install")
            .join(self.product().canonical_name());
        fs::create_dir_all(&lock_dir)?;
        let lock_path = lock_dir.join(format!("{version}.lock"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;

        let waited = match FileExt::try_lock(&file) {
            Ok(()) => false,
            Err(TryLockError::WouldBlock) => {
                if std::env::var_os("OVM_HOOK").is_some()
                    && std::env::var_os("OVM_VERSION").as_deref()
                        == Some(std::ffi::OsStr::new(version))
                {
                    return Err(OvmError::Message(format!(
                        "Cannot wait for {} {version} from its own install hook",
                        self.product().display_name()
                    )));
                }
                eprintln!(
                    "  {} Waiting for another OVM process to install {} {}...",
                    style("…").cyan(),
                    self.product().display_name(),
                    style(version).bold()
                );
                FileExt::lock(&file)?;
                true
            }
            Err(TryLockError::Error(error)) => return Err(error.into()),
        };

        Ok(InstallLock {
            _file: file,
            waited,
        })
    }

    fn standard_source_paths(&self, version: &str, use_npm: bool) -> InstallSourcePaths {
        let version_dir = self.product_dirs.version_dir(version);
        match self.product() {
            Product::Claude if use_npm => InstallSourcePaths {
                root: version_dir.join("npm"),
                destination: self.product_dirs.npm_bin(version),
                legacy_metadata: None,
            },
            Product::Claude => {
                let root = version_dir.join("native");
                InstallSourcePaths {
                    legacy_metadata: Some(root.join("manifest.json")),
                    root,
                    destination: self.product_dirs.native_bin(version),
                }
            }
            Product::Pi | Product::Qm => {
                let root = version_dir.join("release");
                InstallSourcePaths {
                    legacy_metadata: Some(root.join("meta.json")),
                    root,
                    destination: self.product_dirs.bundle_bin(version),
                }
            }
            Product::Codex => {
                let root = version_dir.join("release");
                InstallSourcePaths {
                    legacy_metadata: Some(root.join("meta.json")),
                    root,
                    destination: self.product_dirs.release_bin(version),
                }
            }
        }
    }

    fn dev_source_paths(&self, version: &str) -> InstallSourcePaths {
        let root = self.product_dirs.version_dir(version).join("dev");
        InstallSourcePaths {
            legacy_metadata: Some(root.join("meta.json")),
            root,
            destination: self.product_dirs.dev_bin(version),
        }
    }

    fn run_install_transaction<T>(
        &self,
        version: &str,
        source: &InstallSourcePaths,
        install: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let version_dir = self.product_dirs.version_dir(version);
        let version_dir_existed_before = path_entry_exists(&version_dir);
        self.ensure_dirs()?;

        // The PreInstall hook runs *before* the source tree is prepared, and the
        // ordering is the security property rather than a detail.
        //
        // Every write this transaction makes creates its file rather than
        // opening one that is already there ([`create_new_file`]), which settles
        // what happens when the *final* component of a destination is swapped
        // for a symbolic link. It says nothing about a swapped *parent*: a
        // directory inside the fresh tree replaced by a link to somewhere else
        // is traversed, not opened, so an `O_EXCL` create, a `rename` publish
        // and an archive extraction all land outside the managed tree — the
        // import's rename replacing a file that was never OVM's to touch.
        //
        // The hook was the one supported window in which foreign code ran
        // between the tree being created and being written into, so the window
        // is removed instead of every parent being re-validated at every write:
        // `prepare_install_source` below renames the entire source root away and
        // recreates it, so nothing the hook did *inside* the tree — links,
        // directories, a `.complete` claiming the install already finished —
        // survives into the window where OVM writes. Interference is erased
        // rather than refused, and an ordinary hook (fetching credentials,
        // stopping a service) cannot tell the difference.
        //
        // The hook's contract is only that it runs before an install
        // ([`crate::hooks`]); it was never promised a prepared tree. What
        // remains able to swap a parent mid-transaction is a process writing
        // into OVM's own store for a version this process holds the install lock
        // on — the same unsupported concurrency documented on
        // [`find_file_matching_handle`] — which the post-condition below
        // narrows.
        hooks::run_hook(&self.dirs.hooks, Hook::PreInstall, version);

        self.prepare_install_source(source)?;
        // Resolved once, here, while the root is known to be the directory this
        // transaction just made. See `refuse_if_the_install_root_moved`.
        let prepared_root = source.root.canonicalize()?;

        // Publishing is part of the install, not something that happens after
        // it: an install whose `.complete` could not be written has not
        // succeeded, and must be cleaned up exactly like one whose bytes never
        // arrived. Folding it into the same `Result` is what routes a failed
        // marker write through the cleanup arm below instead of returning with
        // the incomplete source still on disk.
        let published = install().and_then(|value| {
            if self.product().is_bundle() {
                self.product_dirs
                    .validate_required_bundle_members(version)?;
            }
            // Publish the source BEFORE the hook: create `.complete` and
            // remove `.installing` so a PostInstall hook that runs `ovm
            // which`/`use`/launch against the just-installed version sees
            // it as complete. run_hook cannot fail the transaction, so
            // ordering it last needs no rollback.
            //
            // `.complete` is the word "installed" itself, so it is created,
            // never written over: this transaction made the directory it lives
            // in, and a `.complete` already standing there — planted by another
            // writer, or unpacked out of an archive that wanted to look
            // finished — is not a marker OVM would refresh but a claim it must
            // refuse.
            //
            // Last, before the word "installed" is said at all: the tree being
            // published must still be the tree that was prepared.
            refuse_if_the_install_root_moved(&source.root, &prepared_root)?;
            write_new_file(&source.complete_marker(), b"")?;
            fs::remove_file(source.installing_marker())?;
            Ok(value)
        });
        match published {
            Ok(value) => {
                hooks::run_hook(&self.dirs.hooks, Hook::PostInstall, version);
                Ok(value)
            }
            Err(install_error) => {
                let source_cleanup = self.quarantine_and_remove_source(source);
                let version_cleanup = if !version_dir_existed_before
                    && path_entry_exists(&version_dir)
                    && !self.install_is_complete(version)
                {
                    remove_path_entry(&version_dir)
                } else {
                    Ok(())
                };
                if let Err(cleanup_error) = source_cleanup.and(version_cleanup) {
                    return Err(OvmError::Message(format!(
                        "{install_error}; also failed to clean incomplete install at {}: {cleanup_error}",
                        version_dir.display()
                    )));
                }
                Err(install_error)
            }
        }
    }

    /// Move an old/incomplete source out of the published path before deleting
    /// it. Removing `.installing` in place could briefly make a stale binary
    /// look like a legacy complete install to lock-free readers.
    fn prepare_install_source(&self, source: &InstallSourcePaths) -> Result<()> {
        let quarantine = source.quarantine_path()?;
        remove_path_entry(&quarantine)?;
        if path_entry_exists(&source.root) {
            fs::rename(&source.root, &quarantine)?;
        }
        fs::create_dir_all(&source.root)?;
        // The line above just created this directory empty, so the marker is
        // created rather than written: nothing can be standing at that name
        // yet, and if something is, this install is not the only writer in its
        // own tree and must not proceed.
        write_new_file(&source.installing_marker(), b"")?;
        remove_path_entry(&quarantine)
    }

    fn quarantine_and_remove_source(&self, source: &InstallSourcePaths) -> Result<()> {
        let quarantine = source.quarantine_path()?;
        remove_path_entry(&quarantine)?;
        if path_entry_exists(&source.root) {
            fs::rename(&source.root, &quarantine)?;
        }
        remove_path_entry(&quarantine)
    }

    fn report_reused_install(&self, version: &str) {
        eprintln!(
            "  {} Reused {} {} installed by another OVM process",
            style("✓").green(),
            self.product().display_name(),
            style(version).green().bold()
        );
    }

    /// Printed after we waited on the per-version install lock and the wanted
    /// variant is still incomplete. Usually the holder died mid-download, but
    /// the lock is keyed per version, not per variant — the holder may have
    /// completed a *different* variant (native vs npm) — so the message blames
    /// no one. Without it the UI flips silently from "Waiting..." straight to
    /// a fresh download, which reads as if the wait was pointless.
    fn report_taking_over_install(&self, version: &str) {
        eprintln!(
            "  {} {} {} is still incomplete after the other OVM process released it; installing now",
            style("↻").cyan(),
            self.product().display_name(),
            style(version).bold()
        );
    }

    fn install_claude_native(&self, version: &str) -> Result<()> {
        eprintln!(
            "  {} Downloading native binary v{}...",
            style("↓").cyan(),
            version
        );
        gcs::download_binary(version, &self.product_dirs.native_bin(version))?;

        eprintln!(
            "  {} Installed {} v{} {}",
            style("✓").green(),
            self.product().display_name(),
            style(version).green().bold(),
            style("(native)").dim()
        );
        Ok(())
    }

    fn install_claude_npm(&self, version: &str) -> Result<()> {
        let version_dir = self.product_dirs.version_dir(version);
        let raw_dir = version_dir.join("npm").join("raw");
        let extracted_dir = version_dir.join("npm").join("extracted");
        let installed_dir = version_dir.join("npm").join("installed");

        eprintln!(
            "  {} Downloading npm package v{}...",
            style("↓").cyan(),
            version
        );
        let tarball_path = raw_dir.join(format!("claude-code-{version}.tgz"));
        npm::download_tarball(version, &tarball_path)?;

        eprintln!("  {} Extracting...", style("→").dim());
        npm::extract_tarball(&tarball_path, &extracted_dir)?;

        eprintln!("  {} Installing dependencies...", style("→").dim());
        npm::npm_install(&tarball_path, &installed_dir)?;

        if !self.config.keep_tarballs {
            let _ = fs::remove_dir_all(&raw_dir);
        }

        eprintln!(
            "  {} Installed {} v{} {}",
            style("✓").green(),
            self.product().display_name(),
            style(version).green().bold(),
            style("(npm)").dim()
        );
        Ok(())
    }

    fn install_codex_release(&self, version: &str) -> Result<()> {
        eprintln!("  {} Downloading release {}...", style("↓").cyan(), version);
        let metadata = codex::download_release(version, &self.product_dirs.release_bin(version))?;
        write_new_file(
            &self.product_dirs.release_meta_path(version),
            serde_json::to_string_pretty(&metadata)?.as_bytes(),
        )?;

        eprintln!(
            "  {} Installed {} {} {}",
            style("✓").green(),
            self.product().display_name(),
            style(version).green().bold(),
            style("(release)").dim()
        );
        Ok(())
    }

    fn install_pi_release(&self, version: &str) -> Result<()> {
        eprintln!(
            "  {} Downloading release v{}...",
            style("↓").cyan(),
            version
        );
        let bundle_dir = self.product_dirs.release_bundle_dir(version);
        let metadata = pi::download_release(version, &bundle_dir)?;
        write_new_file(
            &self.product_dirs.release_meta_path(version),
            serde_json::to_string_pretty(&metadata)?.as_bytes(),
        )?;

        eprintln!(
            "  {} Installed {} v{} {}",
            style("✓").green(),
            self.product().display_name(),
            style(version).green().bold(),
            style("(release)").dim()
        );
        Ok(())
    }

    fn install_qm_release(&self, version: &str) -> Result<()> {
        crate::node::require_qm_runtime()?;
        eprintln!(
            "  {} Downloading npm package v{}...",
            style("↓").cyan(),
            version
        );
        let bundle_dir = self.product_dirs.release_bundle_dir(version);
        let metadata = qm::download_release(version, &bundle_dir)?;
        write_new_file(
            &self.product_dirs.release_meta_path(version),
            serde_json::to_string_pretty(&metadata)?.as_bytes(),
        )?;

        eprintln!(
            "  {} Installed {} v{} {}",
            style("✓").green(),
            self.product().display_name(),
            style(version).green().bold(),
            style("(npm bundle)").dim()
        );
        Ok(())
    }

    fn archivable_paths(&self, version: &str) -> Vec<PathBuf> {
        let version_dir = self.product_dirs.version_dir(version);

        match self.product() {
            Product::Claude => vec![
                version_dir.join("extracted"),
                version_dir.join("installed"),
                version_dir.join("npm").join("extracted"),
                version_dir.join("npm").join("installed"),
                version_dir.join("native"),
            ],
            Product::Codex | Product::Pi | Product::Qm => {
                vec![version_dir.join("release"), version_dir.join("dev")]
            }
        }
    }

    fn ensure_dirs(&self) -> Result<()> {
        self.dirs.ensure_base_dirs()?;
        self.product_dirs.ensure_dirs()
    }
}

fn validate_dev_label(label: &str) -> Result<()> {
    if label.is_empty() {
        return Err(OvmError::Message("Dev labels cannot be empty.".into()));
    }

    if has_path_separator_or_traversal(label) {
        return Err(OvmError::Message(
            "Dev labels cannot contain path separators or traversal components.".into(),
        ));
    }

    Ok(())
}

/// Refuse to publish when the install root is no longer the directory the
/// transaction prepared.
///
/// `prepared` is `root` canonicalized immediately after
/// [`VersionManager::prepare_install_source`] created it; this re-asks the same
/// question immediately before `.complete` is written. Two ways of being a
/// different tree are checked, because either alone is passable: the name must
/// still be a real directory (`symlink_metadata`, so a link standing there is
/// seen as a link and not followed), and it must still resolve to the same
/// place (`canonicalize`, so a *parent* replaced by a link to somewhere else is
/// caught even though the final component looks untouched).
///
/// What it catches: a swap of the root, or of any directory above it, landing
/// at any point in the write window — the shape that has the closure's writes
/// traverse a link and land outside the managed tree. The install fails, which
/// routes through the transaction's cleanup arm, and no `.complete` is written,
/// so nothing that escaped is ever presented as an installed version.
///
/// What it does not catch, deliberately:
///
///   * anything *inside* the tree. It does not walk the contents looking for
///     symbolic links, because installs legitimately create them — npm's
///     `node_modules/.bin/*`, and a `--link` dev install whose published
///     destination *is* a symlink — so a contents walk would refuse correct
///     installs. Writes inside the tree are defended where they happen, by
///     [`create_new_file`] refusing any destination that already exists.
///   * a swap that is made and undone entirely within the window. This is a
///     check at two points in time, not a lock on the path.
///   * bytes. It says the tree is where it was, not that its contents are what
///     the installer wrote.
///
/// It is a post-condition, not the mechanism: the PreInstall hook now runs
/// before the tree is prepared ([`VersionManager::run_install_transaction`]),
/// so reaching this check at all takes a process mutating OVM's store for a
/// version another process holds the install lock on.
fn refuse_if_the_install_root_moved(root: &Path, prepared: &Path) -> Result<()> {
    let moved = |detail: String| {
        OvmError::Message(format!(
            "Refusing to publish the install at {}: {detail}. The directory this install \
             created and wrote into is not the directory that is there now — another process \
             replaced it while the install was running. Nothing was published.",
            root.display()
        ))
    };

    let metadata = fs::symlink_metadata(root)
        .map_err(|error| moved(format!("it can no longer be read ({error})")))?;
    if !metadata.is_dir() {
        return Err(moved("it is no longer a directory".into()));
    }

    let current = root
        .canonicalize()
        .map_err(|error| moved(format!("it can no longer be resolved ({error})")))?;
    if current != prepared {
        return Err(moved(format!(
            "it now resolves to {} instead of {}",
            current.display(),
            prepared.display()
        )));
    }

    Ok(())
}

fn path_entry_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Whether `path` resolves inside `base`.
///
/// Both sides are resolved first, so a symlink pointing into OVM's store — or
/// macOS's `/var` → `/private/var` — cannot make a contained path look foreign.
pub(crate) fn path_is_inside(base: &Path, path: &Path) -> bool {
    let base = canonicalize_existing_ancestor(base);
    canonicalize_existing_ancestor(path).starts_with(base)
}

/// `" (which resolves to <path>)"`, or nothing when the name already *is* the
/// resolved path.
///
/// A refusal decided on the resolved path has to stay actionable: the path the
/// user typed is the one they can change, and the path it resolved to is the
/// reason it was refused. Naming only one of the two leaves the message either
/// unrecognizable or unexplained.
fn resolved_via(requested: &Path, resolved: &Path) -> String {
    if requested == resolved {
        return String::new();
    }

    format!(" (which resolves to {})", resolved.display())
}

/// Resolve as much of `path` as exists on disk, keeping the rest verbatim.
///
/// `Path::canonicalize` fails outright when the tail does not exist, and a
/// bare fallback to the original path would then compare an unresolved path
/// against a resolved one — which on macOS (`/var` → `/private/var`) answers
/// "different" for two spellings of the same place. Resolving the deepest
/// existing ancestor keeps the comparison honest for paths not yet created.
fn canonicalize_existing_ancestor(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }

    let mut trailing = Vec::new();
    let mut current = path;
    while let (Some(parent), Some(name)) = (current.parent(), current.file_name()) {
        trailing.push(name.to_os_string());
        if let Ok(mut resolved) = parent.canonicalize() {
            resolved.extend(trailing.iter().rev());
            return resolved;
        }
        current = parent;
    }

    path.to_path_buf()
}

/// The file an install reads its bytes from, bound to one open handle.
///
/// Path-based validation is a statement about an instant. Between the check and
/// the read, a symlink can be re-pointed and a name can be given to a different
/// file — and an install from a local path does a great deal between the two:
/// it waits on the per-version install lock, then enters a transaction that
/// *deletes* this version's managed tree before the copy runs. Re-resolving the
/// user's path after all that is how a validated foreign source turned into
/// "delete the store, then fail to read it".
///
/// Both local-source installs bind here — `ovm adopt`
/// ([`VersionManager::install_import`]) and a dev install from `--binary` or
/// `--bundle` ([`VersionManager::install_dev`]) — because both hand a path the
/// user controls to a transaction that deletes before it reads. They differ
/// only in what the refusals below call the command
/// ([`BoundSourceUse`]).
///
/// So the name is resolved exactly once, opened, and never traversed again:
///
///   * [`Self::open`] canonicalizes first, then opens the canonical path with
///     `O_NOFOLLOW` on unix, so a link substituted for the resolved file after
///     the resolution cannot be followed. As a second, platform-independent
///     proof that the name and the handle are the same file, the handle's own
///     `fstat` must say "regular file" and must match a fresh
///     `symlink_metadata` of the resolved path — a swap between resolution and
///     open shows up as a mismatch and is refused.
///   * [`Self::resolved`] is what every name-based refusal is decided on, so
///     those refusals are about the file that was opened — and, for a dev
///     install's `--link`, it is what the published symlink points at, so the
///     managed destination does not inherit a name the user can retarget later.
///   * [`Self::identity`] is `fstat` on the handle and
///     [`Self::copy_to_new_file`] reads the handle, so a retarget *after* the
///     open cannot change either.
///
/// What it does not do: it is not a lock on the file's contents. Someone
/// rewriting the bytes of that same inode in place is still writing to the file
/// this handle reads — that is what the before/after identity comparison in
/// [`VersionManager::stage_verify_and_publish_import`] is for. And a source
/// swapped *before* OVM ever looked is simply a different adoption; the staged
/// copy's `--version` re-check is what notices.
#[derive(Debug)]
struct BoundSource {
    /// The path the user named. Error messages and provenance metadata only —
    /// never re-traversed.
    requested: PathBuf,
    /// Where that name resolved, once, proven below to name `file`.
    resolved: PathBuf,
    file: File,
}

/// Which command bound a [`BoundSource`]. It changes nothing about the binding
/// — only the wording of the refusals in [`BoundSource::bind`], which have to
/// name a command the user can actually re-run.
#[derive(Debug, Clone, Copy)]
enum BoundSourceUse {
    /// `ovm adopt`, via [`InstallRequest::Import`].
    Import,
    /// `ovm install <product> dev --binary/--bundle`, via
    /// [`InstallRequest::Dev`].
    DevInstall,
}

impl BoundSourceUse {
    /// Fills "Cannot {verb} {path}: …".
    fn verb(self) -> &'static str {
        match self {
            Self::Import => "import",
            Self::DevInstall => "install",
        }
    }

    /// Fills "… re-run the {command}."
    fn command(self) -> &'static str {
        match self {
            Self::Import => "adopt",
            Self::DevInstall => "dev install",
        }
    }
}

impl BoundSource {
    fn open(requested: &Path, purpose: BoundSourceUse) -> Result<Self> {
        let resolved = requested.canonicalize()?;
        Self::bind(requested, resolved, purpose)
    }

    /// Open an already-resolved path and prove the handle is that path's
    /// regular file. Split out from [`Self::open`] so a test can hand it the
    /// path a resolution just produced with a symlink swapped in behind it —
    /// the race this exists to lose safely.
    fn bind(requested: &Path, resolved: PathBuf, purpose: BoundSourceUse) -> Result<Self> {
        let file = open_without_following(&resolved).map_err(|error| {
            OvmError::Message(format!(
                "Cannot {verb} {}: {} could not be opened as a regular file ({error}). \
                 If a symbolic link took its place while the path was being resolved, \
                 nothing was installed and nothing was removed — re-run the {command}.",
                requested.display(),
                resolved.display(),
                verb = purpose.verb(),
                command = purpose.command(),
            ))
        })?;

        let opened = file.metadata()?;
        let named = fs::symlink_metadata(&resolved)?;
        if !opened.is_file()
            || named.file_type().is_symlink()
            || !refers_to_same_file(&named, &opened)
        {
            return Err(OvmError::Message(format!(
                "Cannot {verb} {}: {} is no longer the regular file it resolved to — a \
                 symbolic link or a different file is there now. Nothing was installed and \
                 nothing was removed; re-run the {command}.",
                requested.display(),
                resolved.display(),
                verb = purpose.verb(),
                command = purpose.command(),
            )));
        }

        Ok(Self {
            requested: requested.to_path_buf(),
            resolved,
            file,
        })
    }

    fn requested(&self) -> &Path {
        &self.requested
    }

    fn resolved(&self) -> &Path {
        &self.resolved
    }

    /// The bound file's identity, via `fstat` on the handle — no path involved,
    /// so it describes the file being copied and nothing else.
    fn identity(&self) -> Result<SourceIdentity> {
        Ok(SourceIdentity::from_metadata(&self.opened_metadata()?))
    }

    /// `fstat` on the handle. The one description of the source that no
    /// directory swap, retarget or rename can influence, because it never names
    /// anything: see
    /// [`VersionManager::reject_import_of_a_file_this_install_deletes`].
    fn opened_metadata(&self) -> Result<fs::Metadata> {
        Ok(self.file.metadata()?)
    }

    /// Copy the bound file's bytes into a *newly created* `destination`,
    /// reading the handle from the top, and mark that new file executable
    /// through the handle the copy created.
    ///
    /// Two names are dangerous here and neither is used. `fs::copy` would take
    /// the *source* path and follow it again — that is what the handle exists
    /// to prevent. `File::create` would take the *destination* path and follow
    /// it too: a symbolic link standing where the copy is about to write sends
    /// `O_TRUNC` straight through to the link's target, and the target an
    /// attacker points it at is the user's own source file. That is not
    /// hypothetical: OVM truncates the file it is reading, then publishes the
    /// zero bytes it read back. A PreInstall hook could plant such a link while
    /// the hook ran between the tree being prepared and being written into; it
    /// now runs before [`VersionManager::prepare_install_source`], which erases
    /// anything it left in the tree, so what remains is a concurrent writer in
    /// OVM's own store. [`create_new_file`] refuses either way: after prepare,
    /// the destination must not exist at all.
    ///
    /// The caller decides what `destination` is: an import writes a staging
    /// sibling it later renames into place, a dev install writes the published
    /// path directly (see [`VersionManager::install_dev`] for why that is safe
    /// there). Both live under a tree this transaction has just created empty,
    /// so "already exists" always means something interfered.
    fn copy_to_new_file(&self, destination: &Path) -> Result<()> {
        let mut reader = &self.file;
        reader.seek(SeekFrom::Start(0))?;
        let mut writer = create_new_file(destination)?;
        std::io::copy(&mut reader, &mut writer)?;
        make_handle_executable(&writer)?;
        Ok(())
    }
}

/// Create `path` for writing, refusing anything that is already there.
///
/// `File::create` — and `fs::write`, which is built on it — opens with
/// `O_CREAT | O_TRUNC` and follows symbolic links, so a link planted at the
/// path is not overwritten but *dereferenced*: the truncation lands on its
/// target, which is a file this install never meant to touch. `O_CREAT |
/// O_EXCL` neither follows a link nor accepts an existing file.
///
/// Open `path` itself, never a link standing where it is.
#[cfg(unix)]
fn open_without_following(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// No portable `O_NOFOLLOW` off unix, so the symlink and same-file checks in
/// [`BoundSource::bind`] carry this alone — narrower, but OVM does not
/// ship a non-unix build.
#[cfg(not(unix))]
fn open_without_following(path: &Path) -> std::io::Result<File> {
    File::open(path)
}

/// The file this install is about to delete that `bound` is holding, if any.
///
/// The deletion set is exactly what [`VersionManager::run_install_transaction`]
/// removes before or around the copy — the same two paths for a download, an
/// import and a dev install, because all three run through that transaction:
///
///   * `source.root` — quarantined and removed by
///     [`VersionManager::prepare_install_source`] before the closure runs,
///     which takes `source.destination` (a file *inside* the root) and any
///     staged copy with it;
///   * `source.quarantine_path()` — removed by that same step, and again by
///     [`VersionManager::quarantine_and_remove_source`] on failure.
///
/// The version directory is not in the set: it is removed on failure only when
/// it did **not** exist before the transaction, and a directory that did not
/// exist when the handle was bound holds no file that handle could be on. That
/// reasoning carries to dev installs unchanged —
/// [`VersionManager::dev_source_paths`] puts its root under the same version
/// directory, and the dev closure writes nothing outside that root (the
/// destination is `dev/bin/<binary>`, the metadata `dev/meta.json`), so the
/// root plus the quarantine still name everything at risk.
fn file_this_install_deletes(
    bound: &BoundSource,
    source: &InstallSourcePaths,
) -> Result<Option<PathBuf>> {
    let deletable = [source.root.clone(), source.quarantine_path()?];
    find_file_matching_handle(&bound.opened_metadata()?, &deletable)
}

/// The first regular file under `roots` that is the same file as `opened`, or
/// `None` if the handle is none of them.
///
/// The subject is the descriptor: every candidate is compared to it by identity
/// ([`refers_to_same_file`]), so the answer does not depend on how — or through
/// what — the handle's path was spelled. Used by
/// [`VersionManager::reject_import_of_a_file_this_install_deletes`] to decide
/// whether an import is holding a file the install is about to remove.
///
/// Symbolic links are skipped rather than followed, matching what deletion
/// actually does: [`remove_path_entry`] and `remove_dir_all` unlink the link,
/// never its target, so a link inside a doomed tree does not put its target in
/// danger. A path that vanishes mid-walk is likewise not a hazard — it is
/// already gone — so `NotFound` is skipped instead of raised. Anything else
/// (an unreadable directory, say) is an error: an import that cannot see what
/// it is about to delete must not proceed on the assumption that it is safe.
///
/// Be honest about what the walk is: a snapshot, taken one directory at a time.
/// The comparison it makes cannot be fooled, but the enumeration it makes can
/// be out of date — a matching entry renamed out of a doomed root after the
/// walk has read its parent, and renamed back before
/// [`VersionManager::prepare_install_source`] runs, is never visited, and the
/// install proceeds on a handle that is deleted after all. Reaching that means
/// concurrently mutating the contents of OVM's own store for a version another
/// process holds the install lock on — which is exactly what that lock declares
/// unsupported, and which no accident (a stray `--binary`, a hard link, an
/// updater landing mid-adopt) produces. So it is documented rather than
/// defended: closing it would take the deletion itself re-checking identity as
/// it unlinks, a much larger change for a case only a deliberate attacker with
/// write access to the store can stage.
fn find_file_matching_handle(opened: &fs::Metadata, roots: &[PathBuf]) -> Result<Option<PathBuf>> {
    let mut pending: Vec<PathBuf> = roots.to_vec();
    while let Some(path) = pending.pop() {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };

        if metadata.file_type().is_symlink() {
            continue;
        }

        if metadata.is_dir() {
            let entries = match fs::read_dir(&path) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            for entry in entries {
                pending.push(entry?.path());
            }
            continue;
        }

        if metadata.is_file() && refers_to_same_file(&metadata, opened) {
            return Ok(Some(path));
        }
    }

    Ok(None)
}

/// Whether a path's metadata and an open handle's metadata describe one file.
#[cfg(unix)]
fn refers_to_same_file(named: &fs::Metadata, opened: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    named.dev() == opened.dev() && named.ino() == opened.ino()
}

#[cfg(not(unix))]
fn refers_to_same_file(named: &fs::Metadata, opened: &fs::Metadata) -> bool {
    named.len() == opened.len() && named.modified().ok() == opened.modified().ok()
}

/// What a source file looked like at one instant — enough of its identity that
/// a swap or an in-place rewrite shows up as a difference.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceIdentity {
    len: u64,
    modified: Option<SystemTime>,
    /// Device and inode: the file *itself*, not the name pointing at it. A
    /// replace-by-rename (how most updaters land a new build) keeps the path
    /// and changes this. Unix only; elsewhere size and mtime carry the check.
    #[cfg(unix)]
    file: (u64, u64),
}

impl SourceIdentity {
    /// Built from metadata the caller already has. Production callers get that
    /// metadata from an open handle ([`BoundSource::identity`]), so no
    /// path is resolved to answer "what is this file now?".
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            file: {
                use std::os::unix::fs::MetadataExt;
                (metadata.dev(), metadata.ino())
            },
        }
    }

    /// Read an identity from a path. Tests only: they build before/after pairs
    /// by mutating a file on disk, which is the one place a *name* is the
    /// subject. `symlink_metadata`, not `metadata`, so nothing is followed.
    #[cfg(test)]
    fn read(path: &Path) -> Result<Self> {
        Ok(Self::from_metadata(&fs::symlink_metadata(path)?))
    }
}

/// Refuse a staged copy whose source moved under it.
///
/// See [`VersionManager::stage_verify_and_publish_import`] for what this does
/// and does not defend.
fn refuse_if_source_changed_during_copy(
    product: Product,
    binary: &Path,
    before: &SourceIdentity,
    after: &SourceIdentity,
) -> Result<()> {
    if before == after {
        return Ok(());
    }

    Err(OvmError::Message(format!(
        "{} at {} changed on disk while it was being copied, so the copy may be part of one \
         build and part of another. An upgrade landing mid-adopt looks exactly like this; \
         nothing was installed. Re-run the adopt.",
        product.display_name(),
        binary.display(),
    )))
}

/// Where an imported binary is staged before it earns the published name.
///
/// A sibling of the final path, so publishing it is a rename inside one
/// directory — never a cross-device move, whatever filesystem the user's own
/// copy lives on. The staged name is deliberately *not* the product's binary
/// name: verification runs the staged file, and a multi-call binary picks its
/// behavior from `argv[0]` (OVM's own launcher does exactly this), so under the
/// published name such a file would answer as a launcher rather than as itself.
/// It also means a half-copied file never occupies the published path.
fn staged_import_path(destination: &Path) -> Result<PathBuf> {
    let name = destination
        .file_name()
        .ok_or_else(|| OvmError::Config(format!("No file name for {}", destination.display())))?;
    Ok(destination.with_file_name(format!(".{}.importing", name.to_string_lossy())))
}

fn remove_path_entry(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn validate_storage_version_component(product: Product, version: &str) -> Result<()> {
    if version == "latest" {
        return Ok(());
    }

    if let Some(label) = version.strip_prefix("dev:") {
        return validate_dev_label(label);
    }

    if has_path_separator_or_traversal(version) {
        return Err(OvmError::Message(
            "Versions cannot contain path separators or traversal components.".into(),
        ));
    }

    if product.is_official_remote_version(version) {
        return Ok(());
    }

    Err(OvmError::Message(format!(
        "Invalid {} version `{version}`.",
        product.display_name()
    )))
}

fn has_path_separator_or_traversal(value: &str) -> bool {
    use std::path::Component;

    value.contains('/')
        || value.contains('\\')
        || std::path::Path::new(value).components().any(|component| {
            matches!(
                component,
                Component::ParentDir
                    | Component::CurDir
                    | Component::RootDir
                    | Component::Prefix(_)
            )
        })
}

fn dir_size(path: &Path) -> Result<u64> {
    let mut size = 0;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_file() {
            size += metadata.len();
        } else if metadata.is_dir() {
            size += dir_size(&entry.path())?;
        }
    }
    Ok(size)
}

fn version_dir_is_older_than(path: &Path, cutoff: Duration, now: SystemTime) -> bool {
    let Some(modified) = fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
    else {
        return false;
    };
    now.duration_since(modified)
        .map(|age| age >= cutoff)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{
        find_file_matching_handle, path_entry_exists, path_is_inside,
        refuse_if_source_changed_during_copy, refuse_if_the_install_root_moved, remove_path_entry,
        staged_import_path, BoundSource, BoundSourceUse, DevInstallMetadata, DevInstallMode,
        DevInstallSource, InstallRequest, SourceIdentity, VersionManager, COMPLETE_MARKER,
        INSTALLING_MARKER,
    };
    use crate::config::{OvmConfig, OvmDirs, VersionSource};
    use crate::product::Product;
    use crate::release_metadata::ReleaseInstallMetadata;
    use filetime::{set_file_mtime, FileTime};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    /// A runnable stand-in for a foreign product install: prints `version_line`
    /// for any arguments, so `--version` answers it.
    ///
    /// Import verification *runs* the copy it stages, so these fixtures have to
    /// be programs — an inert text file, which earlier import tests used, can no
    /// longer stand in for a binary the store is about to publish.
    fn foreign_binary(dir: &Path, name: &str, version_line: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\necho '{version_line}'\n")).expect("write fixture");
        crate::util::make_executable(&path).expect("chmod fixture");
        path
    }

    /// Turns the publisher-signature half of import verification off while it
    /// is alive, so unsigned shell-script fixtures can exercise the rest.
    ///
    /// `OVM_SKIP_SIGNATURE_VERIFY` is process-wide, so this holds the lock
    /// shared with `crate::sources`: without it, switching verification off here
    /// could decide the outcome of the tests over there whose entire point is
    /// that an unsigned binary is refused.
    struct SignatureVerificationOff {
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for SignatureVerificationOff {
        fn drop(&mut self) {
            std::env::remove_var("OVM_SKIP_SIGNATURE_VERIFY");
        }
    }

    fn signature_verification_off() -> SignatureVerificationOff {
        let guard = signature_env_lock();
        std::env::set_var("OVM_SKIP_SIGNATURE_VERIFY", "1");
        SignatureVerificationOff { _guard: guard }
    }

    fn signature_env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::sources::SIGNATURE_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// An isolated OVM store plus the temp directory holding it.
    ///
    /// The store is a `.ovm` *subdirectory* of the returned temp dir, not the
    /// temp dir itself, so that `dir.path()` is genuinely OUTSIDE the store —
    /// which is what a foreign install is. Fixtures written next to the store
    /// used to sit inside it, and an import now refuses its own store as a
    /// source, so a flat layout would have made these tests assert the wrong
    /// thing about the wrong path.
    fn setup_test_vm(product: Product) -> (VersionManager, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir");
        let dirs = OvmDirs::at(dir.path().join(".ovm"));
        fs::create_dir_all(&dirs.bin).expect("mkdir");

        let vm = VersionManager {
            product_dirs: dirs.product_dirs(product),
            dirs,
            config: OvmConfig::default(),
        };
        (vm, dir)
    }

    fn create_claude_version(vm: &VersionManager, version: &str) {
        let native_dir = vm.product_dirs.version_dir(version).join("native");
        fs::create_dir_all(&native_dir).expect("mkdir");
        fs::write(native_dir.join("claude"), "fake-binary").expect("write");
        fs::write(native_dir.join(COMPLETE_MARKER), "").expect("write marker");
    }

    fn create_codex_release(vm: &VersionManager, version: &str) {
        let release_dir = vm
            .product_dirs
            .version_dir(version)
            .join("release")
            .join("bin");
        fs::create_dir_all(&release_dir).expect("mkdir");
        fs::write(release_dir.join("codex"), "fake-binary").expect("write");
        let metadata = ReleaseInstallMetadata::new(
            version,
            version,
            "codex-aarch64-apple-darwin.tar.gz",
            format!("https://github.com/openai/codex/releases/download/{version}/codex-aarch64-apple-darwin.tar.gz"),
            "deadbeef",
        );
        fs::write(
            vm.product_dirs.release_meta_path(version),
            serde_json::to_string_pretty(&metadata).expect("serialize release metadata"),
        )
        .expect("write release metadata");
    }

    fn create_codex_dev(vm: &VersionManager, version: &str) {
        let dev_dir = vm.product_dirs.version_dir(version).join("dev").join("bin");
        fs::create_dir_all(&dev_dir).expect("mkdir");
        fs::write(dev_dir.join("codex"), "fake-binary").expect("write");
        fs::write(
            dev_dir.parent().expect("dev root").join(COMPLETE_MARKER),
            "",
        )
        .expect("write marker");
    }

    fn age_version_dir(vm: &VersionManager, version: &str, days: u64) {
        let then = SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60);
        set_file_mtime(
            vm.product_dirs.version_dir(version),
            FileTime::from_system_time(then),
        )
        .expect("set mtime");
    }

    #[test]
    fn list_empty() {
        let (vm, _dir) = setup_test_vm(Product::Claude);
        assert!(vm.list_installed().expect("list").is_empty());
    }

    #[test]
    fn empty_version_dir_not_listed() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        fs::create_dir_all(vm.product_dirs.version_dir("rust-v999.999.999"))
            .expect("empty version dir");

        assert!(vm.list_installed().expect("list").is_empty());
    }

    #[test]
    fn killed_install_not_listed() {
        let (vm, _dir) = setup_test_vm(Product::Claude);
        // A SIGKILLed install: non-empty tree, `.installing` still present,
        // no complete source. Must not appear as installed/archived.
        let native_dir = vm.product_dirs.version_dir("2.1.220").join("native");
        fs::create_dir_all(&native_dir).expect("mkdir");
        fs::write(native_dir.join("claude"), "partial-binary").expect("write");
        fs::write(native_dir.join(INSTALLING_MARKER), "").expect("write marker");

        assert!(vm.list_installed().expect("list").is_empty());
        assert!(vm.product_dirs.version_sources("2.1.220").is_empty());

        // A genuine archive (content, no markers, binary pruned) keeps its label.
        let archived = vm.product_dirs.version_dir("2.0.10").join("native");
        fs::create_dir_all(&archived).expect("mkdir");
        fs::write(archived.join("manifest.json"), "{}").expect("write");
        assert_eq!(
            vm.product_dirs.version_sources("2.0.10"),
            vec![VersionSource::Archived]
        );
        assert_eq!(vm.list_installed().expect("list"), vec!["2.0.10"]);
    }

    #[test]
    fn list_installed_sorts_claude_versions() {
        let (vm, _dir) = setup_test_vm(Product::Claude);
        create_claude_version(&vm, "2.1.5");
        create_claude_version(&vm, "2.0.37");
        create_claude_version(&vm, "2.1.71");

        assert_eq!(
            vm.list_installed().expect("list"),
            vec!["2.0.37", "2.1.5", "2.1.71"]
        );
    }

    #[test]
    fn list_installed_sorts_codex_versions() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        create_codex_release(&vm, "rust-v0.120.0");
        create_codex_release(&vm, "rust-v0.118.0");
        create_codex_release(&vm, "dev:resume-fix");

        assert_eq!(
            vm.list_installed().expect("list"),
            vec!["dev:resume-fix", "rust-v0.118.0", "rust-v0.120.0"]
        );
    }

    #[test]
    fn create_codex_release_writes_release_metadata() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        create_codex_release(&vm, "rust-v0.120.0");

        let metadata =
            ReleaseInstallMetadata::read(&vm.product_dirs.release_meta_path("rust-v0.120.0"))
                .expect("read release metadata")
                .expect("present");

        assert_eq!(metadata.kind, "release");
        assert_eq!(metadata.version, "rust-v0.120.0");
        assert_eq!(metadata.resolved_tag, "rust-v0.120.0");
        assert_eq!(metadata.archive_sha256, "deadbeef");
    }

    #[test]
    fn install_exact_existing_version_still_rejects() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        create_codex_release(&vm, "rust-v0.130.0");

        let error = vm
            .install(InstallRequest::Standard {
                use_npm: false,
                version: "rust-v0.130.0".to_string(),
            })
            .expect_err("already installed");

        assert!(error.to_string().contains("already installed"));
    }

    #[test]
    fn install_lock_is_exclusive_and_released_on_drop() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        let first = vm
            .acquire_install_lock("rust-v0.130.0")
            .expect("first lock");
        assert!(!first.waited);

        let second_vm = VersionManager {
            dirs: vm.dirs.clone(),
            product_dirs: vm.product_dirs.clone(),
            config: vm.config.clone(),
        };
        let (sender, receiver) = mpsc::channel();
        let contender = thread::spawn(move || {
            let lock = second_vm
                .acquire_install_lock("rust-v0.130.0")
                .expect("second lock");
            sender.send(lock.waited).expect("send result");
        });

        assert!(
            receiver.recv_timeout(Duration::from_millis(100)).is_err(),
            "contender must remain blocked while the owner holds the lock"
        );
        drop(first);
        assert!(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("contender acquired"),
            "contender should record that it waited"
        );
        contender.join().expect("contender thread");
    }

    #[test]
    fn legacy_install_requires_its_historical_metadata() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        let version = "rust-v0.130.0";
        let release_root = vm.product_dirs.version_dir(version).join("release");
        fs::create_dir_all(release_root.join("bin")).expect("mkdir");
        fs::write(release_root.join("bin/codex"), "fake").expect("binary");
        assert!(!vm.standard_install_is_complete(version));

        fs::write(release_root.join("meta.json"), "{}").expect("metadata");
        assert!(vm.standard_install_is_complete(version));
    }

    #[test]
    fn installing_marker_overrides_binary_and_complete_marker() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        let version = "rust-v0.130.0";
        let release_root = vm.product_dirs.version_dir(version).join("release");
        fs::create_dir_all(release_root.join("bin")).expect("mkdir");
        fs::write(release_root.join("bin/codex"), "fake").expect("binary");
        fs::write(release_root.join(COMPLETE_MARKER), "").expect("complete");
        fs::write(release_root.join(INSTALLING_MARKER), "").expect("installing");

        assert!(!vm.standard_install_is_complete(version));
        assert!(!vm
            .version_sources(version)
            .contains(&VersionSource::Release));
    }

    #[test]
    fn failed_install_transaction_removes_only_its_source() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        let version = "dev:recovery";
        create_codex_release(&vm, version);
        let release_binary = vm.product_dirs.release_bin(version);
        let dev = vm.dev_source_paths(version);

        let error = vm
            .run_install_transaction(version, &dev, || {
                fs::create_dir_all(dev.destination.parent().expect("destination parent"))?;
                fs::write(&dev.destination, "partial")?;
                Err::<(), _>(crate::error::OvmError::Message("boom".into()))
            })
            .expect_err("install fails");

        assert!(error.to_string().contains("boom"));
        assert!(!dev.root.exists(), "incomplete dev source was cleaned");
        assert!(
            release_binary.exists(),
            "valid release source was preserved"
        );
    }

    /// Adoption copies a local binary into the store. It must do that inside
    /// the same per-version install lock a download install takes, or a
    /// concurrent `ovm install` and `ovm adopt` can write the same destination
    /// at the same time — with adoption publishing `.complete` over a tree the
    /// installer is still filling.
    #[test]
    fn import_waits_for_the_per_version_install_lock() {
        let _signature = signature_verification_off();
        let (vm, dir) = setup_test_vm(Product::Codex);
        let version = "rust-v0.130.0";
        let source = foreign_binary(dir.path(), "foreign-codex", "codex-cli 0.130.0");

        let owner = vm.acquire_install_lock(version).expect("owner lock");

        let contender_vm = VersionManager {
            dirs: vm.dirs.clone(),
            product_dirs: vm.product_dirs.clone(),
            config: vm.config.clone(),
        };
        let (sender, receiver) = mpsc::channel();
        let import_source = source.clone();
        let contender = thread::spawn(move || {
            let result = contender_vm.install(InstallRequest::Import {
                version: version.to_string(),
                binary: import_source,
            });
            sender.send(result.is_ok()).expect("send result");
        });

        assert!(
            receiver.recv_timeout(Duration::from_millis(200)).is_err(),
            "import must block while another process holds the install lock"
        );
        assert!(
            !vm.standard_install_is_complete(version),
            "nothing may be published while the lock is held elsewhere"
        );

        drop(owner);
        // Generous: publishing an import now spawns the staged copy to re-check
        // its version, and a first exec on a loaded machine is not instant. The
        // 200 ms probe above is what actually proves the blocking.
        assert!(
            receiver
                .recv_timeout(Duration::from_secs(30))
                .expect("import finished after the lock was released"),
            "import should succeed once the lock is free"
        );
        contender.join().expect("import thread");
        assert!(vm.standard_install_is_complete(version));
    }

    /// A crashed install leaves a half-written source tree. Adoption must
    /// quarantine it and start clean, exactly as a download install does —
    /// copying one file into the leftovers would publish a mix of two installs
    /// as `.complete`.
    #[test]
    fn import_quarantines_a_crashed_source_instead_of_merging_into_it() {
        let _signature = signature_verification_off();
        let (vm, dir) = setup_test_vm(Product::Codex);
        let version = "rust-v0.130.0";
        let source_paths = vm.standard_source_paths(version, false);
        fs::create_dir_all(source_paths.destination.parent().expect("bin dir")).expect("mkdir");
        fs::write(&source_paths.destination, "stale partial").expect("stale binary");
        fs::write(source_paths.root.join("leftover-from-crash"), "junk").expect("stale leftover");
        fs::write(source_paths.installing_marker(), "").expect("installing marker");

        let source = foreign_binary(dir.path(), "foreign-codex", "codex-cli 0.130.0");

        let installed = vm
            .install(InstallRequest::Import {
                version: version.to_string(),
                binary: source.clone(),
            })
            .expect("import publishes");

        assert_eq!(installed, version);
        assert!(
            !source_paths.root.join("leftover-from-crash").exists(),
            "the crashed source tree must be quarantined, not merged into"
        );
        assert_eq!(
            fs::read(&source_paths.destination).expect("imported binary"),
            fs::read(&source).expect("foreign binary"),
            "the published bytes must be the bytes that were verified"
        );
        assert!(
            !staged_import_path(&source_paths.destination)
                .expect("staged path")
                .exists(),
            "the staging copy must not survive a successful publish"
        );
        assert!(source_paths.complete_marker().exists());
        assert!(!source_paths.installing_marker().exists());
        assert!(vm.standard_install_is_complete(version));
    }

    /// An import that arrives after another process published the same version
    /// is the outcome adoption wanted, not a conflict.
    #[test]
    fn import_of_an_already_complete_version_succeeds() {
        let (vm, dir) = setup_test_vm(Product::Codex);
        let version = "rust-v0.130.0";
        create_codex_release(&vm, version);
        let source = dir.path().join("foreign-codex");
        fs::write(&source, "foreign binary").expect("source binary");

        let installed = vm
            .install(InstallRequest::Import {
                version: version.to_string(),
                binary: source,
            })
            .expect("import is idempotent");

        assert_eq!(installed, version);
        assert_eq!(
            fs::read_to_string(vm.product_dirs.release_bin(version)).expect("existing binary"),
            "fake-binary",
            "an existing complete install must not be overwritten"
        );
    }

    /// The authenticity contract: what is published is what was checked.
    ///
    /// Adoption reads a version out of a file and then copies that file. If the
    /// two are not the same bytes — an upgrade landing between the read and the
    /// copy looks exactly like this — the copy would be published under the
    /// earlier build's version. So the staged copy is asked again, and a
    /// different answer aborts the install.
    #[test]
    fn import_refuses_a_staged_copy_that_reports_a_different_version() {
        let _signature = signature_verification_off();
        let (vm, dir) = setup_test_vm(Product::Codex);
        let version = "rust-v0.130.0";
        // The bytes on disk are a different build from the one adopt read.
        let source = foreign_binary(dir.path(), "foreign-codex", "codex-cli 0.131.0");

        let error = vm
            .install(InstallRequest::Import {
                version: version.to_string(),
                binary: source,
            })
            .expect_err("a copy that reports another version must not be published");

        assert!(error.to_string().contains("rust-v0.131.0"), "{error}");
        let source_paths = vm.standard_source_paths(version, false);
        assert!(
            !source_paths.destination.exists(),
            "nothing may be published under a version the binary does not report"
        );
        assert!(
            !staged_import_path(&source_paths.destination)
                .expect("staged path")
                .exists(),
            "the staged copy must be cleaned"
        );
        assert!(
            !source_paths.root.exists(),
            "the failed source tree must go"
        );
        assert!(!vm.standard_install_is_complete(version));
        assert!(
            !vm.product_dirs.version_dir(version).exists(),
            "a version directory this import created must not outlive it"
        );
    }

    /// Downloaded Claude and Codex binaries are checked against the publisher's
    /// Apple team ID; imported ones skipped that entirely, so `ovm adopt` was a
    /// way to get an unverified binary into the store under a real version's
    /// name. Same guard, same path now — mirrors the download-side CLI test in
    /// `tests/lifecycle.rs`. macOS only: the check is a no-op elsewhere, for
    /// imports exactly as for downloads.
    #[cfg(target_os = "macos")]
    #[test]
    fn import_refuses_a_staged_copy_that_fails_the_publisher_signature_check() {
        let _guard = signature_env_lock();
        std::env::remove_var("OVM_SKIP_SIGNATURE_VERIFY");

        let (vm, dir) = setup_test_vm(Product::Codex);
        let version = "rust-v0.130.0";
        // A shell script is not an OpenAI-signed Mach-O — what a substituted or
        // corrupted binary looks like to codesign.
        let source = foreign_binary(dir.path(), "foreign-codex", "codex-cli 0.130.0");

        let error = vm
            .install(InstallRequest::Import {
                version: version.to_string(),
                binary: source,
            })
            .expect_err("an unsigned binary must not be imported");

        assert!(error.to_string().contains("code signature"), "{error}");
        let source_paths = vm.standard_source_paths(version, false);
        assert!(
            !source_paths.destination.exists(),
            "an unverified binary was published"
        );
        assert!(
            !staged_import_path(&source_paths.destination)
                .expect("staged path")
                .exists(),
            "the staged copy must be cleaned"
        );
        assert!(!vm.standard_install_is_complete(version));
    }

    #[test]
    fn import_rejects_a_version_that_escapes_the_store() {
        let (vm, dir) = setup_test_vm(Product::Codex);
        let source = dir.path().join("foreign-codex");
        fs::write(&source, "foreign binary").expect("source binary");

        let error = vm
            .install(InstallRequest::Import {
                version: "../../evil".to_string(),
                binary: source,
            })
            .expect_err("traversal rejected");

        assert!(!error.to_string().is_empty());
    }

    /// The store is not a foreign install, and treating it as one destroyed
    /// the file it was pointed at.
    ///
    /// An import quarantines and removes the version's existing source tree
    /// before copying, so a source *inside* that tree — an install that died
    /// half-way, binary present and no `.complete` — was deleted before the
    /// copy could read it. Refused before the transaction is entered, so
    /// nothing is locked, moved or removed.
    #[test]
    fn import_refuses_a_source_inside_the_store() {
        let _signature = signature_verification_off();
        let (vm, _dir) = setup_test_vm(Product::Codex);
        let version = "rust-v0.130.0";
        let source_paths = vm.standard_source_paths(version, false);
        let bin_dir = source_paths.destination.parent().expect("bin dir");
        fs::create_dir_all(bin_dir).expect("mkdir");
        let managed = foreign_binary(bin_dir, "codex", "codex-cli 0.130.0");
        fs::write(source_paths.installing_marker(), "").expect("installing marker");

        let error = vm
            .install(InstallRequest::Import {
                version: version.to_string(),
                binary: managed.clone(),
            })
            .expect_err("OVM's own store must not be importable as a foreign install");

        assert!(error.to_string().contains("inside OVM's store"), "{error}");
        assert!(
            managed.exists(),
            "the refused import deleted the file it was given"
        );
        assert!(
            source_paths.root.exists(),
            "a refusal must not disturb the tree it refused"
        );
        assert!(!vm.standard_install_is_complete(version));
    }

    /// A symlink is not a way around it: the check resolves both sides.
    #[test]
    fn import_refuses_a_symlink_that_points_into_the_store() {
        let _signature = signature_verification_off();
        let (vm, dir) = setup_test_vm(Product::Codex);
        let version = "rust-v0.130.0";
        let source_paths = vm.standard_source_paths(version, false);
        let bin_dir = source_paths.destination.parent().expect("bin dir");
        fs::create_dir_all(bin_dir).expect("mkdir");
        let managed = foreign_binary(bin_dir, "codex", "codex-cli 0.130.0");
        let link = dir.path().join("codex-link");
        crate::symlink::switch_symlink(&link, &managed).expect("symlink into the store");

        let error = vm
            .install(InstallRequest::Import {
                version: version.to_string(),
                binary: link,
            })
            .expect_err("a symlink into the store must be refused like the store itself");

        assert!(error.to_string().contains("inside OVM's store"), "{error}");
        assert!(managed.exists(), "the refused import deleted its target");
    }

    /// The hole the path-based refusal left: it validated a *name*, and the
    /// name was resolved again later, by which time it could mean something
    /// else.
    ///
    /// An import validated its source, then waited — possibly for minutes — on
    /// the per-version install lock, and only then entered the transaction that
    /// removes this version's managed tree before copying. A symlink re-pointed
    /// during that wait therefore had its *old* target approved and its *new*
    /// target deleted-then-read: the managed tree was destroyed and the run
    /// died on a bare ENOENT. The refusal is re-decided under the lock, on the
    /// path the open handle came from, so the managed tree survives.
    #[test]
    fn import_refuses_a_symlink_retargeted_into_the_store_while_it_waited_for_the_lock() {
        let _signature = signature_verification_off();
        let (vm, dir) = setup_test_vm(Product::Codex);
        let version = "rust-v0.130.0";

        // The incomplete managed tree for this very version — binary present,
        // no `.complete` — which is exactly what the transaction's prepare step
        // quarantines and removes.
        let source_paths = vm.standard_source_paths(version, false);
        let bin_dir = source_paths.destination.parent().expect("bin dir");
        fs::create_dir_all(bin_dir).expect("mkdir");
        // Deliberately not the published name: were the copy allowed to run, it
        // would recreate `bin/codex` from the handle and hide the deletion of
        // everything else the prepare step took with it.
        let managed = foreign_binary(bin_dir, "codex-real", "codex-cli 0.130.0");
        fs::write(source_paths.installing_marker(), "").expect("installing marker");
        let managed_bytes = fs::read(&managed).expect("read the managed binary");

        // What the user passes: a link to a genuinely foreign binary, which the
        // pre-lock check accepts.
        let foreign = foreign_binary(dir.path(), "foreign-codex", "codex-cli 0.130.0");
        let link = dir.path().join("codex-link");
        crate::symlink::switch_symlink(&link, &foreign).expect("symlink to a foreign binary");

        let owner = vm.acquire_install_lock(version).expect("owner lock");
        let contender_vm = VersionManager {
            dirs: vm.dirs.clone(),
            product_dirs: vm.product_dirs.clone(),
            config: vm.config.clone(),
        };
        let (sender, receiver) = mpsc::channel();
        let import_link = link.clone();
        let contender = thread::spawn(move || {
            let result = contender_vm.install(InstallRequest::Import {
                version: version.to_string(),
                binary: import_link,
            });
            sender
                .send(result.err().map(|error| error.to_string()))
                .expect("send result");
        });

        assert!(
            receiver.recv_timeout(Duration::from_millis(200)).is_err(),
            "the import must still be waiting on the install lock"
        );
        // The swap the old check could not see: it happens after the source was
        // approved and before the bytes are read.
        crate::symlink::switch_symlink(&link, &managed).expect("retarget the link into the store");
        drop(owner);

        let error = receiver
            .recv_timeout(Duration::from_secs(30))
            .expect("the import finished once the lock was free")
            .expect("an import whose source became a managed path must be refused");
        contender.join().expect("import thread");

        assert!(error.contains("inside OVM's store"), "{error}");
        assert!(
            managed.exists(),
            "the retargeted import deleted the managed binary at {}",
            managed.display()
        );
        assert_eq!(
            fs::read(&managed).expect("read the managed binary"),
            managed_bytes,
            "the retargeted import rewrote the managed binary"
        );
        assert!(
            source_paths.root.exists(),
            "a refusal must not disturb the tree it refused"
        );
        assert!(!vm.standard_install_is_complete(version));
    }

    /// The hole `O_NOFOLLOW` plus a re-decided containment check still left:
    /// both are statements about a *name*, and only the final component of that
    /// name is pinned. Swap an intermediate directory of the user's path for a
    /// link into the store between the canonicalize and the open, then put the
    /// directory back, and the handle is on a managed file while every path
    /// check agrees the path is foreign — after which the transaction deletes
    /// the tree the import is holding.
    ///
    /// A hard link reaches the identical state with no race to lose: the name
    /// outside the store is a truthful second name for the managed inode, so
    /// canonicalization has nothing to unwind and containment has nothing to
    /// object to. That is what this test hands `install`, and the refusal comes
    /// from the descriptor's own identity instead.
    #[cfg(unix)]
    #[test]
    fn import_refuses_a_handle_on_a_file_this_install_would_delete() {
        let _signature = signature_verification_off();
        let (vm, dir) = setup_test_vm(Product::Codex);
        let version = "rust-v0.130.0";

        // The incomplete managed tree for this very version: binary present,
        // no `.complete`. `prepare_install_source` removes exactly this.
        let source_paths = vm.standard_source_paths(version, false);
        let bin_dir = source_paths.destination.parent().expect("bin dir");
        fs::create_dir_all(bin_dir).expect("mkdir");
        let managed = foreign_binary(bin_dir, "codex", "codex-cli 0.130.0");
        fs::write(source_paths.installing_marker(), "").expect("installing marker");
        let managed_bytes = fs::read(&managed).expect("read the managed binary");
        // The rest of the interrupted download, which the copy could not put
        // back even if it wanted to: without the refusal this is what the
        // adopt destroys while claiming to have left the original untouched.
        let rest_of_the_tree = source_paths.root.join("meta.json");
        fs::write(&rest_of_the_tree, "{}").expect("write the tree's other contents");

        // A second name for the managed inode, outside the store. This is the
        // descriptor/name divergence the directory swap manufactures.
        let adopted = dir.path().join("codex-adopted");
        fs::hard_link(&managed, &adopted).expect("hard link the managed binary out of the store");

        // Neither name-based check has anything to say about it: the path is
        // genuinely outside the store, before and after resolution.
        vm.reject_import_from_the_store(&adopted)
            .expect("the containment check cannot see a foreign name for managed bytes");
        vm.reject_import_from_the_store(&adopted.canonicalize().expect("canonicalize"))
            .expect("nor can it see one after resolution");

        let error = vm
            .install(InstallRequest::Import {
                version: version.to_string(),
                binary: adopted.clone(),
            })
            .expect_err("an import holding a file the install deletes must be refused");

        let error = error.to_string();
        assert!(error.contains("the same file as"), "{error}");
        assert!(
            error.contains("removes before it copies anything"),
            "{error}"
        );
        assert!(
            source_paths.root.exists(),
            "the refused import deleted the managed tree at {}",
            source_paths.root.display()
        );
        assert_eq!(
            fs::read(&managed).expect("read the managed binary"),
            managed_bytes,
            "the refused import disturbed the managed binary"
        );
        assert!(
            rest_of_the_tree.exists(),
            "the refused import deleted {} — the copy can only ever put the binary back, so \
             everything else in the interrupted install is what is actually lost",
            rest_of_the_tree.display()
        );
        assert!(adopted.exists(), "a refusal must delete nothing");
        assert!(!vm.standard_install_is_complete(version));
    }

    /// The invariant itself, without the transaction around it: the walk
    /// answers "is this descriptor one of the files about to be deleted?" by
    /// identity, so the path that opened it is irrelevant.
    #[cfg(unix)]
    #[test]
    fn the_deletion_set_walk_judges_the_handle_not_the_name() {
        let dir = tempdir().expect("tempdir");
        let doomed = dir.path().join("doomed");
        let nested = doomed.join("bin");
        fs::create_dir_all(&nested).expect("mkdir");
        let managed = foreign_binary(&nested, "codex", "codex-cli 0.130.0");
        let spared = foreign_binary(dir.path(), "foreign-codex", "codex-cli 0.130.0");
        let roots = [doomed.clone(), dir.path().join("quarantine")];

        // Opened through a hard link outside the doomed tree — a different
        // name, the same file.
        let alias = dir.path().join("codex-alias");
        fs::hard_link(&managed, &alias).expect("hard link");
        let bound = BoundSource::open(&alias, BoundSourceUse::Import).expect("bind the alias");
        assert_eq!(
            find_file_matching_handle(&bound.opened_metadata().expect("fstat"), &roots)
                .expect("walk the deletion set"),
            Some(managed.clone()),
            "a handle on a file inside the deletion set must be found however it was opened"
        );

        // A file that merely lives next door is not in danger.
        let outside =
            BoundSource::open(&spared, BoundSourceUse::Import).expect("bind the foreign binary");
        assert_eq!(
            find_file_matching_handle(&outside.opened_metadata().expect("fstat"), &roots)
                .expect("walk the deletion set"),
            None,
            "a genuinely foreign source must not be refused"
        );

        // A symlink inside the doomed tree pointing at that foreign file: the
        // deletion unlinks the link, not its target, so the walk must not
        // follow it and call the target doomed.
        crate::symlink::switch_symlink(&nested.join("codex-link"), &spared).expect("symlink");
        assert_eq!(
            find_file_matching_handle(&outside.opened_metadata().expect("fstat"), &roots)
                .expect("walk the deletion set"),
            None,
            "deleting a symlink does not delete its target, so following it would over-refuse"
        );

        // The quarantine path is deleted too, whatever is under it.
        let quarantine_bin = dir.path().join("quarantine").join("bin");
        fs::create_dir_all(&quarantine_bin).expect("mkdir");
        let quarantined = quarantine_bin.join("codex");
        fs::hard_link(&spared, &quarantined).expect("hard link into quarantine");
        assert_eq!(
            find_file_matching_handle(&outside.opened_metadata().expect("fstat"), &roots)
                .expect("walk the deletion set"),
            Some(quarantined),
            "the quarantine path is removed by the same step and belongs in the set"
        );
    }

    /// Canonicalizing and then opening the canonical path is still two steps.
    /// If a symlink is put where the resolved file was, in between, an open
    /// that follows links would read whatever it now points at while every
    /// check describes the link. The bind refuses instead — and, being a
    /// refusal, removes nothing.
    #[cfg(unix)]
    #[test]
    fn binding_refuses_a_resolved_path_that_became_a_symlink() {
        let dir = tempdir().expect("tempdir");
        let real = foreign_binary(dir.path(), "foreign-codex", "codex-cli 0.130.0");
        let link = dir.path().join("codex-link");
        crate::symlink::switch_symlink(&link, &real).expect("symlink");

        // `bind` is handed the path a resolution just returned; a link standing
        // there is precisely the swap that resolution could not have seen.
        let error = BoundSource::bind(&link, link.clone(), BoundSourceUse::Import)
            .expect_err("a resolved path that is a symlink at open time must be refused");

        let error = error.to_string();
        assert!(error.contains("Cannot import"), "{error}");
        assert!(error.contains("symbolic link"), "{error}");
        assert!(real.exists(), "a refused binding must delete nothing");
        assert!(link.exists(), "a refused binding must delete nothing");
    }

    /// The copy is bound to the handle, not to the name. Re-pointing the path
    /// at another file after the open cannot change a single byte that lands in
    /// staging, nor what the identity comparison sees.
    #[test]
    fn the_copy_reads_the_bound_handle_not_the_path() {
        let dir = tempdir().expect("tempdir");
        let source = foreign_binary(dir.path(), "foreign-codex", "codex-cli 0.130.0");
        let original = fs::read(&source).expect("read the source");

        let bound = BoundSource::open(&source, BoundSourceUse::Import).expect("bind the source");
        let before = bound.identity().expect("identity before");

        // The name now belongs to a different file, of a different size.
        let replacement = foreign_binary(
            dir.path(),
            "replacement-codex",
            "codex-cli 9.9.9 and rather more bytes than the original",
        );
        fs::rename(&replacement, &source).expect("replace the source by rename");

        let staged = dir.path().join("staged-import");
        bound
            .copy_to_new_file(&staged)
            .expect("copy from the handle");

        assert_eq!(
            fs::read(&staged).expect("staged bytes"),
            original,
            "the staged copy must be the bytes of the file that was opened and checked"
        );
        assert_eq!(
            bound.identity().expect("identity after"),
            before,
            "a swap behind the path cannot change what the handle describes"
        );
    }

    /// The staged copy is only a snapshot if the source held still while it was
    /// read. Off macOS there is no signature to fall back on, so an updater
    /// rewriting the executable in place during `fs::copy` could hand a mixed
    /// old/new byte stream to a `--version` re-check that still answers
    /// plausibly. The identity read before and after the copy refuses that.
    #[test]
    fn a_source_rewritten_around_the_copy_is_refused() {
        let dir = tempdir().expect("tempdir");
        let source = foreign_binary(dir.path(), "foreign-codex", "codex-cli 0.130.0");

        let before = SourceIdentity::read(&source).expect("identity before");
        refuse_if_source_changed_during_copy(Product::Codex, &source, &before, &before)
            .expect("an untouched source must be accepted");

        // An in-place rewrite: same path, new contents. The mtime is set
        // explicitly so the verdict cannot hinge on clock granularity.
        fs::write(
            &source,
            "#!/bin/sh\necho 'codex-cli 0.131.0 and then some'\n",
        )
        .expect("rewrite source");
        set_file_mtime(&source, FileTime::from_unix_time(1_700_000_000, 0)).expect("set mtime");
        let after = SourceIdentity::read(&source).expect("identity after");

        let error = refuse_if_source_changed_during_copy(Product::Codex, &source, &before, &after)
            .expect_err("a source that moved under the copy must not be published");
        assert!(error.to_string().contains("changed on disk"), "{error}");
    }

    /// The shape that size and mtime alone would miss: an updater landing a
    /// same-size build by rename, with the timestamp restored. The path is
    /// unchanged and the stat looks identical — but it is a different file, and
    /// the device/inode pair says so.
    #[cfg(unix)]
    #[test]
    fn a_source_replaced_by_rename_is_refused_even_when_size_and_mtime_match() {
        let dir = tempdir().expect("tempdir");
        let frozen = FileTime::from_unix_time(1_700_000_000, 0);
        let source = foreign_binary(dir.path(), "foreign-codex", "codex-cli 0.130.0");
        set_file_mtime(&source, frozen).expect("freeze mtime");
        let before = SourceIdentity::read(&source).expect("identity before");

        let replacement = foreign_binary(dir.path(), "replacement-codex", "codex-cli 0.130.0");
        fs::rename(&replacement, &source).expect("replace by rename");
        set_file_mtime(&source, frozen).expect("restore mtime");
        let after = SourceIdentity::read(&source).expect("identity after");

        assert_eq!(before.len, after.len, "premise: the size is unchanged");
        assert_eq!(
            before.modified, after.modified,
            "premise: the mtime is unchanged"
        );
        refuse_if_source_changed_during_copy(Product::Codex, &source, &before, &after)
            .expect_err("a replaced file must be refused even when its stat looks unchanged");
    }

    /// The transaction promises that a failed install leaves nothing behind.
    /// Publishing the markers is part of the install, not an epilogue: a
    /// `.complete` that cannot be written is an install that did not succeed,
    /// and it must be cleaned up like any other rather than returning an error
    /// with the incomplete source still sitting in the store.
    #[test]
    fn a_failed_marker_publication_cleans_the_incomplete_source() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        let version = "rust-v0.130.0";
        let source = vm.standard_source_paths(version, false);

        let error = vm
            .run_install_transaction(version, &source, || {
                fs::create_dir_all(source.destination.parent().expect("bin dir"))?;
                fs::write(&source.destination, "installed bytes")?;
                // A directory where the marker file belongs: `.complete` cannot
                // be written, which is the failure the Ok arm used to return
                // raw, skipping cleanup entirely.
                fs::create_dir_all(source.complete_marker())?;
                Ok(())
            })
            .expect_err("an install that cannot be published has not succeeded");

        assert!(!error.to_string().is_empty());
        assert!(
            !source.root.exists(),
            "an unpublishable install must not leave its source tree behind"
        );
        assert!(
            !vm.product_dirs.version_dir(version).exists(),
            "a version directory this transaction created must not outlive it"
        );
        assert!(!vm.standard_install_is_complete(version));
    }

    #[test]
    fn import_rejects_bundle_products() {
        for (product, version) in [(Product::Pi, "0.79.10"), (Product::Qm, "0.1.4")] {
            let (vm, dir) = setup_test_vm(product);
            let source = dir
                .path()
                .join(format!("foreign-{}", product.canonical_name()));
            fs::write(&source, "foreign binary").expect("source binary");

            let error = vm
                .install(InstallRequest::Import {
                    version: version.to_string(),
                    binary: source,
                })
                .expect_err("bundle products cannot be imported as one file");

            assert!(error.to_string().contains("bundles"), "{error}");
        }
    }

    #[test]
    fn legacy_pi_bundle_without_package_json_remains_complete_and_usable() {
        let (vm, _dir) = setup_test_vm(Product::Pi);
        let version = "0.45.3";
        let release = vm.product_dirs.version_dir(version).join("release");
        let binary = release.join("bundle/pi/pi");
        fs::create_dir_all(binary.parent().expect("binary parent")).expect("bundle directory");
        fs::write(&binary, "legacy pi binary").expect("binary");
        fs::write(release.join("meta.json"), "{}").expect("metadata");
        fs::write(release.join(COMPLETE_MARKER), "").expect("complete marker");

        assert!(vm.standard_install_is_complete(version));
        assert_eq!(vm.list_installed().expect("installed versions"), [version]);
        vm.use_version(version).expect("legacy Pi remains usable");
        assert_eq!(
            vm.current_version().expect("active version").as_deref(),
            Some(version)
        );
        assert_eq!(vm.active_binary_path(version), binary);
    }

    #[test]
    fn dev_install_recovers_crashed_source_and_publishes_markers() {
        let (vm, dir) = setup_test_vm(Product::Codex);
        let version = "dev:recovery";
        let dev = vm.dev_source_paths(version);
        fs::create_dir_all(dev.destination.parent().expect("destination parent")).expect("mkdir");
        fs::write(&dev.destination, "stale partial").expect("partial binary");
        fs::write(dev.installing_marker(), "").expect("installing marker");
        let quarantine = dev.quarantine_path().expect("quarantine path");
        fs::create_dir_all(&quarantine).expect("stale quarantine");
        fs::write(quarantine.join("old"), "older partial").expect("stale quarantine file");
        let source = dir.path().join("new-codex");
        fs::write(&source, "fresh binary").expect("source binary");

        vm.install(InstallRequest::Dev {
            label: "recovery".into(),
            source: DevInstallSource::Binary(source),
            link: false,
        })
        .expect("recover install");

        assert_eq!(fs::read(&dev.destination).expect("binary"), b"fresh binary");
        assert!(dev.complete_marker().exists());
        assert!(!dev.installing_marker().exists());
        assert!(!quarantine.exists());
    }

    /// A dev source inside the incomplete dev tree it would replace names a
    /// file the transaction's prepare step deletes before the closure reads it
    /// — the same self-deletion shape adoption refuses. Re-running a dev
    /// install against the installed copy of an interrupted install is the
    /// realistic trigger.
    #[test]
    fn dev_install_refuses_a_source_inside_the_tree_it_would_replace() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        let version = "dev:selfsource";
        let dev = vm.dev_source_paths(version);
        fs::create_dir_all(dev.destination.parent().expect("destination parent")).expect("mkdir");
        fs::write(&dev.destination, "interrupted install").expect("partial binary");
        fs::write(dev.installing_marker(), "").expect("installing marker");

        let error = vm
            .install(InstallRequest::Dev {
                label: "selfsource".into(),
                source: DevInstallSource::Binary(dev.destination.clone()),
                link: false,
            })
            .expect_err("a source the install would delete must be refused");

        assert!(
            error.to_string().contains("would be deleted"),
            "unexpected error: {error}"
        );
        assert_eq!(
            fs::read(&dev.destination).expect("source survives"),
            b"interrupted install"
        );
        assert!(dev.installing_marker().exists());
    }

    /// A link cannot smuggle the same self-deletion past the refusal wearing a
    /// foreign name — [`path_is_inside`] resolves both sides — and the message
    /// has to say so, because the path the user typed looks perfectly innocent
    /// on its own.
    ///
    /// What deciding this on [`BoundSource::resolved`] rather than on the typed
    /// name adds is not caught here, and could not be caught by any
    /// deterministic test: it makes the refusal and the copy talk about the
    /// same file. The typed name is re-resolved at the instant it is checked,
    /// so a link retargeted a moment later is judged as what it was; the bound
    /// resolution is the file the handle is actually on. The teeth against that
    /// retarget are the identity refusal and the handle-bound copy below —
    /// this is the coherence half.
    #[test]
    fn dev_install_refuses_a_source_that_resolves_into_the_tree_it_would_replace() {
        let (vm, dir) = setup_test_vm(Product::Codex);
        let version = "dev:resolvedself";
        let dev = vm.dev_source_paths(version);
        fs::create_dir_all(dev.destination.parent().expect("destination parent")).expect("mkdir");
        fs::write(&dev.destination, "interrupted install").expect("partial binary");
        fs::write(dev.installing_marker(), "").expect("installing marker");

        let link = dir.path().join("codex-link");
        crate::symlink::switch_symlink(&link, &dev.destination).expect("symlink into the dev tree");

        let error = vm
            .install(InstallRequest::Dev {
                label: "resolvedself".into(),
                source: DevInstallSource::Binary(link.clone()),
                link: false,
            })
            .expect_err("a link into the tree this install replaces must be refused");

        let error = error.to_string();
        assert!(error.contains("would be deleted"), "{error}");
        assert!(
            error.contains("resolves to"),
            "a refusal decided on the resolved path must name it: {error}"
        );
        assert_eq!(
            fs::read(&dev.destination).expect("source survives"),
            b"interrupted install"
        );
        assert!(dev.installing_marker().exists());
        assert!(link.exists(), "a refusal must delete nothing");
    }

    /// Both refusals above judge a *name*, and a name outside the dev tree can
    /// still be a truthful second name for a file inside it: a hard link needs
    /// no race at all — canonicalization has nothing to unwind and containment
    /// nothing to object to. (An intermediate directory swapped for a link into
    /// the tree during the resolve → open window reaches the same state.) Only
    /// the bound descriptor's own identity can tell that the bytes about to be
    /// read are bytes `prepare_install_source` is about to delete.
    #[cfg(unix)]
    #[test]
    fn dev_install_refuses_a_handle_on_a_file_this_install_would_delete() {
        let (vm, dir) = setup_test_vm(Product::Codex);
        let version = "dev:aliased";
        let dev = vm.dev_source_paths(version);
        fs::create_dir_all(dev.destination.parent().expect("destination parent")).expect("mkdir");
        fs::write(&dev.destination, "interrupted install").expect("partial binary");
        fs::write(dev.installing_marker(), "").expect("installing marker");
        // The rest of the interrupted install. Even if the copy were allowed to
        // run it could only ever put the binary back, so this is what a missing
        // refusal actually destroys.
        let rest_of_the_tree = dev.root.join("meta.json");
        fs::write(&rest_of_the_tree, "{}").expect("write the tree's other contents");

        let aliased = dir.path().join("codex-alias");
        fs::hard_link(&dev.destination, &aliased)
            .expect("hard link the dev binary out of the tree");

        // Neither name-based check has anything to say about it, before or
        // after resolution.
        assert!(!path_is_inside(&dev.root, &aliased));
        assert!(!path_is_inside(
            &dev.root,
            &aliased.canonicalize().expect("canonicalize")
        ));

        let error = vm
            .install(InstallRequest::Dev {
                label: "aliased".into(),
                source: DevInstallSource::Binary(aliased.clone()),
                link: false,
            })
            .expect_err("a dev install holding a file it deletes must be refused");

        let error = error.to_string();
        assert!(error.contains("is the same file as"), "{error}");
        assert!(
            error.contains("removes before it copies anything"),
            "{error}"
        );
        assert_eq!(
            fs::read(&dev.destination).expect("the dev binary survives"),
            b"interrupted install",
            "the refused install disturbed the binary it was pointed at"
        );
        assert!(
            rest_of_the_tree.exists(),
            "the refused install deleted {} — the copy can only ever put the binary back, so \
             everything else in the interrupted install is what is actually lost",
            rest_of_the_tree.display()
        );
        assert!(dev.installing_marker().exists());
        assert!(aliased.exists(), "a refusal must delete nothing");
        assert!(!dev.is_complete());
    }

    /// Link mode is gated by the same refusals, and it has its own reason to
    /// be: a symlink to a path the prepare step then deletes is published
    /// dangling, and `.complete` says it is fine.
    #[test]
    fn dev_link_refuses_a_source_inside_the_tree_it_would_replace() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        let version = "dev:linkself";
        let dev = vm.dev_source_paths(version);
        fs::create_dir_all(dev.destination.parent().expect("destination parent")).expect("mkdir");
        fs::write(&dev.destination, "interrupted install").expect("partial binary");
        fs::write(dev.installing_marker(), "").expect("installing marker");

        let error = vm
            .install(InstallRequest::Dev {
                label: "linkself".into(),
                source: DevInstallSource::Binary(dev.destination.clone()),
                link: true,
            })
            .expect_err("a link to a file this install deletes must be refused");

        assert!(error.to_string().contains("would be deleted"), "{error}");
        assert_eq!(
            fs::read(&dev.destination).expect("source survives"),
            b"interrupted install"
        );
        assert!(!dev.is_complete());
    }

    /// A `--link` install publishes a symlink that outlives the command, so
    /// what it points at must be the file OVM resolved and checked — not the
    /// name it was handed. A developer's `--binary` is very often a convenience
    /// link they re-point at whichever build they are testing; if the managed
    /// destination copied that name, retargeting it later would silently change
    /// what the installed version means, and retargeting it *at the destination
    /// itself* would close a symlink cycle.
    #[cfg(unix)]
    #[test]
    fn a_dev_link_points_at_the_resolved_file_not_the_name_it_was_given() {
        let (vm, dir) = setup_test_vm(Product::Codex);
        let build = dir.path().join("target").join("release");
        fs::create_dir_all(&build).expect("mkdir");
        let binary = build.join("codex");
        fs::write(&binary, "fake-binary").expect("write binary");
        let resolved = binary.canonicalize().expect("canonicalize");

        let link = dir.path().join("codex-current");
        crate::symlink::switch_symlink(&link, &binary).expect("symlink to the build");

        vm.install(InstallRequest::Dev {
            label: "resolvedlink".into(),
            source: DevInstallSource::Binary(link.clone()),
            link: true,
        })
        .expect("install dev link");

        let destination = vm.product_dirs.dev_bin("dev:resolvedlink");
        assert_eq!(
            fs::read_link(&destination).expect("read the published link"),
            resolved,
            "the managed destination must name the file OVM checked, not the link it was given"
        );

        // The user's link is theirs to re-point, including at the destination
        // itself. Neither can change what the installed version resolves to.
        crate::symlink::switch_symlink(&link, &destination).expect("retarget at the destination");
        assert_eq!(
            fs::read_link(&destination).expect("read the published link"),
            resolved
        );
        assert_eq!(
            fs::read(&destination).expect("the destination still resolves to bytes"),
            b"fake-binary"
        );

        // Provenance still records what the user asked for — the value the
        // lock-contention reuse check compares against.
        let metadata = vm
            .dev_install_metadata("dev:resolvedlink")
            .expect("metadata")
            .expect("present");
        assert_eq!(metadata.mode.label(), "link");
        assert_eq!(metadata.source, link);
    }

    /// The copy-mode half of the same guarantee, at the moment that actually
    /// matters. `fs::copy` re-follows the source path *inside* the transaction
    /// — after `prepare_install_source` has already deleted the old dev tree —
    /// so a link re-pointed between the checks and the copy decided what got
    /// installed, or made the copy fail with the old tree already gone.
    ///
    /// The PreInstall hook makes that race deterministic: it runs inside the
    /// transaction, after the source has been bound to a handle and before the
    /// copy. The hook swaps the user's link onto a decoy, and the install must
    /// still produce the bytes of the file OVM opened and checked. The link it
    /// retargets lives outside the store, so — unlike a plant *inside* the
    /// install tree — `prepare_install_source` does not erase it; the handle is
    /// what makes the swap inert.
    #[cfg(unix)]
    #[test]
    fn a_dev_copy_reads_the_handle_not_the_path_it_was_given() {
        use std::os::unix::fs::PermissionsExt;

        let (vm, dir) = setup_test_vm(Product::Codex);
        let build = dir.path().join("codex-build");
        fs::write(&build, "the build OVM opened").expect("write build");
        let decoy = dir.path().join("codex-decoy");
        fs::write(&decoy, "a different build entirely").expect("write decoy");
        let link = dir.path().join("codex-current");
        crate::symlink::switch_symlink(&link, &build).expect("symlink to the build");

        fs::create_dir_all(&vm.dirs.hooks).expect("hooks dir");
        let hook = vm.dirs.hooks.join("pre-install.sh");
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\nrm -f '{link}'\nln -s '{decoy}' '{link}'\n",
                link = link.display(),
                decoy = decoy.display(),
            ),
        )
        .expect("write hook");
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod hook");

        vm.install(InstallRequest::Dev {
            label: "raced".into(),
            source: DevInstallSource::Binary(link.clone()),
            link: false,
        })
        .expect("install dev copy");

        assert_eq!(
            fs::read_link(&link).expect("the hook retargeted the user's link"),
            decoy,
            "the hook must have run inside the transaction for this test to mean anything"
        );
        assert_eq!(
            fs::read(vm.product_dirs.dev_bin("dev:raced")).expect("installed binary"),
            b"the build OVM opened",
            "the copy must read the handle OVM bound, not whatever the path means by then"
        );
    }

    /// Write a PreInstall hook that plants a symbolic link at `plant`, pointing
    /// at `target`.
    ///
    /// The hook runs inside the install transaction but *before*
    /// `prepare_install_source`, so a plant anywhere under the source root is
    /// renamed away and deleted with the old tree before OVM writes a byte. The
    /// tests below use it to prove exactly that: the interference is erased, not
    /// merely refused.
    #[cfg(unix)]
    fn plant_symlink_from_pre_install_hook(vm: &VersionManager, plant: &Path, target: &Path) {
        use std::os::unix::fs::PermissionsExt;

        fs::create_dir_all(&vm.dirs.hooks).expect("hooks dir");
        let hook = vm.dirs.hooks.join("pre-install.sh");
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\nmkdir -p '{parent}'\nln -sfn '{target}' '{plant}'\n",
                parent = plant.parent().expect("plant parent").display(),
                target = target.display(),
                plant = plant.display(),
            ),
        )
        .expect("write hook");
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod hook");
    }

    /// Round-18 saw the copy's *destination* as name-bound as its source once
    /// was: `File::create` follows a symbolic link and truncates what it points
    /// at, so a link planted at the published path — aimed at the very file OVM
    /// is copying — made the install truncate its own source, read back the
    /// zero bytes it had just created, publish them, write `.complete` and exit
    /// 0. The user's build was gone and OVM said "Installed".
    ///
    /// Round-19 removed the window rather than the symptom. The hook still
    /// plants the link; `prepare_install_source` now runs afterwards and renames
    /// the whole tree away, so by the time the copy runs there is nothing at the
    /// destination at all. The right outcome is therefore not a refusal but an
    /// ordinary, correct install: the build keeps every byte, and the published
    /// binary is the build.
    #[cfg(unix)]
    #[test]
    fn a_symlink_planted_at_a_dev_copy_destination_is_erased_by_prepare() {
        let (vm, dir) = setup_test_vm(Product::Codex);
        let build = dir.path().join("codex-build");
        let build_bytes = b"the build the user asked OVM to install";
        fs::write(&build, build_bytes).expect("write build");

        let version = "dev:planted";
        let dev = vm.dev_source_paths(version);
        plant_symlink_from_pre_install_hook(&vm, &dev.destination, &build);

        vm.install(InstallRequest::Dev {
            label: "planted".into(),
            source: DevInstallSource::Binary(build.clone()),
            link: false,
        })
        .expect("a plant the prepare step erases must not fail the install");

        assert_eq!(
            fs::read(&build).expect("the user's build"),
            build_bytes,
            "the file being copied must not be truncated through the planted link"
        );
        assert!(
            !fs::symlink_metadata(&dev.destination)
                .expect("published destination")
                .file_type()
                .is_symlink(),
            "the planted link must be gone, not published as the install"
        );
        assert_eq!(
            fs::read(&dev.destination).expect("published binary"),
            build_bytes,
            "the published binary must be the bytes of the build OVM opened"
        );
        assert!(dev.is_complete());
    }

    /// The same planting, against the import's staging path. Adoption copies to
    /// `.<binary>.importing` and only renames it into place once the staged
    /// bytes have passed every proof — but those proofs run *after* the copy, so
    /// a `File::create` here destroyed the original before anything could
    /// object, and the identity re-check then compared two views of a file that
    /// was already empty. With the hook moved ahead of the prepare step the
    /// staging path is empty again by the time the copy reaches it, so the adopt
    /// simply succeeds and the adopted file is untouched.
    #[cfg(unix)]
    #[test]
    fn a_symlink_planted_at_the_import_staging_path_is_erased_by_prepare() {
        let _signature = signature_verification_off();
        let (vm, dir) = setup_test_vm(Product::Codex);
        let version = "rust-v0.130.0";
        let source = foreign_binary(dir.path(), "foreign-codex", "codex-cli 0.130.0");
        let original = fs::read(&source).expect("read the source");

        let source_paths = vm.standard_source_paths(version, false);
        let staged = staged_import_path(&source_paths.destination).expect("staged path");
        plant_symlink_from_pre_install_hook(&vm, &staged, &source);

        vm.install(InstallRequest::Import {
            version: version.to_string(),
            binary: source.clone(),
        })
        .expect("a plant the prepare step erases must not fail the import");

        assert_eq!(
            fs::read(&source).expect("the user's binary"),
            original,
            "adoption must never truncate the file it is adopting"
        );
        assert_eq!(
            fs::read(&source_paths.destination).expect("published binary"),
            original,
            "the published binary must be the bytes OVM read from the handle"
        );
        assert!(
            !path_entry_exists(&staged),
            "the staging path must not outlive the publish"
        );
        assert!(source_paths.complete_marker().exists());
    }

    /// `.complete` is the sentence "this version is installed", and `fs::write`
    /// would have spoken it through a symbolic link — truncating the link's
    /// target and leaving the store with no marker at all, or with one an
    /// outside writer chose the location of. A hook can still plant that link,
    /// but only before the tree it lives in is recreated: the bystander keeps
    /// its bytes and the marker written is a real, empty file in OVM's own tree.
    #[cfg(unix)]
    #[test]
    fn a_symlink_planted_at_the_complete_marker_is_erased_by_prepare() {
        let (vm, dir) = setup_test_vm(Product::Codex);
        let build = dir.path().join("codex-build");
        fs::write(&build, "a build worth keeping").expect("write build");
        let victim = dir.path().join("innocent-bystander");
        let victim_bytes = b"bytes no install has any business touching";
        fs::write(&victim, victim_bytes).expect("write victim");

        let version = "dev:markerplanted";
        let dev = vm.dev_source_paths(version);
        plant_symlink_from_pre_install_hook(&vm, &dev.complete_marker(), &victim);

        vm.install(InstallRequest::Dev {
            label: "markerplanted".into(),
            source: DevInstallSource::Binary(build),
            link: false,
        })
        .expect("a plant the prepare step erases must not fail the install");

        assert_eq!(
            fs::read(&victim).expect("the bystander"),
            victim_bytes,
            "the marker write must not reach through the planted link"
        );
        let marker = fs::symlink_metadata(dev.complete_marker()).expect("the published marker");
        assert!(
            !marker.file_type().is_symlink() && marker.is_file(),
            "`.complete` must be a real file OVM created, not a link someone else chose"
        );
        assert_eq!(marker.len(), 0);
        assert!(vm.install_is_complete(version));
    }

    /// The round-19 blocker, end to end: a *parent* swap.
    ///
    /// `create_new_file` binds the final component of every destination, and
    /// nothing else. A directory inside the fresh tree replaced by a link to an
    /// outside directory is traversed rather than opened, so every later step
    /// lands out there instead: the import's `rename` publish replaces a file
    /// that was never OVM's — the reviewer's demonstration — and the install
    /// still writes `.complete` and reports success.
    ///
    /// Nothing guards each write's parents. Instead the hook, the one supported
    /// way foreign code runs inside the transaction, was moved ahead of
    /// `prepare_install_source`, which renames the whole source root away and
    /// recreates it. The swap the hook makes is therefore erased before a single
    /// byte is written: the external directory is untouched, `release/bin` is a
    /// real directory again, and the published binary is the adopted file.
    #[cfg(unix)]
    #[test]
    fn a_hook_swapping_a_parent_directory_for_a_link_out_of_the_tree_is_erased_by_prepare() {
        let _signature = signature_verification_off();
        let (vm, dir) = setup_test_vm(Product::Codex);
        let version = "rust-v0.130.0";
        let source = foreign_binary(dir.path(), "foreign-codex", "codex-cli 0.130.0");
        let original = fs::read(&source).expect("read the source");

        let source_paths = vm.standard_source_paths(version, false);
        let bin_dir = source_paths
            .destination
            .parent()
            .expect("destination parent")
            .to_path_buf();

        // Somewhere entirely outside the store, holding a file that happens to
        // share the published binary's name: what the publish `rename` replaces
        // if it is allowed to traverse the swapped parent.
        let outside = dir.path().join("not-ovms-directory");
        fs::create_dir_all(&outside).expect("outside dir");
        let victim = outside.join(
            source_paths
                .destination
                .file_name()
                .expect("destination name"),
        );
        let victim_bytes = b"a file OVM was never pointed at";
        fs::write(&victim, victim_bytes).expect("write victim");

        // The hook swaps `release/bin` itself for a link to that directory.
        plant_symlink_from_pre_install_hook(&vm, &bin_dir, &outside);

        vm.install(InstallRequest::Import {
            version: version.to_string(),
            binary: source.clone(),
        })
        .expect("a swapped parent the prepare step erases must not fail the import");

        assert_eq!(
            fs::read(&victim).expect("the file outside the store"),
            victim_bytes,
            "the publish reached through the swapped parent and replaced a file outside the store"
        );
        assert_eq!(
            fs::read_dir(&outside).expect("outside dir").count(),
            1,
            "nothing may be created outside the store"
        );
        assert!(
            !fs::symlink_metadata(&bin_dir)
                .expect("bin dir")
                .file_type()
                .is_symlink(),
            "the planted directory link must be gone, replaced by OVM's own directory"
        );
        assert_eq!(
            fs::read(&source_paths.destination).expect("published binary"),
            original
        );
        assert_eq!(
            fs::read(&source).expect("the user's binary"),
            original,
            "the adopted file must be untouched"
        );
        assert!(source_paths.complete_marker().exists());
    }

    /// The same parent swap against an *extraction*, which writes many files
    /// through paths it builds by joining onto a directory it never re-checks.
    /// `tar` unpacks with `File::create`, so a swapped `extracted/` used to
    /// overwrite whatever the outside directory happened to hold.
    ///
    /// The four standard installers download before they extract, so this drives
    /// the transaction directly with the real npm extraction as its closure —
    /// the same `run_install_transaction`, the same hook, the same prepare step,
    /// with the network left out.
    #[cfg(unix)]
    #[test]
    fn a_hook_swapping_an_extraction_directory_is_erased_by_prepare() {
        let (vm, dir) = setup_test_vm(Product::Claude);
        let version = "2.1.71";
        let source = vm.standard_source_paths(version, true);
        let extracted = source.root.join("extracted");

        let outside = dir.path().join("not-ovms-directory");
        fs::create_dir_all(outside.join("package")).expect("outside dir");
        let victim = outside.join("package").join("index.js");
        let victim_bytes = b"console.log('a file OVM was never pointed at');";
        fs::write(&victim, victim_bytes).expect("write victim");

        let tarball = dir.path().join("claude-code-2.1.71.tgz");
        write_tar_gz(
            &tarball,
            &[
                ("package/index.js", b"console.log('the real package');"),
                ("package/package.json", b"{}"),
            ],
        );

        plant_symlink_from_pre_install_hook(&vm, &extracted, &outside);

        vm.run_install_transaction(version, &source, || {
            crate::sources::npm::extract_tarball(&tarball, &extracted)
        })
        .expect("a swapped extraction directory the prepare step erases must not fail");

        assert_eq!(
            fs::read(&victim).expect("the file outside the store"),
            victim_bytes,
            "extraction reached through the swapped directory and overwrote a file outside the store"
        );
        assert!(
            !fs::symlink_metadata(&extracted)
                .expect("extraction dir")
                .file_type()
                .is_symlink(),
            "the planted directory link must be gone, replaced by OVM's own directory"
        );
        assert_eq!(
            fs::read(extracted.join("package").join("index.js")).expect("extracted entry"),
            b"console.log('the real package');"
        );
        assert!(source.complete_marker().exists());
    }

    /// Build a gzipped tar of `entries` — enough of an npm tarball for
    /// [`crate::sources::npm::extract_tarball`] to unpack.
    #[cfg(unix)]
    fn write_tar_gz(path: &Path, entries: &[(&str, &[u8])]) {
        let file = fs::File::create(path).expect("archive file");
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (name, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, name, *contents)
                .expect("append entry");
        }
        builder
            .into_inner()
            .expect("finish tar")
            .finish()
            .expect("finish gzip");
    }

    /// The belt behind the reorder. A process writing into OVM's own store for a
    /// version this process holds the install lock on is unsupported, not
    /// impossible, and it is the only writer left that can swap a parent
    /// mid-transaction. The closure runs in exactly that window, so it plays the
    /// part of that process: the swap must be caught before `.complete` is
    /// written, and the install must clean up like any other failure.
    #[cfg(unix)]
    #[test]
    fn an_install_root_swapped_during_the_write_window_is_refused_before_publishing() {
        let (vm, dir) = setup_test_vm(Product::Codex);
        let version = "rust-v0.130.0";
        let source = vm.standard_source_paths(version, false);

        let outside = dir.path().join("not-ovms-directory");
        fs::create_dir_all(&outside).expect("outside dir");

        let error = vm
            .run_install_transaction(version, &source, || {
                fs::create_dir_all(source.destination.parent().expect("bin dir"))?;
                fs::write(&source.destination, "installed bytes")?;
                // The unsupported concurrent writer, made deterministic.
                remove_path_entry(&source.root)?;
                std::os::unix::fs::symlink(&outside, &source.root)?;
                Ok(())
            })
            .expect_err("a tree that is no longer the prepared tree must not be published");

        let error = error.to_string();
        assert!(error.contains("Refusing to publish the install"), "{error}");
        assert!(
            !outside.join(COMPLETE_MARKER).exists(),
            "`.complete` must not be written outside the store"
        );
        assert!(
            fs::read_dir(&outside).expect("outside dir").count() == 0,
            "nothing may be created outside the store"
        );
        assert!(!vm.standard_install_is_complete(version));
    }

    /// The belt's two verdicts, decided directly. `prepared` is the root as it
    /// was resolved the moment the prepare step created it.
    #[cfg(unix)]
    #[test]
    fn the_install_root_post_condition_accepts_the_tree_it_prepared() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("release");
        fs::create_dir_all(&root).expect("prepare the root");
        let prepared = root.canonicalize().expect("canonicalize");

        // Contents change constantly during an install — that is the install.
        fs::create_dir_all(root.join("bin")).expect("mkdir");
        fs::write(root.join("bin").join("codex"), "installed bytes").expect("write");
        // Including symlinks the install legitimately creates (npm's
        // `node_modules/.bin`, a `--link` dev publish), which is why the check
        // never walks the contents.
        std::os::unix::fs::symlink(root.join("bin").join("codex"), root.join("linked"))
            .expect("a symlink an install may legitimately create");

        refuse_if_the_install_root_moved(&root, &prepared)
            .expect("an unmoved root must be publishable");
    }

    #[cfg(unix)]
    #[test]
    fn the_install_root_post_condition_refuses_a_root_replaced_by_a_link() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("release");
        fs::create_dir_all(&root).expect("prepare the root");
        let prepared = root.canonicalize().expect("canonicalize");

        let outside = dir.path().join("elsewhere");
        fs::create_dir_all(&outside).expect("outside dir");
        fs::remove_dir_all(&root).expect("remove the root");
        std::os::unix::fs::symlink(&outside, &root).expect("swap the root for a link");

        let error = refuse_if_the_install_root_moved(&root, &prepared)
            .expect_err("a root replaced by a link must be refused");
        assert!(
            error.to_string().contains("no longer a directory"),
            "{error}"
        );
    }

    /// The shape the round-19 blocker actually used: the root itself is still a
    /// perfectly ordinary directory. It is a *parent* that was swapped, so only
    /// re-resolving the path can tell.
    #[cfg(unix)]
    #[test]
    fn the_install_root_post_condition_refuses_a_swapped_parent() {
        let dir = tempdir().expect("tempdir");
        let parent = dir.path().join("version");
        let root = parent.join("release");
        fs::create_dir_all(&root).expect("prepare the root");
        let prepared = root.canonicalize().expect("canonicalize");

        let outside = dir.path().join("elsewhere");
        fs::create_dir_all(outside.join("release")).expect("outside tree");
        fs::remove_dir_all(&parent).expect("remove the parent");
        std::os::unix::fs::symlink(&outside, &parent).expect("swap the parent for a link");

        assert!(
            fs::symlink_metadata(&root).expect("root").is_dir(),
            "premise: the root itself still looks like an ordinary directory"
        );

        let error = refuse_if_the_install_root_moved(&root, &prepared)
            .expect_err("a root that now resolves elsewhere must be refused");
        let error = error.to_string();
        assert!(error.contains("now resolves to"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn the_install_root_post_condition_refuses_a_root_that_is_gone() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path().join("release");
        fs::create_dir_all(&root).expect("prepare the root");
        let prepared = root.canonicalize().expect("canonicalize");
        fs::remove_dir_all(&root).expect("remove the root");

        let error = refuse_if_the_install_root_moved(&root, &prepared)
            .expect_err("a root that no longer exists must be refused");
        assert!(
            error.to_string().contains("can no longer be read"),
            "{error}"
        );
    }

    /// The publish step is a `rename`, and the reason it needs no `O_EXCL` twin
    /// is a platform guarantee worth pinning rather than asserting in prose: a
    /// symbolic link at the destination is *replaced*, not followed. Were it
    /// followed, publishing would truncate the link's target exactly as
    /// `File::create` did.
    #[cfg(unix)]
    #[test]
    fn a_symlink_at_the_publish_target_is_replaced_not_followed() {
        let dir = tempdir().expect("tempdir");
        let victim = dir.path().join("victim");
        let victim_bytes = b"the file a planted link would aim at";
        fs::write(&victim, victim_bytes).expect("write victim");

        let staged = dir.path().join("staged");
        fs::write(&staged, "the staged, verified bytes").expect("write staged");
        let destination = dir.path().join("destination");
        crate::symlink::switch_symlink(&destination, &victim).expect("plant the link");

        fs::rename(&staged, &destination).expect("publish over the link");

        assert_eq!(
            fs::read(&victim).expect("the victim"),
            victim_bytes,
            "rename must not write through a symlink at its destination"
        );
        assert!(
            !fs::symlink_metadata(&destination)
                .expect("destination")
                .file_type()
                .is_symlink(),
            "the planted link must be gone, replaced by the published file"
        );
        assert_eq!(
            fs::read(&destination).expect("published"),
            b"the staged, verified bytes"
        );
    }

    #[test]
    #[cfg(unix)]
    fn post_install_hook_sees_a_published_complete_source() {
        use std::os::unix::fs::PermissionsExt;

        let (vm, _dir) = setup_test_vm(Product::Codex);
        let version = "dev:hooked";
        let dev = vm.dev_source_paths(version);
        let sentinel = _dir.path().join("hook-saw-complete");

        // The hook fires only when the version is already published: `.complete`
        // present AND `.installing` gone. It writes a sentinel iff both hold,
        // proving markers are finalized before PostInstall runs.
        fs::create_dir_all(&vm.dirs.hooks).expect("hooks dir");
        let hook = vm.dirs.hooks.join("post-install.sh");
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\nif [ -f '{complete}' ] && [ ! -f '{installing}' ]; then : > '{sentinel}'; fi\n",
                complete = dev.complete_marker().display(),
                installing = dev.installing_marker().display(),
                sentinel = sentinel.display(),
            ),
        )
        .expect("write hook");
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod hook");

        let source = _dir.path().join("hooked-codex");
        fs::write(&source, "fresh binary").expect("source binary");
        vm.install(InstallRequest::Dev {
            label: "hooked".into(),
            source: DevInstallSource::Binary(source),
            link: false,
        })
        .expect("install with hook");

        assert!(
            sentinel.exists(),
            "PostInstall hook must observe the published, complete source"
        );
        assert!(dev.complete_marker().exists());
        assert!(!dev.installing_marker().exists());
    }

    #[test]
    fn waited_dev_install_rejects_a_different_source() {
        let (vm, dir) = setup_test_vm(Product::Codex);
        let version = "dev:shared";
        let first_source = dir.path().join("first-codex");
        let second_source = dir.path().join("second-codex");
        fs::write(&first_source, "first").expect("first source");
        fs::write(&second_source, "second").expect("second source");

        let owner_lock = vm.acquire_install_lock(version).expect("owner lock");
        let dev = vm.dev_source_paths(version);
        fs::create_dir_all(dev.destination.parent().expect("destination parent")).expect("mkdir");
        fs::write(&dev.destination, "first").expect("installed binary");
        let metadata = DevInstallMetadata::collect(first_source, DevInstallMode::Copy);
        fs::write(
            vm.product_dirs.dev_meta_path(version),
            serde_json::to_string_pretty(&metadata).expect("serialize metadata"),
        )
        .expect("write metadata");
        fs::write(dev.complete_marker(), "").expect("complete marker");

        let second_vm = VersionManager {
            dirs: vm.dirs.clone(),
            product_dirs: vm.product_dirs.clone(),
            config: vm.config.clone(),
        };
        let (sender, receiver) = mpsc::channel();
        let contender = thread::spawn(move || {
            let result = second_vm.install(InstallRequest::Dev {
                label: "shared".into(),
                source: DevInstallSource::Binary(second_source),
                link: false,
            });
            sender.send(result).expect("send result");
        });

        assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
        drop(owner_lock);
        let error = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("contender result")
            .expect_err("different source must not be reused");
        assert!(error.to_string().contains("requested"));
        contender.join().expect("contender thread");
    }

    /// The other half of that comparison, and the one that has to keep working
    /// after the source is bound and resolved: the SAME command re-run while
    /// another process was installing it must be *recognized*, not refused. So
    /// whatever `install_dev` records as the source has to be what a repeated
    /// invocation computes again — which is why the metadata keeps the path the
    /// user asked for rather than the one it resolved to.
    #[test]
    fn waited_dev_install_reuses_an_identical_source() {
        let (vm, dir) = setup_test_vm(Product::Codex);
        let version = "dev:sameshared";
        let source = dir.path().join("codex-build");
        fs::write(&source, "first").expect("source");

        let owner_lock = vm.acquire_install_lock(version).expect("owner lock");
        let dev = vm.dev_source_paths(version);
        fs::create_dir_all(dev.destination.parent().expect("destination parent")).expect("mkdir");
        fs::write(&dev.destination, "first").expect("installed binary");
        let metadata = DevInstallMetadata::collect(source.clone(), DevInstallMode::Copy);
        fs::write(
            vm.product_dirs.dev_meta_path(version),
            serde_json::to_string_pretty(&metadata).expect("serialize metadata"),
        )
        .expect("write metadata");
        fs::write(dev.complete_marker(), "").expect("complete marker");

        let second_vm = VersionManager {
            dirs: vm.dirs.clone(),
            product_dirs: vm.product_dirs.clone(),
            config: vm.config.clone(),
        };
        let (sender, receiver) = mpsc::channel();
        let contender_source = source.clone();
        let contender = thread::spawn(move || {
            let result = second_vm.install(InstallRequest::Dev {
                label: "sameshared".into(),
                source: DevInstallSource::Binary(contender_source),
                link: false,
            });
            sender.send(result).expect("send result");
        });

        assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
        drop(owner_lock);
        let installed = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("contender result")
            .expect("the same source must be reused, not refused");
        contender.join().expect("contender thread");

        assert_eq!(installed, version);
        assert_eq!(
            fs::read(&dev.destination).expect("binary"),
            b"first",
            "reuse must not rewrite the install another process published"
        );
    }

    #[test]
    fn use_version_switches_symlink() {
        let (vm, _dir) = setup_test_vm(Product::Claude);
        create_claude_version(&vm, "2.1.71");

        vm.use_version("2.1.71").expect("use version");

        assert_eq!(
            vm.current_version().expect("current"),
            Some("2.1.71".into())
        );
    }

    #[test]
    fn use_latest_switches_to_newest_installed_release() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        create_codex_release(&vm, "rust-v0.118.0");
        create_codex_release(&vm, "rust-v0.120.0");
        fs::create_dir_all(vm.product_dirs.version_dir("dev:resume-fix")).expect("mkdir");

        vm.use_version("latest").expect("use latest");

        assert_eq!(
            vm.current_version().expect("current"),
            Some("rust-v0.120.0".into())
        );
    }

    #[test]
    fn use_latest_ignores_archived_release_versions() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        create_codex_release(&vm, "rust-v0.118.0");
        fs::create_dir_all(vm.product_dirs.version_dir("rust-v0.120.0")).expect("mkdir");

        vm.use_version("latest").expect("use latest");

        assert_eq!(
            vm.current_version().expect("current"),
            Some("rust-v0.118.0".into())
        );
    }

    #[test]
    fn use_latest_rejects_when_no_installed_releases_exist() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        fs::create_dir_all(vm.product_dirs.version_dir("dev:resume-fix")).expect("mkdir");

        let error = vm
            .use_version("latest")
            .expect_err("missing latest release");

        assert!(error
            .to_string()
            .contains("No installed release versions found for Codex"));
    }

    #[test]
    fn prune_plan_selects_old_inactive_releases_only() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        create_codex_release(&vm, "rust-v0.118.0");
        create_codex_release(&vm, "rust-v0.120.0");
        create_codex_dev(&vm, "dev:resume-fix");
        vm.use_version("rust-v0.120.0").expect("use active");
        age_version_dir(&vm, "rust-v0.118.0", 31);
        age_version_dir(&vm, "rust-v0.120.0", 31);
        age_version_dir(&vm, "dev:resume-fix", 31);

        let planned = vm
            .plan_inactive_installs_older_than(30)
            .expect("plan prune");

        assert_eq!(planned, vec!["rust-v0.118.0".to_string()]);
    }

    #[test]
    fn prune_plan_does_not_touch_disk() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        create_codex_release(&vm, "rust-v0.118.0");
        create_codex_release(&vm, "rust-v0.119.0");
        create_codex_release(&vm, "rust-v0.120.0");
        vm.use_version("rust-v0.120.0").expect("use active");
        age_version_dir(&vm, "rust-v0.118.0", 31);
        age_version_dir(&vm, "rust-v0.119.0", 31);

        let planned = vm
            .plan_inactive_installs_older_than(30)
            .expect("plan prune");

        assert_eq!(planned.len(), 2);
        for version in ["rust-v0.118.0", "rust-v0.119.0", "rust-v0.120.0"] {
            assert!(
                vm.product_dirs
                    .version_dir(version)
                    .join("release")
                    .join("bin")
                    .join("codex")
                    .exists(),
                "planning must not remove {version}"
            );
        }
    }

    #[test]
    fn prune_plan_is_empty_without_active_version() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        create_codex_release(&vm, "rust-v0.118.0");
        age_version_dir(&vm, "rust-v0.118.0", 31);

        let planned = vm
            .plan_inactive_installs_older_than(30)
            .expect("plan prune");

        assert!(planned.is_empty());
        assert!(vm.product_dirs.version_dir("rust-v0.118.0").exists());
    }

    #[test]
    fn listing_with_sources_agrees_with_listing_and_with_version_sources() {
        // `list_installed` is now a projection of this, and retention planning
        // consumes the sources instead of re-deriving them, so the two must not
        // be able to disagree.
        let (vm, _dir) = setup_test_vm(Product::Codex);
        create_codex_release(&vm, "rust-v0.120.0");
        create_codex_release(&vm, "rust-v0.118.0");

        let with_sources = vm.list_installed_with_sources().expect("list with sources");
        let names: Vec<String> = with_sources
            .iter()
            .map(|(version, _)| version.clone())
            .collect();

        assert_eq!(names, vm.list_installed().expect("list"));
        assert_eq!(names, vec!["rust-v0.118.0", "rust-v0.120.0"]);
        for (version, sources) in &with_sources {
            assert_eq!(sources, &vm.version_sources(version));
        }
    }

    #[test]
    fn prune_plan_skips_versions_inside_the_retention_window() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        create_codex_release(&vm, "rust-v0.118.0");
        create_codex_release(&vm, "rust-v0.120.0");
        vm.use_version("rust-v0.120.0").expect("use active");
        age_version_dir(&vm, "rust-v0.118.0", 5);

        let planned = vm
            .plan_inactive_installs_older_than(30)
            .expect("plan prune");

        assert!(planned.is_empty());
    }

    #[test]
    fn measure_versions_reports_sizes_before_removal() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        create_codex_release(&vm, "rust-v0.118.0");

        let measured = vm.measure_versions(&["rust-v0.118.0".to_string()]);

        assert_eq!(measured.len(), 1);
        assert_eq!(measured[0].version, "rust-v0.118.0");
        assert!(measured[0].bytes > 0);
        assert!(vm.product_dirs.version_dir("rust-v0.118.0").exists());
    }

    #[test]
    fn remove_versions_deletes_only_the_named_versions() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        create_codex_release(&vm, "rust-v0.118.0");
        create_codex_release(&vm, "rust-v0.119.0");
        create_codex_release(&vm, "rust-v0.120.0");
        vm.use_version("rust-v0.120.0").expect("use active");

        let (freed, count) = vm
            .remove_versions(&["rust-v0.118.0".to_string()])
            .expect("remove");

        assert_eq!(count, 1);
        assert!(freed > 0);
        assert!(!vm.product_dirs.version_dir("rust-v0.118.0").exists());
        assert!(vm.product_dirs.version_dir("rust-v0.119.0").exists());
        assert!(vm.product_dirs.version_dir("rust-v0.120.0").exists());
    }

    #[test]
    fn use_version_rejects_missing_versions() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        let error = vm.use_version("0.118.0").expect_err("missing version");
        assert!(error.to_string().contains("not installed"));
    }

    #[test]
    fn install_dev_copy_creates_dev_version() {
        let (vm, dir) = setup_test_vm(Product::Codex);
        let binary = dir.path().join("codex-dev");
        fs::write(&binary, "fake-binary").expect("write binary");

        let installed = vm
            .install(InstallRequest::Dev {
                label: "resume-fix".into(),
                source: DevInstallSource::Binary(binary.clone()),
                link: false,
            })
            .expect("install dev");

        assert_eq!(installed, "dev:resume-fix");
        assert!(vm.product_dirs.dev_bin("dev:resume-fix").exists());
        let metadata = vm
            .dev_install_metadata("dev:resume-fix")
            .expect("metadata")
            .expect("present");
        assert_eq!(metadata.mode.label(), "copy");
        assert_eq!(metadata.source, binary);
    }

    #[test]
    fn install_dev_link_creates_symlink() {
        let (vm, dir) = setup_test_vm(Product::Codex);
        let bundle = dir.path().join("target").join("release");
        fs::create_dir_all(&bundle).expect("mkdir");
        fs::write(bundle.join("codex"), "fake-binary").expect("write binary");

        vm.install(InstallRequest::Dev {
            label: "linked".into(),
            source: DevInstallSource::Bundle(bundle.clone()),
            link: true,
        })
        .expect("install dev");

        assert!(vm.product_dirs.dev_bin("dev:linked").is_symlink());
    }

    #[test]
    fn install_rejects_versions_with_path_components() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        let error = vm
            .install(InstallRequest::Standard {
                use_npm: false,
                version: "../outside".into(),
            })
            .expect_err("path-like versions are rejected");

        assert!(error.to_string().contains("path separators"));
    }

    #[test]
    fn use_version_rejects_versions_with_path_components() {
        let (vm, _dir) = setup_test_vm(Product::Claude);
        let error = vm
            .use_version("../outside")
            .expect_err("path-like versions are rejected");

        assert!(error.to_string().contains("path separators"));
    }

    #[test]
    fn dev_install_rejects_traversal_labels() {
        let (vm, dir) = setup_test_vm(Product::Codex);
        let binary = dir.path().join("target").join("debug").join("codex");
        fs::create_dir_all(binary.parent().expect("parent")).expect("mkdir");
        fs::write(&binary, "fake-binary").expect("write binary");

        let error = vm
            .install(InstallRequest::Dev {
                label: "../linked".into(),
                source: DevInstallSource::Binary(binary),
                link: false,
            })
            .expect_err("path-like dev labels are rejected");

        assert!(error.to_string().contains("path separators"));
    }
}

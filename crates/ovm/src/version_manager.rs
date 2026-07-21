use crate::config::{
    install_source_is_complete, OvmConfig, OvmDirs, ProductDirs, VersionSource, COMPLETE_MARKER,
    INSTALLING_MARKER,
};
use crate::dev_metadata::{DevInstallMetadata, DevInstallMode};
use crate::error::{OvmError, Result};
use crate::hooks::{self, Hook};
use crate::product::Product;
use crate::sources::{codex, gcs, npm, pi, registry};
use crate::symlink;
use console::style;
use fs4::{FileExt, TryLockError};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
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
}

pub enum DevInstallSource {
    Binary(PathBuf),
    Bundle(PathBuf),
}

impl DevInstallSource {
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
        let product_dirs = dirs.product_dirs(product);
        Ok(Self {
            dirs,
            product_dirs,
            config,
        })
    }

    pub fn product(&self) -> Product {
        self.product_dirs.product
    }

    pub fn list_installed(&self) -> Result<Vec<String>> {
        let mut versions = Vec::new();

        if !self.product_dirs.versions.exists() {
            return Ok(versions);
        }

        for entry in fs::read_dir(&self.product_dirs.versions)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let file_name = entry.file_name();
                if let Some(name) = file_name.to_str() {
                    versions.push(name.to_string());
                }
            }
        }

        self.product().sort_versions(&mut versions);
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
        self.standard_source_paths(version, false).is_complete()
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
            Product::Codex | Product::Pi => {
                self.standard_source_paths(version, false).is_complete()
            }
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
            return Err(OvmError::Message(format!(
                "{} {version} is not installed. Run: {}",
                self.product().display_name(),
                self.product().install_example(&version)
            )));
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
        }
    }

    pub fn uninstall(&self, version: &str) -> Result<()> {
        let version = self.product().normalize_version(version);
        validate_storage_version_component(self.product(), &version)?;

        if !self.version_exists(&version) {
            return Err(OvmError::Message(format!(
                "{} {version} is not installed. Run: {}",
                self.product().display_name(),
                self.product().install_example(&version)
            )));
        }

        if let Some(current) = self.current_version()? {
            if current == version {
                return Err(OvmError::Message(format!(
                    "Cannot uninstall active {} version {version}. Switch first: {}",
                    self.product().canonical_name(),
                    self.product().use_example("other-version")
                )));
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
            return Err(OvmError::Message(format!(
                "{} {version} is not installed. Run: {}",
                self.product().display_name(),
                self.product().install_example(&version)
            )));
        }

        if let Some(current) = self.current_version()? {
            if current == version {
                return Err(OvmError::Message(format!(
                    "Cannot archive active {} version {version}. Switch first: {}",
                    self.product().canonical_name(),
                    self.product().use_example("other-version")
                )));
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

    pub fn prune_inactive_installs_older_than(&self, days: u64) -> Result<(u64, usize)> {
        let Some(current) = self.current_version()? else {
            return Ok((0, 0));
        };
        let cutoff = Duration::from_secs(days.saturating_mul(24 * 60 * 60));
        let now = SystemTime::now();
        let mut total_freed = 0;
        let mut count = 0;

        for version in self.list_installed()? {
            if version == current || version.starts_with("dev:") {
                continue;
            }
            let sources = self.version_sources(&version);
            if sources.contains(&VersionSource::Archived) || sources.contains(&VersionSource::Dev) {
                continue;
            }

            let version_dir = self.product_dirs.version_dir(&version);
            if !version_dir_is_older_than(&version_dir, cutoff, now) {
                continue;
            }

            let freed = dir_size(&version_dir)?;
            self.uninstall(&version)?;
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

        if source.is_complete() {
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
            }
            Ok(version.clone())
        });
        drop(install_lock);
        result
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
            _ => Err(OvmError::Message(format!(
                "{} does not support npm latest resolution.",
                self.product().display_name()
            ))),
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
        let result = self.run_install_transaction(&version, &install_paths, || {
            let destination = &install_paths.destination;
            let destination_parent = destination.parent().ok_or_else(|| {
                OvmError::Config(format!("No parent directory for {}", destination.display()))
            })?;
            fs::create_dir_all(destination_parent)?;

            if link {
                symlink::switch_symlink(destination, &source_binary)?;
            } else {
                fs::copy(&source_binary, destination)?;
                crate::util::make_executable(destination)?;
            }

            let metadata = DevInstallMetadata::collect(
                source_binary,
                if link {
                    DevInstallMode::Link
                } else {
                    DevInstallMode::Copy
                },
            );
            fs::write(
                self.product_dirs.dev_meta_path(&version),
                serde_json::to_string_pretty(&metadata)?,
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
        if self.product() == Product::Claude && use_npm {
            InstallSourcePaths {
                root: version_dir.join("npm"),
                destination: self.product_dirs.npm_bin(version),
                legacy_metadata: None,
            }
        } else if self.product() == Product::Claude {
            let root = version_dir.join("native");
            InstallSourcePaths {
                legacy_metadata: Some(root.join("manifest.json")),
                root,
                destination: self.product_dirs.native_bin(version),
            }
        } else if self.product() == Product::Pi {
            let root = version_dir.join("release");
            InstallSourcePaths {
                legacy_metadata: Some(root.join("meta.json")),
                root,
                destination: self.product_dirs.pi_bundle_bin(version),
            }
        } else {
            let root = version_dir.join("release");
            InstallSourcePaths {
                legacy_metadata: Some(root.join("meta.json")),
                root,
                destination: self.product_dirs.release_bin(version),
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
        self.ensure_dirs()?;
        self.prepare_install_source(source)?;

        hooks::run_hook(&self.dirs.hooks, Hook::PreInstall, version);
        match install() {
            Ok(value) => {
                // Publish the source BEFORE the hook: create `.complete` and
                // remove `.installing` so a PostInstall hook that runs `ovm
                // which`/`use`/launch against the just-installed version sees
                // it as complete. run_hook cannot fail the transaction, so
                // ordering it last needs no rollback.
                fs::write(source.complete_marker(), b"")?;
                fs::remove_file(source.installing_marker())?;
                hooks::run_hook(&self.dirs.hooks, Hook::PostInstall, version);
                Ok(value)
            }
            Err(install_error) => {
                if let Err(cleanup_error) = self.quarantine_and_remove_source(source) {
                    return Err(OvmError::Message(format!(
                        "{install_error}; also failed to clean incomplete install at {}: {cleanup_error}",
                        source.root.display()
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
        fs::write(source.installing_marker(), b"")?;
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
        fs::write(
            self.product_dirs.release_meta_path(version),
            serde_json::to_string_pretty(&metadata)?,
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
        fs::write(
            self.product_dirs.release_meta_path(version),
            serde_json::to_string_pretty(&metadata)?,
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
            Product::Codex | Product::Pi => {
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

fn path_entry_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
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
        DevInstallMetadata, DevInstallMode, DevInstallSource, InstallRequest, VersionManager,
        COMPLETE_MARKER, INSTALLING_MARKER,
    };
    use crate::config::{OvmConfig, OvmDirs, VersionSource};
    use crate::product::Product;
    use crate::release_metadata::ReleaseInstallMetadata;
    use filetime::{set_file_mtime, FileTime};
    use std::fs;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    fn setup_test_vm(product: Product) -> (VersionManager, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir");
        let dirs = OvmDirs {
            base: dir.path().to_path_buf(),
            hooks: dir.path().join("hooks"),
            config_file: dir.path().join("config.json"),
            bin: dir.path().join("bin"),
            products: dir.path().join("products"),
        };
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
    fn prune_inactive_installs_removes_old_inactive_releases_only() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        create_codex_release(&vm, "rust-v0.118.0");
        create_codex_release(&vm, "rust-v0.120.0");
        create_codex_dev(&vm, "dev:resume-fix");
        vm.use_version("rust-v0.120.0").expect("use active");
        age_version_dir(&vm, "rust-v0.118.0", 31);
        age_version_dir(&vm, "rust-v0.120.0", 31);
        age_version_dir(&vm, "dev:resume-fix", 31);

        let (_freed, count) = vm.prune_inactive_installs_older_than(30).expect("prune");

        assert_eq!(count, 1);
        assert!(!vm.product_dirs.version_dir("rust-v0.118.0").exists());
        assert!(vm.product_dirs.version_dir("rust-v0.120.0").exists());
        assert!(vm.product_dirs.version_dir("dev:resume-fix").exists());
    }

    #[test]
    fn prune_inactive_installs_is_noop_without_active_version() {
        let (vm, _dir) = setup_test_vm(Product::Codex);
        create_codex_release(&vm, "rust-v0.118.0");
        age_version_dir(&vm, "rust-v0.118.0", 31);

        let (_freed, count) = vm.prune_inactive_installs_older_than(30).expect("prune");

        assert_eq!(count, 0);
        assert!(vm.product_dirs.version_dir("rust-v0.118.0").exists());
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

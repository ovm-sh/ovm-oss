use crate::error::{OvmError, Result};
use crate::product::Product;
use fs4::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OvmConfig {
    #[serde(default = "default_true")]
    pub keep_tarballs: bool,

    #[serde(default = "default_download_delay")]
    pub download_delay: u64,

    #[serde(default = "default_true")]
    pub check_for_updates: bool,

    #[serde(default = "default_update_interval")]
    pub update_check_interval: u64,

    #[serde(default)]
    pub yolo: YoloConfig,

    #[serde(default)]
    pub auto_update: AutoUpdateConfig,

    #[serde(default)]
    pub cleanup: CleanupConfig,

    #[serde(default, rename = "self")]
    pub self_: SelfConfig,

    #[serde(default)]
    pub advanced: AdvancedConfig,

    /// Preserve settings written by a newer OVM when an older binary updates
    /// a field it understands.
    #[serde(default, flatten)]
    pub(crate) extra: serde_json::Map<String, serde_json::Value>,

    /// The document [`load`](Self::load) read, kept verbatim so [`save`](Self::save)
    /// can reconstruct the state this process started from for a field-level
    /// three-way merge when another process saved first.
    ///
    /// Held as text rather than as the merge baseline itself because the
    /// baseline is only ever needed by `save`: every read-only load — and a
    /// launch does several — would otherwise pay a full re-serialization of the
    /// config it just parsed. `None` means "no config file on disk".
    #[serde(skip)]
    pub(crate) baseline_source: Option<String>,

    /// JSON paths explicitly assigned by a command. This distinguishes an
    /// intentional write of a default value from an untouched synthesized
    /// default when two processes loaded a missing config concurrently.
    #[serde(skip)]
    pub(crate) explicit_changes: BTreeSet<String>,
}

/// Power-user toggles that are off by default. Kept separate from the top-level
/// config so opt-in features don't clutter the common settings surface.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdvancedConfig {
    /// Legacy gate from when OVM was hidden from the `ovm select` product
    /// picker by default. OVM is now always listed; this flag survives only
    /// as a fallback default for `self.manageVersions` (see
    /// [`OvmConfig::self_versions_manageable`]).
    #[serde(default)]
    pub self_in_picker: bool,

    #[serde(default, flatten)]
    pub(crate) extra: serde_json::Map<String, serde_json::Value>,
}

impl OvmConfig {
    /// Whether the OVM entry in the picker offers version swapping.
    ///
    /// OVM itself is always listed as a product; this only gates the version
    /// rows inside its menu. An explicit `self.manageVersions` wins; with no
    /// explicit setting it defaults on for the alpha self-update channel (and
    /// the legacy `advanced.selfInPicker` flag), off for a default stable
    /// user — who still sees the menu and its toggle, just not the swap list.
    pub fn self_versions_manageable(&self) -> bool {
        self.self_
            .manage_versions
            .unwrap_or(self.self_.channel == SelfChannel::Alpha || self.advanced.self_in_picker)
    }

    /// What an unset `self.manageVersions` defaults to; the picker shows this
    /// as the toggle's effective value until the user sets it explicitly.
    pub fn self_manage_default(&self) -> bool {
        self.self_.channel == SelfChannel::Alpha || self.advanced.self_in_picker
    }

    pub fn set_self_channel(&mut self, channel: SelfChannel) {
        self.self_.channel = channel;
        self.mark_explicit("self.channel");
    }

    pub fn set_self_auto_update(&mut self, policy: AutoUpdatePolicy) {
        self.self_.auto_update = policy;
        self.mark_explicit("self.autoUpdate");
    }

    pub fn set_self_manage_versions(&mut self, value: Option<bool>) {
        self.self_.manage_versions = value;
        self.mark_explicit("self.manageVersions");
    }

    pub fn set_self_retention_days(&mut self, value: Option<u32>) {
        self.self_.retention_days = value;
        self.mark_explicit("self.retentionDays");
    }

    pub fn set_auto_update_default(&mut self, policy: AutoUpdatePolicy) {
        self.auto_update.set_default(policy);
        self.mark_explicit("autoUpdate.default");
    }

    pub fn set_auto_update_product(&mut self, product: Product, policy: AutoUpdatePolicy) {
        self.auto_update.set_product(product, policy);
        self.mark_explicit(match product {
            Product::Claude => "autoUpdate.claude",
            Product::Codex => "autoUpdate.codex",
            Product::Pi => "autoUpdate.pi",
        });
    }

    pub fn set_cleanup_retention(&mut self, retention: CleanupRetention) {
        self.cleanup.retention = retention;
        self.mark_explicit("cleanup.retention");
    }

    fn mark_explicit(&mut self, path: &str) {
        self.explicit_changes.insert(path.to_string());
    }
}

/// Persistent settings for OVM's own self-management (`ovm self`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelfConfig {
    /// Which release channel `ovm self update` follows when no `--channel`
    /// flag is passed. Defaults to `stable`.
    #[serde(default)]
    pub channel: SelfChannel,

    /// Whether launches keep OVM itself up to date. Unlike products, OVM's own
    /// updates default to `on`: a launch stages the newer release silently and
    /// activates it atomically at the start of the next invocation. `notify`
    /// prompts instead; `off` disables launch-time self-updates entirely.
    #[serde(default)]
    pub auto_update: AutoUpdatePolicy,

    /// Whether the picker's OVM entry offers version swapping. Unset means
    /// "inherit": on for the alpha channel (or legacy `advanced.selfInPicker`),
    /// off otherwise. See [`OvmConfig::self_versions_manageable`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manage_versions: Option<bool>,

    /// How long inactive self-managed OVM *release* versions are kept after a
    /// successful self-update, in days. Unset keeps them forever. The active
    /// and previous versions are always kept (dev snapshots have their own
    /// keep-current-and-previous pruning).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u32>,

    #[serde(default, flatten)]
    pub(crate) extra: serde_json::Map<String, serde_json::Value>,
}

/// Opt-in release channel for `ovm self update`.
///
/// `stable` tracks GitHub's latest non-prerelease; `alpha` tracks the
/// highest-semver release including prereleases (e.g. `v0.2.0-alpha.3`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SelfChannel {
    #[default]
    Stable,
    Alpha,
}

impl SelfChannel {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "stable" => Some(Self::Stable),
            "alpha" => Some(Self::Alpha),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Alpha => "alpha",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct YoloConfig {
    #[serde(default)]
    pub claude: bool,

    #[serde(default)]
    pub codex: bool,

    #[serde(default, flatten)]
    pub(crate) extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AutoUpdatePolicy {
    Off,
    #[default]
    On,
    /// Announce a newer version on launch instead of updating silently: an
    /// interactive terminal gets a one-keypress install/snooze prompt, a
    /// non-interactive one gets a single deduplicated notice.
    Notify,
}

impl AutoUpdatePolicy {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "on" => Some(Self::On),
            "notify" => Some(Self::Notify),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::Notify => "notify",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoUpdateConfig {
    #[serde(default)]
    pub default: AutoUpdatePolicy,

    #[serde(default)]
    pub claude: Option<AutoUpdatePolicy>,

    #[serde(default)]
    pub codex: Option<AutoUpdatePolicy>,

    #[serde(default)]
    pub pi: Option<AutoUpdatePolicy>,

    #[serde(default, flatten)]
    pub(crate) extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupConfig {
    #[serde(default)]
    pub retention: CleanupRetention,

    #[serde(default, flatten)]
    pub(crate) extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CleanupRetention {
    #[default]
    #[serde(rename = "30")]
    Days30,
    #[serde(rename = "60")]
    Days60,
    #[serde(rename = "never")]
    Never,
}

impl CleanupRetention {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "30" | "30d" | "30days" | "30-days" => Some(Self::Days30),
            "60" | "60d" | "60days" | "60-days" => Some(Self::Days60),
            "never" | "off" => Some(Self::Never),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Days30 => "30 days",
            Self::Days60 => "60 days",
            Self::Never => "never",
        }
    }

    pub fn days(self) -> Option<u64> {
        match self {
            Self::Days30 => Some(30),
            Self::Days60 => Some(60),
            Self::Never => None,
        }
    }
}

impl AutoUpdateConfig {
    pub fn policy_for(&self, product: Product) -> AutoUpdatePolicy {
        match product {
            Product::Claude => self.claude,
            Product::Codex => self.codex,
            Product::Pi => self.pi,
        }
        .unwrap_or(self.default)
    }

    pub fn set_default(&mut self, policy: AutoUpdatePolicy) {
        self.default = policy;
    }

    pub fn set_product(&mut self, product: Product, policy: AutoUpdatePolicy) {
        match product {
            Product::Claude => self.claude = Some(policy),
            Product::Codex => self.codex = Some(policy),
            Product::Pi => self.pi = Some(policy),
        }
    }
}

impl YoloConfig {
    /// Whether a product should launch in yolo (dangerous/skip-permissions) mode by default.
    /// Pi has no permission system — it's always unrestricted — so the concept doesn't apply.
    pub fn is_default(&self, product: Product) -> bool {
        match product {
            Product::Claude => self.claude,
            Product::Codex => self.codex,
            Product::Pi => false,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_download_delay() -> u64 {
    100
}

fn default_update_interval() -> u64 {
    24
}

impl Default for OvmConfig {
    fn default() -> Self {
        Self {
            keep_tarballs: true,
            download_delay: 100,
            check_for_updates: true,
            update_check_interval: 24,
            yolo: YoloConfig::default(),
            auto_update: AutoUpdateConfig::default(),
            cleanup: CleanupConfig::default(),
            self_: SelfConfig::default(),
            advanced: AdvancedConfig::default(),
            extra: serde_json::Map::new(),
            baseline_source: None,
            explicit_changes: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OvmDirs {
    pub base: PathBuf,
    pub hooks: PathBuf,
    pub config_file: PathBuf,
    pub bin: PathBuf,
    pub(crate) products: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ProductDirs {
    pub product: Product,
    pub state_root: PathBuf,
    pub versions: PathBuf,
    pub current: PathBuf,
    pub active_bin: PathBuf,
    /// Records the version the user explicitly switched to. Its presence means
    /// the active version is a deliberate pin, not latest-tracking, so a plain
    /// launch under auto-update `on` must not silently jump it to the newest
    /// release. Follow-latest actions (`ovm <product> latest`, auto-update)
    /// remove it; absence means "track latest".
    pub pin: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionSource {
    Native,
    Npm,
    Release,
    Dev,
    Archived,
}

pub(crate) const INSTALLING_MARKER: &str = ".installing";
pub(crate) const COMPLETE_MARKER: &str = ".complete";

/// Whether a source subtree is safe to publish to readers. New installs carry
/// `.complete`; pre-marker installs are accepted only when their historical
/// binary and metadata layout is intact. `.installing` always wins because a
/// crashed writer may already have exposed its binary.
pub(crate) fn install_source_is_complete(
    root: &Path,
    binary: &Path,
    legacy_metadata: Option<&Path>,
) -> bool {
    if root.join(INSTALLING_MARKER).exists() || !binary.exists() {
        return false;
    }
    if root.join(COMPLETE_MARKER).exists() {
        return true;
    }
    legacy_metadata.is_none_or(Path::exists)
}

impl VersionSource {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Npm => "npm",
            Self::Release => "release",
            Self::Dev => "dev",
            Self::Archived => "archived",
        }
    }
}

impl OvmDirs {
    pub fn new() -> Result<Self> {
        let home = dirs::home_dir()
            .ok_or_else(|| OvmError::Config("Cannot determine home directory".into()))?;
        Ok(Self::at(home.join(".ovm")))
    }

    pub fn at(base: PathBuf) -> Self {
        Self {
            hooks: base.join("hooks"),
            config_file: base.join("config.json"),
            bin: base.join("bin"),
            products: base.join("products"),
            base,
        }
    }

    pub fn ensure_base_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.base)?;
        std::fs::create_dir_all(&self.bin)?;
        std::fs::create_dir_all(&self.hooks)?;
        Ok(())
    }

    pub fn product_dirs(&self, product: Product) -> ProductDirs {
        let state_root = self.products.join(product.canonical_name());

        ProductDirs {
            product,
            versions: state_root.join("versions"),
            current: state_root.join("current"),
            active_bin: self.bin.join(product.binary_name()),
            pin: state_root.join("pinned"),
            state_root,
        }
    }
}

impl ProductDirs {
    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.state_root)?;
        std::fs::create_dir_all(&self.versions)?;
        if let Some(parent) = self.active_bin.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(())
    }

    pub fn version_dir(&self, version: &str) -> PathBuf {
        self.versions.join(version)
    }

    pub fn resolved_binary(&self, version: &str) -> PathBuf {
        let bin_name = self.product.binary_name();
        match self.product {
            Product::Claude => {
                let version_dir = self.version_dir(version);
                let native_root = version_dir.join("native");
                let native = native_root.join(bin_name);
                if install_source_is_complete(
                    &native_root,
                    &native,
                    Some(&native_root.join("manifest.json")),
                ) {
                    return native;
                }

                let npm_bin = self.npm_bin(version);
                if install_source_is_complete(&version_dir.join("npm"), &npm_bin, None) {
                    return npm_bin;
                }

                native
            }
            Product::Codex => {
                let version_dir = self.version_dir(version);
                let release_root = version_dir.join("release");
                let release = release_root.join("bin").join(bin_name);
                if install_source_is_complete(
                    &release_root,
                    &release,
                    Some(&release_root.join("meta.json")),
                ) {
                    return release;
                }

                let dev_root = version_dir.join("dev");
                let dev = dev_root.join("bin").join(bin_name);
                if install_source_is_complete(&dev_root, &dev, Some(&dev_root.join("meta.json"))) {
                    return dev;
                }

                release
            }
            Product::Pi => {
                // Pi extracts to release/bundle/pi/pi (full bundle with package.json, etc.)
                self.pi_bundle_bin(version)
            }
        }
    }

    pub fn native_bin(&self, version: &str) -> PathBuf {
        self.version_dir(version)
            .join("native")
            .join(self.product.binary_name())
    }

    pub fn npm_bin(&self, version: &str) -> PathBuf {
        let version_dir = self.version_dir(version);
        let bin_name = self.product.binary_name();
        version_dir
            .join("npm")
            .join("installed")
            .join("node_modules")
            .join(".bin")
            .join(bin_name)
    }

    pub fn release_bin(&self, version: &str) -> PathBuf {
        self.version_dir(version)
            .join("release")
            .join("bin")
            .join(self.product.binary_name())
    }

    /// Pi ships as a full bundle (binary + package.json + assets).
    /// This is the directory where the tarball is extracted.
    pub fn release_bundle_dir(&self, version: &str) -> PathBuf {
        self.version_dir(version).join("release").join("bundle")
    }

    /// Path to the pi binary inside the extracted bundle.
    pub fn pi_bundle_bin(&self, version: &str) -> PathBuf {
        self.release_bundle_dir(version).join("pi").join("pi")
    }

    pub fn dev_bin(&self, version: &str) -> PathBuf {
        self.version_dir(version)
            .join("dev")
            .join("bin")
            .join(self.product.binary_name())
    }

    pub fn dev_meta_path(&self, version: &str) -> PathBuf {
        self.version_dir(version).join("dev").join("meta.json")
    }

    pub fn release_meta_path(&self, version: &str) -> PathBuf {
        self.version_dir(version).join("release").join("meta.json")
    }

    /// Whether any of this version's source subtrees still carries an
    /// `.installing` marker — a crashed or killed install, not an archive.
    pub fn version_has_installing_marker(&self, version: &str) -> bool {
        let version_dir = self.version_dir(version);
        let roots: &[&str] = match self.product {
            Product::Claude => &["native", "npm"],
            Product::Codex => &["release", "dev"],
            Product::Pi => &["release"],
        };
        roots
            .iter()
            .any(|root| version_dir.join(root).join(INSTALLING_MARKER).exists())
    }

    pub fn version_sources(&self, version: &str) -> Vec<VersionSource> {
        let version_dir = self.version_dir(version);
        let bin_name = self.product.binary_name();
        let mut sources = Vec::new();

        match self.product {
            Product::Claude => {
                let native_root = version_dir.join("native");
                if install_source_is_complete(
                    &native_root,
                    &native_root.join(bin_name),
                    Some(&native_root.join("manifest.json")),
                ) {
                    sources.push(VersionSource::Native);
                }
                let npm_root = version_dir.join("npm");
                if install_source_is_complete(&npm_root, &self.npm_bin(version), None) {
                    sources.push(VersionSource::Npm);
                }
            }
            Product::Codex => {
                let release_root = version_dir.join("release");
                if install_source_is_complete(
                    &release_root,
                    &release_root.join("bin").join(bin_name),
                    Some(&release_root.join("meta.json")),
                ) {
                    sources.push(VersionSource::Release);
                }
                let dev_root = version_dir.join("dev");
                if install_source_is_complete(
                    &dev_root,
                    &dev_root.join("bin").join(bin_name),
                    Some(&dev_root.join("meta.json")),
                ) {
                    sources.push(VersionSource::Dev);
                }
            }
            Product::Pi => {
                let release_root = version_dir.join("release");
                if install_source_is_complete(
                    &release_root,
                    &self.pi_bundle_bin(version),
                    Some(&release_root.join("meta.json")),
                ) {
                    sources.push(VersionSource::Release);
                }
            }
        }

        // A crashed install (`.installing` still present, nothing complete) is
        // not an archive: reporting it as one surfaces phantom versions in
        // `ovm list`. It stays invisible until a retry completes or replaces it.
        if sources.is_empty()
            && version_dir.exists()
            && !self.version_has_installing_marker(version)
        {
            sources.push(VersionSource::Archived);
        }

        sources
    }
}

impl OvmConfig {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                let mut config: Self = serde_json::from_str(&contents)?;
                config.baseline_source = Some(contents);
                Ok(config)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// The merge baseline: this config exactly as it was loaded, before any
    /// command mutated it. Reconstructed from the text `load` kept — parsing
    /// the same document twice is deterministic, so this is the value `load`
    /// used to compute eagerly. `Null` means there was no config file, which
    /// [`save`](Self::save) reads as "compare against the built-in defaults".
    fn baseline(&self) -> Result<serde_json::Value> {
        match &self.baseline_source {
            Some(contents) => {
                let as_loaded: Self = serde_json::from_str(contents)?;
                Ok(serde_json::to_value(&as_loaded)?)
            }
            None => Ok(serde_json::Value::Null),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;

        // Serialize writers with a stable sidecar lock. Each writer uses its
        // own temporary file, so a crash or concurrent process can never
        // expose a partial JSON document or trample another writer's temp.
        let lock_path = path.with_extension(
            path.extension()
                .map(|extension| format!("{}.lock", extension.to_string_lossy()))
                .unwrap_or_else(|| "lock".to_string()),
        );
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)?;
        FileExt::lock(&lock)?;

        let desired = serde_json::to_value(self)?;
        let mut merged = match std::fs::read(path) {
            Ok(contents) => {
                let mut current: serde_json::Value = serde_json::from_slice(&contents)?;
                let baseline = self.baseline()?;
                if baseline.is_null() {
                    let missing_file_baseline = serde_json::to_value(Self::default())?;
                    merge_config_changes(&mut current, &missing_file_baseline, &desired);
                } else {
                    merge_config_changes(&mut current, &baseline, &desired);
                }
                current
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => desired.clone(),
            Err(error) => return Err(error.into()),
        };
        for path in &self.explicit_changes {
            let segments: Vec<&str> = path.split('.').collect();
            apply_explicit_config_change(&mut merged, &desired, &segments);
        }
        let payload = serde_json::to_vec_pretty(&merged)?;
        let mut pending = tempfile::NamedTempFile::new_in(parent)?;
        pending.write_all(&payload)?;
        pending.write_all(b"\n")?;
        pending.as_file().sync_all()?;
        pending.persist(path).map_err(|error| error.error)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }
}

fn apply_explicit_config_change(
    current: &mut serde_json::Value,
    desired: &serde_json::Value,
    path: &[&str],
) {
    let Some((key, remainder)) = path.split_first() else {
        *current = desired.clone();
        return;
    };
    let Some(desired_object) = desired.as_object() else {
        return;
    };
    if !current.is_object() {
        *current = serde_json::Value::Object(serde_json::Map::new());
    }
    let current_object = current.as_object_mut().expect("object initialized above");
    if remainder.is_empty() {
        match desired_object.get(*key) {
            Some(value) => {
                current_object.insert((*key).to_string(), value.clone());
            }
            None => {
                current_object.remove(*key);
            }
        }
        return;
    }

    match desired_object.get(*key) {
        Some(desired_child) => {
            let current_child = current_object
                .entry((*key).to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            apply_explicit_config_change(current_child, desired_child, remainder);
        }
        None => {
            current_object.remove(*key);
        }
    }
}

/// Apply only values changed from `baseline` to the latest on-disk document.
/// This prevents two load-mutate-save processes changing unrelated settings
/// from erasing each other's update while retaining unknown future fields.
fn merge_config_changes(
    current: &mut serde_json::Value,
    baseline: &serde_json::Value,
    desired: &serde_json::Value,
) {
    if baseline == desired {
        return;
    }

    match (current, baseline, desired) {
        (
            serde_json::Value::Object(current),
            serde_json::Value::Object(baseline),
            serde_json::Value::Object(desired),
        ) => {
            for (key, desired_value) in desired {
                let baseline_value = baseline.get(key).unwrap_or(&serde_json::Value::Null);
                if baseline_value == desired_value {
                    continue;
                }
                match current.get_mut(key) {
                    Some(current_value) => {
                        merge_config_changes(current_value, baseline_value, desired_value)
                    }
                    None => {
                        current.insert(key.clone(), desired_value.clone());
                    }
                }
            }
            for key in baseline.keys().filter(|key| !desired.contains_key(*key)) {
                current.remove(key);
            }
        }
        (current, _, desired) => *current = desired.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AutoUpdatePolicy, CleanupRetention, OvmConfig, OvmDirs, ProductDirs, SelfChannel,
        VersionSource,
    };
    use crate::product::Product;
    use std::path::Path;

    #[test]
    fn self_channel_defaults_to_stable() {
        let config = OvmConfig::default();
        assert_eq!(config.self_.channel, SelfChannel::Stable);
    }

    #[test]
    fn self_auto_update_defaults_to_on() {
        let config = OvmConfig::default();
        assert_eq!(config.self_.auto_update, AutoUpdatePolicy::On);
    }

    #[test]
    fn auto_update_policy_parses_notify() {
        assert_eq!(
            AutoUpdatePolicy::parse("notify"),
            Some(AutoUpdatePolicy::Notify)
        );
        assert_eq!(AutoUpdatePolicy::parse("on"), Some(AutoUpdatePolicy::On));
        assert_eq!(AutoUpdatePolicy::parse("off"), Some(AutoUpdatePolicy::Off));
        assert_eq!(AutoUpdatePolicy::parse("sometimes"), None);
        assert_eq!(AutoUpdatePolicy::Notify.label(), "notify");
    }

    #[test]
    fn self_auto_update_round_trips_through_config_json() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.json");
        let mut config = OvmConfig::default();
        config.self_.auto_update = AutoUpdatePolicy::Notify;
        config.save(&path).expect("save");

        let contents = std::fs::read_to_string(&path).expect("read");
        assert!(contents.contains("\"autoUpdate\""), "{contents}");
        assert!(contents.contains("\"notify\""), "{contents}");

        let reloaded = OvmConfig::load(&path).expect("load");
        assert_eq!(reloaded.self_.auto_update, AutoUpdatePolicy::Notify);
    }

    #[test]
    fn config_save_preserves_unknown_fields_for_newer_versions() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{
                "keepTarballs": true,
                "futureTopLevel": {"enabled": true},
                "self": {"channel": "stable", "futureSelfSetting": "kept"},
                "autoUpdate": {"default": "on", "futureProduct": "notify"}
            }"#,
        )
        .expect("seed newer config");

        let mut config = OvmConfig::load(&path).expect("load");
        config.keep_tarballs = false;
        config.save(&path).expect("save");

        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read saved config"))
                .expect("valid JSON");
        assert_eq!(saved["futureTopLevel"]["enabled"], true);
        assert_eq!(saved["self"]["futureSelfSetting"], "kept");
        assert_eq!(saved["autoUpdate"]["futureProduct"], "notify");
        assert_eq!(saved["keepTarballs"], false);
    }

    #[test]
    fn concurrent_config_saves_never_publish_torn_json() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.json");
        OvmConfig::default().save(&path).expect("initial save");

        let mut writers = Vec::new();
        for writer in 0..8_u64 {
            let path = path.clone();
            writers.push(std::thread::spawn(move || {
                for iteration in 0..25_u64 {
                    let config = OvmConfig {
                        download_delay: writer * 100 + iteration,
                        ..OvmConfig::default()
                    };
                    config.save(&path).expect("concurrent save");
                }
            }));
        }
        for writer in writers {
            writer.join().expect("writer did not panic");
        }

        OvmConfig::load(&path).expect("published config is complete JSON");
        let leftovers = std::fs::read_dir(tmp.path())
            .expect("read tempdir")
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains("tmp"))
            .count();
        assert_eq!(leftovers, 0, "temporary files must not leak");
    }

    #[test]
    fn concurrent_config_updates_preserve_unrelated_known_fields() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.json");
        OvmConfig::default().save(&path).expect("initial save");

        let mut first = OvmConfig::load(&path).expect("first snapshot");
        let mut second = OvmConfig::load(&path).expect("second snapshot");
        first.yolo.claude = true;
        second.cleanup.retention = CleanupRetention::Days60;

        first.save(&path).expect("first update");
        second.save(&path).expect("second update");

        let merged = OvmConfig::load(&path).expect("merged config");
        assert!(merged.yolo.claude);
        assert_eq!(merged.cleanup.retention, CleanupRetention::Days60);
    }

    #[test]
    fn first_config_updates_preserve_unrelated_known_fields() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.json");

        let mut first = OvmConfig::load(&path).expect("first missing snapshot");
        let mut second = OvmConfig::load(&path).expect("second missing snapshot");
        first.yolo.claude = true;
        second.cleanup.retention = CleanupRetention::Days60;

        first.save(&path).expect("first update");
        second.save(&path).expect("second update");

        let merged = OvmConfig::load(&path).expect("merged config");
        assert!(merged.yolo.claude);
        assert_eq!(merged.cleanup.retention, CleanupRetention::Days60);
    }

    #[test]
    fn first_config_explicit_default_wins_same_field_race() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.json");

        let mut first = OvmConfig::load(&path).expect("first missing snapshot");
        let mut second = OvmConfig::load(&path).expect("second missing snapshot");
        first.set_auto_update_default(AutoUpdatePolicy::Off);
        second.set_auto_update_default(AutoUpdatePolicy::On);

        first.save(&path).expect("first update");
        second.save(&path).expect("explicit default update");

        let merged = OvmConfig::load(&path).expect("merged config");
        assert_eq!(merged.auto_update.default, AutoUpdatePolicy::On);
    }

    #[test]
    fn config_without_self_auto_update_defaults_to_on() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.json");
        std::fs::write(&path, "{\"self\": {\"channel\": \"alpha\"}}").expect("write");
        let config = OvmConfig::load(&path).expect("load");
        assert_eq!(config.self_.auto_update, AutoUpdatePolicy::On);
        assert_eq!(config.self_.channel, SelfChannel::Alpha);
    }

    #[test]
    fn self_channel_parser_accepts_stable_and_alpha() {
        assert_eq!(SelfChannel::parse("stable"), Some(SelfChannel::Stable));
        assert_eq!(SelfChannel::parse("alpha"), Some(SelfChannel::Alpha));
        assert_eq!(SelfChannel::parse("beta"), None);
        assert_eq!(SelfChannel::parse("nightly"), None);
    }

    #[test]
    fn self_channel_round_trips_through_config_json() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.json");
        let mut config = OvmConfig::default();
        config.self_.channel = SelfChannel::Alpha;
        config.save(&path).expect("save");

        let contents = std::fs::read_to_string(&path).expect("read");
        assert!(contents.contains("\"self\""), "{contents}");
        assert!(contents.contains("\"alpha\""), "{contents}");

        let reloaded = OvmConfig::load(&path).expect("load");
        assert_eq!(reloaded.self_.channel, SelfChannel::Alpha);
    }

    #[test]
    fn self_versions_not_manageable_for_default_stable_user() {
        let config = OvmConfig::default();
        assert!(
            !config.self_versions_manageable(),
            "stable channel with no explicit setting must not offer version swapping"
        );
        assert!(!config.self_manage_default());
    }

    #[test]
    fn self_versions_manageable_via_alpha_channel() {
        let mut config = OvmConfig::default();
        config.self_.channel = SelfChannel::Alpha;
        assert!(
            config.self_versions_manageable(),
            "alpha channel should default version swapping on"
        );
        assert!(config.self_manage_default());
    }

    #[test]
    fn self_versions_manageable_via_legacy_advanced_flag() {
        let mut config = OvmConfig::default();
        config.advanced.self_in_picker = true;
        assert!(
            config.self_versions_manageable(),
            "legacy advanced.selfInPicker=true should still default swapping on"
        );
    }

    #[test]
    fn explicit_manage_versions_overrides_channel_defaults() {
        let mut config = OvmConfig::default();
        config.self_.channel = SelfChannel::Alpha;
        config.self_.manage_versions = Some(false);
        assert!(
            !config.self_versions_manageable(),
            "explicit off must beat the alpha-channel default"
        );

        let mut config = OvmConfig::default();
        config.self_.manage_versions = Some(true);
        assert!(
            config.self_versions_manageable(),
            "explicit on must work for a stable-channel user"
        );
    }

    #[test]
    fn self_manage_and_retention_round_trip_through_config_json() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.json");
        let mut config = OvmConfig::default();
        config.self_.manage_versions = Some(true);
        config.self_.retention_days = Some(30);
        config.save(&path).expect("save");

        let contents = std::fs::read_to_string(&path).expect("read");
        assert!(contents.contains("\"manageVersions\""), "{contents}");
        assert!(contents.contains("\"retentionDays\""), "{contents}");

        let reloaded = OvmConfig::load(&path).expect("load");
        assert_eq!(reloaded.self_.manage_versions, Some(true));
        assert_eq!(reloaded.self_.retention_days, Some(30));
    }

    #[test]
    fn unset_manage_and_retention_stay_out_of_config_json() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.json");
        OvmConfig::default().save(&path).expect("save");
        let contents = std::fs::read_to_string(&path).expect("read");
        assert!(!contents.contains("manageVersions"), "{contents}");
        assert!(!contents.contains("retentionDays"), "{contents}");
    }

    #[test]
    fn advanced_self_in_picker_round_trips_through_config_json() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.json");
        let mut config = OvmConfig::default();
        config.advanced.self_in_picker = true;
        config.save(&path).expect("save");

        let contents = std::fs::read_to_string(&path).expect("read");
        assert!(contents.contains("\"advanced\""), "{contents}");
        assert!(contents.contains("\"selfInPicker\""), "{contents}");

        let reloaded = OvmConfig::load(&path).expect("load");
        assert!(reloaded.advanced.self_in_picker);
    }

    #[test]
    fn config_without_advanced_section_defaults_self_in_picker_off() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.json");
        std::fs::write(&path, "{\"keepTarballs\": true}").expect("write");
        let config = OvmConfig::load(&path).expect("load");
        assert!(!config.advanced.self_in_picker);
    }

    #[test]
    fn config_without_self_section_defaults_to_stable() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.json");
        std::fs::write(&path, "{\"keepTarballs\": true}").expect("write");
        let config = OvmConfig::load(&path).expect("load");
        assert_eq!(config.self_.channel, SelfChannel::Stable);
    }

    #[test]
    fn test_default_config() {
        let config = OvmConfig::default();
        assert!(config.keep_tarballs);
        assert_eq!(config.download_delay, 100);
        assert!(config.check_for_updates);
        assert_eq!(config.update_check_interval, 24);
        assert_eq!(
            config.auto_update.policy_for(Product::Claude),
            AutoUpdatePolicy::On
        );
        assert_eq!(config.cleanup.retention, CleanupRetention::Days30);
    }

    #[test]
    fn test_config_load_missing_file() {
        let config = OvmConfig::load(Path::new("/nonexistent/config.json")).expect("defaults");
        assert!(config.keep_tarballs);
    }

    #[test]
    fn auto_update_product_policy_overrides_default() {
        let mut config = OvmConfig::default();
        config.auto_update.set_default(AutoUpdatePolicy::Off);
        config
            .auto_update
            .set_product(Product::Codex, AutoUpdatePolicy::On);

        assert_eq!(
            config.auto_update.policy_for(Product::Claude),
            AutoUpdatePolicy::Off
        );
        assert_eq!(
            config.auto_update.policy_for(Product::Codex),
            AutoUpdatePolicy::On
        );
    }

    #[test]
    fn cleanup_retention_parser_accepts_supported_values() {
        assert_eq!(
            CleanupRetention::parse("30"),
            Some(CleanupRetention::Days30)
        );
        assert_eq!(
            CleanupRetention::parse("60"),
            Some(CleanupRetention::Days60)
        );
        assert_eq!(
            CleanupRetention::parse("never"),
            Some(CleanupRetention::Never)
        );
        assert_eq!(CleanupRetention::parse("90"), None);
    }

    #[test]
    fn all_products_use_namespaced_state_roots() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dirs = OvmDirs::at(tmp.path().to_path_buf());
        for product in Product::ALL {
            let pd = dirs.product_dirs(product);
            assert_eq!(
                pd.state_root,
                dirs.base.join("products").join(product.canonical_name()),
                "{} should live under products/",
                product.canonical_name()
            );
            assert_eq!(pd.active_bin, dirs.bin.join(product.binary_name()));
        }
    }

    #[test]
    fn detects_codex_dev_source() {
        let dir = tempfile::tempdir().expect("tempdir");
        let product_dirs = ProductDirs {
            product: Product::Codex,
            state_root: dir.path().to_path_buf(),
            versions: dir.path().join("versions"),
            current: dir.path().join("current"),
            active_bin: dir.path().join("bin").join("codex"),
            pin: dir.path().join("pinned"),
        };

        std::fs::create_dir_all(product_dirs.dev_bin("dev:test").parent().expect("parent"))
            .expect("mkdir");
        std::fs::write(product_dirs.dev_bin("dev:test"), "binary").expect("write");
        std::fs::write(
            product_dirs.version_dir("dev:test").join("dev/.complete"),
            "",
        )
        .expect("write completion marker");

        assert_eq!(
            product_dirs.version_sources("dev:test"),
            vec![VersionSource::Dev]
        );
    }
}

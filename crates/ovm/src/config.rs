use crate::error::{OvmError, Result};
use crate::product::Product;
use serde::{Deserialize, Serialize};
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
}

/// Power-user toggles that are off by default. Kept separate from the top-level
/// config so opt-in features don't clutter the common settings surface.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdvancedConfig {
    /// Surface OVM itself as a switchable entry in the `ovm select` product
    /// picker. Off by default (OVM is not listed). `ovm self channel alpha`
    /// also surfaces it, so opting into the alpha self-update channel is the
    /// primary way to enable this; this flag is the explicit override for
    /// stable-channel users who still want it.
    #[serde(default)]
    pub self_in_picker: bool,
}

impl OvmConfig {
    /// Whether OVM itself should appear as a switchable entry in the
    /// `ovm select` product picker. Gated behind the alpha self-update channel
    /// (`self.channel == alpha`) OR the explicit `advanced.selfInPicker` flag.
    /// A default stable user with neither set never sees OVM in the picker.
    pub fn ovm_in_picker(&self) -> bool {
        self.self_.channel == SelfChannel::Alpha || self.advanced.self_in_picker
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
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupConfig {
    #[serde(default)]
    pub retention: CleanupRetention,
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

        if sources.is_empty() && version_dir.exists() {
            sources.push(VersionSource::Archived);
        }

        sources
    }
}

impl OvmConfig {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(contents) => Ok(serde_json::from_str(&contents)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
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
    fn ovm_in_picker_is_off_for_default_stable_user() {
        let config = OvmConfig::default();
        assert!(
            !config.ovm_in_picker(),
            "stable channel with no flag must hide OVM from the picker"
        );
    }

    #[test]
    fn ovm_in_picker_on_via_alpha_channel() {
        let mut config = OvmConfig::default();
        config.self_.channel = SelfChannel::Alpha;
        assert!(
            config.ovm_in_picker(),
            "alpha channel should surface OVM in the picker"
        );
    }

    #[test]
    fn ovm_in_picker_on_via_advanced_flag() {
        let mut config = OvmConfig::default();
        config.advanced.self_in_picker = true;
        assert!(
            config.ovm_in_picker(),
            "advanced.selfInPicker=true should surface OVM even on stable"
        );
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

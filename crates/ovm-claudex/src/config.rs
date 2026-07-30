//! claudex configuration: the model registry, proxy settings, and the
//! pin/auto-update policy. Stored as JSON at `~/.ovm/claudex/config.json`
//! (matching OVM's own `config.json` convention).

use crate::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudexConfig {
    pub proxy: ProxySettings,
    pub models: ModelRegistry,
    pub tuning: Tuning,
    /// Default policy: the managed proxy binary follows upstream releases.
    /// Set to false (or set `pin`) when a known-good pair must be held still.
    pub auto_update_proxy: bool,
    /// When set, freezes the (Claude Code, CLIProxyAPI) pair: launches pass
    /// `--ovm-version` for Claude and resolve the pinned proxy version instead
    /// of `current`. The escape hatch for "it stopped working".
    pub pin: Option<PinnedPair>,
}

impl Default for ClaudexConfig {
    fn default() -> Self {
        Self {
            proxy: ProxySettings::default(),
            models: ModelRegistry::default(),
            tuning: Tuning::default(),
            auto_update_proxy: true,
            pin: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxySettings {
    pub port: u16,
    /// Random local key generated at setup; shared only between the launcher
    /// (as `ANTHROPIC_AUTH_TOKEN`) and the proxy's `api-keys` list.
    pub api_key: String,
}

impl Default for ProxySettings {
    fn default() -> Self {
        Self {
            port: 8317,
            api_key: String::new(),
        }
    }
}

/// Which backend models fill Claude Code's built-in tier slots. The native
/// `/model` picker switches between these; raw IDs still work via `--model`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelRegistry {
    pub opus: String,
    pub sonnet: String,
    pub haiku: String,
    /// Model used when launching without an explicit `--model`.
    pub default: String,
    /// Model spawned subagents use.
    pub subagent: String,
    /// Extra selectable models, documented in the generated CLAUDE.md so
    /// they're discoverable; selected manually via `/model <id>`.
    pub extra: Vec<String>,
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self {
            opus: "gpt-5.6-sol".into(),
            sonnet: "gpt-5.6-terra".into(),
            haiku: "gpt-5.6-luna".into(),
            default: "gpt-5.6-sol".into(),
            subagent: "gpt-5.6-terra".into(),
            extra: Vec::new(),
        }
    }
}

/// Launch tuning from the origin recipe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tuning {
    pub always_enable_effort: bool,
    pub max_tool_use_concurrency: u32,
    pub enable_tool_search: bool,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            always_enable_effort: true,
            max_tool_use_concurrency: 3,
            enable_tool_search: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PinnedPair {
    /// Claude Code version, passed as `ovm cc --ovm-version <v>`.
    pub claude: String,
    /// Managed CLIProxyAPI version, resolved instead of the `current` symlink.
    pub proxy: String,
}

impl ClaudexConfig {
    /// Load from disk. `Ok(None)` when the file doesn't exist yet (not set up).
    pub fn load(path: &Path) -> Result<Option<Self>> {
        match std::fs::read_to_string(path) {
            Ok(contents) => Ok(Some(serde_json::from_str(&contents)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut contents = serde_json::to_string_pretty(self)?;
        contents.push('\n');
        write_private(path, &contents)
    }
}

/// Write a file containing credentials owner-readable only (0600).
///
/// Atomic and symlink-safe: contents land in a same-directory temp file
/// created 0600 from the first byte (no chmod-after-write exposure window),
/// then rename into place — a crash never leaves a partial file, and a
/// symlink at the destination is replaced rather than followed.
pub fn write_private(path: &Path, contents: &str) -> Result<()> {
    write_atomic(path, contents, Some(0o600))
}

/// Atomic write via same-directory temp file + rename. `mode` sets unix
/// permissions from creation when given.
pub fn write_atomic(path: &Path, contents: &str, mode: Option<u32>) -> Result<()> {
    use std::io::Write;

    let file_name = path
        .file_name()
        .ok_or_else(|| crate::ClaudexError::Message(format!("bad path: {}", path.display())))?;
    // Unique per process + per call (atomic counter) so concurrent writers —
    // including threads of the same process — can never collide on the temp
    // pathname.
    static WRITE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = WRITE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut tmp = path.to_path_buf();
    tmp.set_file_name(format!(
        ".{}.{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id(),
        seq
    ));

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;

    let mut file = options.open(&tmp)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    drop(file);

    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 32 random bytes from the OS, hex-encoded — the local proxy key.
pub fn generate_api_key() -> Result<String> {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_origin_recipe() {
        let config = ClaudexConfig::default();
        assert_eq!(config.models.opus, "gpt-5.6-sol");
        assert_eq!(config.models.sonnet, "gpt-5.6-terra");
        assert_eq!(config.models.haiku, "gpt-5.6-luna");
        assert_eq!(config.models.subagent, "gpt-5.6-terra");
        assert_eq!(config.proxy.port, 8317);
        assert!(config.tuning.always_enable_effort);
        assert_eq!(config.tuning.max_tool_use_concurrency, 3);
        assert!(!config.tuning.enable_tool_search);
        assert!(config.auto_update_proxy, "default policy is auto-update");
        assert!(config.pin.is_none());
    }

    #[test]
    fn round_trips_through_disk() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.json");

        let mut config = ClaudexConfig::default();
        config.proxy.api_key = "abc123".into();
        config.pin = Some(PinnedPair {
            claude: "2.1.207".into(),
            proxy: "7.2.55".into(),
        });
        config.save(&path).expect("save");

        let loaded = ClaudexConfig::load(&path).expect("load").expect("exists");
        assert_eq!(loaded, config);
    }

    #[test]
    #[cfg(unix)]
    fn saved_config_is_owner_readable_only() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.json");
        ClaudexConfig::default().save(&path).expect("save");

        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "config holds the proxy key");
    }

    #[test]
    fn load_returns_none_when_missing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let loaded = ClaudexConfig::load(&temp.path().join("nope.json")).expect("load");
        assert!(loaded.is_none());
    }

    #[test]
    fn unknown_and_missing_fields_do_not_break_load() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.json");
        // A future field plus a sparse config from an older version.
        std::fs::write(&path, r#"{"future_field": true, "proxy": {"port": 9999}}"#).expect("write");

        let loaded = ClaudexConfig::load(&path).expect("load").expect("exists");
        assert_eq!(loaded.proxy.port, 9999);
        assert_eq!(loaded.models.opus, "gpt-5.6-sol");
    }

    #[test]
    #[cfg(unix)]
    fn write_private_is_atomic_and_replaces_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("config.json");
        let decoy = temp.path().join("decoy.txt");
        std::fs::write(&decoy, "decoy").unwrap();
        // A hostile symlink at the destination must be replaced, not followed.
        std::os::unix::fs::symlink(&decoy, &target).unwrap();
        // A leftover temp file from a crashed run must not break the write.
        std::fs::write(temp.path().join(".config.json.tmp"), "stale").unwrap();

        write_private(&target, "secret").expect("write");

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "secret");
        assert_eq!(std::fs::read_to_string(&decoy).unwrap(), "decoy");
        let mode = std::fs::symlink_metadata(&target)
            .unwrap()
            .permissions()
            .mode();
        assert!(
            mode & 0o170000 == 0o100000,
            "destination must be a regular file now"
        );
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn api_keys_are_hex_and_unique() {
        let a = generate_api_key().expect("key");
        let b = generate_api_key().expect("key");
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}

//! Storage layout. Everything claudex owns lives under one base directory
//! (default `~/.ovm/claudex/`, overridable via `OVM_CLAUDEX_HOME` for tests)
//! so teardown is a single directory delete and the user's real Claude and
//! OVM state are provably untouched.

use crate::{ClaudexError, Result};
use std::path::{Path, PathBuf};

pub struct ClaudexDirs {
    pub base: PathBuf,
}

impl ClaudexDirs {
    pub fn new() -> Result<Self> {
        if let Ok(base) = std::env::var("OVM_CLAUDEX_HOME") {
            if !base.is_empty() {
                return Ok(Self::at(PathBuf::from(base)));
            }
        }
        let home = dirs::home_dir()
            .ok_or_else(|| ClaudexError::Message("Could not determine home directory.".into()))?;
        Ok(Self::at(home.join(".ovm").join("claudex")))
    }

    pub fn at(base: PathBuf) -> Self {
        Self { base }
    }

    /// Isolated Claude home — the value of `CLAUDE_CONFIG_DIR`. Sessions,
    /// settings, and onboarding state for claudex live here, never in `~/.claude`.
    pub fn claude_home(&self) -> PathBuf {
        self.base.join("claude")
    }

    /// claudex's own config (model registry, proxy port/key, pin policy).
    pub fn config_file(&self) -> PathBuf {
        self.base.join("config.json")
    }

    pub fn proxy_dir(&self) -> PathBuf {
        self.base.join("proxy")
    }

    /// Generated CLIProxyAPI YAML config (localhost-only bind).
    pub fn proxy_config_file(&self) -> PathBuf {
        self.proxy_dir().join("config.yaml")
    }

    /// Where CLIProxyAPI stores provider OAuth tokens (its `auth-dir`).
    pub fn proxy_auth_dir(&self) -> PathBuf {
        self.proxy_dir().join("auth")
    }

    /// Managed proxy binaries, one directory per version.
    pub fn proxy_versions_dir(&self) -> PathBuf {
        self.proxy_dir().join("versions")
    }

    /// Symlink to the active managed proxy binary.
    pub fn proxy_current(&self) -> PathBuf {
        self.proxy_dir().join("current")
    }

    pub fn proxy_pid_file(&self) -> PathBuf {
        self.proxy_dir().join("cliproxyapi.pid")
    }

    pub fn proxy_log_file(&self) -> PathBuf {
        self.base.join("logs").join("proxy.log")
    }

    /// Serializes proxy downloads and publication across OVM processes.
    pub fn proxy_update_lock(&self) -> PathBuf {
        self.proxy_dir().join("update.lock")
    }

    /// Shared for a live claudex session; exclusive while changing the
    /// running proxy. The OS releases the lock if a launcher exits or dies.
    pub fn proxy_sessions_lock(&self) -> PathBuf {
        self.proxy_dir().join("sessions.lock")
    }

    pub fn proxy_update_cache(&self) -> PathBuf {
        self.proxy_dir().join("update-cache.json")
    }

    pub fn proxy_pending_update(&self) -> PathBuf {
        self.proxy_dir().join("pending-update.json")
    }

    /// Private, durable Claude-history → claudex → Codex relationships.
    pub fn history_relationships_dir(&self) -> PathBuf {
        self.base.join("history").join("relationships")
    }

    pub fn ensure_layout(&self) -> Result<()> {
        for dir in [
            self.base.clone(),
            self.claude_home(),
            self.proxy_dir(),
            self.proxy_auth_dir(),
            self.proxy_versions_dir(),
            self.base.join("logs"),
            self.history_relationships_dir(),
        ] {
            std::fs::create_dir_all(&dir)?;
        }
        // OAuth tokens, session associations, and the proxy log (which can
        // contain upstream bearer tokens) are private local state — owner-only.
        // The proxy tree and base are locked down too so no path to them is
        // world-traversable on hosts where $HOME is 0755.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for private_dir in [
                self.base.clone(),
                self.proxy_dir(),
                self.proxy_auth_dir(),
                self.base.join("logs"),
                self.history_relationships_dir(),
            ] {
                std::fs::set_permissions(&private_dir, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(())
    }
}

/// The user's *real* Claude home (`~/.claude`), used read-only: to copy the
/// theme during setup and to `@import` the user's global CLAUDE.md.
pub fn real_claude_home() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".claude"))
}

/// The user's real top-level Claude config (`~/.claude.json`), read-only.
pub fn real_claude_config() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".claude.json"))
}

/// Directory to install the `claudex` shim into: `~/.local/bin` when present
/// (it's where Claude Code's own launcher lives, so it's on PATH), else none.
pub fn shim_install_dir() -> Option<PathBuf> {
    let dir = dirs::home_dir()?.join(".local").join("bin");
    dir.is_dir().then_some(dir)
}

/// Human-friendly display of a path, with the home directory shown as `~`.
pub fn display(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_hangs_off_one_base_directory() {
        let dirs = ClaudexDirs::at(PathBuf::from("/tmp/x"));
        for path in [
            dirs.claude_home(),
            dirs.config_file(),
            dirs.proxy_config_file(),
            dirs.proxy_auth_dir(),
            dirs.proxy_versions_dir(),
            dirs.proxy_current(),
            dirs.proxy_pid_file(),
            dirs.proxy_log_file(),
            dirs.proxy_update_lock(),
            dirs.proxy_sessions_lock(),
            dirs.proxy_update_cache(),
            dirs.proxy_pending_update(),
            dirs.history_relationships_dir(),
        ] {
            assert!(
                path.starts_with("/tmp/x"),
                "{} escapes the base directory",
                path.display()
            );
        }
    }

    #[test]
    fn ensure_layout_creates_all_directories() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = ClaudexDirs::at(temp.path().join("claudex"));
        dirs.ensure_layout().expect("layout");
        assert!(dirs.claude_home().is_dir());
        assert!(dirs.proxy_auth_dir().is_dir());
        assert!(dirs.proxy_versions_dir().is_dir());
        assert!(dirs.proxy_log_file().parent().unwrap().is_dir());
        assert!(dirs.history_relationships_dir().is_dir());
    }

    #[test]
    #[cfg(unix)]
    fn auth_dir_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = ClaudexDirs::at(temp.path().join("claudex"));
        dirs.ensure_layout().expect("layout");

        let mode = std::fs::metadata(dirs.proxy_auth_dir())
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "auth dir holds OAuth tokens");

        let feedback_mode = std::fs::metadata(dirs.history_relationships_dir())
            .expect("feedback metadata")
            .permissions()
            .mode();
        assert_eq!(
            feedback_mode & 0o777,
            0o700,
            "feedback dir holds session associations"
        );
    }
}

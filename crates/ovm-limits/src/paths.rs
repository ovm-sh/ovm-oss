//! Everything limits owns lives under one base directory (`~/.ovm/limits`,
//! or `OVM_LIMITS_HOME`), so a purge is one `rm -rf` and tests can point the
//! whole plugin at a temp dir.

use crate::{LimitsError, Result};
use std::path::{Path, PathBuf};

pub const HOME_ENV: &str = "OVM_LIMITS_HOME";

#[derive(Debug, Clone)]
pub struct LimitsDirs {
    base: PathBuf,
}

impl LimitsDirs {
    pub fn new() -> Result<Self> {
        if let Some(base) = std::env::var_os(HOME_ENV).filter(|value| !value.is_empty()) {
            return Ok(Self::at(PathBuf::from(base)));
        }
        let home =
            dirs::home_dir().ok_or_else(|| LimitsError::Message("no home directory".into()))?;
        Ok(Self::at(home.join(".ovm").join("limits")))
    }

    pub fn at(base: PathBuf) -> Self {
        Self { base }
    }

    pub fn base(&self) -> &Path {
        &self.base
    }

    /// The account list.
    pub fn config_file(&self) -> PathBuf {
        self.base.join("config.json")
    }

    /// One file per account, rewritten on every poll.
    pub fn snapshots_dir(&self) -> PathBuf {
        self.base.join("snapshots")
    }

    /// The id is used verbatim: `config::validate_id` guarantees it is
    /// file-name safe, and distinct ids give distinct files.
    pub fn snapshot_file(&self, id: &str) -> PathBuf {
        self.snapshots_dir().join(format!("{id}.json"))
    }

    /// Raw statusline payloads from the most recent Claude poll, one per account.
    /// Kept for diagnosis; `poll` truncates it before each run.
    pub fn capture_file(&self, id: &str) -> PathBuf {
        self.base.join("capture").join(format!("{id}.jsonl"))
    }

    /// The merged view every consumer reads.
    pub fn merged_file(&self) -> PathBuf {
        self.base.join("limits.json")
    }

    /// The public feed: surprise resets and poll timing, nothing private.
    pub fn public_file(&self) -> PathBuf {
        self.base.join("resets.json")
    }

    /// One JSON object per line: every event a poll noticed, oldest first.
    pub fn events_file(&self) -> PathBuf {
        self.base.join("events.jsonl")
    }

    /// Where the launchd agent writes its combined stdout/stderr.
    pub fn agent_log_file(&self) -> PathBuf {
        self.base.join("agent.log")
    }

    /// Working directory for the throwaway Claude sessions. Empty on purpose:
    /// no CLAUDE.md, no .mcp.json, nothing for the session to load.
    pub fn scratch_dir(&self) -> PathBuf {
        self.base.join("scratch")
    }

    /// The home a polled account runs in. Every account gets its own, under
    /// our directory — never `~/.claude` or `~/.codex`. A poll drives a real
    /// session, and a real session refreshes its OAuth token; when that token
    /// is the one your own editor sessions hold, refreshing it signs them out
    /// (2026-09-06). A home of our own means a grant of our own.
    pub fn poll_home(&self, id: &str) -> PathBuf {
        self.homes_dir().join(id)
    }

    pub fn homes_dir(&self) -> PathBuf {
        self.base.join("homes")
    }

    pub fn ensure_layout(&self) -> Result<()> {
        for dir in [
            self.base.clone(),
            self.snapshots_dir(),
            self.base.join("capture"),
            self.homes_dir(),
            self.scratch_dir(),
        ] {
            std::fs::create_dir_all(&dir)?;
        }
        Ok(())
    }
}

pub fn display(path: &Path) -> String {
    let text = path.to_string_lossy().into_owned();
    match dirs::home_dir() {
        Some(home) => {
            let home = home.to_string_lossy();
            match text.strip_prefix(home.as_ref()) {
                Some(rest) => format!("~{rest}"),
                None => text,
            }
        }
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_hangs_off_one_base_directory() {
        let dirs = LimitsDirs::at(PathBuf::from("/tmp/limits-test"));
        assert_eq!(
            dirs.config_file(),
            PathBuf::from("/tmp/limits-test/config.json")
        );
        assert_eq!(
            dirs.merged_file(),
            PathBuf::from("/tmp/limits-test/limits.json")
        );
        assert_eq!(
            dirs.snapshot_file("claude-1"),
            PathBuf::from("/tmp/limits-test/snapshots/claude-1.json")
        );
    }

    #[test]
    fn ensure_layout_creates_all_directories() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = LimitsDirs::at(temp.path().join("limits"));
        dirs.ensure_layout().unwrap();
        assert!(dirs.snapshots_dir().is_dir());
        assert!(dirs.scratch_dir().is_dir());
        assert!(dirs.base.join("capture").is_dir());
    }
}

use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DevInstallMode {
    Copy,
    Link,
}

impl DevInstallMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Link => "link",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DevInstallMetadata {
    #[serde(default = "default_kind")]
    pub(crate) kind: String,
    pub mode: DevInstallMode,
    pub source: PathBuf,
    pub git_repo_root: Option<PathBuf>,
    pub git_branch: Option<String>,
    pub git_commit: Option<String>,
}

fn default_kind() -> String {
    "dev".to_string()
}

impl DevInstallMetadata {
    pub fn collect(source: PathBuf, mode: DevInstallMode) -> Self {
        let git = GitMetadata::from_source_path(&source);

        Self {
            kind: default_kind(),
            mode,
            source,
            git_repo_root: git.repo_root,
            git_branch: git.branch,
            git_commit: git.commit,
        }
    }

    pub fn read(path: &Path) -> Result<Option<Self>> {
        let contents = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let metadata: Self = serde_json::from_str(&contents)?;
        Ok(Some(metadata.enriched()))
    }

    fn enriched(mut self) -> Self {
        if self.git_repo_root.is_some() && self.git_branch.is_some() && self.git_commit.is_some() {
            return self;
        }

        let git = GitMetadata::from_source_path(&self.source);
        if self.git_repo_root.is_none() {
            self.git_repo_root = git.repo_root;
        }
        if self.git_branch.is_none() {
            self.git_branch = git.branch;
        }
        if self.git_commit.is_none() {
            self.git_commit = git.commit;
        }
        self
    }
}

#[derive(Debug, Default)]
struct GitMetadata {
    repo_root: Option<PathBuf>,
    branch: Option<String>,
    commit: Option<String>,
}

impl GitMetadata {
    fn from_source_path(source: &Path) -> Self {
        let Some(search_root) = source.parent() else {
            return Self::default();
        };
        let Some(repo_root) = run_git(search_root, &["rev-parse", "--show-toplevel"]) else {
            return Self::default();
        };
        let repo_root = PathBuf::from(repo_root);

        Self {
            repo_root: Some(repo_root.clone()),
            branch: run_git(&repo_root, &["branch", "--show-current"]),
            commit: run_git(&repo_root, &["rev-parse", "--short=12", "HEAD"]),
        }
    }
}

fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{DevInstallMetadata, DevInstallMode};
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use tempfile::tempdir;

    #[test]
    fn collects_git_metadata_from_source_repo() {
        let dir = tempdir().expect("tempdir");
        init_git_repo(dir.path());

        let binary = dir.path().join("target").join("debug").join("codex");
        fs::create_dir_all(binary.parent().expect("parent")).expect("mkdir");
        fs::write(&binary, "fake-binary").expect("write binary");
        fs::write(dir.path().join("README.md"), "hello").expect("write readme");
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "init"]);

        let metadata = DevInstallMetadata::collect(binary, DevInstallMode::Copy);

        assert_eq!(metadata.mode, DevInstallMode::Copy);
        assert_eq!(metadata.git_branch.as_deref(), Some("main"));
        assert_eq!(
            metadata
                .git_repo_root
                .as_deref()
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str()),
            dir.path().file_name().and_then(|name| name.to_str())
        );
        assert_eq!(metadata.git_commit.as_ref().map(String::len), Some(12));
    }

    #[test]
    fn read_returns_none_for_missing_metadata() {
        let dir = tempdir().expect("tempdir");

        let metadata = DevInstallMetadata::read(&dir.path().join("missing.json")).expect("read");

        assert_eq!(metadata, None);
    }

    #[test]
    fn read_backfills_git_fields_for_older_metadata() {
        let dir = tempdir().expect("tempdir");
        init_git_repo(dir.path());

        let binary = dir.path().join("target").join("debug").join("codex");
        fs::create_dir_all(binary.parent().expect("parent")).expect("mkdir");
        fs::write(&binary, "fake-binary").expect("write binary");
        fs::write(dir.path().join("README.md"), "hello").expect("write readme");
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "init"]);

        let meta_path = dir.path().join("meta.json");
        fs::write(
            &meta_path,
            serde_json::json!({
                "kind": "dev",
                "mode": "copy",
                "source": binary,
            })
            .to_string(),
        )
        .expect("write metadata");

        let metadata = DevInstallMetadata::read(&meta_path)
            .expect("read")
            .expect("metadata");

        assert_eq!(metadata.git_branch.as_deref(), Some("main"));
        assert_eq!(metadata.git_commit.as_ref().map(String::len), Some(12));
    }

    fn init_git_repo(repo: &Path) {
        git(repo, &["init", "--initial-branch=main"]);
        git(repo, &["config", "user.name", "OVM Tests"]);
        git(repo, &["config", "user.email", "ovm-tests@example.com"]);
    }

    fn git(repo: &Path, args: &[&str]) {
        // Run hermetically: when the suite runs from inside a git hook (e.g. the
        // pre-commit gate), git exports GIT_DIR/GIT_INDEX_FILE/GIT_WORK_TREE for
        // the outer repo. Those leak into this child and redirect its commands
        // at the real repository, breaking the tempdir init/commit. Clear them
        // so the tempdir is the only repository this helper ever touches.
        let mut command = Command::new("git");
        command.args(args).current_dir(repo);
        for key in [
            "GIT_DIR",
            "GIT_INDEX_FILE",
            "GIT_WORK_TREE",
            "GIT_PREFIX",
            "GIT_COMMON_DIR",
            "GIT_OBJECT_DIRECTORY",
        ] {
            command.env_remove(key);
        }
        let status = command.status().expect("git status");
        assert!(status.success(), "git {:?} failed", args);
    }
}

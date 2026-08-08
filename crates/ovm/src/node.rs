use crate::error::{OvmError, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

const QM_MINIMUM_NODE_MAJOR: u64 = 24;

/// Find the `node` executable the managed script's `/usr/bin/env` shebang will
/// select: the first executable named `node` on this process's PATH.
pub fn find_node() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("node"))
        .find(|candidate| is_executable_file(candidate))
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

pub fn require_qm_runtime() -> Result<PathBuf> {
    check_node_at(find_node().as_deref())
}

fn check_node_at(node: Option<&Path>) -> Result<PathBuf> {
    let node = node.ok_or_else(|| {
        OvmError::Message(
            "QM requires Node.js >=24.0.0, but `node` was not found on PATH. Install Node.js 24 or newer and retry."
                .into(),
        )
    })?;
    let output = Command::new(node)
        .arg("--version")
        .output()
        .map_err(|error| {
            OvmError::Message(format!(
                "QM requires Node.js >=24.0.0, but `{}` could not be run: {error}",
                node.display()
            ))
        })?;
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let version_text = raw.strip_prefix('v').unwrap_or(&raw);
    let version = semver::Version::parse(version_text).map_err(|_| {
        OvmError::Message(format!(
            "QM requires Node.js >=24.0.0, but `{}` reported an unrecognised version `{raw}`.",
            node.display()
        ))
    })?;

    if !output.status.success() || version.major < QM_MINIMUM_NODE_MAJOR {
        return Err(OvmError::Message(format!(
            "QM requires Node.js >=24.0.0, but found Node.js {version} at {}. Put Node.js 24 or newer first on PATH and retry.",
            node.display()
        )));
    }

    Ok(node.to_path_buf())
}

/// Discover npm binary, preferring fnm-managed Node
pub fn find_npm() -> Option<String> {
    // Check if fnm is available and use its npm
    if let Ok(output) = Command::new("fnm")
        .args(["exec", "--", "which", "npm"])
        .output()
    {
        if output.status.success() {
            if let Ok(path) = String::from_utf8(output.stdout) {
                let path = path.trim();
                if !path.is_empty() {
                    return Some(path.to_string());
                }
            }
        }
    }

    // Fall back to npm on PATH
    if let Ok(output) = Command::new("which").arg("npm").output() {
        if output.status.success() {
            if let Ok(path) = String::from_utf8(output.stdout) {
                let path = path.trim();
                if !path.is_empty() {
                    return Some(path.to_string());
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_node(dir: &Path, version: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let path = dir.join("node");
        std::fs::write(&path, format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n"))
            .expect("write fake node");
        let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("permissions");
        path
    }

    #[test]
    fn qm_runtime_refuses_absent_node() {
        let error = check_node_at(None).expect_err("missing node");
        assert!(error.to_string().contains("not found on PATH"), "{error}");
    }

    #[test]
    fn qm_runtime_refuses_node_below_minimum() {
        let dir = tempfile::tempdir().expect("tempdir");
        let node = fake_node(dir.path(), "v23.11.1");
        let error = check_node_at(Some(&node)).expect_err("old node");
        let message = error.to_string();
        assert!(message.contains("Node.js 23.11.1"), "{message}");
        assert!(message.contains("Node.js 24 or newer"), "{message}");
    }

    #[test]
    fn qm_runtime_accepts_supported_node() {
        let dir = tempfile::tempdir().expect("tempdir");
        let node = fake_node(dir.path(), "v24.0.0");
        assert_eq!(check_node_at(Some(&node)).expect("supported node"), node);
    }
}

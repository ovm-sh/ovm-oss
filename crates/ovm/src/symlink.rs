use crate::error::{OvmError, Result};
use std::path::Path;

pub fn read_current_version(link: &Path) -> Result<Option<String>> {
    if !link.exists() && !link.is_symlink() {
        return Ok(None);
    }

    let target = std::fs::read_link(link).map_err(|source| OvmError::SymlinkRead {
        path: link.to_path_buf(),
        source,
    })?;

    let version = target
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string());

    Ok(version)
}

pub fn switch_symlink(link: &Path, target: &Path) -> Result<()> {
    let parent = link.parent().ok_or_else(|| OvmError::SymlinkCreate {
        path: link.to_path_buf(),
        source: std::io::Error::other("no parent directory"),
    })?;

    let temp = parent.join(format!(".ovm-tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&temp);

    std::os::unix::fs::symlink(target, &temp).map_err(|source| OvmError::SymlinkCreate {
        path: temp.clone(),
        source,
    })?;

    std::fs::rename(&temp, link).map_err(|source| OvmError::SymlinkCreate {
        path: link.to_path_buf(),
        source,
    })?;

    Ok(())
}

/// Remove a symlink if it exists
#[cfg(test)]
pub fn remove_symlink(link: &Path) -> Result<()> {
    if link.is_symlink() {
        std::fs::remove_file(link)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_read_nonexistent_symlink() {
        let result = read_current_version(Path::new("/nonexistent/link")).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_switch_symlink_atomic() {
        let dir = tempdir().unwrap();
        let target_a = dir.path().join("versions/2.0.37");
        let target_b = dir.path().join("versions/2.1.71");
        let link = dir.path().join("current");

        std::fs::create_dir_all(&target_a).unwrap();
        std::fs::create_dir_all(&target_b).unwrap();

        switch_symlink(&link, &target_a).unwrap();
        assert_eq!(read_current_version(&link).unwrap(), Some("2.0.37".into()));

        switch_symlink(&link, &target_b).unwrap();
        assert_eq!(read_current_version(&link).unwrap(), Some("2.1.71".into()));
    }

    #[test]
    fn test_remove_symlink() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("link");

        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        remove_symlink(&link).unwrap();
        assert!(!link.exists());
    }
}

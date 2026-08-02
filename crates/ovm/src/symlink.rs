use crate::error::{OvmError, Result};
use std::path::Path;

pub fn read_current_version(link: &Path) -> Result<Option<String>> {
    // Only genuine absence is "no version selected". `exists()` collapses
    // every metadata error to false, so an unreadable pointer (EACCES, a
    // failing disk) used to read as no-current-version — and a cleanup survey
    // that plans nothing because it could not look then stamped itself
    // complete. Errors other than NotFound must surface as errors.
    match std::fs::symlink_metadata(link) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(OvmError::SymlinkRead {
                path: link.to_path_buf(),
                source,
            })
        }
        Ok(_) => {}
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

    /// An unreadable pointer is an error, never "no version selected" — a
    /// cleanup survey that plans nothing because it could not look must not
    /// stamp itself complete on the strength of that blindness.
    #[test]
    #[cfg(unix)]
    fn an_unreadable_current_pointer_is_an_error_not_absence() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let sealed = dir.path().join("sealed");
        std::fs::create_dir_all(&sealed).unwrap();
        let target = dir.path().join("2.0.37");
        std::fs::create_dir_all(&target).unwrap();
        let link = sealed.join("current");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::symlink_metadata(&link).is_ok() {
            // Root sees through the seal; the scenario cannot exist for it.
            std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }

        let result = read_current_version(&link);

        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o755)).unwrap();
        result.expect_err("an unreadable pointer must surface as an error");

        let absent = dir.path().join("never-created");
        assert_eq!(read_current_version(&absent).unwrap(), None);
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

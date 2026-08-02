use crate::error::{OvmError, Result};
use std::fs::{File, OpenOptions};
use std::path::Path;

/// Set file permissions to 0o755 (executable).
pub fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }

    Ok(())
}

/// Ensure the parent directory of a path exists.
pub fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

/// Open a path for writing that must not exist yet — the only way any install
/// flow may create a file inside a transaction's freshly prepared tree.
///
/// `File::create` follows a symlink standing at the destination and truncates
/// whatever it points to, so a link planted in the prepare-to-write window
/// would have a download or copy destroy an arbitrary file — demonstrated
/// against an adopt source before this existed. `create_new` (`O_CREAT|O_EXCL`)
/// refuses both a link and an existing file. The one supported way foreign code
/// used to run inside that window, the PreInstall hook, now runs *before* the
/// tree is prepared, so what is left to plant anything there is a process
/// writing into OVM's store for a version another process holds the install
/// lock on; the guard stays because that residual is documented, not defended.
/// Every destination this is used for sits in a directory the install
/// transaction just created empty under the per-version lock, so an existing
/// entry is never a state OVM produced: it is interference, and the honest
/// response is to fail the install — which routes through the transaction's
/// cleanup arm and publishes nothing — rather than to write somewhere unknown.
pub(crate) fn create_new_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                OvmError::Message(format!(
                    "Refusing to write {}: something is already there, in a directory this \
                     install had just created empty. A hook or another process interfered with \
                     the install; nothing was installed and nothing was published.",
                    path.display()
                ))
            } else {
                error.into()
            }
        })
}

/// [`create_new_file`] with contents — the `fs::write` of the install flows,
/// for markers, manifests and metadata written into the same fresh tree.
pub(crate) fn write_new_file(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    create_new_file(path)?.write_all(contents)?;
    Ok(())
}

/// Mark a just-created file executable through the handle that created it,
/// never through its path. `fs::set_permissions` takes a name and follows it,
/// so it carries the same defect as `File::create`: whatever link is standing
/// at that name by then is what gets chmod'd. `fchmod` on the descriptor can
/// only reach the file this transaction made.
#[cfg(unix)]
pub(crate) fn make_handle_executable(file: &File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// [`make_executable`] is a no-op off unix; so is this.
#[cfg(not(unix))]
pub(crate) fn make_handle_executable(_file: &File) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{create_new_file, write_new_file};
    use tempfile::tempdir;

    /// The install transaction erases whatever a PreInstall hook left in the
    /// tree, so nothing *supported* reaches these guards any more. They are kept
    /// — and tested directly rather than through a hook — for the residual the
    /// transaction cannot erase: a process writing into OVM's store for a
    /// version another process holds the install lock on.
    #[test]
    fn create_new_file_refuses_a_file_that_is_already_there() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("destination");
        std::fs::write(&path, b"bytes this install did not write").expect("pre-existing file");

        let error = create_new_file(&path).expect_err("an existing destination must be refused");

        assert!(error.to_string().contains("Refusing to write"), "{error}");
        assert_eq!(
            std::fs::read(&path).expect("the file"),
            b"bytes this install did not write",
            "a refused create must not truncate what was there"
        );
    }

    /// The defect `create_new` exists to close: `File::create` opens the link's
    /// *target* with `O_TRUNC`, so the file destroyed is one the install was
    /// never pointed at.
    #[cfg(unix)]
    #[test]
    fn create_new_file_refuses_a_symlink_without_touching_its_target() {
        let dir = tempdir().expect("tempdir");
        let victim = dir.path().join("victim");
        let victim_bytes = b"bytes no install has any business touching";
        std::fs::write(&victim, victim_bytes).expect("victim");
        let path = dir.path().join("destination");
        std::os::unix::fs::symlink(&victim, &path).expect("plant the link");

        let error = create_new_file(&path).expect_err("a planted link must be refused");

        assert!(error.to_string().contains("Refusing to write"), "{error}");
        assert_eq!(
            std::fs::read(&victim).expect("the victim"),
            victim_bytes,
            "the refused create reached through the link"
        );
        assert!(
            std::fs::symlink_metadata(&path)
                .expect("the link")
                .file_type()
                .is_symlink(),
            "a refusal must leave the planted link alone rather than replace it"
        );
    }

    #[test]
    fn write_new_file_refuses_a_file_that_is_already_there() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("marker");
        std::fs::write(&path, b"a claim this install did not make").expect("pre-existing marker");

        let error =
            write_new_file(&path, b"published").expect_err("an existing marker must be refused");

        assert!(error.to_string().contains("Refusing to write"), "{error}");
        assert_eq!(
            std::fs::read(&path).expect("the marker"),
            b"a claim this install did not make"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_new_file_refuses_a_symlink_without_touching_its_target() {
        let dir = tempdir().expect("tempdir");
        let victim = dir.path().join("victim");
        let victim_bytes = b"bytes no marker write has any business touching";
        std::fs::write(&victim, victim_bytes).expect("victim");
        let path = dir.path().join("marker");
        std::os::unix::fs::symlink(&victim, &path).expect("plant the link");

        let error =
            write_new_file(&path, b"published").expect_err("a planted link must be refused");

        assert!(error.to_string().contains("Refusing to write"), "{error}");
        assert_eq!(
            std::fs::read(&victim).expect("the victim"),
            victim_bytes,
            "the refused marker write reached through the link"
        );
    }

    #[test]
    fn create_new_file_creates_a_destination_that_is_not_there() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("destination");

        write_new_file(&path, b"published").expect("a fresh destination must be writable");

        assert_eq!(std::fs::read(&path).expect("the file"), b"published");
    }
}

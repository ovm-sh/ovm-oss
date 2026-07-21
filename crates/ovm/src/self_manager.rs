use crate::bundle_manifest::{safe_binary_name, BundleManifest, BUNDLE_MANIFEST_NAME};
use crate::config::{OvmDirs, COMPLETE_MARKER};
use crate::error::{OvmError, Result};
use crate::product::Product;
use crate::{symlink, util};
use fs4::{FileExt, TryLockError};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const OPERATION_LOCK_TIMEOUT: Duration = Duration::from_secs(60);
const OPERATION_LOCK_RETRY: Duration = Duration::from_millis(50);

pub const SELF_CHILD_ENV: &str = "OVM_SELF_MANAGED_CHILD";
const SELF_LOCK_HELPER_PATH_ENV: &str = "OVM_SELF_LOCK_HELPER_PATH";
const SELF_LOCK_HELPER_READY_ENV: &str = "OVM_SELF_LOCK_HELPER_READY";

#[derive(Debug, Clone)]
pub struct SelfDirs {
    pub root: PathBuf,
    pub versions: PathBuf,
    pub current: PathBuf,
    pub previous: PathBuf,
    pub launcher_dir_file: PathBuf,
    pub side_links_file: PathBuf,
    pub operation_lock: PathBuf,
}

impl SelfDirs {
    pub fn at(base: &Path) -> Self {
        let root = base.join("self");
        Self {
            versions: root.join("versions"),
            current: root.join("current"),
            previous: root.join("previous"),
            launcher_dir_file: root.join("launcher-dir"),
            side_links_file: root.join("side-links"),
            operation_lock: root.join(".operation.lock"),
            root,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SelfManager {
    pub ovm_dirs: OvmDirs,
    pub dirs: SelfDirs,
}

#[derive(Debug)]
pub struct SelfOperationLock {
    _file: File,
}

#[derive(Debug, Clone)]
pub(crate) struct PathSnapshot {
    path: PathBuf,
    state: PathState,
}

#[derive(Debug, Clone)]
enum PathState {
    Missing,
    Symlink(PathBuf),
    File { contents: Vec<u8>, mode: u32 },
}

#[derive(Debug, Clone)]
pub(crate) struct SelectionSnapshot {
    current: PathSnapshot,
    previous: PathSnapshot,
    side_links: PathSnapshot,
    launchers: Vec<PathSnapshot>,
    previous_side_names: HashSet<String>,
}

impl SelfManager {
    pub fn new() -> Result<Self> {
        Ok(Self::at(OvmDirs::new()?))
    }

    pub fn at(ovm_dirs: OvmDirs) -> Self {
        let dirs = SelfDirs::at(&ovm_dirs.base);
        Self { ovm_dirs, dirs }
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        self.ovm_dirs.ensure_base_dirs()?;
        std::fs::create_dir_all(&self.dirs.versions)?;
        std::fs::create_dir_all(self.launcher_dir())?;
        Ok(())
    }

    pub fn acquire_operation_lock(&self) -> Result<SelfOperationLock> {
        std::fs::create_dir_all(&self.dirs.root)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.dirs.operation_lock)?;
        let started = Instant::now();
        let mut announced_wait = false;

        loop {
            match FileExt::try_lock(&file) {
                Ok(()) => return Ok(SelfOperationLock { _file: file }),
                Err(TryLockError::WouldBlock) => {
                    if started.elapsed() >= OPERATION_LOCK_TIMEOUT {
                        return Err(OvmError::Message(format!(
                            "Timed out waiting for another OVM self-management operation at {}",
                            self.dirs.operation_lock.display()
                        )));
                    }
                    if !announced_wait {
                        eprintln!("Waiting for another OVM self-management operation to finish...");
                        announced_wait = true;
                    }
                    std::thread::sleep(OPERATION_LOCK_RETRY);
                }
                Err(TryLockError::Error(error)) => return Err(error.into()),
            }
        }
    }

    pub fn launcher_dir(&self) -> PathBuf {
        std::fs::read_to_string(&self.dirs.launcher_dir_file)
            .ok()
            .map(|value| PathBuf::from(value.trim()))
            .filter(|path| path.is_absolute())
            .unwrap_or_else(|| self.ovm_dirs.bin.clone())
    }

    pub fn control_plane_path(&self) -> PathBuf {
        self.launcher_dir().join("ovm")
    }

    pub fn version_dir(&self, version: &str) -> PathBuf {
        self.dirs.versions.join(version)
    }

    pub fn current_version(&self) -> Result<Option<String>> {
        symlink::read_current_version(&self.dirs.current)
    }

    pub fn previous_version(&self) -> Result<Option<String>> {
        symlink::read_current_version(&self.dirs.previous)
    }

    pub fn load_manifest(&self, version: &str) -> Result<BundleManifest> {
        BundleManifest::load(&self.version_dir(version).join(BUNDLE_MANIFEST_NAME))
    }

    pub fn is_complete(&self, version: &str) -> bool {
        self.require_complete(version).is_ok()
    }

    pub fn require_complete(&self, version: &str) -> Result<BundleManifest> {
        validate_version_id(version)?;
        let root = self.version_dir(version);
        if !root.join(COMPLETE_MARKER).is_file() {
            return Err(OvmError::Message(format!(
                "OVM self version {version} is incomplete"
            )));
        }
        let manifest = BundleManifest::load(&root.join(BUNDLE_MANIFEST_NAME))?;
        for entry in manifest.entries() {
            let binary = root.join(&entry.binary);
            if !binary.is_file() {
                return Err(OvmError::Message(format!(
                    "OVM self version {version} is missing {}",
                    entry.binary
                )));
            }
        }
        Ok(manifest)
    }

    pub fn list_versions(&self) -> Result<Vec<String>> {
        if !self.dirs.versions.is_dir() {
            return Ok(Vec::new());
        }
        let mut versions = Vec::new();
        for entry in std::fs::read_dir(&self.dirs.versions)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Some(version) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !version.starts_with('.') && self.is_complete(&version) {
                versions.push(version);
            }
        }
        versions.sort();
        Ok(versions)
    }

    pub fn install_bundle(
        &self,
        version: &str,
        manifest: &BundleManifest,
        source_dir: &Path,
    ) -> Result<PathBuf> {
        validate_version_id(version)?;
        self.ensure_dirs()?;
        validate_source_bundle(manifest, source_dir)?;

        let destination = self.version_dir(version);
        if destination.exists() {
            if self.bundle_matches(&destination, manifest, source_dir)? {
                return Ok(destination);
            }
            return Err(OvmError::Message(format!(
                "OVM self version {version} already exists with different contents"
            )));
        }

        let staging = tempfile::Builder::new()
            .prefix(".installing-")
            .tempdir_in(&self.dirs.versions)?;
        std::fs::write(staging.path().join(BUNDLE_MANIFEST_NAME), manifest.to_tsv())?;
        for entry in manifest.entries() {
            let destination = staging.path().join(&entry.binary);
            std::fs::copy(source_dir.join(&entry.binary), &destination)?;
            util::make_executable(&destination)?;
        }
        std::fs::write(staging.path().join(COMPLETE_MARKER), b"")?;

        let staging_path = staging.keep();
        if destination.exists() {
            let matches = self.bundle_matches(&destination, manifest, source_dir)?;
            let _ = std::fs::remove_dir_all(&staging_path);
            if matches {
                return Ok(destination);
            }
            return Err(OvmError::Message(format!(
                "OVM self version {version} already exists with different contents"
            )));
        }
        if let Err(error) = std::fs::rename(&staging_path, &destination) {
            let result = if destination.exists()
                && self.bundle_matches(&destination, manifest, source_dir)?
            {
                Ok(destination.clone())
            } else {
                Err(error.into())
            };
            let _ = std::fs::remove_dir_all(&staging_path);
            return result;
        }
        Ok(destination)
    }

    pub fn refresh_control_plane(&self, version: &str) -> Result<()> {
        let manifest = self.require_complete(version)?;
        let source = self.version_dir(version).join(&manifest.main().binary);
        self.ensure_dirs()?;
        self.back_up_control_plane()?;
        atomic_copy_executable(&source, &self.control_plane_path())
    }

    pub fn repair_control_plane(&self) -> Result<()> {
        let backup = self.dirs.root.join("control-previous");
        if !backup.is_file() {
            return Err(OvmError::Message(
                "OVM has no previous control plane to restore".into(),
            ));
        }
        self.ensure_dirs()?;
        atomic_copy_executable(&backup, &self.control_plane_path())
    }

    fn back_up_control_plane(&self) -> Result<()> {
        let control = self.control_plane_path();
        if !control.is_file() {
            return Ok(());
        }
        atomic_copy_executable(&control, &self.dirs.root.join("control-previous"))
    }

    pub(crate) fn snapshot_selection(&self, version: &str) -> Result<SelectionSnapshot> {
        let next_manifest = self.require_complete(version)?;
        self.ensure_dirs()?;

        let old_version = self.current_version()?;
        // A damaged active version must not block escape to a known-good one.
        // Side-link ownership is persisted independently of that manifest.
        let old_manifest = old_version
            .as_deref()
            .and_then(|current| self.load_manifest(current).ok());
        let previous_side_names = self
            .read_managed_side_links()
            .unwrap_or_else(|| side_names(old_manifest.as_ref()));

        self.validate_switch_path(&self.dirs.current)?;
        if old_version.as_deref() != Some(version) && old_version.is_some() {
            self.validate_switch_path(&self.dirs.previous)?;
        }
        self.validate_side_links(&next_manifest)?;

        let mut paths = HashSet::new();
        for name in &previous_side_names {
            let path = self.launcher_dir().join(name);
            if path.is_symlink() && self.is_managed_side_link(&path) {
                paths.insert(path);
            }
        }
        for entry in next_manifest.side_entries() {
            paths.insert(self.launcher_dir().join(&entry.binary));
        }
        for product in Product::ALL {
            let launcher = self.ovm_dirs.bin.join(product.binary_name());
            if self.product_launcher_is_managed(&launcher)? {
                paths.insert(launcher);
            }
        }
        let mut paths = paths.into_iter().collect::<Vec<_>>();
        paths.sort();
        let launchers = paths
            .into_iter()
            .map(PathSnapshot::capture)
            .collect::<Result<Vec<_>>>()?;

        Ok(SelectionSnapshot {
            current: PathSnapshot::capture(self.dirs.current.clone())?,
            previous: PathSnapshot::capture(self.dirs.previous.clone())?,
            side_links: PathSnapshot::capture(self.dirs.side_links_file.clone())?,
            launchers,
            previous_side_names,
        })
    }

    pub(crate) fn snapshot_control_plane(&self) -> Result<PathSnapshot> {
        PathSnapshot::capture(self.control_plane_path())
    }

    pub(crate) fn snapshot_control_backup(&self) -> Result<PathSnapshot> {
        PathSnapshot::capture(self.dirs.root.join("control-previous"))
    }

    pub(crate) fn restore_path(&self, snapshot: &PathSnapshot) -> Result<()> {
        snapshot.restore()
    }

    pub(crate) fn restore_selection(&self, snapshot: &SelectionSnapshot) -> Result<()> {
        // Selection pointers first — `current` decides what actually runs, so
        // it must never be left pointing at a failed version because a
        // launcher restore errored. Every path is attempted; errors aggregate.
        let mut errors = Vec::new();
        let mut attempt = |result: Result<()>| {
            if let Err(error) = result {
                errors.push(error.to_string());
            }
        };
        attempt(snapshot.current.restore());
        attempt(snapshot.previous.restore());
        attempt(snapshot.side_links.restore());
        for launcher in &snapshot.launchers {
            attempt(launcher.restore());
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(OvmError::Message(errors.join("; ")))
        }
    }

    pub fn use_version(&self, version: &str) -> Result<()> {
        let snapshot = self.snapshot_selection(version)?;
        if let Err(error) = self.apply_version(version, &snapshot.previous_side_names) {
            return Err(with_recovery_error(
                error,
                self.restore_selection(&snapshot),
            ));
        }
        Ok(())
    }

    fn apply_version(&self, version: &str, old_side_names: &HashSet<String>) -> Result<()> {
        let next_manifest = self.require_complete(version)?;
        let old_version = self.current_version()?;

        self.prepare_side_links(&next_manifest)?;
        if old_version.as_deref() != Some(version) {
            if let Some(current) = old_version.as_deref() {
                symlink::switch_symlink(&self.dirs.previous, &self.version_dir(current))?;
            }
            symlink::switch_symlink(&self.dirs.current, &self.version_dir(version))?;
        }
        self.remove_obsolete_side_links(old_side_names, &next_manifest)?;
        self.write_managed_side_links(&next_manifest)?;
        self.reconcile_product_launchers()
    }

    pub fn rollback(&self) -> Result<String> {
        let previous = self.previous_version()?.ok_or_else(|| {
            OvmError::Message("OVM has no previous self-managed version to restore".into())
        })?;
        self.use_version(&previous)?;
        Ok(previous)
    }

    pub fn prune_inactive_dev_versions(&self) -> Result<Vec<String>> {
        let current = self.current_version()?;
        let previous = self.previous_version()?;
        let mut removed = Vec::new();
        for version in self.list_versions()? {
            if !version.starts_with("dev-")
                || current.as_deref() == Some(&version)
                || previous.as_deref() == Some(&version)
            {
                continue;
            }
            std::fs::remove_dir_all(self.version_dir(&version))?;
            removed.push(version);
        }
        Ok(removed)
    }

    pub fn is_control_plane_executable(&self, executable: &Path) -> bool {
        let control = self.control_plane_path();
        let Ok(metadata) = std::fs::symlink_metadata(&control) else {
            return false;
        };
        if !metadata.file_type().is_file() {
            return false;
        }
        same_path(executable, &control)
    }

    pub fn is_managed_version_executable(&self, executable: &Path) -> bool {
        let Ok(executable) = std::fs::canonicalize(executable) else {
            return false;
        };
        self.path_is_under_versions(&executable)
            && executable.file_name().and_then(|name| name.to_str()) == Some("ovm")
            && self.is_control_plane_executable(&self.control_plane_path())
    }

    fn validate_switch_path(&self, path: &Path) -> Result<()> {
        if path.exists() || path.is_symlink() {
            let metadata = std::fs::symlink_metadata(path)?;
            if !metadata.file_type().is_symlink() {
                return Err(OvmError::Message(format!(
                    "Refusing to replace non-symlink self pointer at {}",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    fn read_managed_side_links(&self) -> Option<HashSet<String>> {
        let contents = std::fs::read_to_string(&self.dirs.side_links_file).ok()?;
        let mut names = HashSet::new();
        for name in contents.lines().filter(|name| !name.is_empty()) {
            if name == "ovm" || !safe_binary_name(name) || !names.insert(name.to_string()) {
                return None;
            }
        }
        Some(names)
    }

    fn write_managed_side_links(&self, manifest: &BundleManifest) -> Result<()> {
        let mut contents = String::new();
        for entry in manifest.side_entries() {
            contents.push_str(&entry.binary);
            contents.push('\n');
        }
        atomic_write(&self.dirs.side_links_file, contents.as_bytes())
    }

    fn validate_side_links(&self, manifest: &BundleManifest) -> Result<()> {
        // Validate the entire set before creating any link. A conflict late in
        // the manifest must leave both current and the launcher directory intact.
        for entry in manifest.side_entries() {
            let link = self.launcher_dir().join(&entry.binary);
            if link.exists() || link.is_symlink() {
                let metadata = std::fs::symlink_metadata(&link)?;
                if !metadata.file_type().is_symlink() || !self.is_managed_side_link(&link) {
                    return Err(OvmError::Message(format!(
                        "Refusing to replace foreign side binary at {}",
                        link.display()
                    )));
                }
            }
        }
        Ok(())
    }

    fn prepare_side_links(&self, manifest: &BundleManifest) -> Result<()> {
        self.validate_side_links(manifest)?;
        for entry in manifest.side_entries() {
            symlink::switch_symlink(&self.launcher_dir().join(&entry.binary), Path::new("ovm"))?;
        }
        Ok(())
    }

    fn remove_obsolete_side_links(
        &self,
        previous_names: &HashSet<String>,
        next: &BundleManifest,
    ) -> Result<()> {
        let next_names = next
            .side_entries()
            .map(|entry| entry.binary.as_str())
            .collect::<HashSet<_>>();
        for name in previous_names {
            if next_names.contains(name.as_str()) {
                continue;
            }
            let link = self.launcher_dir().join(name);
            if link.is_symlink() && self.is_managed_side_link(&link) {
                std::fs::remove_file(link)?;
            }
        }
        Ok(())
    }

    fn reconcile_product_launchers(&self) -> Result<()> {
        let control = self.control_plane_path();
        for product in Product::ALL {
            let launcher = self.ovm_dirs.bin.join(product.binary_name());
            if self.product_launcher_is_managed(&launcher)? {
                symlink::switch_symlink(&launcher, &control)?;
            }
        }
        Ok(())
    }

    fn product_launcher_is_managed(&self, launcher: &Path) -> Result<bool> {
        if !launcher.is_symlink() {
            return Ok(false);
        }
        let target = std::fs::read_link(launcher)?;
        Ok(target == self.control_plane_path()
            || target == Path::new("ovm")
            || self.link_target_is_managed(launcher, &target))
    }

    fn is_managed_side_link(&self, link: &Path) -> bool {
        let Ok(target) = std::fs::read_link(link) else {
            return false;
        };
        target == Path::new("ovm")
            || target == self.control_plane_path()
            || self.link_target_is_managed(link, &target)
    }

    fn link_target_is_managed(&self, link: &Path, target: &Path) -> bool {
        let absolute = if target.is_absolute() {
            target.to_path_buf()
        } else {
            link.parent().unwrap_or_else(|| Path::new(".")).join(target)
        };
        self.path_is_under_versions(&absolute) || same_path(&absolute, &self.dirs.current)
    }

    fn path_is_under_versions(&self, path: &Path) -> bool {
        let versions = std::fs::canonicalize(&self.dirs.versions)
            .unwrap_or_else(|_| self.dirs.versions.clone());
        std::fs::canonicalize(path)
            .map(|path| path.starts_with(versions))
            .unwrap_or(false)
    }

    fn bundle_matches(
        &self,
        destination: &Path,
        manifest: &BundleManifest,
        source_dir: &Path,
    ) -> Result<bool> {
        if !destination.join(COMPLETE_MARKER).is_file() {
            return Ok(false);
        }
        let installed = BundleManifest::load(&destination.join(BUNDLE_MANIFEST_NAME))?;
        if &installed != manifest {
            return Ok(false);
        }
        for entry in manifest.entries() {
            if digest_file(&destination.join(&entry.binary))?
                != digest_file(&source_dir.join(&entry.binary))?
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

pub fn run_lock_helper_if_requested() -> Result<bool> {
    let Some(lock_path) = std::env::var_os(SELF_LOCK_HELPER_PATH_ENV) else {
        return Ok(false);
    };
    let ready_path = std::env::var_os(SELF_LOCK_HELPER_READY_ENV).ok_or_else(|| {
        OvmError::Message(format!(
            "{SELF_LOCK_HELPER_READY_ENV} is required for the self-lock helper"
        ))
    })?;
    let lock_path = PathBuf::from(lock_path);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    FileExt::lock(&file)?;
    std::fs::write(ready_path, b"")?;

    let mut input = std::io::stdin().lock();
    let mut buffer = [0_u8; 1024];
    while input.read(&mut buffer)? != 0 {}
    FileExt::unlock(&file)?;
    Ok(true)
}

/// Whether the self-managed child marker means this process should run
/// directly (`true`) or still be dispatched to its real versioned binary
/// (`false`). Only the main `ovm` binary runs directly when it carries the
/// marker; a SIDE-binary invocation (e.g. `ovm-claudex`, reached because its
/// `~/.ovm/bin` symlink points back at the control-plane `ovm`) must still be
/// dispatched to its real versioned binary — the control-plane `ovm` has no
/// such subcommand and would otherwise fall through to the CLI parser and
/// error with "unrecognized subcommand".
fn child_marker_runs_directly(invocation: &str) -> bool {
    invocation == "ovm"
}

pub fn proxy_if_needed(args: &[String]) -> Result<()> {
    let invocation = args
        .first()
        .and_then(|value| Path::new(value).file_name())
        .and_then(|value| value.to_str())
        .unwrap_or("ovm");

    if std::env::var_os(SELF_CHILD_ENV).is_some() {
        // Consume the marker: launched products must not inherit it, or an
        // `ovm` they shell out to skips proxying and runs the control-plane
        // binary directly — silently a different version than selected.
        std::env::remove_var(SELF_CHILD_ENV);
        if child_marker_runs_directly(invocation) {
            return Ok(());
        }
    }

    let manager = SelfManager::new()?;
    let executable = std::env::current_exe()?;
    if !manager.is_control_plane_executable(&executable) {
        return Ok(());
    }

    if invocation == "ovm" && is_self_management_command(args.get(1).map(String::as_str)) {
        return Ok(());
    }

    // Resolve `current` exactly once: reading the version and the manifest
    // through two separate reads of the `current` symlink could race a
    // concurrent A→B switch and pair A's manifest with B's version directory.
    let version = manager.current_version()?.ok_or_else(|| {
        OvmError::Message("no current OVM version is selected in the control plane".into())
    })?;
    let manifest = manager.load_manifest(&version)?;
    let target_name = manifest
        .side_entries()
        .find(|entry| entry.binary == invocation)
        .map(|entry| entry.binary.as_str())
        .unwrap_or("ovm");
    let target = manager.version_dir(&version).join(target_name);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new(&target);
        command
            .args(args.iter().skip(1))
            .arg0(args.first().map(String::as_str).unwrap_or("ovm"))
            .env(SELF_CHILD_ENV, "1");
        let error = command.exec();
        Err(OvmError::Message(format!(
            "failed to launch self-managed OVM at {}: {error}",
            target.display()
        )))
    }

    #[cfg(not(unix))]
    {
        let _ = target;
        Err(OvmError::Message(
            "OVM self-management requires a Unix platform".into(),
        ))
    }
}

pub fn is_self_management_command(command: Option<&str>) -> bool {
    matches!(command, Some("self" | "self-update" | "selfupdate"))
}

impl PathSnapshot {
    fn capture(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let state = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                PathState::Symlink(std::fs::read_link(&path)?)
            }
            Ok(metadata) if metadata.file_type().is_file() => PathState::File {
                contents: std::fs::read(&path)?,
                mode: metadata.permissions().mode(),
            },
            Ok(_) => {
                return Err(OvmError::Message(format!(
                    "Refusing to snapshot unsupported path at {}",
                    path.display()
                )))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => PathState::Missing,
            Err(error) => return Err(error.into()),
        };
        Ok(Self { path, state })
    }

    fn restore(&self) -> Result<()> {
        let parent = self.path.parent().ok_or_else(|| {
            OvmError::Message(format!("{} has no parent directory", self.path.display()))
        })?;
        std::fs::create_dir_all(parent)?;
        match &self.state {
            PathState::Missing => remove_file_or_symlink(&self.path),
            PathState::Symlink(target) => {
                remove_file_or_symlink(&self.path)?;
                symlink::switch_symlink(&self.path, target)
            }
            PathState::File { contents, mode } => {
                remove_symlink_if_present(&self.path)?;
                // Mode is set on the temp file BEFORE it is published: the
                // control plane must never be observable as a non-executable
                // file, even transiently or after a power loss mid-restore.
                atomic_write_with_mode(&self.path, contents, Some(*mode))
            }
        }
    }
}

fn remove_file_or_symlink(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() || metadata.file_type().is_symlink() => {
            std::fs::remove_file(path)?;
            Ok(())
        }
        Ok(_) => Err(OvmError::Message(format!(
            "Refusing to replace directory at {} while restoring OVM state",
            path.display()
        ))),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn remove_symlink_if_present(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            std::fs::remove_file(path)?;
            Ok(())
        }
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(OvmError::Message(format!(
            "Refusing to replace directory at {} while restoring OVM state",
            path.display()
        ))),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn with_recovery_error(error: OvmError, recovery: Result<()>) -> OvmError {
    match recovery {
        Ok(()) => error,
        Err(recovery_error) => OvmError::Message(format!(
            "{error}; restoring the previous OVM state also failed: {recovery_error}"
        )),
    }
}

fn validate_source_bundle(manifest: &BundleManifest, source_dir: &Path) -> Result<()> {
    for entry in manifest.entries() {
        let binary = source_dir.join(&entry.binary);
        if !binary.is_file() {
            return Err(OvmError::Message(format!(
                "Bundle source is missing {}",
                binary.display()
            )));
        }
    }
    Ok(())
}

fn validate_version_id(version: &str) -> Result<()> {
    if version.is_empty()
        || version.len() > 128
        || version.starts_with('.')
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+' | b'_'))
    {
        return Err(OvmError::Message(format!(
            "Invalid OVM self version identifier `{version}`"
        )));
    }
    Ok(())
}

fn side_names(manifest: Option<&BundleManifest>) -> HashSet<String> {
    manifest
        .into_iter()
        .flat_map(BundleManifest::side_entries)
        .map(|entry| entry.binary.clone())
        .collect()
}

fn atomic_write(destination: &Path, contents: &[u8]) -> Result<()> {
    atomic_write_with_mode(destination, contents, None)
}

fn atomic_write_with_mode(destination: &Path, contents: &[u8], mode: Option<u32>) -> Result<()> {
    let parent = destination.parent().ok_or_else(|| {
        OvmError::Message(format!("{} has no parent directory", destination.display()))
    })?;
    std::fs::create_dir_all(parent)?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    staged.write_all(contents)?;
    staged.as_file_mut().flush()?;
    staged.as_file().sync_all()?;
    if let Some(mode) = mode {
        let mut permissions = staged.as_file().metadata()?.permissions();
        permissions.set_mode(mode);
        staged.as_file().set_permissions(permissions)?;
    }
    staged.persist(destination).map_err(|error| error.error)?;
    Ok(())
}

fn atomic_copy_executable(source: &Path, destination: &Path) -> Result<()> {
    let parent = destination.parent().ok_or_else(|| {
        OvmError::Message(format!("{} has no parent directory", destination.display()))
    })?;
    std::fs::create_dir_all(parent)?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    let mut input = File::open(source)?;
    std::io::copy(&mut input, staged.as_file_mut())?;
    staged.as_file_mut().flush()?;
    staged.as_file().sync_all()?;
    util::make_executable(staged.path())?;
    staged.persist(destination).map_err(|error| error.error)?;
    Ok(())
}

fn digest_file(path: &Path) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_vec())
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use tempfile::tempdir;

    fn fixture_manifest(side_names: &[&str]) -> BundleManifest {
        let mut contents = "ovm-bundle-v1\nmain\tovm\tovm\n".to_string();
        for name in side_names {
            contents.push_str(&format!("side\t{name}\t{name}\n"));
        }
        BundleManifest::parse(&contents).unwrap()
    }

    fn fixture_source(root: &Path, manifest: &BundleManifest, marker: &str) -> PathBuf {
        let source = root.join(format!("source-{marker}"));
        std::fs::create_dir_all(&source).unwrap();
        for name in manifest.binary_names() {
            let path = source.join(name);
            std::fs::write(&path, format!("{marker}:{name}")).unwrap();
            util::make_executable(&path).unwrap();
        }
        source
    }

    fn manager(root: &Path) -> SelfManager {
        SelfManager::at(OvmDirs::at(root.join(".ovm")))
    }

    #[test]
    fn installs_immutable_complete_bundles() {
        let temp = tempdir().unwrap();
        let manager = manager(temp.path());
        let manifest = fixture_manifest(&["ovm-side"]);
        let source = fixture_source(temp.path(), &manifest, "one");

        let installed = manager
            .install_bundle("dev-one", &manifest, &source)
            .unwrap();
        assert!(installed.join(COMPLETE_MARKER).is_file());
        assert!(manager.is_complete("dev-one"));
        assert_eq!(
            manager
                .install_bundle("dev-one", &manifest, &source)
                .unwrap(),
            installed
        );

        std::fs::write(source.join("ovm"), "changed").unwrap();
        assert!(manager
            .install_bundle("dev-one", &manifest, &source)
            .is_err());
    }

    #[test]
    fn switches_and_rolls_back_dynamic_side_sets() {
        let temp = tempdir().unwrap();
        let manager = manager(temp.path());
        let first = fixture_manifest(&["ovm-alpha"]);
        let second = fixture_manifest(&["ovm-beta", "ovm-gamma"]);
        let first_source = fixture_source(temp.path(), &first, "first");
        let second_source = fixture_source(temp.path(), &second, "second");
        manager
            .install_bundle("dev-first", &first, &first_source)
            .unwrap();
        manager
            .install_bundle("dev-second", &second, &second_source)
            .unwrap();

        manager.use_version("dev-first").unwrap();
        assert_eq!(
            manager.current_version().unwrap().as_deref(),
            Some("dev-first")
        );
        assert!(manager.ovm_dirs.bin.join("ovm-alpha").is_symlink());

        manager.use_version("dev-second").unwrap();
        assert_eq!(
            manager.current_version().unwrap().as_deref(),
            Some("dev-second")
        );
        assert_eq!(
            manager.previous_version().unwrap().as_deref(),
            Some("dev-first")
        );
        assert!(!manager.ovm_dirs.bin.join("ovm-alpha").exists());
        assert!(manager.ovm_dirs.bin.join("ovm-beta").is_symlink());
        assert!(manager.ovm_dirs.bin.join("ovm-gamma").is_symlink());

        assert_eq!(manager.rollback().unwrap(), "dev-first");
        assert_eq!(
            manager.current_version().unwrap().as_deref(),
            Some("dev-first")
        );
        assert_eq!(
            manager.previous_version().unwrap().as_deref(),
            Some("dev-second")
        );
    }

    #[test]
    fn preserves_foreign_side_paths() {
        let temp = tempdir().unwrap();
        let manager = manager(temp.path());
        let manifest = fixture_manifest(&["ovm-first", "ovm-side"]);
        let source = fixture_source(temp.path(), &manifest, "one");
        manager
            .install_bundle("dev-one", &manifest, &source)
            .unwrap();
        std::fs::create_dir_all(&manager.ovm_dirs.bin).unwrap();
        let foreign = manager.ovm_dirs.bin.join("ovm-side");
        std::fs::write(&foreign, "foreign").unwrap();

        assert!(manager.use_version("dev-one").is_err());
        assert_eq!(std::fs::read_to_string(foreign).unwrap(), "foreign");
        assert!(!manager.ovm_dirs.bin.join("ovm-first").exists());
        assert!(manager.current_version().unwrap().is_none());
    }

    #[test]
    fn preserves_unreadable_foreign_product_launchers() {
        let temp = tempdir().unwrap();
        let manager = manager(temp.path());
        let manifest = fixture_manifest(&[]);
        let source = fixture_source(temp.path(), &manifest, "foreign-product");
        manager
            .install_bundle("foreign-product", &manifest, &source)
            .unwrap();
        std::fs::create_dir_all(&manager.ovm_dirs.bin).unwrap();
        let codex = manager.ovm_dirs.bin.join("codex");
        std::fs::write(&codex, "foreign").unwrap();
        let mut permissions = std::fs::metadata(&codex).unwrap().permissions();
        permissions.set_mode(0o111);
        std::fs::set_permissions(&codex, permissions).unwrap();

        manager.use_version("foreign-product").unwrap();
        assert!(codex.is_file());
        assert!(!codex.is_symlink());
    }

    #[test]
    fn canonicalizes_symlinked_ovm_home_for_launcher_reconciliation() {
        let temp = tempdir().unwrap();
        let physical = temp.path().join("physical-state");
        let logical = temp.path().join("home/.ovm");
        std::fs::create_dir_all(logical.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&physical).unwrap();
        symlink(&physical, &logical).unwrap();
        let manager = SelfManager::at(OvmDirs::at(logical));
        let manifest = fixture_manifest(&[]);
        let source = fixture_source(temp.path(), &manifest, "canonical");
        manager
            .install_bundle("canonical", &manifest, &source)
            .unwrap();
        manager.refresh_control_plane("canonical").unwrap();
        manager.use_version("canonical").unwrap();

        let physical_binary =
            std::fs::canonicalize(manager.version_dir("canonical").join("ovm")).unwrap();
        assert!(manager.is_managed_version_executable(&physical_binary));
        let codex = manager.ovm_dirs.bin.join("codex");
        symlink(&physical_binary, &codex).unwrap();
        manager.use_version("canonical").unwrap();
        assert_eq!(
            std::fs::read_link(codex).unwrap(),
            manager.control_plane_path()
        );
    }

    #[test]
    fn pointer_conflict_does_not_publish_new_side_links() {
        let temp = tempdir().unwrap();
        let manager = manager(temp.path());
        let first = fixture_manifest(&[]);
        let second = fixture_manifest(&["ovm-new-side"]);
        let first_source = fixture_source(temp.path(), &first, "first-pointer");
        let second_source = fixture_source(temp.path(), &second, "second-pointer");
        manager
            .install_bundle("first", &first, &first_source)
            .unwrap();
        manager
            .install_bundle("second", &second, &second_source)
            .unwrap();
        manager.use_version("first").unwrap();
        std::fs::create_dir_all(&manager.dirs.previous).unwrap();

        assert!(manager.use_version("second").is_err());
        assert_eq!(manager.current_version().unwrap().as_deref(), Some("first"));
        assert!(!manager.ovm_dirs.bin.join("ovm-new-side").exists());
    }

    #[test]
    fn corrupt_side_link_record_falls_back_to_the_current_manifest() {
        let temp = tempdir().unwrap();
        let manager = manager(temp.path());
        let first = fixture_manifest(&["ovm-beta", "ovm-gamma"]);
        let second = fixture_manifest(&[]);
        let first_source = fixture_source(temp.path(), &first, "record-first");
        let second_source = fixture_source(temp.path(), &second, "record-second");
        manager
            .install_bundle("first", &first, &first_source)
            .unwrap();
        manager
            .install_bundle("second", &second, &second_source)
            .unwrap();
        manager.use_version("first").unwrap();
        std::fs::write(&manager.dirs.side_links_file, "ovm-beta\novm-beta\n").unwrap();

        manager.use_version("second").unwrap();
        assert!(!manager.ovm_dirs.bin.join("ovm-beta").exists());
        assert!(!manager.ovm_dirs.bin.join("ovm-gamma").exists());
        assert_eq!(
            std::fs::read_to_string(&manager.dirs.side_links_file).unwrap(),
            ""
        );
    }

    #[test]
    fn rollback_escapes_a_corrupt_current_manifest() {
        let temp = tempdir().unwrap();
        let manager = manager(temp.path());
        let good_manifest = fixture_manifest(&[]);
        let broken_manifest = fixture_manifest(&["ovm-old-side"]);
        let good_source = fixture_source(temp.path(), &good_manifest, "good");
        let broken_source = fixture_source(temp.path(), &broken_manifest, "broken");
        manager
            .install_bundle("good", &good_manifest, &good_source)
            .unwrap();
        manager
            .install_bundle("broken", &broken_manifest, &broken_source)
            .unwrap();
        manager.use_version("good").unwrap();
        manager.use_version("broken").unwrap();
        assert!(manager.ovm_dirs.bin.join("ovm-old-side").is_symlink());
        std::fs::write(
            manager.version_dir("broken").join(BUNDLE_MANIFEST_NAME),
            "corrupt",
        )
        .unwrap();

        assert_eq!(manager.rollback().unwrap(), "good");
        assert_eq!(manager.current_version().unwrap().as_deref(), Some("good"));
        assert!(!manager.ovm_dirs.bin.join("ovm-old-side").exists());
    }

    #[test]
    fn refreshes_control_plane_as_executable_regular_file() {
        let temp = tempdir().unwrap();
        let manager = manager(temp.path());
        let manifest = fixture_manifest(&[]);
        let source = fixture_source(temp.path(), &manifest, "one");
        manager
            .install_bundle("dev-one", &manifest, &source)
            .unwrap();
        manager.ensure_dirs().unwrap();
        let control = manager.control_plane_path();
        std::fs::write(&control, "old-control").unwrap();
        util::make_executable(&control).unwrap();
        manager.refresh_control_plane("dev-one").unwrap();

        assert!(control.is_file());
        assert!(!control.is_symlink());
        assert_ne!(
            std::fs::metadata(&control).unwrap().permissions().mode() & 0o111,
            0
        );
        assert_eq!(std::fs::read_to_string(&control).unwrap(), "one:ovm");
        manager.repair_control_plane().unwrap();
        assert_eq!(std::fs::read_to_string(control).unwrap(), "old-control");
    }

    #[test]
    fn prunes_only_inactive_dev_versions() {
        let temp = tempdir().unwrap();
        let manager = manager(temp.path());
        let manifest = fixture_manifest(&[]);
        for version in ["dev-one", "dev-two", "dev-three", "0.1.0"] {
            let source = fixture_source(temp.path(), &manifest, version);
            manager.install_bundle(version, &manifest, &source).unwrap();
        }
        manager.use_version("dev-one").unwrap();
        manager.use_version("dev-two").unwrap();

        assert_eq!(
            manager.prune_inactive_dev_versions().unwrap(),
            vec!["dev-three"]
        );
        assert!(manager.is_complete("dev-one"));
        assert!(manager.is_complete("dev-two"));
        assert!(manager.is_complete("0.1.0"));
    }

    #[test]
    fn restores_an_exact_selection_snapshot() {
        let temp = tempdir().unwrap();
        let manager = manager(temp.path());
        let first = fixture_manifest(&["ovm-old-side"]);
        let second = fixture_manifest(&["ovm-new-side"]);
        let first_source = fixture_source(temp.path(), &first, "snapshot-first");
        let second_source = fixture_source(temp.path(), &second, "snapshot-second");
        manager
            .install_bundle("first", &first, &first_source)
            .unwrap();
        manager
            .install_bundle("second", &second, &second_source)
            .unwrap();
        manager.refresh_control_plane("first").unwrap();
        manager.use_version("first").unwrap();
        let codex = manager.ovm_dirs.bin.join("codex");
        let original_codex = manager.version_dir("first").join("ovm");
        symlink(&original_codex, &codex).unwrap();

        let snapshot = manager.snapshot_selection("second").unwrap();
        manager.use_version("second").unwrap();
        assert_eq!(
            manager.current_version().unwrap().as_deref(),
            Some("second")
        );
        assert!(manager.ovm_dirs.bin.join("ovm-new-side").is_symlink());

        manager.restore_selection(&snapshot).unwrap();
        assert_eq!(manager.current_version().unwrap().as_deref(), Some("first"));
        assert!(manager.previous_version().unwrap().is_none());
        assert!(manager.ovm_dirs.bin.join("ovm-old-side").is_symlink());
        assert!(!manager.ovm_dirs.bin.join("ovm-new-side").exists());
        assert_eq!(std::fs::read_link(&codex).unwrap(), original_codex);
        assert_eq!(
            std::fs::read_to_string(&manager.dirs.side_links_file).unwrap(),
            "ovm-old-side\n"
        );
    }

    #[test]
    fn serializes_self_management_operations() {
        let temp = tempdir().unwrap();
        let manager = manager(temp.path());
        let operation = manager.acquire_operation_lock().unwrap();
        let contender = manager.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let lock = contender.acquire_operation_lock().unwrap();
            sender.send(()).unwrap();
            drop(lock);
        });

        assert!(receiver.recv_timeout(Duration::from_millis(150)).is_err());
        drop(operation);
        receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        thread.join().unwrap();
        assert!(manager.dirs.operation_lock.is_file());
    }

    #[test]
    fn identifies_self_management_commands() {
        assert!(is_self_management_command(Some("self")));
        assert!(is_self_management_command(Some("self-update")));
        assert!(is_self_management_command(Some("selfupdate")));
        assert!(!is_self_management_command(Some("list")));
        assert!(!is_self_management_command(None));
    }

    #[test]
    fn child_marker_only_runs_the_main_binary_directly() {
        // The main `ovm` binary carrying the marker runs directly (it IS the
        // selected version the control plane exec'd).
        assert!(child_marker_runs_directly("ovm"));
        // Side-binary invocations reach the control plane via their
        // ~/.ovm/bin symlink; the control-plane `ovm` can't serve as them, so
        // they must be dispatched to their real versioned binary rather than
        // run directly (regression: `ovm ccxy` / the claudex session hook
        // fell through to the CLI parser and errored).
        assert!(!child_marker_runs_directly("ovm-claudex"));
        assert!(!child_marker_runs_directly("ovm-codex-skew"));
    }
}

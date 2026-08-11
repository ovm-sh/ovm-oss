//! `ovm self uninstall` — the supported way back out.
//!
//! `install.sh` edits shell profiles, drops a control plane and side-binary
//! shims in `~/.ovm/bin`, and keeps immutable snapshots under `~/.ovm/self`.
//! Until this existed there was no removal path for any of it, so leaving OVM
//! meant hand-editing an rc file most people never knew had been touched.
//!
//! Four rules shape the command:
//!
//! 1. **Only ever remove what OVM wrote.** The PATH block is matched by the
//!    exact markers the installer writes and nothing else in the profile is
//!    rewritten; shims are removed only when they are still symlinks OVM owns,
//!    and the control plane only once it proves to be one of ours.
//! 2. **Product installs are not OVM.** Uninstalling the manager must not
//!    delete the Claude/Codex/Pi versions it installed, or a reinstall starts
//!    from an empty store. `--purge` is the explicit way to take those too.
//! 3. **Nothing recursive leaves the tree.** A symlinked `~/.ovm` is refused:
//!    following it makes a mistaken link capable of deleting an unrelated tree.
//!    Real directories are checked for identity and mount boundaries, renamed
//!    to a quarantine entry, then deleted. See [`Boundary`].
//! 4. **A path we could not read is not a path that was clean.** Scan errors
//!    become failures and a non-zero exit, never a quiet omission from the plan.

use crate::bundle_manifest::BundleManifest;
use crate::error::{OvmError, Result};
use crate::product::Product;
use crate::self_manager::{ControlPlaneOwnership, SelfManager};
use console::{style, Term};
use std::collections::BTreeSet;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

/// The exact markers `install.sh` wraps its PATH line in. Matching these — not
/// "any line mentioning .ovm/bin" — is what makes removal safe: a line the user
/// wrote themselves is not ours to delete.
const PATH_BLOCK_BEGIN: &str = "# >>> ovm >>>";
const PATH_BLOCK_END: &str = "# <<< ovm <<<";

/// Typed, not `[y/N]`: this removes an install, and a stray keypress should not
/// be able to answer it.
const CONFIRM_WORD: &str = "uninstall";

pub fn run(purge: bool, assume_yes: bool) -> Result<()> {
    let manager = SelfManager::new()?;
    let home = dirs::home_dir()
        .ok_or_else(|| OvmError::Message("Cannot determine home directory".into()))?;
    let zdotdir = std::env::var_os("ZDOTDIR").map(PathBuf::from);
    let plan = Plan::build(&manager, home, zdotdir.as_deref(), purge);

    if let Some(target) = &plan.base_link {
        return Err(OvmError::Message(format!(
            "Refusing to uninstall through symlinked {} -> {}. Remove the link and its target manually after verifying the target is OVM's directory.",
            plan.base.display(),
            target.display()
        )));
    }

    plan.describe();
    if !assume_yes && !confirm(purge)? {
        println!("Cancelled — nothing was removed.");
        return Ok(());
    }

    // Hold the self-operation lock for the destructive phase only, so a
    // concurrent `ovm self update` cannot be swapping the very files we delete.
    // Taking it would CREATE `~/.ovm/self` to hold the lock file, so it is only
    // taken when that directory already exists — an uninstall on a machine with
    // no install must not build the tree it is about to report as removed.
    let _operation = plan
        .self_root
        .is_some()
        .then(|| manager.acquire_operation_lock())
        .transpose()?;
    plan.revalidate_managed_entries(&manager)?;
    let report = plan.execute(&manager);
    report.print();

    if report.failures.is_empty() {
        crate::mochi::say(
            crate::mochi::HAPPY,
            &format!(
                "{} OVM uninstalled. Thanks for the fish.",
                style("✓").green()
            ),
        );
        return Ok(());
    }
    Err(OvmError::Message(format!(
        "OVM was only partially uninstalled — {} item(s) could not be removed (listed above).",
        report.failures.len()
    )))
}

/// Everything the command intends to touch, resolved once so the preview the
/// user confirms is the same set the removal walks.
struct Plan {
    home: PathBuf,
    purge: bool,
    /// Profiles that actually carry (or might carry) our block.
    profiles: Vec<PathBuf>,
    side_links: Vec<PathBuf>,
    product_launchers: Vec<PathBuf>,
    control_plane: Option<PathBuf>,
    /// An `ovm` at the recorded launcher path that OVM did not write. Named in
    /// the preview and in the report so its survival is a stated outcome
    /// rather than a silent one.
    foreign_control: Option<PathBuf>,
    self_root: Option<PathBuf>,
    /// `~/.ovm/claudex`, when the claudex plugin has state there. Never a
    /// removal target of its own — it is named in the preview because `--purge`
    /// takes it along with the tree, and it holds conversation history.
    claudex: Option<PathBuf>,
    /// Paths the plan could not even look at. Kept because "I could not tell"
    /// is not "it is not there": an unsearchable `~/.ovm/bin` would otherwise
    /// render as a clean uninstall while its executables stay on `$PATH`.
    unscannable: Vec<(PathBuf, String)>,
    base: PathBuf,
    /// Where `~/.ovm` actually is, when it is a symlink — an install kept on
    /// external storage. Shown in the preview, because "removes ~/.ovm" hides
    /// which disk the bytes are on.
    base_link: Option<PathBuf>,
    /// Confines every recursive deletion to the resolved `~/.ovm`.
    boundary: Boundary,
    bin: PathBuf,
}

impl Plan {
    fn build(manager: &SelfManager, home: PathBuf, zdotdir: Option<&Path>, purge: bool) -> Self {
        let profiles = profile_candidates(&home, zdotdir)
            .into_iter()
            .filter(|path| profile_may_hold_block(path))
            .collect();
        let mut unscannable = Vec::new();

        let launcher_dir = manager.launcher_dir();
        let mut side_links = Vec::new();
        for name in managed_side_names(manager) {
            let link = launcher_dir.join(name);
            match inspect(&link) {
                Ok(None) => {}
                Ok(Some(metadata)) => {
                    if metadata.file_type().is_symlink() && manager.is_managed_side_link(&link) {
                        side_links.push(link);
                    }
                }
                Err(error) => unscannable.push((link, error.to_string())),
            }
        }

        let mut product_launchers = Vec::new();
        for product in Product::ALL {
            let launcher = manager.ovm_dirs.bin.join(product.binary_name());
            match inspect(&launcher) {
                Ok(None) => {}
                Ok(Some(_)) => match manager.product_launcher_is_managed(&launcher) {
                    Ok(true) => product_launchers.push(launcher),
                    Ok(false) => {}
                    Err(error) => unscannable.push((launcher, error.to_string())),
                },
                Err(error) => unscannable.push((launcher, error.to_string())),
            }
        }

        // The control plane is a plain executable (a symlink under the retired
        // checkout workflow), and its path comes from a file OVM recorded — so
        // it is removed only once it has been shown to be OVM's. A *directory*
        // there, or somebody else's `ovm`, is never a deletion candidate.
        let control = manager.control_plane_path();
        let mut foreign_control = None;
        let control_plane = match manager.control_plane_ownership() {
            Ok(ControlPlaneOwnership::Managed) => Some(control),
            Ok(ControlPlaneOwnership::Absent) => None,
            Ok(ControlPlaneOwnership::Foreign) => {
                foreign_control = Some(control);
                None
            }
            Err(error) => {
                unscannable.push((control, error.to_string()));
                None
            }
        };

        let root = manager.dirs.root.clone();
        let self_root = match inspect(&root) {
            Ok(Some(metadata))
                if metadata.file_type().is_dir() || metadata.file_type().is_symlink() =>
            {
                Some(root)
            }
            Ok(_) => None,
            Err(error) => {
                unscannable.push((root, error.to_string()));
                None
            }
        };

        let claudex = manager.ovm_dirs.base.join("claudex");
        // An unreadable claudex directory is still named in the preview: with
        // `--purge` it is about to be deleted either way.
        let claudex = (!matches!(inspect(&claudex), Ok(None))).then_some(claudex);

        let base = manager.ovm_dirs.base.clone();
        let base_link = base
            .is_symlink()
            .then(|| std::fs::canonicalize(&base).ok())
            .flatten();
        let mut boundary = Boundary::at(&base, self_root.as_deref());
        for path in side_links
            .iter()
            .chain(&product_launchers)
            .chain(control_plane.iter())
        {
            boundary.remember(path);
        }

        Self {
            purge,
            profiles,
            side_links,
            product_launchers,
            control_plane,
            foreign_control,
            self_root,
            claudex,
            unscannable,
            boundary,
            base_link,
            base,
            bin: manager.ovm_dirs.bin.clone(),
            home,
        }
    }

    fn describe(&self) {
        if let Some(target) = &self.base_link {
            println!(
                "{} points at {} — the contents at the target are what gets removed.",
                tilde(&self.home, &self.base),
                target.display()
            );
        }
        println!("Uninstalling OVM removes:");
        if self.profiles.is_empty() {
            println!("  the OVM PATH block from your shell profiles (none found)");
        } else {
            for profile in &self.profiles {
                println!("  the OVM PATH block in {}", tilde(&self.home, profile));
            }
        }
        for path in self
            .side_links
            .iter()
            .chain(&self.product_launchers)
            .chain(self.control_plane.iter())
        {
            println!("  {}", tilde(&self.home, path));
        }
        if let Some(root) = &self.self_root {
            println!("  {} (OVM's own snapshots)", tilde(&self.home, root));
        }

        if self.purge {
            println!(
                "  {} — {}",
                tilde(&self.home, &self.base),
                style("the whole tree, including every installed product version and your config")
                    .yellow()
            );
            // Named separately because "the whole tree" does not read like
            // "your chat history". claudex keeps an isolated Claude home in
            // there, and its sessions exist nowhere else — not in ~/.claude.
            if let Some(claudex) = &self.claudex {
                println!(
                    "  {} — {}",
                    tilde(&self.home, claudex),
                    style(
                        "claudex's own Claude home: every claudex session and its history, \
                         plus the proxy's auth and config. Nothing restores it."
                    )
                    .yellow()
                );
            }
        } else {
            println!("and keeps:");
            println!(
                "  {} (installed Claude/Codex/Pi versions)",
                tilde(&self.home, &self.base.join("products"))
            );
            println!(
                "  {} (your settings)",
                tilde(&self.home, &self.base.join("config.json"))
            );
            if let Some(claudex) = &self.claudex {
                println!(
                    "  {} (claudex sessions, history, and proxy auth)",
                    tilde(&self.home, claudex)
                );
            }
            println!(
                "Use {} to remove those too.",
                style("ovm self uninstall --purge").cyan()
            );
        }

        if let Some(control) = &self.foreign_control {
            println!(
                "and leaves {} alone — that `ovm` is not one OVM wrote.",
                tilde(&self.home, control)
            );
        }

        if !self.unscannable.is_empty() {
            println!("and cannot look at (they are reported, never removed blind):");
            for (path, error) in &self.unscannable {
                println!("  {} — {error}", tilde(&self.home, path));
            }
        }
    }

    /// Remove everything the plan named, continuing past failures so one
    /// unwritable profile cannot strand the rest of the install on disk.
    ///
    /// Order is deliberate: OVM's own snapshots go before the control plane, so
    /// an interrupted run still leaves a working `ovm` to re-run the command
    /// with. The launcher directory is only removed once it is empty — anything
    /// a user put there is theirs.
    fn execute(&self, manager: &SelfManager) -> Report {
        let mut report = Report::default();

        // Anything the plan could not inspect fails the run. Exiting zero here
        // would tell the user OVM is gone while an executable it cannot see
        // sits in a directory that is still on their `$PATH`.
        for (path, error) in &self.unscannable {
            report.failed(format!(
                "{}: could not be inspected ({error}), so it was left alone — \
                 check it by hand",
                tilde(&self.home, path)
            ));
        }

        if let Some(control) = &self.foreign_control {
            report.kept(format!(
                "{} — not OVM's, left in place",
                tilde(&self.home, control)
            ));
        }

        for profile in &self.profiles {
            match remove_path_block(profile) {
                Ok(ProfileOutcome::Absent) => {}
                Ok(ProfileOutcome::Removed) => {
                    report.removed(format!("PATH block in {}", tilde(&self.home, profile)));
                }
                Ok(ProfileOutcome::FileRemoved) => {
                    report.removed(tilde(&self.home, profile));
                }
                Ok(ProfileOutcome::Unterminated) => report.failed(format!(
                    "{}: `{PATH_BLOCK_BEGIN}` has no closing `{PATH_BLOCK_END}` line — \
                     remove those lines by hand",
                    tilde(&self.home, profile)
                )),
                Ok(ProfileOutcome::NotUtf8) => report.failed(format!(
                    "{}: not valid UTF-8, so the OVM block could not be removed safely — \
                     remove it by hand",
                    tilde(&self.home, profile)
                )),
                Err(error) => {
                    report.failed(format!("{}: {error}", tilde(&self.home, profile)));
                }
            }
        }

        for path in &self.side_links {
            if matches!(inspect(path), Ok(Some(metadata)) if metadata.file_type().is_symlink())
                && manager.is_managed_side_link(path)
            {
                self.attempt(&mut report, path, Recursion::None);
            } else {
                report.failed(format!(
                    "{} changed after confirmation and is no longer an OVM-managed side link",
                    tilde(&self.home, path)
                ));
            }
        }
        for path in &self.product_launchers {
            match manager.product_launcher_is_managed(path) {
                Ok(true) => self.attempt(&mut report, path, Recursion::None),
                Ok(false) => report.failed(format!(
                    "{} changed after confirmation and is no longer an OVM-managed launcher",
                    tilde(&self.home, path)
                )),
                Err(error) => report.failed(format!("{}: {error}", tilde(&self.home, path))),
            }
        }
        if let Some(path) = &self.control_plane {
            match manager.control_plane_ownership() {
                Ok(ControlPlaneOwnership::Managed) => {
                    self.attempt(&mut report, path, Recursion::None)
                }
                Ok(_) => report.failed(format!(
                    "{} changed after confirmation and is no longer OVM's control plane",
                    tilde(&self.home, path)
                )),
                Err(error) => report.failed(format!("{}: {error}", tilde(&self.home, path))),
            }
        }
        if let Some(root) = &self.self_root {
            self.attempt(&mut report, root, Recursion::Snapshots);
        }

        if self.purge {
            self.attempt(&mut report, &self.base, Recursion::OvmTree);
        } else if is_empty_dir(&self.bin) {
            self.attempt(&mut report, &self.bin, Recursion::EmptyOnly);
        }

        report
    }

    fn revalidate_managed_entries(&self, manager: &SelfManager) -> Result<()> {
        for path in &self.side_links {
            let still_managed = matches!(inspect(path), Ok(Some(metadata)) if metadata.file_type().is_symlink())
                && manager.is_managed_side_link(path);
            if !still_managed {
                return Err(OvmError::Message(format!(
                    "{} changed after confirmation; refusing to remove anything",
                    path.display()
                )));
            }
        }
        for path in &self.product_launchers {
            if !manager.product_launcher_is_managed(path)? {
                return Err(OvmError::Message(format!(
                    "{} changed after confirmation; refusing to remove anything",
                    path.display()
                )));
            }
        }
        if self.control_plane.is_some()
            && manager.control_plane_ownership()? != ControlPlaneOwnership::Managed
        {
            return Err(OvmError::Message(
                "OVM's control plane changed after confirmation; refusing to remove anything"
                    .into(),
            ));
        }
        Ok(())
    }

    fn attempt(&self, report: &mut Report, path: &Path, recursion: Recursion) {
        match remove_entry(path, &self.boundary, recursion) {
            Ok(true) => report.removed(tilde(&self.home, path)),
            Ok(false) => {}
            Err(error) => report.failed(format!("{}: {error}", tilde(&self.home, path))),
        }
    }
}

#[derive(Default)]
struct Report {
    removed: Vec<String>,
    /// Things found at OVM's paths that turned out not to be OVM's. Not a
    /// failure — a decision, and one worth saying out loud.
    kept: Vec<String>,
    failures: Vec<String>,
}

impl Report {
    fn removed(&mut self, label: String) {
        self.removed.push(label);
    }

    fn kept(&mut self, label: String) {
        self.kept.push(label);
    }

    fn failed(&mut self, message: String) {
        self.failures.push(message);
    }

    fn print(&self) {
        if self.removed.is_empty() {
            println!("Nothing left to remove — OVM was not installed here.");
        } else {
            println!("Removed:");
            for item in &self.removed {
                println!("  {} {item}", style("-").dim());
            }
        }
        if !self.kept.is_empty() {
            println!("Left in place:");
            for item in &self.kept {
                println!("  {} {item}", style("=").dim());
            }
        }
        if !self.failures.is_empty() {
            println!("Could not remove:");
            for failure in &self.failures {
                println!("  {} {failure}", style("!").yellow());
            }
        }
        println!(
            "Reinstall any time: {}",
            style("curl -fsSL https://ovm.sh/install | sh").cyan()
        );
    }
}

/// Ask before deleting. A non-interactive shell can never answer, so it is told
/// how to proceed rather than being blocked on a prompt nobody will ever see.
fn confirm(purge: bool) -> Result<bool> {
    let flags = if purge { " --purge" } else { "" };
    if !Term::stderr().is_term() || !std::io::stdin().is_terminal() {
        return Err(OvmError::Message(format!(
            "Refusing to uninstall OVM without confirmation: this shell is not interactive. \
             Re-run `ovm self uninstall{flags}` in a terminal, or pass \
             `ovm self uninstall{flags} --yes`."
        )));
    }

    eprint!(
        "  {} Type {} to confirm: ",
        style("?").yellow().bold(),
        style(CONFIRM_WORD).bold()
    );
    let _ = std::io::stderr().flush();

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim() == CONFIRM_WORD)
}

/// Every profile `install.sh` is capable of writing its block into, across all
/// four shell branches. The installer picks one branch from `$SHELL`, but the
/// shell a person uninstalls from is not always the one they installed from —
/// scanning the whole set is what makes "it left a line in my .bashrc" a state
/// this command can actually fix.
fn profile_candidates(home: &Path, zdotdir: Option<&Path>) -> Vec<PathBuf> {
    let zdotdir = zdotdir.unwrap_or(home);
    let mut candidates = vec![
        zdotdir.join(".zshrc"),
        zdotdir.join(".zprofile"),
        home.join(".zshrc"),
        home.join(".zprofile"),
        home.join(".bashrc"),
        home.join(".bash_profile"),
        home.join(".config/fish/conf.d/ovm.fish"),
        home.join(".profile"),
    ];
    let mut seen = BTreeSet::new();
    candidates.retain(|path| seen.insert(path.clone()));
    candidates
}

/// Cheap pre-filter for the preview. Anything unreadable is kept as a candidate
/// so the removal pass reports it instead of silently skipping it.
fn profile_may_hold_block(path: &Path) -> bool {
    match std::fs::read(path) {
        Ok(bytes) => contains_marker(&bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

fn contains_marker(bytes: &[u8]) -> bool {
    bytes
        .windows(PATH_BLOCK_BEGIN.len())
        .any(|window| window == PATH_BLOCK_BEGIN.as_bytes())
}

#[derive(Debug, PartialEq, Eq)]
enum ProfileOutcome {
    Absent,
    Removed,
    /// The installer-owned fish snippet held nothing but our block.
    FileRemoved,
    Unterminated,
    NotUtf8,
}

fn remove_path_block(path: &Path) -> Result<ProfileOutcome> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ProfileOutcome::Absent)
        }
        Err(error) => return Err(error.into()),
    };
    let contents = match String::from_utf8(bytes) {
        Ok(contents) => contents,
        // A profile we cannot decode is one we must not rewrite — rewriting it
        // would corrupt the bytes we did not understand. Report it only when our
        // marker is actually in there; otherwise it is simply not our file.
        Err(error) if contains_marker(error.as_bytes()) => return Ok(ProfileOutcome::NotUtf8),
        Err(_) => return Ok(ProfileOutcome::Absent),
    };

    match strip_path_block(&contents) {
        BlockStrip::Absent => Ok(ProfileOutcome::Absent),
        BlockStrip::Unterminated => Ok(ProfileOutcome::Unterminated),
        BlockStrip::Removed(rest) => {
            // `conf.d/ovm.fish` exists only because the installer created it for
            // the block; an emptied one is litter, not a user file. Every other
            // profile keeps its identity: the file the path resolves to is
            // replaced atomically, so a `.zshrc` symlinked into a dotfiles repo
            // still points at the same file afterwards, with the same mode.
            if rest.trim().is_empty() && path.file_name() == Some(std::ffi::OsStr::new("ovm.fish"))
            {
                std::fs::remove_file(path)?;
                return Ok(ProfileOutcome::FileRemoved);
            }
            rewrite_profile(path, &rest)?;
            Ok(ProfileOutcome::Removed)
        }
    }
}

/// Replace a shell profile with `contents` without ever leaving a truncated
/// one behind: the new text is staged beside the real file and renamed over it,
/// so a failure mid-write loses the temp file rather than the user's config.
///
/// Symlinked profiles (a `.zshrc` linked into a dotfiles repo) are followed on
/// purpose — the rename lands on the *target*, next to which the temp file was
/// staged so the two are on one filesystem. The link itself is never touched,
/// and the target's mode is carried over: a fresh temp file is `0600`, and a
/// profile silently losing its read bits is not a change an uninstall may make.
fn rewrite_profile(path: &Path, contents: &str) -> Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let parent = target.parent().ok_or_else(|| {
        OvmError::Message(format!("{} has no parent directory", target.display()))
    })?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    staged.write_all(contents.as_bytes())?;
    staged.as_file_mut().flush()?;
    staged.as_file().sync_all()?;
    if let Ok(metadata) = std::fs::metadata(&target) {
        staged.as_file().set_permissions(metadata.permissions())?;
    }
    staged.persist(&target).map_err(|error| error.error)?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum BlockStrip {
    Absent,
    Removed(String),
    /// A begin marker with no end marker after it. Guessing where the block
    /// stops could delete the rest of someone's shell config, so we refuse.
    Unterminated,
}

/// Remove every complete `>>> ovm >>>` … `<<< ovm <<<` block, plus the single
/// blank line the installer prints before it. Every other byte is preserved.
fn strip_path_block(contents: &str) -> BlockStrip {
    let lines = contents.split_inclusive('\n').collect::<Vec<_>>();
    let mut kept = String::with_capacity(contents.len());
    let mut index = 0;
    let mut removed_any = false;

    while index < lines.len() {
        if lines[index].trim() != PATH_BLOCK_BEGIN {
            kept.push_str(lines[index]);
            index += 1;
            continue;
        }
        // A second begin marker before the end one means the file has an
        // unterminated block in it. Pairing this begin with a *later* block's
        // end would swallow everything in between — which is the user's own
        // config, not ours. Refuse the whole file instead.
        let mut end = None;
        for (offset, line) in lines[index + 1..].iter().enumerate() {
            match line.trim() {
                PATH_BLOCK_BEGIN => return BlockStrip::Unterminated,
                PATH_BLOCK_END => {
                    end = Some(index + 1 + offset);
                    break;
                }
                _ => {}
            }
        }
        let Some(end) = end else {
            return BlockStrip::Unterminated;
        };
        // The installer writes "\n" before the block; drop that one separator so
        // repeated install/uninstall cycles cannot accrete blank lines. `== "\n"`
        // catches the block sitting at the very top of the file, where the
        // separator is the whole of what precedes it.
        if kept == "\n" || kept.ends_with("\n\n") {
            kept.truncate(kept.len() - 1);
        }
        removed_any = true;
        index = end + 1;
    }

    if removed_any {
        BlockStrip::Removed(kept)
    } else {
        BlockStrip::Absent
    }
}

/// Side-binary names OVM may have shimmed into the launcher directory: what the
/// active bundle declares, what the installer recorded, and what this build
/// ships. The union matters because a shim left by a *previous* bundle is
/// exactly the leftover an uninstall exists to clear.
fn managed_side_names(manager: &SelfManager) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    if let Ok(manifest) = BundleManifest::embedded() {
        names.extend(manifest.side_entries().map(|entry| entry.binary.clone()));
    }
    if let Ok(Some(version)) = manager.current_version() {
        if let Ok(manifest) = manager.load_manifest(&version) {
            names.extend(manifest.side_entries().map(|entry| entry.binary.clone()));
        }
    }
    if let Some(recorded) = manager.read_managed_side_links() {
        names.extend(recorded);
    }
    names
}

/// Look at `path` without following it. `Ok(None)` is the one clean way to be
/// absent; every other error is returned rather than folded into "not there",
/// because a plan that cannot see a path has no business calling it removed.
fn inspect(path: &Path) -> std::io::Result<Option<std::fs::Metadata>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn is_empty_dir(path: &Path) -> bool {
    std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none())
}

/// How much of a directory a removal is allowed to take, and what that
/// directory has to look like first. `remove_dir_all` walks wherever the path
/// leads it, so nothing recursive happens without one of these saying so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recursion {
    /// A file or a symlink. A directory here is a surprise, and refused.
    None,
    /// `~/.ovm` itself, under `--purge`.
    OvmTree,
    /// `~/.ovm/self` — the snapshot store.
    Snapshots,
    /// The launcher directory: removable once empty, never recursively.
    EmptyOnly,
}

impl Recursion {
    fn recurses(self) -> bool {
        matches!(self, Self::OvmTree | Self::Snapshots)
    }

    /// Names that prove the directory is the one we think it is. Deleting a
    /// tree recursively on the strength of its *path* alone is how a wrong
    /// `~/.ovm` symlink turns into somebody's home directory.
    fn markers(self) -> &'static [&'static str] {
        match self {
            Self::OvmTree => &["self", "products", "bin", "config.json", "hooks", "claudex"],
            Self::Snapshots => &[
                "versions",
                "current",
                "previous",
                "launcher-dir",
                "side-links",
                "control-previous",
            ],
            Self::None | Self::EmptyOnly => &[],
        }
    }
}

/// Where recursive deletion may reach: inside `~/.ovm`, and nowhere else.
///
/// The base is canonicalized only to confine descendants. The root itself must
/// be a real directory; recursive symlink targets are never followed.
struct Boundary {
    base: Option<PathBuf>,
    planned: Vec<(PathBuf, std::fs::Metadata)>,
}

#[derive(Debug)]
struct AuthorizedDirectory {
    metadata: std::fs::Metadata,
}

impl Boundary {
    fn at(base: &Path, self_root: Option<&Path>) -> Self {
        let mut planned = Vec::new();
        if let Ok(metadata) = std::fs::symlink_metadata(base) {
            planned.push((base.to_path_buf(), metadata));
        }
        if let Some(self_root) = self_root {
            if let Ok(metadata) = std::fs::symlink_metadata(self_root) {
                planned.push((self_root.to_path_buf(), metadata));
            }
        }
        Self {
            base: std::fs::canonicalize(base).ok(),
            planned,
        }
    }

    fn remember(&mut self, path: &Path) {
        if let Ok(metadata) = std::fs::symlink_metadata(path) {
            self.planned.push((path.to_path_buf(), metadata));
        }
    }

    fn expected(&self, path: &Path) -> Option<&std::fs::Metadata> {
        self.planned
            .iter()
            .find(|(planned_path, _)| planned_path == path)
            .map(|(_, metadata)| metadata)
    }

    /// The directory to hand `remove_dir_all`, once it has been shown to be
    /// inside the tree and to look like what it claims to be.
    fn authorize(&self, path: &Path, recursion: Recursion) -> Result<AuthorizedDirectory> {
        let Some(base) = &self.base else {
            return Err(OvmError::Message(
                "OVM's own directory could not be resolved, so nothing was deleted recursively"
                    .into(),
            ));
        };
        let metadata = std::fs::symlink_metadata(path)?;
        if let Some(expected) = self.expected(path) {
            if !same_entry(expected, &metadata) {
                return Err(OvmError::Message(format!(
                    "{} changed after confirmation; refusing recursive deletion",
                    path.display()
                )));
            }
        }
        if metadata.file_type().is_symlink() {
            return Err(OvmError::Message(format!(
                "{} is a symlink; refusing recursive deletion",
                path.display()
            )));
        }
        let resolved = std::fs::canonicalize(path)?;
        if !resolved.starts_with(base) {
            return Err(OvmError::Message(format!(
                "it leads to {}, outside OVM's tree at {} — refusing to delete recursively",
                resolved.display(),
                base.display()
            )));
        }
        let mut entries = std::fs::read_dir(&resolved)?;
        if entries.next().is_none() {
            return Ok(AuthorizedDirectory { metadata });
        }
        let marker_count = recursion
            .markers()
            .iter()
            .filter(|marker| resolved.join(marker).symlink_metadata().is_ok())
            .count();
        if marker_count >= recursion.minimum_markers() {
            ensure_single_device(&resolved)?;
            return Ok(AuthorizedDirectory { metadata });
        }
        Err(OvmError::Message(format!(
            "{} does not look like the OVM directory it stands in for — \
             refusing to delete its contents",
            resolved.display()
        )))
    }
}

impl Recursion {
    fn minimum_markers(self) -> usize {
        match self {
            Self::OvmTree => 2,
            Self::Snapshots => 1,
            Self::None | Self::EmptyOnly => 0,
        }
    }
}

#[cfg(unix)]
fn ensure_single_device(root: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let root_device = std::fs::symlink_metadata(root)?.dev();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.dev() != root_device {
            return Err(OvmError::Message(format!(
                "{} crosses a filesystem boundary; refusing recursive deletion",
                path.display()
            )));
        }
        if metadata.file_type().is_dir() {
            for entry in std::fs::read_dir(&path)? {
                pending.push(entry?.path());
            }
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_single_device(_root: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn same_entry(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_entry(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.file_type() == right.file_type() && left.len() == right.len()
}

fn remove_recursive_quarantined(
    path: &Path,
    boundary: &Boundary,
    recursion: Recursion,
) -> Result<()> {
    let authorized = boundary.authorize(path, recursion)?;
    let quarantine = quarantine_path(path)?;
    std::fs::rename(path, &quarantine)?;
    let after = std::fs::symlink_metadata(&quarantine)?;
    if !same_entry(&authorized.metadata, &after) {
        let _ = std::fs::rename(&quarantine, path);
        return Err(OvmError::Message(
            "the directory changed while it was being quarantined; refusing deletion".into(),
        ));
    }
    if let Err(error) = ensure_single_device(&quarantine) {
        let _ = std::fs::rename(&quarantine, path);
        return Err(error);
    }
    std::fs::remove_dir_all(&quarantine)?;
    Ok(())
}

fn quarantine_path(path: &Path) -> Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_QUARANTINE: AtomicU64 = AtomicU64::new(0);

    let parent = path
        .parent()
        .ok_or_else(|| OvmError::Message(format!("{} has no parent directory", path.display())))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("ovm");
    for _ in 0..100 {
        let nonce = NEXT_QUARANTINE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{name}.ovm-remove-{}-{nonce}", std::process::id()));
        if candidate.symlink_metadata().is_err() {
            return Ok(candidate);
        }
    }
    Err(OvmError::Message(format!(
        "could not reserve a temporary uninstall path beside {}",
        path.display()
    )))
}

fn remove_non_recursive_quarantined(
    path: &Path,
    boundary: &Boundary,
    observed: &std::fs::Metadata,
) -> Result<()> {
    let expected = boundary.expected(path).unwrap_or(observed);
    if !same_entry(expected, observed) {
        return Err(OvmError::Message(format!(
            "{} changed after confirmation; refusing deletion",
            path.display()
        )));
    }
    let quarantine = quarantine_path(path)?;
    std::fs::rename(path, &quarantine)?;
    let after = std::fs::symlink_metadata(&quarantine)?;
    if !same_entry(expected, &after) || after.file_type().is_dir() {
        let _ = std::fs::rename(&quarantine, path);
        return Err(OvmError::Message(format!(
            "{} changed while it was being quarantined; refusing deletion",
            path.display()
        )));
    }
    std::fs::remove_file(&quarantine)?;
    Ok(())
}

/// Remove a file, symlink, or directory. `Ok(false)` means there was nothing
/// there — an already-clean machine is a success, not an error.
///
/// A symlink is unlinked, never followed, with one deliberate exception: when
/// the entry stands in for a directory OVM's own tree owns (a `~/.ovm` kept on
/// external storage), the contents at the target are what the user asked to
/// remove. Unlinking only the link there would report the tree as gone while
/// every byte of it stayed on disk.
fn remove_entry(path: &Path, boundary: &Boundary, recursion: Recursion) -> Result<bool> {
    let Some(metadata) = inspect(path)? else {
        return Ok(false);
    };
    let file_type = metadata.file_type();

    if file_type.is_dir() {
        if recursion.recurses() {
            remove_recursive_quarantined(path, boundary, recursion)?;
        } else {
            // `remove_dir` refuses a non-empty directory, which is the point:
            // nothing unexpected gets taken along with it.
            std::fs::remove_dir(path)?;
        }
        return Ok(true);
    }

    if file_type.is_symlink() && recursion.recurses() {
        return Err(OvmError::Message(format!(
            "{} is a symlink; refusing recursive deletion",
            path.display()
        )));
    }

    remove_non_recursive_quarantined(path, boundary, &metadata)?;
    Ok(true)
}

/// Print `~/.ovm/...` rather than a full home path — the same convention the
/// rest of OVM's output uses.
fn tilde(home: &Path, path: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: &str = "\n# >>> ovm >>>\nexport PATH=\"$HOME/.ovm/bin:$PATH\"\n# <<< ovm <<<\n";

    #[test]
    fn strips_only_the_installer_block() {
        let contents = format!("export EDITOR=vim\n{BLOCK}alias ll='ls -l'\n");
        let BlockStrip::Removed(rest) = strip_path_block(&contents) else {
            panic!("block should be removed");
        };
        assert_eq!(rest, "export EDITOR=vim\nalias ll='ls -l'\n");
    }

    #[test]
    fn keeps_a_profile_without_the_block_untouched() {
        // A user's own PATH line mentioning ~/.ovm/bin is not ours to delete.
        let contents = "export PATH=\"$HOME/.ovm/bin:$PATH\"\n";
        assert_eq!(strip_path_block(contents), BlockStrip::Absent);
    }

    #[test]
    fn refuses_an_unterminated_block_instead_of_guessing() {
        let contents = "before\n# >>> ovm >>>\nexport PATH=\"x\"\nafter\n";
        assert_eq!(strip_path_block(contents), BlockStrip::Unterminated);
    }

    #[test]
    fn refuses_a_historical_begin_that_would_swallow_config_up_to_a_later_block() {
        // A begin marker whose end was lost (hand-edited profile), followed by
        // the user's own lines and then a complete block. Pairing the stray
        // begin with the later end would delete the exports in between.
        let contents = format!(
            "# >>> ovm >>>\nexport PATH=\"$HOME/.ovm/bin:$PATH\"\n\
             export EDITOR=vim\nexport SECRET_SAUCE=1\n{BLOCK}"
        );
        assert_eq!(strip_path_block(&contents), BlockStrip::Unterminated);
    }

    #[test]
    fn strips_repeated_blocks_and_a_block_without_a_trailing_newline() {
        let contents = format!("first\n{BLOCK}{BLOCK}");
        let BlockStrip::Removed(rest) = strip_path_block(&contents) else {
            panic!("blocks should be removed");
        };
        assert_eq!(rest, "first\n");

        let unterminated_newline = "# >>> ovm >>>\nexport PATH=\"x\"\n# <<< ovm <<<";
        let BlockStrip::Removed(rest) = strip_path_block(unterminated_newline) else {
            panic!("block should be removed");
        };
        assert_eq!(rest, "");
    }

    #[test]
    fn profile_candidates_follow_zdotdir_without_duplicating_home() {
        let home = Path::new("/h");
        let plain = profile_candidates(home, None);
        assert!(plain.contains(&home.join(".zshrc")));
        assert_eq!(
            plain
                .iter()
                .filter(|path| **path == home.join(".zshrc"))
                .count(),
            1
        );

        let zdotdir = PathBuf::from("/h/.config/zsh");
        let with_zdotdir = profile_candidates(home, Some(&zdotdir));
        assert!(with_zdotdir.contains(&zdotdir.join(".zshrc")));
        assert!(with_zdotdir.contains(&home.join(".zshrc")));
    }

    #[test]
    fn removing_the_block_rewrites_the_profile_in_place() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join(".zshrc");
        std::fs::write(&profile, format!("keep me\n{BLOCK}")).unwrap();

        assert_eq!(
            remove_path_block(&profile).unwrap(),
            ProfileOutcome::Removed
        );
        assert_eq!(std::fs::read_to_string(&profile).unwrap(), "keep me\n");

        // Second run is a no-op, not an error.
        assert_eq!(remove_path_block(&profile).unwrap(), ProfileOutcome::Absent);
        assert_eq!(
            remove_path_block(&temp.path().join("nope")).unwrap(),
            ProfileOutcome::Absent
        );
    }

    #[test]
    #[cfg(unix)]
    fn rewriting_a_symlinked_profile_replaces_the_target_and_keeps_the_link() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let dotfiles = temp.path().join("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();
        let real = dotfiles.join("zshrc");
        std::fs::write(&real, format!("keep me\n{BLOCK}")).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o644)).unwrap();
        let profile = temp.path().join(".zshrc");
        std::os::unix::fs::symlink(&real, &profile).unwrap();

        assert_eq!(
            remove_path_block(&profile).unwrap(),
            ProfileOutcome::Removed
        );
        // The link is still a link, pointing where it did, and the dotfiles
        // repo's own file is the one that changed.
        assert!(profile.is_symlink());
        assert_eq!(std::fs::read_link(&profile).unwrap(), real);
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "keep me\n");
        assert_eq!(
            std::fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o644
        );
        // No staging litter left in the dotfiles directory.
        assert_eq!(std::fs::read_dir(&dotfiles).unwrap().count(), 1);
    }

    #[test]
    fn an_emptied_installer_owned_fish_snippet_is_deleted() {
        let temp = tempfile::tempdir().unwrap();
        let fish = temp.path().join("ovm.fish");
        std::fs::write(&fish, BLOCK).unwrap();

        assert_eq!(
            remove_path_block(&fish).unwrap(),
            ProfileOutcome::FileRemoved
        );
        assert!(!fish.exists());

        // A fish snippet the user extended keeps existing, and the installer's
        // leading separator goes with the block rather than being left behind.
        let kept = temp.path().join("ovm.fish");
        std::fs::write(&kept, format!("{BLOCK}set -x MY_VAR 1\n")).unwrap();
        assert_eq!(remove_path_block(&kept).unwrap(), ProfileOutcome::Removed);
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "set -x MY_VAR 1\n");
    }

    /// A base with the directories a real install has, so the shape check that
    /// guards recursive deletion has something to recognise.
    fn ovm_tree(root: &Path) -> PathBuf {
        let base = root.join(".ovm");
        std::fs::create_dir_all(base.join("self/versions/1.0.0")).unwrap();
        std::fs::create_dir_all(base.join("products")).unwrap();
        std::fs::write(base.join("config.json"), "{}\n").unwrap();
        base
    }

    #[test]
    fn remove_entry_reports_missing_paths_as_nothing_to_do() {
        let temp = tempfile::tempdir().unwrap();
        let base = ovm_tree(temp.path());
        let boundary = Boundary::at(&base, Some(&base.join("self")));
        assert!(!remove_entry(&base.join("absent"), &boundary, Recursion::None).unwrap());

        let file = base.join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(remove_entry(&file, &boundary, Recursion::None).unwrap());

        assert!(remove_entry(&base.join("self"), &boundary, Recursion::Snapshots).unwrap());
        assert!(!base.join("self").exists());
    }

    #[test]
    #[cfg(unix)]
    fn remove_entry_removes_a_dangling_symlink_without_following_it() {
        let temp = tempfile::tempdir().unwrap();
        let base = ovm_tree(temp.path());
        let link = base.join("bin/ovm-claudex");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(base.join("gone"), &link).unwrap();
        assert!(remove_entry(&link, &Boundary::at(&base, None), Recursion::None).unwrap());
        assert!(!link.is_symlink());
    }

    #[test]
    #[cfg(unix)]
    fn a_symlinked_ovm_home_is_refused_without_touching_its_target() {
        let temp = tempfile::tempdir().unwrap();
        let external = ovm_tree(&temp.path().join("external"));
        let base = temp.path().join("home/.ovm");
        std::fs::create_dir_all(base.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&external, &base).unwrap();

        let error = remove_entry(&base, &Boundary::at(&base, None), Recursion::OvmTree)
            .expect_err("recursive deletion must never follow an OVM-home symlink");
        assert!(error.to_string().contains("is a symlink"), "{error}");
        assert!(external.exists());
        assert!(base.is_symlink());
    }

    #[test]
    #[cfg(unix)]
    fn a_directory_symlinked_out_of_the_tree_is_refused_not_deleted() {
        // `~/.ovm/self` pointing at somebody else's directory. The old code
        // followed the path straight into it.
        let temp = tempfile::tempdir().unwrap();
        let base = ovm_tree(temp.path());
        let foreign = temp.path().join("not-ovms");
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("thesis.txt"), b"years of work\n").unwrap();
        let self_root = base.join("self");
        std::fs::remove_dir_all(&self_root).unwrap();
        std::os::unix::fs::symlink(&foreign, &self_root).unwrap();

        let error = remove_entry(
            &self_root,
            &Boundary::at(&base, Some(&self_root)),
            Recursion::Snapshots,
        )
        .expect_err("an escape must be refused, not silently skipped");
        assert!(error.to_string().contains("is a symlink"), "{error}");
        assert!(foreign.join("thesis.txt").exists());
        assert!(self_root.is_symlink());
    }

    #[test]
    fn a_directory_that_does_not_look_like_ovms_keeps_its_contents() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join(".ovm");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("tax-returns.pdf"), b"not ours\n").unwrap();

        let error = remove_entry(&base, &Boundary::at(&base, None), Recursion::OvmTree)
            .expect_err("a tree without a single OVM marker must not be deleted");
        assert!(error.to_string().contains("does not look like"), "{error}");
        assert!(base.join("tax-returns.pdf").exists());

        // An empty directory has nothing to lose, so it still goes.
        let empty = temp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(remove_entry(
            &empty,
            &Boundary::at(temp.path(), Some(&empty)),
            Recursion::OvmTree,
        )
        .unwrap());
    }

    #[test]
    fn a_planned_recursive_directory_cannot_be_swapped_before_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let base = ovm_tree(temp.path());
        let self_root = base.join("self");
        let boundary = Boundary::at(&base, Some(&self_root));
        let original = base.join("original-self");
        std::fs::rename(&self_root, &original).unwrap();
        // A replacement can imitate the marker shape, but not the inode that
        // was shown in the confirmation plan.
        std::fs::create_dir_all(self_root.join("versions")).unwrap();

        let error = boundary
            .authorize(&self_root, Recursion::Snapshots)
            .expect_err("a replacement directory must not inherit authorization");
        assert!(error.to_string().contains("changed after confirmation"));
        assert!(original.join("versions/1.0.0").exists());
    }

    #[test]
    fn a_non_recursive_target_never_takes_a_directorys_contents() {
        let temp = tempfile::tempdir().unwrap();
        let base = ovm_tree(temp.path());
        let bin = base.join("bin");
        std::fs::create_dir_all(bin.join("surprise")).unwrap();

        assert!(remove_entry(&bin, &Boundary::at(&base, None), Recursion::EmptyOnly).is_err());
        assert!(bin.join("surprise").is_dir());

        std::fs::remove_dir(bin.join("surprise")).unwrap();
        assert!(remove_entry(&bin, &Boundary::at(&base, None), Recursion::EmptyOnly).unwrap());
    }

    #[test]
    fn tilde_shortens_only_paths_under_home() {
        let home = Path::new("/h");
        assert_eq!(tilde(home, Path::new("/h/.ovm/bin")), "~/.ovm/bin");
        assert_eq!(tilde(home, Path::new("/opt/ovm")), "/opt/ovm");
    }
}

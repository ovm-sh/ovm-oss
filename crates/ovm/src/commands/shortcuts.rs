//! `ovm shortcuts` — bare launch commands without shell rc edits.
//!
//! Writes one-line shims into `~/.ovm/bin` so `ccy`, `cxy`, `ccx`, `ccxy`,
//! and `claudex` work as commands in any shell. Each shim just execs the
//! matching `ovm` subcommand, so version resolution, auto-update, and yolo
//! flag handling all stay inside OVM and the shims can never go stale.
//!
//! They share the directory the installer already put on PATH, rather than
//! `~/.local/bin` where they used to live. That directory needed a PATH entry
//! of its own, and a warning of its own when it was missing, so a fresh
//! machine had two independent ways to reach `ccy: command not found` after a
//! successful install. `~/.ovm/bin` has neither: `ovm` resolves from there, so
//! the shims beside it resolve too, and the tour can end by telling a reader
//! to type `ccy` without that being a guess. Shims already written to
//! `~/.local/bin` keep working — they exec `ovm` by name — so [`run`] points
//! them out instead of deleting anything someone may rely on.
//!
//! Coexists with the claude-yolo rc block from mochiexists.com/yolo: shell
//! aliases take precedence over PATH files and expand to the same
//! OVM-managed binaries, so nothing needs migrating — we detect the block
//! and say so instead of touching anyone's shell config.

use crate::config::OvmDirs;
use crate::error::{OvmError, Result};
use console::style;
use std::path::{Path, PathBuf};

/// (command name, human description). Each shim execs `ovm <command name>`, so
/// the subcommand is the name itself and is not repeated here.
const SHORTCUTS: [(&str, &str); 9] = [
    ("ccy", "claude --yolo"),
    ("cxy", "codex --yolo"),
    ("cxf", "codex --fast (priority tier)"),
    ("cxyf", "codex --yolo --fast"),
    ("claudex", "Claude Code on GPT-5.6"),
    ("ccx", "claudex"),
    ("ccxy", "claudex --yolo"),
    ("ccxf", "claudex --fast"),
    ("ccxyf", "claudex --yolo --fast"),
];

/// Marker the claude-yolo installer writes into shell rc files.
const YOLO_BLOCK_MARKER: &str = ">>> claude-yolo >>>";

#[derive(Debug, PartialEq, Eq)]
enum ExistingFile {
    Missing,
    /// A shim we (or ovm-claudex setup) wrote — safe to refresh.
    Ours,
    /// Something else lives at that name — never overwrite silently.
    Foreign,
}

pub fn run(assume_yes: bool) -> Result<()> {
    let home = dirs::home_dir()
        .ok_or_else(|| OvmError::Message("Could not determine home directory.".into()))?;
    let bin_dir = OvmDirs::new()?.bin;

    eprintln!();
    eprintln!(
        "  {} Bare shortcuts — no shell config edits, just files in {}:",
        style("→").cyan(),
        style(tilde(&bin_dir, &home)).bold()
    );
    for (name, description) in SHORTCUTS {
        let state = match classify(&bin_dir.join(name), name) {
            ExistingFile::Missing => style("will install").dim().to_string(),
            ExistingFile::Ours => style("installed").green().to_string(),
            ExistingFile::Foreign => style("exists (not ovm's — will skip)").yellow().to_string(),
        };
        eprintln!("    {:<8} {:<28} {state}", name, style(description).dim());
    }
    eprintln!();

    if !assume_yes && !confirm("Install/refresh these shortcuts?")? {
        eprintln!("  {} Cancelled — nothing was changed.", style("✗").dim());
        return Ok(());
    }

    let installed = install_all(&bin_dir, |name, path| {
        eprintln!(
            "  {} Skipped {name}: {} isn't an ovm shim.",
            style("!").yellow(),
            path.display()
        );
    })?;
    eprintln!(
        "  {} {installed} shortcut{} ready in {}",
        style("✓").green(),
        if installed == 1 { "" } else { "s" },
        tilde(&bin_dir, &home)
    );

    // Only reachable when someone has taken ~/.ovm/bin back off their PATH:
    // the installer put it there, and `ovm` itself resolved through it to get
    // here. Still worth saying — the shims are inert without it — and it is
    // the same line the installer writes into the shell rc.
    if !dir_on_path(&bin_dir) {
        eprintln!(
            "  {} {} is not on your PATH — add this to your shell rc:",
            style("!").yellow(),
            tilde(&bin_dir, &home)
        );
        eprintln!("      export PATH=\"$HOME/.ovm/bin:$PATH\"");
    }

    let stale = legacy_shims(&home);
    if !stale.is_empty() {
        eprintln!(
            "  {} ~/.local/bin still holds {} older shim{}. Shims there exec `ovm`",
            style("ℹ").cyan(),
            stale.len(),
            if stale.len() == 1 { "" } else { "s" }
        );
        eprintln!("    by name, so they keep working — remove them whenever you like.");
    }

    let rc_files = [home.join(".zshrc"), home.join(".bashrc")];
    for rc in yolo_block_locations(&rc_files) {
        eprintln!(
            "  {} Found the claude-yolo block in {} — its ccy/cxy aliases take",
            style("ℹ").cyan(),
            rc.display()
        );
        eprintln!("    precedence and run the same OVM-managed binaries, so both can");
        eprintln!("    coexist. It also enables `claude --yolo` on the bare launcher.");
    }

    Ok(())
}

fn confirm(question: &str) -> Result<bool> {
    if !console::Term::stderr().is_term() {
        return Ok(true);
    }
    eprint!("  {} {} [Y/n] ", style("?").yellow().bold(), question);
    use std::io::Write;
    std::io::stderr().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_lowercase();
    Ok(answer.is_empty() || answer == "y" || answer == "yes")
}

/// Write every shim into `bin_dir`, reporting anything it refused to touch
/// through `on_skip`. Returns how many shims are in place afterwards.
fn install_all(bin_dir: &Path, mut on_skip: impl FnMut(&str, &Path)) -> Result<usize> {
    std::fs::create_dir_all(bin_dir)?;
    let mut installed = 0;
    for (name, _) in SHORTCUTS {
        let path = bin_dir.join(name);
        match classify(&path, name) {
            ExistingFile::Foreign => on_skip(name, &path),
            _ => {
                write_shim(&path, name)?;
                installed += 1;
            }
        }
    }
    Ok(installed)
}

/// Install the shims as part of the tour, silently.
///
/// The tour now ends by telling the reader to type `ccy`, which is only true
/// if the shims exist — so it writes them rather than asking. A smaller step
/// than it looks: the files land in OVM's own `bin`, the directory the
/// installer created and put on PATH minutes earlier, and [`classify`] still
/// refuses to overwrite anything that is not already ours. Best-effort, because
/// a tour must not fail on its last act.
pub(crate) fn install_for_tour() {
    if let Ok(dirs) = OvmDirs::new() {
        let _ = install_all(&dirs.bin, |_, _| {});
    }
}

/// Whether `name` is on PATH as a shim we wrote.
///
/// The summary asks before it prints: a reader whose `ccy` is their own script
/// (so we skipped it) must be shown `ovm ccy`, not a command that runs someone
/// else's code.
pub(crate) fn shim_is_ready(name: &str) -> bool {
    let Ok(dirs) = OvmDirs::new() else {
        return false;
    };
    dir_on_path(&dirs.bin) && classify(&dirs.bin.join(name), name) == ExistingFile::Ours
}

/// Shims still sitting in the directory the shortcuts used to live in.
fn legacy_shims(home: &Path) -> Vec<PathBuf> {
    let old = home.join(".local").join("bin");
    SHORTCUTS
        .iter()
        .map(|(name, _)| (old.join(name), *name))
        .filter(|(path, name)| classify(path, name) == ExistingFile::Ours)
        .map(|(path, _)| path)
        .collect()
}

/// `~`-relative rendering, so a path on screen matches the one in the docs.
fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

fn shim_contents(name: &str) -> String {
    format!("#!/bin/sh\nexec ovm {name} \"$@\"\n")
}

fn classify(path: &Path, name: &str) -> ExistingFile {
    // A symlink — even a dangling one — must never be treated as writable:
    // write_shim would follow it and could drop the shim outside the bin dir.
    // Any existing symlink is foreign; leave it untouched.
    if path
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
    {
        return ExistingFile::Foreign;
    }
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            // Ownership requires the EXACT canonical shim for this shortcut.
            // A broad `exec ovm ` substring match would classify any
            // user-authored wrapper mentioning it (even in a comment) as ours
            // and clobber it. The shim template has only ever had this one
            // form, so an exact byte-compare is both safe and sufficient.
            if contents == shim_contents(name) {
                ExistingFile::Ours
            } else {
                ExistingFile::Foreign
            }
        }
        // Only a genuinely absent path is safe to write. Anything else —
        // unreadable, or non-UTF-8 like a real compiled binary — must never
        // be clobbered, so treat it as foreign and leave it alone.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ExistingFile::Missing,
        Err(_) => ExistingFile::Foreign,
    }
}

fn write_shim(path: &Path, name: &str) -> Result<()> {
    std::fs::write(path, shim_contents(name))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

pub(crate) fn dir_on_path(dir: &Path) -> bool {
    let Some(path_env) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_env).any(|entry| entry == dir)
}

/// Which of the given rc files contain the claude-yolo installer block.
fn yolo_block_locations(rc_files: &[PathBuf]) -> Vec<PathBuf> {
    rc_files
        .iter()
        .filter(|rc| {
            std::fs::read_to_string(rc)
                .map(|contents| contents.contains(YOLO_BLOCK_MARKER))
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shims_exec_the_matching_ovm_subcommand() {
        assert_eq!(shim_contents("ccxy"), "#!/bin/sh\nexec ovm ccxy \"$@\"\n");
        assert_eq!(shim_contents("ccy"), "#!/bin/sh\nexec ovm ccy \"$@\"\n");
    }

    #[test]
    fn install_all_writes_every_shim_and_never_clobbers_a_foreign_one() {
        let temp = tempfile::tempdir().expect("tempdir");
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let mine = "#!/bin/sh\necho my own ccy\n";
        std::fs::write(bin.join("ccy"), mine).unwrap();

        let mut skipped = Vec::new();
        let installed = install_all(&bin, |name, _| skipped.push(name.to_string())).unwrap();

        assert_eq!(skipped, vec!["ccy".to_string()]);
        assert_eq!(installed, SHORTCUTS.len() - 1);
        assert_eq!(
            std::fs::read_to_string(bin.join("ccxy")).unwrap(),
            shim_contents("ccxy")
        );
        assert_eq!(std::fs::read_to_string(bin.join("ccy")).unwrap(), mine);
    }

    /// The tour's closing line names the bare shims, so the shims have to be
    /// the ones OVM's own PATH entry already covers.
    #[test]
    fn shims_land_beside_ovm_not_in_local_bin() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = OvmDirs::at(temp.path().join(".ovm"));
        install_all(&dirs.bin, |_, _| {}).unwrap();
        assert!(dirs.bin.join("ccy").exists());
        assert!(dirs.bin.join("ccxy").exists());
        assert!(!temp.path().join(".local").join("bin").exists());
    }

    #[test]
    fn legacy_shims_finds_only_our_old_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let old = temp.path().join(".local").join("bin");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("ccy"), shim_contents("ccy")).unwrap();
        std::fs::write(old.join("cxy"), "#!/bin/sh\nnot ours\n").unwrap();

        let found = legacy_shims(temp.path());
        assert_eq!(found, vec![old.join("ccy")]);
    }

    #[test]
    fn tilde_shortens_only_paths_under_home() {
        let home = Path::new("/Users/example");
        assert_eq!(tilde(&home.join(".ovm").join("bin"), home), "~/.ovm/bin");
        assert_eq!(tilde(Path::new("/opt/ovm/bin"), home), "/opt/ovm/bin");
    }

    #[test]
    fn classify_distinguishes_ours_foreign_and_missing() {
        let temp = tempfile::tempdir().expect("tempdir");

        let ours = temp.path().join("ccy");
        std::fs::write(&ours, shim_contents("ccy")).unwrap();
        assert_eq!(classify(&ours, "ccy"), ExistingFile::Ours);

        // ovm-claudex setup's shims are byte-identical to ours.
        let claudex_style = temp.path().join("ccxy");
        std::fs::write(&claudex_style, "#!/bin/sh\nexec ovm ccxy \"$@\"\n").unwrap();
        assert_eq!(classify(&claudex_style, "ccxy"), ExistingFile::Ours);

        let foreign = temp.path().join("cxy");
        std::fs::write(&foreign, "#!/bin/sh\necho my own thing\n").unwrap();
        assert_eq!(classify(&foreign, "cxy"), ExistingFile::Foreign);

        assert_eq!(
            classify(&temp.path().join("nope"), "nope"),
            ExistingFile::Missing
        );
    }

    #[test]
    fn classify_rejects_foreign_wrapper_that_merely_mentions_exec_ovm() {
        let temp = tempfile::tempdir().expect("tempdir");

        // A user-authored wrapper that references `exec ovm ` in a comment
        // must survive untouched — the broad substring match would have
        // classified it as ours and clobbered it.
        let wrapper = temp.path().join("ccy");
        std::fs::write(
            &wrapper,
            "#!/bin/sh\n# falls back to `exec ovm ccy` when unset\nexec my-launcher \"$@\"\n",
        )
        .unwrap();
        assert_eq!(classify(&wrapper, "ccy"), ExistingFile::Foreign);

        // The canonical shim for a DIFFERENT shortcut is also foreign here:
        // ownership is per-name, so we never rewrite one name's shim as another.
        let other = temp.path().join("ccy");
        std::fs::write(&other, shim_contents("cxy")).unwrap();
        assert_eq!(classify(&other, "ccy"), ExistingFile::Foreign);
    }

    #[test]
    #[cfg(unix)]
    fn classify_treats_symlinks_as_foreign_never_missing() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().expect("tempdir");

        // A dangling symlink must NOT read as Missing — writing through it
        // would drop the shim at the (attacker-chosen) link target.
        let dangling = temp.path().join("ccy");
        symlink(temp.path().join("does-not-exist"), &dangling).unwrap();
        assert_eq!(classify(&dangling, "ccy"), ExistingFile::Foreign);

        // A symlink to one of our own shims is still foreign — we never follow
        // it to overwrite the target in place.
        let real_shim = temp.path().join("real");
        std::fs::write(&real_shim, shim_contents("ccxy")).unwrap();
        let link = temp.path().join("ccxy");
        symlink(&real_shim, &link).unwrap();
        assert_eq!(classify(&link, "ccxy"), ExistingFile::Foreign);
    }

    #[test]
    fn write_shim_is_executable_and_idempotent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("ccxy");

        write_shim(&path, "ccxy").expect("write");
        write_shim(&path, "ccxy").expect("rewrite");

        assert_eq!(classify(&path, "ccxy"), ExistingFile::Ours);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "shim must be executable");
        }
    }

    #[test]
    fn yolo_block_detection_finds_only_marked_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let zshrc = temp.path().join(".zshrc");
        let bashrc = temp.path().join(".bashrc");
        std::fs::write(
            &zshrc,
            "# stuff\n# >>> claude-yolo >>>\nalias ccy='claude --yolo'\n# <<< claude-yolo <<<\n",
        )
        .unwrap();
        std::fs::write(&bashrc, "# plain bashrc\n").unwrap();

        let found = yolo_block_locations(&[zshrc.clone(), bashrc, temp.path().join(".profile")]);
        assert_eq!(found, vec![zshrc]);
    }
}

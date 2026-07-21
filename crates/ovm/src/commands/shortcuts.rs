//! `ovm shortcuts` — bare launch commands without shell rc edits.
//!
//! Writes one-line shims into `~/.local/bin` so `ccy`, `cxy`, `ccx`, `ccxy`,
//! and `claudex` work as commands in any shell. Each shim just execs the
//! matching `ovm` subcommand, so version resolution, auto-update, and yolo
//! flag handling all stay inside OVM and the shims can never go stale.
//!
//! Coexists with the claude-yolo rc block from mochiexists.com/yolo: shell
//! aliases take precedence over PATH files and expand to the same
//! OVM-managed binaries, so nothing needs migrating — we detect the block
//! and say so instead of touching anyone's shell config.

use crate::error::{OvmError, Result};
use console::style;
use std::path::{Path, PathBuf};

/// (command name, `ovm` subcommand it execs, human description)
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
    let bin_dir = home.join(".local").join("bin");

    eprintln!();
    eprintln!(
        "  {} Bare shortcuts — no shell config edits, just files in {}:",
        style("→").cyan(),
        style("~/.local/bin").bold()
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

    std::fs::create_dir_all(&bin_dir)?;
    let mut installed = 0;
    for (name, _) in SHORTCUTS {
        let path = bin_dir.join(name);
        match classify(&path, name) {
            ExistingFile::Foreign => {
                eprintln!(
                    "  {} Skipped {name}: {} isn't an ovm shim.",
                    style("!").yellow(),
                    path.display()
                );
            }
            _ => {
                write_shim(&path, name)?;
                installed += 1;
            }
        }
    }
    eprintln!(
        "  {} {installed} shortcut{} ready in ~/.local/bin",
        style("✓").green(),
        if installed == 1 { "" } else { "s" }
    );

    if !dir_on_path(&bin_dir) {
        eprintln!(
            "  {} ~/.local/bin is not on your PATH — add this to your shell rc:",
            style("!").yellow()
        );
        eprintln!("      export PATH=\"$HOME/.local/bin:$PATH\"");
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

fn dir_on_path(dir: &Path) -> bool {
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

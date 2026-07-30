//! `ovm claudex uninstall` — stop the proxy, remove ovm-owned shims, and
//! (only with `--purge`, after confirmation) delete `~/.ovm/claudex/` with
//! its credentials and isolated history.

use crate::paths::{display, shim_install_dir, ClaudexDirs};
use crate::{proxy, ClaudexError, Result};
use console::style;

pub fn run(purge: bool) -> Result<()> {
    let dirs = ClaudexDirs::new()?;

    proxy::stop(&dirs)?;
    remove_owned_shims()?;

    if !purge {
        eprintln!(
            "  {} Kept {} (config, OAuth grant, isolated history).",
            style("—").dim(),
            display(&dirs.base)
        );
        eprintln!("    Remove everything with: ovm claudex uninstall --purge");
        return Ok(());
    }

    if !dirs.base.exists() {
        eprintln!("  {} Nothing to purge.", style("—").dim());
        return Ok(());
    }
    if !confirm_purge(&display(&dirs.base))? {
        eprintln!("  {} Purge cancelled — data kept.", style("✗").dim());
        return Ok(());
    }
    std::fs::remove_dir_all(&dirs.base)?;
    eprintln!(
        "  {} Removed {} — claudex is fully uninstalled.",
        style("✓").green(),
        display(&dirs.base)
    );
    Ok(())
}

/// Delete the claudex shims, but only files that are actually ovm's.
fn remove_owned_shims() -> Result<()> {
    let Some(bin_dir) = shim_install_dir() else {
        return Ok(());
    };
    for name in crate::setup::CLAUDEX_SHIMS {
        let shim = bin_dir.join(name);
        match std::fs::read_to_string(&shim) {
            Ok(contents) if contents.starts_with("#!/bin/sh\nexec ovm ") => {
                std::fs::remove_file(&shim)?;
                eprintln!("  {} Removed shim {}", style("✓").green(), display(&shim));
            }
            Ok(_) => {
                eprintln!(
                    "  {} Left {} alone — it isn't ovm's shim.",
                    style("!").yellow(),
                    display(&shim)
                );
            }
            Err(_) => {}
        }
    }
    Ok(())
}

/// Purging deletes credentials and history — never do it without an explicit
/// interactive yes. Non-interactive runs must keep their hands off.
fn confirm_purge(target: &str) -> Result<bool> {
    if !console::Term::stderr().is_term() {
        return Err(ClaudexError::Message(
            "--purge needs an interactive terminal to confirm deletion.".into(),
        ));
    }
    eprint!(
        "  {} Delete {target} including the Codex OAuth grant and all claudex history? [y/N] ",
        style("?").red().bold()
    );
    use std::io::Write;
    std::io::stderr().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_lowercase();
    Ok(answer == "y" || answer == "yes")
}

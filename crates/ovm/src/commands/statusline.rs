//! `ovm statusline` — put Echo in the Claude Code statusline.
//!
//! The tour offers this in chapter iii; this is the same install for anyone
//! who skipped it there or wants it on another machine.

use crate::config::OvmDirs;
use crate::error::Result;
use console::style;

pub fn run(action: Option<&str>) -> Result<()> {
    match action {
        None | Some("install") => install(),
        Some("update") => update(),
        Some("status") => status(),
        Some(other) => Err(crate::error::OvmError::Message(format!(
            "unknown statusline action `{other}` — use install, update or status"
        ))),
    }
}

/// Refresh the installed Echo to the one inside this ovm; settings untouched.
fn update() -> Result<()> {
    let dirs = OvmDirs::new()?;
    if crate::claude_settings::installed_version(&dirs.base).is_none() {
        eprintln!("  Echo is not installed here — run: ovm statusline");
        return Ok(());
    }
    if crate::claude_settings::refresh_installed(&dirs.base)? {
        eprintln!(
            "  {} Echo updated to {}",
            style("✓").green(),
            crate::claude_settings::bundled_version().unwrap_or("the bundled version")
        );
    } else {
        eprintln!("  {} Echo is already current", style("✓").green());
    }
    Ok(())
}

fn status() -> Result<()> {
    let dirs = OvmDirs::new()?;
    let bundled = crate::claude_settings::bundled_version().unwrap_or("unversioned");
    match crate::claude_settings::installed_version(&dirs.base) {
        None => println!("  Echo: not installed (this ovm ships {bundled})"),
        Some(installed) => {
            let installed = installed.unwrap_or_else(|| "unversioned (before 2026.09.27)".into());
            let in_statusline = if crate::claude_settings::is_installed(&dirs.base) {
                "in the statusline"
            } else {
                "not in the statusline"
            };
            println!("  Echo: {installed} installed, {in_statusline}; this ovm ships {bundled}");
        }
    }
    Ok(())
}

fn install() -> Result<()> {
    let dirs = OvmDirs::new()?;
    if let Some(existing) = crate::claude_settings::foreign_command(&dirs.base) {
        eprintln!(
            "  {} Replacing your current statusline: {}",
            style("!").yellow(),
            style(existing).dim()
        );
        eprintln!("    A copy is kept next to your Claude settings.");
    }
    let script = crate::claude_settings::install(&dirs.base)?;
    eprintln!(
        "  {} Echo is in your statusline — new Claude sessions will show them",
        style("✓").green()
    );
    eprintln!("    {}", style(script.display()).dim());
    Ok(())
}

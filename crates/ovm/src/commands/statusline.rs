//! `ovm statusline` — put Echo in the Claude Code statusline.
//!
//! The tour offers this in chapter iii; this is the same install for anyone
//! who skipped it there or wants it on another machine.

use crate::config::OvmDirs;
use crate::error::Result;
use console::style;

pub fn run() -> Result<()> {
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

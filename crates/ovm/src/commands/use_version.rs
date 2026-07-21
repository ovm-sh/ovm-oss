use crate::config::AutoUpdatePolicy;
use crate::error::Result;
use crate::mochi;
use crate::version_manager::VersionManager;
use console::style;

/// One-line heads-up that the just-made explicit selection pauses silent
/// auto-updates. Only relevant under policy `on` (the only policy whose launch
/// behavior a pin changes) and only when a pin is actually in place — the
/// follow-latest paths clear it before this runs, so they stay quiet.
pub fn note_pin(vm: &VersionManager) {
    if vm.config.auto_update.policy_for(vm.product()) != AutoUpdatePolicy::On {
        return;
    }
    let Some(pinned) = vm.read_pin() else {
        return;
    };
    eprintln!(
        "  {} Pinned {} at {} — launches will ask before auto-updating. {} resumes auto-updates.",
        style("→").dim(),
        vm.product().display_name(),
        style(&pinned).bold(),
        style(format!("ovm use {} latest", vm.product().canonical_name())).cyan()
    );
}

pub fn run(vm: &VersionManager, version: &str) -> Result<()> {
    vm.use_version(version)?;
    super::maintain_claude_launcher(vm);
    let active_version = vm
        .current_version()?
        .unwrap_or_else(|| vm.product().normalize_version(version));
    let msg = format!(
        "Now using {} {}",
        vm.product().display_name(),
        style(&active_version).green().bold()
    );
    eprintln!();
    for (i, line) in mochi::HAPPY.lines().enumerate() {
        if i == 1 {
            eprintln!("{}  {}", style(line).green(), msg);
        } else {
            eprintln!("{}", style(line).green());
        }
    }

    // A newer version may have migrated the shared on-disk state DB in a way this
    // one can't read; run optional product companions when installed so they can
    // warn before the user runs it degraded. Fail-open.
    crate::companions::run(
        &vm.dirs,
        vm.product(),
        crate::companions::Event::PostSwitch,
        &active_version,
        &vm.active_binary_path(&active_version),
    );
    Ok(())
}

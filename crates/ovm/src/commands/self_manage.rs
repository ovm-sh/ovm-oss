use crate::config::{OvmConfig, OvmDirs, SelfChannel};
use crate::error::{OvmError, Result};
use crate::self_manager::SelfManager;
use console::style;

/// Guard the one-way door: releases older than the self-managed floor have no
/// `ovm self` commands, so once active they cannot switch back. Interactive
/// runs get an explicit y/N confirmation; non-interactive runs refuse.
fn confirm_one_way_door(version: &str) -> Result<()> {
    if SelfManager::can_switch_back(version) {
        return Ok(());
    }
    let warning = format!(
        "OVM {version} predates self-management: once active it has no `ovm self` \
         commands, so it cannot switch back. Recovery means rerunning the installer."
    );
    let term = console::Term::stderr();
    if !term.is_term() {
        return Err(OvmError::Message(format!(
            "{warning} Refusing in non-interactive mode."
        )));
    }
    eprintln!("  {} {warning}", style("!").yellow().bold());
    eprint!("  Switch anyway? [y/N] ");
    let confirmed = matches!(term.read_char(), Ok('y') | Ok('Y'));
    eprintln!();
    if confirmed {
        Ok(())
    } else {
        Err(OvmError::Message("Switch cancelled.".into()))
    }
}

fn confirmed_rollback_target(
    manager: &SelfManager,
    confirm: impl FnOnce(&str) -> Result<()>,
) -> Result<String> {
    let previous = manager.previous_version()?.ok_or_else(|| {
        OvmError::Message("OVM has no previous self-managed version to restore".into())
    })?;
    confirm(&previous)?;
    Ok(previous)
}

/// Show or set the persistent self-update channel (`ovm self channel`).
pub fn channel(value: Option<&str>) -> Result<()> {
    let dirs = OvmDirs::new()?;
    let mut config = OvmConfig::load(&dirs.config_file)?;
    match value {
        None => {
            println!(
                "self-update channel: {}",
                style(config.self_.channel.label()).green()
            );
        }
        Some(value) => {
            let channel = SelfChannel::parse(value).ok_or_else(|| {
                OvmError::Message("Unknown self-update channel. Use `stable` or `alpha`.".into())
            })?;
            config.set_self_channel(channel);
            config.save(&dirs.config_file)?;
            println!(
                "{} self-update channel: {}",
                style("✓").green(),
                style(channel.label()).green()
            );
        }
    }
    Ok(())
}

pub fn current() -> Result<()> {
    let manager = SelfManager::new()?;
    let version = manager.current_version()?.ok_or_else(|| {
        OvmError::Message(
            "OVM is not using the direct self-managed install. Run the direct installer first."
                .into(),
        )
    })?;
    manager.require_complete(&version)?;
    println!("{version}");
    Ok(())
}

pub fn list() -> Result<()> {
    let manager = SelfManager::new()?;
    let versions = manager.list_versions()?;
    if versions.is_empty() {
        return Err(OvmError::Message(
            "No self-managed OVM versions are installed.".into(),
        ));
    }

    let current = manager.current_version()?;
    let previous = manager.previous_version()?;
    for version in versions {
        let marker = if current.as_deref() == Some(&version) {
            style("*").green().bold().to_string()
        } else if previous.as_deref() == Some(&version) {
            style("-").yellow().to_string()
        } else {
            " ".into()
        };
        let label = if current.as_deref() == Some(&version) {
            " current"
        } else if previous.as_deref() == Some(&version) {
            " previous"
        } else {
            ""
        };
        println!("{marker} {version}{label}");
    }
    Ok(())
}

pub fn use_version(version: &str) -> Result<()> {
    confirm_one_way_door(version)?;
    let manager = SelfManager::new()?;
    let _operation = manager.acquire_operation_lock()?;
    // The switch's rollback snapshot lives only in memory — a Ctrl-C mid-swap
    // must not strand half-updated selection state.
    let _signals = crate::commands::self_update::BlockedTerminationSignals::new()?;
    manager.use_version(version)?;
    println!(
        "{} OVM will use {} on the next command",
        style("✓").green(),
        style(version).bold()
    );
    Ok(())
}

pub fn rollback() -> Result<()> {
    let manager = SelfManager::new()?;
    let _operation = manager.acquire_operation_lock()?;
    // Resolve and guard the exact rollback target while holding the operation
    // lock so another self-management command cannot change `previous` between
    // confirmation and activation.
    let target = confirmed_rollback_target(&manager, confirm_one_way_door)?;
    // The switch's rollback snapshot lives only in memory — a Ctrl-C mid-swap
    // must not strand half-updated selection state.
    let _signals = crate::commands::self_update::BlockedTerminationSignals::new()?;
    let version = manager.rollback_to(&target)?;
    println!(
        "{} OVM rolled back to {}",
        style("✓").green(),
        style(version).bold()
    );
    Ok(())
}

pub fn repair_control() -> Result<()> {
    let manager = SelfManager::new()?;
    let _operation = manager.acquire_operation_lock()?;
    // The switch's rollback snapshot lives only in memory — a Ctrl-C mid-swap
    // must not strand half-updated selection state.
    let _signals = crate::commands::self_update::BlockedTerminationSignals::new()?;
    manager.repair_control_plane()?;
    println!(
        "{} Restored the previous OVM control plane",
        style("✓").green()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OvmDirs;
    use std::cell::Cell;
    use std::os::unix::fs::symlink;

    #[test]
    fn rollback_checks_the_exact_previous_version_before_switching() {
        let temp = tempfile::tempdir().unwrap();
        let manager = SelfManager::at(OvmDirs::at(temp.path().join(".ovm")));
        std::fs::create_dir_all(&manager.dirs.versions).unwrap();
        let current = manager.version_dir("0.0.3-alpha.2");
        let previous = manager.version_dir("0.0.2");
        std::fs::create_dir_all(&current).unwrap();
        std::fs::create_dir_all(&previous).unwrap();
        symlink(&current, &manager.dirs.current).unwrap();
        symlink(&previous, &manager.dirs.previous).unwrap();

        let called = Cell::new(false);
        let error = confirmed_rollback_target(&manager, |version| {
            called.set(true);
            assert_eq!(version, "0.0.2");
            Err(OvmError::Message("blocked one-way rollback".into()))
        })
        .unwrap_err();

        assert!(called.get());
        assert!(error.to_string().contains("blocked one-way rollback"));
        assert_eq!(
            manager.current_version().unwrap().as_deref(),
            Some("0.0.3-alpha.2")
        );
        assert_eq!(
            manager.previous_version().unwrap().as_deref(),
            Some("0.0.2")
        );
    }
}

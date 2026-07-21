use crate::config::{OvmConfig, OvmDirs, SelfChannel};
use crate::error::{OvmError, Result};
use crate::self_manager::SelfManager;
use console::style;

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
            config.self_.channel = channel;
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
    // The switch's rollback snapshot lives only in memory — a Ctrl-C mid-swap
    // must not strand half-updated selection state.
    let _signals = crate::commands::self_update::BlockedTerminationSignals::new()?;
    let version = manager.rollback()?;
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

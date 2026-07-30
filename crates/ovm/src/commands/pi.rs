use crate::error::{OvmError, Result};
use crate::product::Product;
use crate::version_manager::{InstallRequest, VersionManager};
use console::style;
use std::process::{Command, Stdio};

pub fn run(args: &[String]) -> Result<()> {
    if is_update_command(args) {
        return run_update(args);
    }

    super::launch::run(Product::Pi, args)
}

fn is_update_command(args: &[String]) -> bool {
    args.first().is_some_and(|arg| arg == "update")
}

fn run_update(args: &[String]) -> Result<()> {
    let vm = VersionManager::new(Product::Pi)?;

    match update_intent(args) {
        UpdateIntent::ExtensionsThenSelf => {
            run_active_pi(&vm, &["update", "--extensions"])?;
            update_self_with_ovm(&vm)
        }
        UpdateIntent::SelfOnly => update_self_with_ovm(&vm),
        UpdateIntent::DelegateToPi => {
            let passthrough = args.iter().map(String::as_str).collect::<Vec<_>>();
            run_active_pi(&vm, &passthrough)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpdateIntent {
    ExtensionsThenSelf,
    SelfOnly,
    DelegateToPi,
}

fn update_intent(args: &[String]) -> UpdateIntent {
    if args.iter().any(|arg| arg == "--extensions") {
        return UpdateIntent::DelegateToPi;
    }

    match args.get(1).map(String::as_str) {
        None => UpdateIntent::ExtensionsThenSelf,
        Some("self" | "pi") if args.len() == 2 => UpdateIntent::SelfOnly,
        Some(_) => UpdateIntent::DelegateToPi,
    }
}

fn run_active_pi(vm: &VersionManager, args: &[&str]) -> Result<()> {
    let version = vm.current_version()?.ok_or(OvmError::NoActiveVersion)?;
    let binary = vm.active_binary_path(&version);
    if !vm.install_is_complete(&version) {
        return Err(OvmError::Message(format!(
            "Pi {version} is archived or incomplete. Reinstall with: {}",
            Product::Pi.install_example(&version)
        )));
    }

    let status = Command::new(&binary)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if status.success() {
        return Ok(());
    }

    Err(OvmError::Message(format!(
        "pi {} failed with exit code {}",
        args.join(" "),
        status
            .code()
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string())
    )))
}

fn update_self_with_ovm(vm: &VersionManager) -> Result<()> {
    let before = vm.current_version()?.ok_or(OvmError::NoActiveVersion)?;
    let installed = vm.install(InstallRequest::Standard {
        use_npm: false,
        version: "latest".to_string(),
    })?;

    vm.use_version(&installed)?;

    if installed == before {
        eprintln!("pi is already up to date (v{installed})");
    } else {
        eprintln!(
            "  {} Updated Pi {} -> {} {}",
            style("✓").green(),
            style(before).dim(),
            style(&installed).green().bold(),
            style("(via OVM)").dim()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{update_intent, UpdateIntent};

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn bare_update_updates_extensions_then_self() {
        assert_eq!(
            update_intent(&args(&["update"])),
            UpdateIntent::ExtensionsThenSelf
        );
    }

    #[test]
    fn self_sources_update_self_only() {
        assert_eq!(
            update_intent(&args(&["update", "self"])),
            UpdateIntent::SelfOnly
        );
        assert_eq!(
            update_intent(&args(&["update", "pi"])),
            UpdateIntent::SelfOnly
        );
    }

    #[test]
    fn package_and_extensions_updates_delegate_to_pi() {
        assert_eq!(
            update_intent(&args(&["update", "--extensions"])),
            UpdateIntent::DelegateToPi
        );
        assert_eq!(
            update_intent(&args(&["update", "npm:@localaicat/pi"])),
            UpdateIntent::DelegateToPi
        );
    }
}

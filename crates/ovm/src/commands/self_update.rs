mod direct;
pub(crate) use direct::{
    activate_release, resolved_latest_version, stage_latest, BlockedTerminationSignals,
};

use crate::bundle_manifest::BundleManifest;
use crate::config::{OvmConfig, OvmDirs, SelfChannel};
use crate::error::{OvmError, Result};
use console::style;
use semver::Version;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const BREW_STABLE_FORMULA: &str = "ovm";
const BREW_BETA_FORMULA: &str = "ovm-beta";
const BREW_TAP_STABLE: &str = "ovm-sh/ovm/ovm";
const BREW_TAP_BETA: &str = "ovm-sh/ovm/ovm-beta";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfUpdateChannel {
    Stable,
    Beta,
    Alpha,
}

impl SelfUpdateChannel {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "stable" => Some(Self::Stable),
            "beta" | "next" => Some(Self::Beta),
            "alpha" => Some(Self::Alpha),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Beta => "beta",
            Self::Alpha => "alpha",
        }
    }
}

impl From<SelfChannel> for SelfUpdateChannel {
    fn from(channel: SelfChannel) -> Self {
        match channel {
            SelfChannel::Stable => Self::Stable,
            SelfChannel::Alpha => Self::Alpha,
        }
    }
}

/// Resolve the effective channel for a `self update` run: an explicit
/// `--channel` flag always wins; otherwise the persisted `self.channel`
/// setting (default `stable`) applies. The flag accepts `beta` for the
/// package-manager prerelease lane even though only `stable`/`alpha` persist.
pub fn resolve_channel(flag: Option<&str>, configured: SelfChannel) -> Result<SelfUpdateChannel> {
    match flag {
        Some(value) => SelfUpdateChannel::parse(value).ok_or_else(|| {
            OvmError::Message(
                "Unknown self-update channel. Use `stable`, `alpha`, or `beta`.".into(),
            )
        }),
        None => Ok(configured.into()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfUpdateMethod {
    Auto,
    Direct,
    Brew,
    Cargo,
    Dev,
}

impl SelfUpdateMethod {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "direct" | "script" => Some(Self::Direct),
            "brew" | "homebrew" => Some(Self::Brew),
            "cargo" => Some(Self::Cargo),
            "dev" => Some(Self::Dev),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Direct => "direct",
            Self::Brew => "brew",
            Self::Cargo => "cargo",
            Self::Dev => "dev",
        }
    }
}

pub fn run(channel: Option<&str>, method: &str, dry_run: bool) -> Result<()> {
    let configured = OvmDirs::new()
        .and_then(|dirs| OvmConfig::load(&dirs.config_file))
        .map(|config| config.self_.channel)
        .unwrap_or_default();
    let channel = resolve_channel(channel, configured)?;
    let method = SelfUpdateMethod::parse(method).ok_or_else(|| {
        OvmError::Message(
            "Unknown self-update method. Use `auto`, `direct`, `brew`, `cargo`, or `dev`.".into(),
        )
    })?;
    let resolved = match method {
        SelfUpdateMethod::Auto => detect_install_method()?,
        explicit => explicit,
    };

    eprintln!(
        "  {} self-update channel={} method={}",
        style("→").cyan(),
        style(channel.label()).bold(),
        style(resolved.label()).bold()
    );

    match resolved {
        SelfUpdateMethod::Auto => unreachable!("auto is resolved above"),
        SelfUpdateMethod::Direct => direct::update(channel, dry_run),
        SelfUpdateMethod::Brew => update_with_brew(channel, dry_run),
        SelfUpdateMethod::Cargo => update_with_cargo(channel, dry_run),
        SelfUpdateMethod::Dev => update_dev_checkout(dry_run),
    }
}

fn detect_install_method() -> Result<SelfUpdateMethod> {
    let raw_exe = std::env::current_exe()?;
    let exe = std::fs::canonicalize(&raw_exe).unwrap_or(raw_exe);
    let text = exe.to_string_lossy();
    let self_manager = crate::self_manager::SelfManager::new()?;

    if self_manager.is_control_plane_executable(&exe)
        || self_manager.is_managed_version_executable(&exe)
    {
        return Ok(SelfUpdateMethod::Direct);
    }

    if text.contains("/target/debug/") || text.contains("/target/release/") {
        return Ok(SelfUpdateMethod::Dev);
    }

    if text.contains("/Cellar/ovm/") || text.contains("/Cellar/ovm-beta/") {
        return Ok(SelfUpdateMethod::Brew);
    }

    if is_under_cargo_home(&exe) {
        return Ok(SelfUpdateMethod::Cargo);
    }

    Err(OvmError::Message(format!(
        "Could not detect how ovm is installed at {}. Rerun with `--method direct`, `--method brew`, `--method cargo`, or `--method dev`.",
        exe.display()
    )))
}

fn is_under_cargo_home(exe: &Path) -> bool {
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".cargo")));
    cargo_home
        .map(|home| exe.starts_with(home.join("bin")))
        .unwrap_or(false)
}

fn update_with_brew(channel: SelfUpdateChannel, dry_run: bool) -> Result<()> {
    run_step(dry_run, "brew", &["update"])?;

    match channel {
        SelfUpdateChannel::Stable => {
            if brew_formula_installed(BREW_BETA_FORMULA) {
                run_step(dry_run, "brew", &["unlink", BREW_BETA_FORMULA])?;
            }
            if brew_formula_installed(BREW_STABLE_FORMULA) {
                run_step(dry_run, "brew", &["upgrade", BREW_STABLE_FORMULA])?;
            } else {
                run_step(dry_run, "brew", &["install", BREW_TAP_STABLE])?;
            }
            run_step(
                dry_run,
                "brew",
                &["link", "--overwrite", BREW_STABLE_FORMULA],
            )?;
        }
        // Alpha rides the same prerelease Homebrew formula as beta: alpha tags
        // publish the `ovm-beta` channel (see docs/dev-practice.md).
        SelfUpdateChannel::Beta | SelfUpdateChannel::Alpha => {
            if brew_formula_installed(BREW_STABLE_FORMULA) {
                run_step(dry_run, "brew", &["unlink", BREW_STABLE_FORMULA])?;
            }
            if brew_formula_installed(BREW_BETA_FORMULA) {
                run_step(dry_run, "brew", &["upgrade", BREW_BETA_FORMULA])?;
            } else {
                run_step(dry_run, "brew", &["install", BREW_TAP_BETA])?;
            }
            run_step(dry_run, "brew", &["link", "--overwrite", BREW_BETA_FORMULA])?;
        }
    }

    Ok(())
}

fn brew_formula_installed(formula: &str) -> bool {
    if let Ok(installed) = std::env::var("OVM_SELF_UPDATE_BREW_INSTALLED") {
        return installed
            .split(',')
            .map(str::trim)
            .any(|candidate| candidate == formula);
    }

    let Ok(mut child) = Command::new("brew")
        .args(["list", "--formula", formula])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };

    wait_with_timeout(&mut child, Duration::from_secs(5))
        .map(|status| status.success())
        .unwrap_or(false)
}

fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Err(_) => return None,
        }
    }
}

fn update_with_cargo(channel: SelfUpdateChannel, dry_run: bool) -> Result<()> {
    let manifest = BundleManifest::embedded()?;
    let release_version = direct::release_version(channel)?;

    for package in cargo_package_order(&manifest) {
        install_cargo_crate(&package, Some(&release_version), dry_run)?;
    }
    Ok(())
}

fn cargo_package_order(manifest: &BundleManifest) -> Vec<String> {
    let mut packages = manifest
        .side_entries()
        .filter_map(|entry| entry.cargo_package.clone())
        .collect::<Vec<_>>();
    packages.push(
        manifest
            .main()
            .cargo_package
            .clone()
            .expect("validated main bundle entry has a Cargo package"),
    );
    packages
}

fn install_cargo_crate(name: &str, version: Option<&str>, dry_run: bool) -> Result<()> {
    let mut args = vec!["install", name, "--locked", "--force"];
    if let Some(version) = version {
        args.push("--version");
        args.push(version);
    }
    run_step(dry_run, "cargo", &args)
}

fn update_dev_checkout(dry_run: bool) -> Result<()> {
    let root = dev_checkout_root()?;
    run_step_in_dir(dry_run, "git", &["pull", "--ff-only"], &root)?;
    run_step_in_dir(dry_run, "cargo", &["build", "--release"], &root)?;
    Ok(())
}

fn dev_checkout_root() -> Result<PathBuf> {
    let raw_exe = std::env::current_exe()?;
    let exe = std::fs::canonicalize(&raw_exe).unwrap_or(raw_exe);
    let Some(profile_dir) = exe.parent() else {
        return Err(dev_checkout_error(&exe));
    };
    let Some(target_dir) = profile_dir.parent() else {
        return Err(dev_checkout_error(&exe));
    };
    if target_dir.file_name().and_then(|name| name.to_str()) != Some("target") {
        return Err(dev_checkout_error(&exe));
    }
    let Some(root) = target_dir.parent() else {
        return Err(dev_checkout_error(&exe));
    };
    if !root.join("crates").join("ovm").join("Cargo.toml").exists() {
        return Err(dev_checkout_error(&exe));
    }
    Ok(root.to_path_buf())
}

fn dev_checkout_error(exe: &Path) -> OvmError {
    OvmError::Message(format!(
        "`--method dev` requires ovm to run from a checkout target dir; current executable is {}",
        exe.display()
    ))
}

fn latest_beta_newer_than_stable(versions: Vec<Version>) -> Option<Version> {
    let latest_stable = versions
        .iter()
        .filter(|version| version.pre.is_empty())
        .max()
        .cloned();

    versions
        .into_iter()
        .filter(|version| !version.pre.is_empty())
        .filter(|version| {
            latest_stable
                .as_ref()
                .map(|stable| version > stable)
                .unwrap_or(true)
        })
        .max()
}

fn run_step(dry_run: bool, program: &str, args: &[&str]) -> Result<()> {
    run_step_with_cwd(dry_run, program, args, None)
}

fn run_step_in_dir(dry_run: bool, program: &str, args: &[&str], cwd: &Path) -> Result<()> {
    run_step_with_cwd(dry_run, program, args, Some(cwd))
}

fn run_step_with_cwd(
    dry_run: bool,
    program: &str,
    args: &[&str],
    cwd: Option<&Path>,
) -> Result<()> {
    println!("{} {}", style(program).cyan(), args.join(" "));
    if dry_run {
        return Ok(());
    }

    let mut command = Command::new(program);
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }

    let status = command
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|error| OvmError::Message(format!("failed to run `{program}`: {error}")))?;

    if status.success() {
        Ok(())
    } else {
        Err(OvmError::Message(format!(
            "`{} {}` failed with exit status {}",
            program,
            args.join(" "),
            status
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        cargo_package_order, latest_beta_newer_than_stable, resolve_channel, SelfUpdateChannel,
        SelfUpdateMethod,
    };
    use crate::bundle_manifest::BundleManifest;
    use crate::config::SelfChannel;
    use semver::Version;

    #[test]
    fn parses_channels() {
        assert_eq!(
            SelfUpdateChannel::parse("stable"),
            Some(SelfUpdateChannel::Stable)
        );
        assert_eq!(
            SelfUpdateChannel::parse("beta"),
            Some(SelfUpdateChannel::Beta)
        );
        assert_eq!(
            SelfUpdateChannel::parse("next"),
            Some(SelfUpdateChannel::Beta)
        );
        assert_eq!(
            SelfUpdateChannel::parse("alpha"),
            Some(SelfUpdateChannel::Alpha)
        );
        assert_eq!(SelfUpdateChannel::parse("nightly"), None);
    }

    #[test]
    fn resolve_channel_flag_overrides_config() {
        // Flag wins over the persisted setting in both directions.
        assert_eq!(
            resolve_channel(Some("alpha"), SelfChannel::Stable).unwrap(),
            SelfUpdateChannel::Alpha
        );
        assert_eq!(
            resolve_channel(Some("stable"), SelfChannel::Alpha).unwrap(),
            SelfUpdateChannel::Stable
        );
        // The one-shot flag still reaches the package-manager beta lane.
        assert_eq!(
            resolve_channel(Some("beta"), SelfChannel::Stable).unwrap(),
            SelfUpdateChannel::Beta
        );
    }

    #[test]
    fn resolve_channel_falls_back_to_config() {
        assert_eq!(
            resolve_channel(None, SelfChannel::Alpha).unwrap(),
            SelfUpdateChannel::Alpha
        );
        assert_eq!(
            resolve_channel(None, SelfChannel::Stable).unwrap(),
            SelfUpdateChannel::Stable
        );
    }

    #[test]
    fn resolve_channel_rejects_unknown_flag() {
        assert!(resolve_channel(Some("nightly"), SelfChannel::Stable).is_err());
    }

    #[test]
    fn parses_methods() {
        assert_eq!(
            SelfUpdateMethod::parse("auto"),
            Some(SelfUpdateMethod::Auto)
        );
        assert_eq!(
            SelfUpdateMethod::parse("direct"),
            Some(SelfUpdateMethod::Direct)
        );
        assert_eq!(
            SelfUpdateMethod::parse("script"),
            Some(SelfUpdateMethod::Direct)
        );
        assert_eq!(
            SelfUpdateMethod::parse("brew"),
            Some(SelfUpdateMethod::Brew)
        );
        assert_eq!(
            SelfUpdateMethod::parse("homebrew"),
            Some(SelfUpdateMethod::Brew)
        );
        assert_eq!(
            SelfUpdateMethod::parse("cargo"),
            Some(SelfUpdateMethod::Cargo)
        );
        assert_eq!(SelfUpdateMethod::parse("dev"), Some(SelfUpdateMethod::Dev));
        assert_eq!(SelfUpdateMethod::parse("sparkle"), None);
    }

    #[test]
    fn cargo_package_order_is_manifest_driven_and_main_last() {
        let manifest = BundleManifest::parse(
            "ovm-bundle-v1\nmain\tovm\tovm\nside\tovm-first\tovm-first\nside\tovm-path-only\t-\nside\tovm-future\tovm-future\n",
        )
        .unwrap();
        assert_eq!(
            cargo_package_order(&manifest),
            vec!["ovm-first", "ovm-future", "ovm"]
        );
    }

    #[test]
    fn latest_beta_must_be_newer_than_latest_stable() {
        let versions = ["0.0.2-beta.1", "0.0.3"]
            .into_iter()
            .map(|version| Version::parse(version).unwrap())
            .collect();
        assert_eq!(latest_beta_newer_than_stable(versions), None);

        let versions = ["0.0.1", "0.0.2-beta.1"]
            .into_iter()
            .map(|version| Version::parse(version).unwrap())
            .collect();
        assert_eq!(
            latest_beta_newer_than_stable(versions),
            Some(Version::parse("0.0.2-beta.1").unwrap())
        );
    }
}

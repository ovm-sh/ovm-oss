//! `ovm claudex doctor` — one screen answering "why doesn't claudex work?".

use crate::config::ClaudexConfig;
use crate::paths::{display, ClaudexDirs};
use crate::{proxy, Result};
use console::style;

pub fn run() -> Result<()> {
    let dirs = ClaudexDirs::new()?;
    let mut healthy = true;

    eprintln!();
    eprintln!("  {}  claudex doctor", style("(≈^.^≈)").magenta());
    eprintln!();

    let config = match ClaudexConfig::load(&dirs.config_file())? {
        Some(config) => {
            ok(&format!("config: {}", display(&dirs.config_file())));
            Some(config)
        }
        None => {
            bad("config missing — run: ovm claudex setup", &mut healthy);
            None
        }
    };

    if dirs.claude_home().join(".claude.json").is_file() {
        ok(&format!(
            "isolated Claude home: {}",
            display(&dirs.claude_home())
        ));
    } else {
        bad(
            "isolated Claude home not seeded — run: ovm claudex setup",
            &mut healthy,
        );
    }

    if let Some(config) = &config {
        match proxy::resolve_binary(&dirs, config) {
            Some(binary) => ok(&format!(
                "cliproxyapi {} ({})",
                binary.version_label(),
                display(binary.path())
            )),
            None => bad(
                "cliproxyapi not found — run: ovm claudex setup (or `brew install cliproxyapi`)",
                &mut healthy,
            ),
        }

        let mut proxy_verified = false;
        match proxy::probe(
            config.proxy.port,
            &config.proxy.api_key,
            proxy::ProbeIdentity::Pidfile(&dirs),
        ) {
            proxy::ProxyProbe::Verified => {
                proxy_verified = true;
                ok(&format!(
                    "proxy verified on 127.0.0.1:{} (answers /v1/models with our key)",
                    config.proxy.port
                ));
                check_model_registry(config, &mut healthy);
            }
            proxy::ProxyProbe::ForeignListener(why) => bad(
                &format!(
                    "port 127.0.0.1:{} is occupied by something that isn't our proxy ({why})",
                    config.proxy.port
                ),
                &mut healthy,
            ),
            proxy::ProxyProbe::Unverified(why) => bad(
                &format!(
                    "port 127.0.0.1:{} has a listener claudex could not verify as its proxy \
                     ({why}). {}",
                    config.proxy.port,
                    proxy::unverified_identity_remedy(config.proxy.port, &why)
                ),
                &mut healthy,
            ),
            proxy::ProxyProbe::Down => {
                eprintln!(
                    "  {} proxy not running (will start on next launch)",
                    style("—").dim()
                );
            }
        }

        check_credentials(&dirs, &mut healthy);
        if proxy_verified && proxy::has_oauth_grant(&dirs) {
            check_live_credential(config, &mut healthy);
        }

        if let Some(pin) = &config.pin {
            eprintln!(
                "  {} pinned pair: claude {} + proxy {}",
                style("⚲").yellow(),
                pin.claude,
                pin.proxy
            );
        }
    }

    match claude_version() {
        Some(version) => ok(&format!("active Claude Code: {version}")),
        None => bad(
            "no active Claude Code — run: ovm install claude",
            &mut healthy,
        ),
    }

    eprintln!();
    if healthy {
        eprintln!("  {} All good.", style("✓").green().bold());
    } else {
        eprintln!("  {} Problems found — see above.", style("✗").red().bold());
        std::process::exit(1);
    }
    Ok(())
}

/// What the live proxy's model list says about the configured registry.
#[derive(Debug, PartialEq, Eq)]
enum RegistryStatus {
    /// Nothing to check against: the proxy advertises no models at all, or the
    /// config names none. Either way the green line would be a claim about an
    /// empty set, so it is never printed.
    NothingToVerify,
    /// Configured tier models the proxy does not expose.
    Missing(Vec<String>),
    /// Tier models resolve, but the `-fast` aliases are not exposed yet.
    FastAliasesMissing,
    Ready,
}

/// The distinct tier models the config asks the proxy to serve.
fn required_models(config: &ClaudexConfig) -> Vec<String> {
    let mut required: Vec<String> = vec![
        config.models.opus.clone(),
        config.models.sonnet.clone(),
        config.models.haiku.clone(),
        config.models.default.clone(),
        config.models.subagent.clone(),
    ]
    .into_iter()
    .filter(|model| !model.trim().is_empty())
    .collect();
    required.sort();
    required.dedup();
    required
}

/// Compare the configured tier models against what the proxy actually serves.
///
/// Both sets must be non-empty before anything can be called verified: an
/// "all of them are present" test over an empty set is vacuously true, which
/// would let doctor report a healthy registry against a proxy serving nothing.
fn model_registry_status(config: &ClaudexConfig, available: &[String]) -> RegistryStatus {
    let required = required_models(config);
    if required.is_empty() || available.is_empty() {
        return RegistryStatus::NothingToVerify;
    }

    let missing: Vec<String> = required
        .iter()
        .filter(|model| !available.contains(model))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return RegistryStatus::Missing(missing);
    }
    if !required
        .iter()
        .all(|model| available.contains(&format!("{model}-fast")))
    {
        return RegistryStatus::FastAliasesMissing;
    }
    RegistryStatus::Ready
}

/// Every registry model — and its fast alias — must be selectable through
/// the live proxy, or `/model` choices will 502 at first use.
fn check_model_registry(config: &ClaudexConfig, healthy: &mut bool) {
    let Some(available) = proxy::list_models(config.proxy.port, &config.proxy.api_key) else {
        bad("could not list models from the proxy", healthy);
        return;
    };

    match model_registry_status(config, &available) {
        RegistryStatus::Ready => {
            ok("model registry live: all tier models + fast aliases selectable")
        }
        RegistryStatus::FastAliasesMissing => eprintln!(
            "  {} fast aliases not exposed yet — re-run `ovm claudex setup` to regenerate the proxy config",
            style("!").yellow()
        ),
        RegistryStatus::NothingToVerify => bad(
            "the proxy advertises no models (or none are configured) — nothing is selectable; \
             check the Codex OAuth grant, then re-run: ovm claudex setup",
            healthy,
        ),
        RegistryStatus::Missing(missing) => bad(
            &format!(
                "registry models not available on this account/proxy: {}",
                missing.join(", ")
            ),
            healthy,
        ),
    }
}

/// A grant file can be long dead — OpenAI invalidates the refresh-token
/// family when the same account logs in through the Codex CLI, and every
/// static check stays green (2026-08-17: doctor said "All good" for a grant
/// the upstream had been rejecting for hours). Exercise it for real.
fn check_live_credential(config: &ClaudexConfig, healthy: &mut bool) {
    match proxy::probe_codex_credential(
        config.proxy.port,
        &config.proxy.api_key,
        &config.models.default,
    ) {
        proxy::CredentialProbe::Working => {
            ok("Codex credential answers upstream (live completion)")
        }
        proxy::CredentialProbe::Rejected(why) => bad(
            &format!("Codex credential rejected upstream ({why}) — reconnect: ovm claudex setup"),
            healthy,
        ),
        proxy::CredentialProbe::Inconclusive(why) => eprintln!(
            "  {} could not verify the Codex credential upstream ({why})",
            style("!").yellow()
        ),
    }
}

/// The OAuth grant must exist and credential storage must stay owner-only.
fn check_credentials(dirs: &ClaudexDirs, healthy: &mut bool) {
    let auth_dir = dirs.proxy_auth_dir();
    let has_grant = crate::proxy::has_oauth_grant(dirs);
    if has_grant {
        ok("Codex OAuth grant on disk");
    } else {
        bad("no Codex OAuth grant — run: ovm claudex setup", healthy);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let world_open = |path: &std::path::Path, want: u32| {
            std::fs::metadata(path)
                .map(|meta| {
                    meta.permissions().mode() & 0o077 != 0
                        || meta.permissions().mode() & 0o777 > want
                })
                .unwrap_or(false)
        };
        if world_open(&auth_dir, 0o700) {
            bad(
                &format!(
                    "{} is group/world-accessible — run: chmod 700 {}",
                    display(&auth_dir),
                    display(&auth_dir)
                ),
                healthy,
            );
        }
        let config_file = dirs.config_file();
        if config_file.exists() && world_open(&config_file, 0o600) {
            bad(
                &format!(
                    "{} is group/world-readable — run: chmod 600 {}",
                    display(&config_file),
                    display(&config_file)
                ),
                healthy,
            );
        }
    }
}

fn ok(message: &str) {
    eprintln!("  {} {message}", style("✓").green());
}

fn bad(message: &str, healthy: &mut bool) {
    *healthy = false;
    eprintln!("  {} {message}", style("✗").red());
}

fn claude_version() -> Option<String> {
    let output = std::process::Command::new("ovm")
        .args(["current", "claude"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!version.is_empty()).then_some(version)
}

#[cfg(test)]
mod tests {
    use super::{model_registry_status, required_models, RegistryStatus};
    use crate::config::ClaudexConfig;

    fn models(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    /// A proxy serving nothing must never read as a verified registry: with an
    /// empty available set, "every required model is present" is only true
    /// because there is nothing to check.
    #[test]
    fn a_proxy_serving_no_models_is_never_reported_ready() {
        let config = ClaudexConfig::default();
        assert_eq!(
            model_registry_status(&config, &[]),
            RegistryStatus::NothingToVerify
        );
    }

    /// Same guard from the other side: a config that names no tier models has
    /// nothing to verify either.
    #[test]
    fn a_config_naming_no_models_is_never_reported_ready() {
        let mut config = ClaudexConfig::default();
        config.models.opus = String::new();
        config.models.sonnet = "   ".into();
        config.models.haiku = String::new();
        config.models.default = String::new();
        config.models.subagent = String::new();
        assert!(required_models(&config).is_empty());
        assert_eq!(
            model_registry_status(&config, &models(&["gpt-5.6-sol"])),
            RegistryStatus::NothingToVerify
        );
    }

    #[test]
    fn a_partially_stocked_proxy_names_the_missing_models() {
        let config = ClaudexConfig::default();
        match model_registry_status(&config, &models(&["gpt-5.6-sol", "gpt-5.6-sol-fast"])) {
            RegistryStatus::Missing(missing) => {
                assert!(
                    missing.contains(&"gpt-5.6-terra".to_string()),
                    "{missing:?}"
                );
                assert!(missing.contains(&"gpt-5.6-luna".to_string()), "{missing:?}");
                assert!(!missing.contains(&"gpt-5.6-sol".to_string()), "{missing:?}");
            }
            other => panic!("expected missing models, got {other:?}"),
        }
    }

    #[test]
    fn tier_models_without_fast_aliases_are_not_ready() {
        let config = ClaudexConfig::default();
        assert_eq!(
            model_registry_status(
                &config,
                &models(&["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"])
            ),
            RegistryStatus::FastAliasesMissing
        );
    }

    #[test]
    fn a_fully_stocked_proxy_is_ready() {
        let config = ClaudexConfig::default();
        assert_eq!(
            model_registry_status(
                &config,
                &models(&[
                    "gpt-5.6-sol",
                    "gpt-5.6-terra",
                    "gpt-5.6-luna",
                    "gpt-5.6-sol-fast",
                    "gpt-5.6-terra-fast",
                    "gpt-5.6-luna-fast",
                ])
            ),
            RegistryStatus::Ready
        );
    }
}

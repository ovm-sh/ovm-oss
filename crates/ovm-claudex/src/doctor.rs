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

        match proxy::probe(
            config.proxy.port,
            &config.proxy.api_key,
            proxy::ProbeIdentity::Pidfile(&dirs),
        ) {
            proxy::ProxyProbe::Verified => {
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
                     ({why}); run `ovm claudex stop` and relaunch",
                    config.proxy.port
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

/// Every registry model — and its fast alias — must be selectable through
/// the live proxy, or `/model` choices will 502 at first use.
fn check_model_registry(config: &ClaudexConfig, healthy: &mut bool) {
    let Some(available) = proxy::list_models(config.proxy.port, &config.proxy.api_key) else {
        bad("could not list models from the proxy", healthy);
        return;
    };

    let mut required: Vec<String> = vec![
        config.models.opus.clone(),
        config.models.sonnet.clone(),
        config.models.haiku.clone(),
        config.models.default.clone(),
        config.models.subagent.clone(),
    ];
    required.sort();
    required.dedup();

    let missing: Vec<&String> = required
        .iter()
        .filter(|model| !available.contains(model))
        .collect();
    if missing.is_empty() {
        let fast_ready = required
            .iter()
            .all(|model| available.contains(&format!("{model}-fast")));
        if fast_ready {
            ok("model registry live: all tier models + fast aliases selectable");
        } else {
            eprintln!(
                "  {} fast aliases not exposed yet — re-run `ovm claudex setup` to regenerate the proxy config",
                style("!").yellow()
            );
        }
    } else {
        bad(
            &format!(
                "registry models not available on this account/proxy: {}",
                missing
                    .iter()
                    .map(|model| model.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            healthy,
        );
    }
}

/// The OAuth grant must exist and credential storage must stay owner-only.
fn check_credentials(dirs: &ClaudexDirs, healthy: &mut bool) {
    let auth_dir = dirs.proxy_auth_dir();
    let has_grant = std::fs::read_dir(&auth_dir)
        .map(|entries| entries.flatten().any(|entry| entry.path().is_file()))
        .unwrap_or(false);
    if has_grant {
        ok("Codex OAuth grant present");
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

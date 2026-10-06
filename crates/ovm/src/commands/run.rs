//! `ovm run [account] [claude args…]` — Claude Code as one of your accounts,
//! and `ovm accounts …` to manage them. See [`crate::accounts`].
//!
//! Several accounts can run at the same moment, one per terminal. Moving a
//! session is a choice made by hand: exit, then `ovm run <other> --resume
//! <session id>` — transcripts are shared, so the other account picks up the
//! same conversation. Nothing here ever switches accounts on its own.

use crate::accounts::{self, Accounts, ApiProvider, Binding, Entry, Layout};
use crate::config::OvmDirs;
use crate::error::{OvmError, Result};
use console::style;
use std::path::{Path, PathBuf};

/// The environment variable Echo reads to show which account a session is.
pub const ACCOUNT_ENV: &str = "OVM_ACCOUNT";

pub fn run(args: &[String]) -> Result<()> {
    let dirs = OvmDirs::new()?;
    let layout = Layout::new(&dirs.base)?;
    let registry = Accounts::load(&layout.file)?;
    // Bindings are stored canonical; compare like with like.
    let cwd = std::env::current_dir()?;
    let cwd = cwd.canonicalize().unwrap_or(cwd);

    // The first argument is an account when one by that name exists;
    // otherwise everything goes to Claude Code and the directory decides.
    // When failover is configured, the chain replaces the default.
    let limits_dir = dirs.base.join("limits");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    let (label, claude_args, why) = match args.first() {
        Some(first) if registry.accounts.contains_key(first) => (
            Some(first.clone()),
            &args[1..],
            "named on the command line".to_string(),
        ),
        _ => match registry.resolve_with_failover(&cwd, &limits_dir, now) {
            Some(pick) => {
                narrate_skipped(&pick.skipped);
                (Some(pick.label), args, pick.why)
            }
            None => (None, args, String::new()),
        },
    };

    let Some(label) = label else {
        if let Some(first) = args.first().filter(|a| !a.starts_with('-')) {
            return Err(OvmError::Message(format!(
                "no account called `{first}` — see: ovm accounts list (add one: ovm accounts add {first})"
            )));
        }
        eprintln!(
            "  {} no account bound here and no default — running as ~/.claude",
            style("→").dim()
        );
        return super::claude::run(claude_args);
    };
    if !registry.accounts.contains_key(&label) {
        return Err(OvmError::Message(format!(
            "`{label}` is bound or default but not an account — see: ovm accounts list"
        )));
    }

    enter(&layout, &registry, &label, &why)?;
    super::claude::run(claude_args)
}

/// One line naming the chain accounts a failover walk passed over, each with
/// the window that decided: `↷ skipped a (5h 96% ≥ 95%), b (7d 99% ≥ 95%)`.
fn narrate_skipped(skipped: &[accounts::Skip]) {
    if skipped.is_empty() {
        return;
    }
    let list: Vec<String> = skipped
        .iter()
        .map(|skip| format!("{} ({})", skip.label, skip.reading))
        .collect();
    eprintln!("  {} skipped {}", style("↷").dim(), list.join(", "));
}

/// Point this process at an account: its folder prepared, `CLAUDE_CONFIG_DIR`
/// and [`ACCOUNT_ENV`] set for the Claude Code about to be exec'd, a line
/// saying who and why, and — once signed in — the folder handed to limits.
fn enter(layout: &Layout, registry: &Accounts, label: &str, why: &str) -> Result<()> {
    let entry = &registry.accounts[label];
    let folder = layout.folder(label);
    accounts::prepare(layout, &folder)?;
    let login = accounts::login(&folder.join(".claude.json"));

    // A paid API account: inject the provider env vars and show a billing
    // notice. No OAuth login required — the key or profile is the credential.
    if let Some(provider) = &entry.provider {
        eprintln!(
            "  {} {label} ({}) — {why}",
            style("→").dim(),
            provider.display_name()
        );
        eprintln!(
            "  {} this account uses {} API billing — usage is metered by your cloud provider",
            style("$").yellow().bold(),
            provider.display_name()
        );
        provider.set_env();
    } else {
        match &login {
            Some(login) => eprintln!(
                "  {} {label} ({}, {}) — {why}",
                style("→").dim(),
                login.email.as_deref().unwrap_or("signed in"),
                accounts::kind(Some(login), entry)
            ),
            None => {
                eprintln!("  {} {label} (not signed in) — {why}", style("→").dim());
                eprintln!(
                    "  {} {label} is not signed in yet — type {} in the session that opens",
                    style("!").yellow(),
                    style("/login").bold()
                );
            }
        }
    }
    if login.is_some() && entry.provider.is_none() {
        watch_in_limits(label, &folder, entry.kind.as_deref());
    }
    // Launch-time only: this process execs Claude Code and does nothing else,
    // so the environment is set before any thread exists.
    std::env::set_var("CLAUDE_CONFIG_DIR", &folder);
    std::env::set_var(ACCOUNT_ENV, label);
    Ok(())
}

/// A plain Claude launch (`claude`, `ovm claude`, the yolo shortcuts) started
/// in a bound directory runs as the bound account. Only a binding — a choice
/// made for that directory up front — and never the default, which stays an
/// `ovm run` notion; and never when `CLAUDE_CONFIG_DIR` is already set, so a
/// limits poll, `ovm run` itself, or anyone who chose a home keeps it. Best
/// effort: a broken accounts file leaves the launch as it would have been.
pub fn apply_binding() {
    if std::env::var_os("CLAUDE_CONFIG_DIR").is_some() {
        return;
    }
    let Ok(dirs) = OvmDirs::new() else { return };
    let Ok(layout) = Layout::new(&dirs.base) else {
        return;
    };
    if !layout.file.is_file() {
        return;
    }
    let Ok(registry) = Accounts::load(&layout.file) else {
        return;
    };
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    let Some((label, why)) = registry.bound(&cwd) else {
        return;
    };
    if !registry.accounts.contains_key(&label) {
        return;
    }
    if let Err(error) = enter(&layout, &registry, &label, &why) {
        eprintln!(
            "  {} {label} is bound here but could not be used ({error}) — running as ~/.claude",
            style("!").yellow()
        );
        std::env::remove_var("CLAUDE_CONFIG_DIR");
        std::env::remove_var(ACCOUNT_ENV);
    }
}

/// Make a signed-in account folder visible in `ovm limits`, as a live-only
/// account: its numbers come from the statusline of the sessions it runs, and
/// nothing ever polls it (a poll would rotate the login those sessions hold).
/// `ovm limits add` refuses a login it already watches, so this is quiet on
/// every launch after the first, and for a folder whose login is already a
/// polled account. Best effort: `ovm run` never fails over it.
fn watch_in_limits(label: &str, folder: &Path, kind: Option<&str>) {
    let Some(limits) = crate::plugins::find_for_dispatch("limits") else {
        return;
    };
    let mut command = std::process::Command::new(limits);
    command
        .args([
            "add",
            "claude",
            label,
            "--live-only",
            "--no-login",
            "--home",
        ])
        .arg(folder)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match kind {
        Some("team") => {
            command.arg("--team");
        }
        Some("personal") => {
            command.arg("--personal");
        }
        _ => {}
    }
    if command.status().is_ok_and(|status| status.success()) {
        eprintln!(
            "  {} {label} is now in ovm limits (live-only: its sessions report its windows)",
            style("→").dim()
        );
    }
}

pub fn accounts_command(args: &[String]) -> Result<()> {
    let dirs = OvmDirs::new()?;
    let layout = Layout::new(&dirs.base)?;
    let mut registry = Accounts::load(&layout.file)?;
    let usage = || {
        OvmError::Message(
            "usage: ovm accounts [list] | add <label> [--team|--personal] \
             | add <label> --azure <endpoint> [--api-key <key>] \
             | add <label> --bedrock <region> [--profile <name>] \
             | remove <label> | bind <dir> <label> | unbind <dir> | default <label|none> \
             | failover <label1> <label2> … | failover none | threshold <percent>"
                .into(),
        )
    };
    match args.first().map(String::as_str) {
        None | Some("list") => {
            list(&layout, &registry);
            Ok(())
        }
        Some("add") => {
            let label = args.get(1).ok_or_else(usage)?;
            accounts::validate_label(label)?;
            let (kind, provider) = parse_add_flags(&args[2..])?;
            registry.accounts.insert(
                label.clone(),
                Entry {
                    kind,
                    provider: provider.clone(),
                },
            );
            let prepared = accounts::prepare(&layout, &layout.folder(label))?;
            registry.save(&layout.file)?;
            let shares = if prepared.linked.is_empty() {
                String::new()
            } else {
                format!(" (shares {})", prepared.linked.join(", "))
            };
            eprintln!(
                "  {} {label} → {}{shares}",
                style("✓").green(),
                layout.folder(label).display(),
            );
            match &provider {
                Some(p) => eprintln!(
                    "    {} API account — usage is billed by {}",
                    p.display_name(),
                    p.display_name()
                ),
                None => eprintln!("    sign it in once: ovm run {label}, then /login"),
            }
            Ok(())
        }
        Some("remove") => {
            let label = args.get(1).ok_or_else(usage)?;
            if registry.accounts.remove(label).is_none() {
                return Err(OvmError::Message(format!("no account `{label}`")));
            }
            registry.bindings.retain(|b| &b.account != label);
            if registry.default.as_deref() == Some(label) {
                registry.default = None;
            }
            registry.failover.chain.retain(|l| l != label);
            registry.save(&layout.file)?;
            // The folder holds a login; deleting it is the person's call.
            eprintln!(
                "  {} {label} forgotten; its folder (and login) is still at {}",
                style("✓").green(),
                layout.folder(label).display()
            );
            Ok(())
        }
        Some("bind") => {
            let (Some(dir), Some(label)) = (args.get(1), args.get(2)) else {
                return Err(usage());
            };
            if !registry.accounts.contains_key(label) {
                return Err(OvmError::Message(format!("no account `{label}`")));
            }
            let dir = absolute(Path::new(dir))?;
            registry.bindings.retain(|b| b.dir != dir);
            registry.bindings.push(Binding {
                dir: dir.clone(),
                account: label.clone(),
            });
            registry.save(&layout.file)?;
            eprintln!("  {} {} → {label}", style("✓").green(), dir.display());
            Ok(())
        }
        Some("unbind") => {
            let dir = absolute(Path::new(args.get(1).ok_or_else(usage)?))?;
            let before = registry.bindings.len();
            registry.bindings.retain(|b| b.dir != dir);
            if registry.bindings.len() == before {
                return Err(OvmError::Message(format!(
                    "nothing is bound at {}",
                    dir.display()
                )));
            }
            registry.save(&layout.file)?;
            eprintln!("  {} {} unbound", style("✓").green(), dir.display());
            Ok(())
        }
        Some("default") => {
            let label = args.get(1).ok_or_else(usage)?;
            if label == "none" {
                registry.default = None;
            } else if registry.accounts.contains_key(label) {
                registry.default = Some(label.clone());
            } else {
                return Err(OvmError::Message(format!("no account `{label}`")));
            }
            registry.save(&layout.file)?;
            eprintln!("  {} default: {label}", style("✓").green());
            Ok(())
        }
        Some("failover") => {
            let first = args.get(1).ok_or_else(usage)?;
            if first == "none" {
                registry.failover.chain.clear();
                registry.save(&layout.file)?;
                eprintln!("  {} failover disabled", style("✓").green());
                return Ok(());
            }
            let labels: Vec<String> = args[1..].to_vec();
            for label in &labels {
                if !registry.accounts.contains_key(label) {
                    return Err(OvmError::Message(format!(
                        "no account `{label}` — add it first: ovm accounts add {label}"
                    )));
                }
            }
            registry.failover.chain = labels;
            registry.save(&layout.file)?;
            eprintln!(
                "  {} failover: {} (threshold: {:.0}%)",
                style("✓").green(),
                registry.failover.chain.join(" → "),
                registry.failover.threshold
            );
            Ok(())
        }
        Some("threshold") => {
            let value = args
                .get(1)
                .ok_or_else(usage)?
                .trim_end_matches('%')
                .parse::<f64>()
                .map_err(|_| {
                    OvmError::Message("threshold must be a number (e.g. 95 or 95%)".into())
                })?;
            if !(1.0..=100.0).contains(&value) {
                return Err(OvmError::Message(
                    "threshold must be between 1 and 100".into(),
                ));
            }
            registry.failover.threshold = value;
            registry.save(&layout.file)?;
            eprintln!("  {} threshold: {value:.0}%", style("✓").green());
            Ok(())
        }
        Some(_) => Err(usage()),
    }
}

fn list(layout: &Layout, registry: &Accounts) {
    let main = accounts::login(&layout.main_json);
    println!(
        "  {:<14} {:<9} {}",
        style("~/.claude").bold(),
        accounts::kind(main.as_ref(), &Entry::default()),
        describe(main.as_ref())
    );
    for (label, entry) in &registry.accounts {
        let default = if registry.default.as_deref() == Some(label) {
            "  (default)"
        } else {
            ""
        };
        if let Some(provider) = &entry.provider {
            let detail = match provider {
                ApiProvider::Azure { endpoint, .. } => endpoint.clone(),
                ApiProvider::Bedrock { region, profile } => match profile {
                    Some(p) => format!("{region} ({p})"),
                    None => region.clone(),
                },
            };
            println!(
                "  {:<14} {:<9} {}{default}",
                style(label).bold(),
                provider.display_name(),
                detail
            );
        } else {
            let login = accounts::login(&layout.folder(label).join(".claude.json"));
            println!(
                "  {:<14} {:<9} {}{default}",
                style(label).bold(),
                accounts::kind(login.as_ref(), entry),
                describe(login.as_ref())
            );
        }
    }
    for binding in &registry.bindings {
        println!(
            "  {} {} → {}",
            style("bind").dim(),
            binding.dir.display(),
            binding.account
        );
    }
    if !registry.failover.is_empty() {
        println!(
            "  {} {} (threshold: {:.0}%)",
            style("failover").dim(),
            registry.failover.chain.join(" → "),
            registry.failover.threshold
        );
    }
    if registry.accounts.is_empty() {
        println!("  no accounts yet — add one: ovm accounts add <label>");
    }
}

fn describe(login: Option<&accounts::Login>) -> String {
    let Some(login) = login else {
        return style("not signed in").yellow().to_string();
    };
    let mut parts = vec![login.email.clone().unwrap_or_else(|| "signed in".into())];
    if let Some(org) = &login.org_name {
        parts.push(org.clone());
    }
    if let Some(tier) = login.seat_tier.as_ref().or(login.rate_limit_tier.as_ref()) {
        parts.push(tier.clone());
    }
    parts.join(" · ")
}

fn absolute(path: &Path) -> Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(joined.canonicalize().unwrap_or(joined))
}

/// Parse the flags after `ovm accounts add <label>`.
/// Returns `(kind, provider)` — at most one of the two is set.
fn parse_add_flags(flags: &[String]) -> Result<(Option<String>, Option<ApiProvider>)> {
    if flags.is_empty() {
        return Ok((None, None));
    }
    match flags[0].as_str() {
        "--team" => Ok((Some("team".into()), None)),
        "--personal" => Ok((Some("personal".into()), None)),
        "--azure" => {
            let endpoint = flags
                .get(1)
                .ok_or_else(|| OvmError::Message("--azure needs an endpoint URL".into()))?;
            let mut api_key = None;
            if flags.get(2).map(String::as_str) == Some("--api-key") {
                api_key = flags.get(3).cloned();
                if api_key.is_none() {
                    return Err(OvmError::Message("--api-key needs a value".into()));
                }
            }
            Ok((
                None,
                Some(ApiProvider::Azure {
                    endpoint: endpoint.clone(),
                    api_key,
                }),
            ))
        }
        "--bedrock" => {
            let region = flags.get(1).ok_or_else(|| {
                OvmError::Message("--bedrock needs a region (e.g. us-east-1)".into())
            })?;
            let mut profile = None;
            if flags.get(2).map(String::as_str) == Some("--profile") {
                profile = flags.get(3).cloned();
                if profile.is_none() {
                    return Err(OvmError::Message("--profile needs a value".into()));
                }
            }
            Ok((
                None,
                Some(ApiProvider::Bedrock {
                    region: region.clone(),
                    profile,
                }),
            ))
        }
        other => Err(OvmError::Message(format!(
            "unknown flag `{other}` — expected --team, --personal, --azure, or --bedrock"
        ))),
    }
}

//! `ovm limits doctor` — one screen answering "why is an account not polling?".

use crate::paths::{display, LimitsDirs};
use crate::registry::{Provider, Registry};
use crate::snapshot;
use crate::Result;
use console::style;

pub fn run(dirs: &LimitsDirs) -> Result<()> {
    let mut healthy = true;
    println!();
    println!("  {}  limits doctor", style("(≈^.^≈)").magenta());
    println!();

    let config_path = dirs.config_file();
    let registry = Registry::load(&config_path)?.unwrap_or_default();
    if registry.accounts.is_empty() {
        warn("no accounts yet — run: ovm limits (nothing is polled or written until then)");
    } else {
        ok(&format!(
            "registry: {} ({} account(s), a Claude poll at most every {} min)",
            display(&config_path),
            registry.accounts.len(),
            registry.interval_minutes
        ));
    }
    if cfg!(target_os = "macos") {
        ok(&format!("agent: {}", crate::agent::describe()));
    }

    match which("ovm") {
        true => ok("ovm on PATH (polls launch the OVM-selected claude / codex)"),
        false => bad(
            "ovm not on PATH — polls cannot launch the products",
            &mut healthy,
        ),
    }

    let snapshots = snapshot::load_snapshots(dirs)?;
    let now = snapshot::now();
    for account in &registry.accounts {
        println!();
        println!("  {}", style(account.display()).bold());
        let home = account.poll_home(dirs);
        let signed_in = account.signed_in(dirs);
        println!("     home: {}", display(&home));
        if signed_in {
            ok("signed in");
            if account.provider == Provider::Claude && cfg!(target_os = "macos") {
                warn("credential lives in this home's own Keychain entry: polls from SSH or launchd will look signed out");
            }
        } else {
            bad(
                &format!("not signed in — run: ovm limits login {}", account.id),
                &mut healthy,
            );
        }
        match snapshots
            .iter()
            .find(|s| s.provider == account.provider && s.id == account.id)
        {
            Some(snapshot) => {
                let age = crate::show::human(now.saturating_sub(snapshot.captured_at));
                match (&snapshot.error, signed_in) {
                    (Some(error), _) => bad(&format!("last poll failed: {error}"), &mut healthy),
                    (None, false) => warn(&format!(
                        "the {age}-old numbers predate this home's login — sign in and poll again"
                    )),
                    (None, true) => ok(&format!(
                        "last poll {age} ago, {} window(s)",
                        snapshot.windows.len()
                    )),
                }
            }
            None if signed_in => warn("never polled — run: ovm limits poll"),
            None => {}
        }
    }

    println!();
    if healthy {
        println!("  {}", style("all good").green());
    } else {
        println!("  {}", style("something needs attention").yellow());
    }
    println!();
    Ok(())
}

fn which(binary: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(binary).is_file()))
        .unwrap_or(false)
}

fn ok(text: &str) {
    println!("  {} {text}", style("✓").green());
}

fn warn(text: &str) {
    println!("  {} {text}", style("!").yellow());
}

fn bad(text: &str, healthy: &mut bool) {
    *healthy = false;
    println!("  {} {text}", style("✗").red());
}

//! The background poller: a per-user launchd agent that runs
//! `ovm limits poll --due` every five minutes. It runs inside the login
//! session, so Claude Code can reach its Keychain credential — the reason
//! this lives on the user's own Mac rather than on a headless box.
//!
//! macOS only for now. Elsewhere the same effect is a cron line calling
//! `ovm limits poll --due`.

use crate::paths::{display, LimitsDirs};
use crate::{LimitsError, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const LABEL: &str = "sh.ovm.limits";
pub const TICK_SECONDS: u64 = 300;

/// Test hooks: where the plist goes, and whether launchctl is actually called.
pub const AGENTS_DIR_ENV: &str = "OVM_LIMITS_LAUNCH_AGENTS_DIR";
pub const NO_LAUNCHCTL_ENV: &str = "OVM_LIMITS_NO_LAUNCHCTL";

pub fn plist_path() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os(AGENTS_DIR_ENV).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir).join(format!("{LABEL}.plist")));
    }
    let home = dirs::home_dir().ok_or_else(|| LimitsError::Message("no home directory".into()))?;
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

pub fn is_installed() -> bool {
    plist_path().map(|p| p.is_file()).unwrap_or(false)
}

fn supported() -> Result<()> {
    if cfg!(target_os = "macos") || std::env::var_os(NO_LAUNCHCTL_ENV).is_some() {
        return Ok(());
    }
    Err(LimitsError::Message(
        "the background agent is macOS-only for now — schedule `ovm limits poll --due` \
         from cron or a systemd user timer instead"
            .into(),
    ))
}

pub fn install(dirs: &LimitsDirs) -> Result<PathBuf> {
    supported()?;
    let path = plist_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let ovm = ovm_binary();
    let limits_home = std::env::var_os(crate::paths::HOME_ENV);
    for text in [
        ovm.to_string_lossy(),
        dirs.agent_log_file().to_string_lossy(),
        limits_home
            .as_deref()
            .map(|h| h.to_string_lossy())
            .unwrap_or_default(),
    ] {
        if text.chars().any(char::is_control) {
            return Err(LimitsError::Message(
                "a path the agent needs contains control characters, which a plist cannot carry"
                    .into(),
            ));
        }
    }
    let contents = render_plist(&ovm, dirs, limits_home.as_deref());
    crate::snapshot::write_atomic(&path, contents.as_bytes())?;
    launchctl(&["bootout", &domain_target()]).ok(); // a previous copy may not be loaded
    launchctl(&["bootstrap", &domain(), &path.to_string_lossy()])?;
    Ok(path)
}

pub fn uninstall() -> Result<bool> {
    let path = plist_path()?;
    if !path.is_file() {
        return Ok(false);
    }
    if cfg!(target_os = "macos") {
        // A job that is not loaded is fine to remove; anything else means the
        // agent would keep running after its plist is gone — refuse instead.
        if let Err(error) = launchctl(&["bootout", &domain_target()]) {
            let text = error.to_string();
            let not_loaded = text.contains("No such process")
                || text.contains("Could not find")
                || text.contains("not find service");
            if !not_loaded {
                return Err(error);
            }
        }
    }
    std::fs::remove_file(&path)?;
    Ok(true)
}

pub fn describe() -> String {
    match plist_path() {
        Ok(path) if path.is_file() => format!(
            "installed: {} (every {} min, polls when due)",
            display(&path),
            TICK_SECONDS / 60
        ),
        Ok(_) => "not installed — run: ovm limits agent install".into(),
        Err(error) => format!("unknown ({error})"),
    }
}

/// The `ovm` the agent should run: the one on PATH now, else the standard
/// launcher location. Resolved at install time because launchd starts agents
/// with a bare environment.
fn ovm_binary() -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                // launchd has no working directory to resolve a relative
                // entry against, so only absolute candidates count.
                .filter(|dir| dir.is_absolute())
                .map(|dir| dir.join("ovm"))
                .find(|candidate| candidate.is_file())
        })
        .or_else(|| dirs::home_dir().map(|home| home.join(".ovm").join("bin").join("ovm")))
        .unwrap_or_else(|| PathBuf::from("ovm"))
}

/// Pure, so tests can pin the exact agent definition.
pub fn render_plist(
    ovm: &Path,
    dirs: &LimitsDirs,
    limits_home_override: Option<&std::ffi::OsStr>,
) -> String {
    let log = dirs.agent_log_file();
    let ovm_dir = ovm
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let search_path = format!("{ovm_dir}:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin");
    let mut env = format!(
        "      <key>PATH</key>\n      <string>{}</string>\n",
        xml(&search_path)
    );
    if let Some(home) = limits_home_override {
        env.push_str(&format!(
            "      <key>{}</key>\n      <string>{}</string>\n",
            crate::paths::HOME_ENV,
            xml(&home.to_string_lossy())
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!--
  OVM limits background poller. Installed by `ovm limits agent install` (or
  `b` in `ovm limits`); removed by `ovm limits uninstall`. Every tick runs
  `ovm limits poll --due`, which only spends a Claude turn when an account's
  interval has passed or one of its windows has reset.
-->
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{ovm}</string>
    <string>limits</string>
    <string>poll</string>
    <string>--due</string>
  </array>
  <key>StartInterval</key>
  <integer>{TICK_SECONDS}</integer>
  <key>RunAtLoad</key>
  <true/>
  <key>EnvironmentVariables</key>
  <dict>
{env}  </dict>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
  <key>ProcessType</key>
  <string>Background</string>
</dict>
</plist>
"#,
        ovm = xml(&ovm.to_string_lossy()),
        log = xml(&log.to_string_lossy()),
    )
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn domain() -> String {
    // SAFETY: getuid has no preconditions.
    format!("gui/{}", unsafe { libc::getuid() })
}

fn domain_target() -> String {
    format!("{}/{LABEL}", domain())
}

fn launchctl(args: &[&str]) -> Result<()> {
    if std::env::var_os(NO_LAUNCHCTL_ENV).is_some() {
        return Ok(());
    }
    let output = Command::new("launchctl").args(args).output()?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(LimitsError::Message(format!(
        "launchctl {} failed: {}",
        args.join(" "),
        stderr.trim()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plist_runs_ovm_limits_poll_due_every_five_minutes_and_logs_under_limits() {
        let dirs = LimitsDirs::at(PathBuf::from("/tmp/lim its"));
        let plist = render_plist(Path::new("/opt/x&y/bin/ovm"), &dirs, None);
        assert!(plist.contains("<string>sh.ovm.limits</string>"));
        assert!(
            plist.contains("<string>/opt/x&amp;y/bin/ovm</string>"),
            "{plist}"
        );
        assert!(plist.contains(
            "<string>limits</string>\n    <string>poll</string>\n    <string>--due</string>"
        ));
        assert!(plist.contains("<integer>300</integer>"));
        assert!(plist.contains("<string>/tmp/lim its/agent.log</string>"));
        assert!(
            plist.contains("/opt/x&amp;y/bin:/opt/homebrew/bin"),
            "PATH leads with ovm's dir"
        );
        assert!(
            !plist.contains("OVM_LIMITS_HOME"),
            "no override unless the env carries one"
        );
    }

    #[test]
    fn a_relocated_limits_home_is_passed_to_the_agent() {
        let dirs = LimitsDirs::at(PathBuf::from("/tmp/l"));
        let plist = render_plist(
            Path::new("/x/ovm"),
            &dirs,
            Some(std::ffi::OsStr::new("/tmp/l")),
        );
        assert!(
            plist.contains("<key>OVM_LIMITS_HOME</key>\n      <string>/tmp/l</string>"),
            "{plist}"
        );
    }
}

use crate::error::{OvmError, Result};
use crate::sources::github_releases;
use crate::version_manager::VersionManager;
use console::style;

/// Show release notes for a specific version.
pub fn run(vm: &VersionManager, version: &str) -> Result<()> {
    let version = vm.product().normalize_version(version);
    // The version is interpolated into the GitHub API URL path below; block
    // separators/traversal so it can't address an arbitrary API endpoint.
    // Path checks alone aren't enough for a URL: `%2e%2e` parses as a
    // dot-segment, `?` starts a query, and `#` truncates at a fragment — so
    // also restrict to the characters real product versions use.
    vm.reject_version_traversal(&version)?;
    // `+` is included for semver build metadata; it is literal in URL paths.
    let url_safe = version
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'));
    if !url_safe {
        return Err(OvmError::Message(format!(
            "Invalid {} version `{version}`.",
            vm.product().display_name()
        )));
    }

    eprintln!(
        "  {} Fetching release notes for {}...",
        style("→").dim(),
        style(&version).bold()
    );

    let body = github_releases::get_release_notes(vm.product(), &version)?;

    match body {
        Some(notes) => {
            println!(
                "\n  {} {} {}\n",
                vm.product().display_name(),
                style(&version).green().bold(),
                style("release notes").dim()
            );

            for line in notes.lines() {
                if line.starts_with("## ") {
                    println!("  {}", style(line).bold());
                } else {
                    println!("  {}", line);
                }
            }
            println!();
        }
        None => {
            return Err(OvmError::Message(format!(
                "No release notes found for {} {version}.",
                vm.product().display_name()
            )));
        }
    }

    Ok(())
}

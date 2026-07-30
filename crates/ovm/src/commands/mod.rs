pub mod adopt;
pub mod archive;
pub mod autoupdate;
pub mod claude;
pub mod clean;
pub mod cleanup;
pub mod codex;
pub mod completions;
pub mod current;
pub mod doctor;
pub mod help;
pub mod info;
pub mod install;
pub mod launch;
pub mod list;
pub mod pi;
pub mod refresh_cache;
pub mod select;
pub mod self_autoupdate;
pub mod self_manage;
pub mod self_update;
pub mod shortcuts;
pub mod stats;
pub mod uninstall;
pub mod use_version;
pub mod which;

/// Keep `~/.local/bin/claude` an OVM-owned launcher so Claude Code's interactive
/// startup probe (`native_check_install`) doesn't print
/// `⚠ claude command at ~/.local/bin/claude missing or broken`. Claude-only,
/// silent (unless `OVM_VERBOSE`), idempotent, and best-effort — never blocks a
/// switch or launch. The destructive repairs (config flip, native-tree delete)
/// stay behind `ovm doctor claude --fix`.
pub(crate) fn maintain_claude_launcher(vm: &crate::version_manager::VersionManager) {
    if vm.product() != crate::product::Product::Claude {
        return;
    }
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let native = home.join(".local").join("bin").join("claude");
    let managed = vm.dirs.bin.join(vm.product().binary_name());
    if let Ok(Some(action)) = crate::claude_install::ensure_owned_launcher(&native, &managed) {
        if std::env::var_os("OVM_VERBOSE").is_some() {
            eprintln!("  {} claude launcher: {action}", console::style("·").dim());
        }
    }
}

/// Print a one-line nudge when Claude's native updater can still fight OVM
/// (install method is `native`, or the launcher is foreign). A leftover native
/// tree by itself is inert when the method is global and the launcher is owned;
/// `ovm doctor claude` still reports that disk cleanup without alarming on each
/// launch. Informs only — repair remains the explicit doctor command.
pub(crate) fn nudge_if_claude_install_drift(vm: &crate::version_manager::VersionManager) {
    if vm.product() != crate::product::Product::Claude {
        return;
    }
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let status = crate::claude_install::ClaudeHygiene::new(&home).inspect();
    if status.install_method_is_native()
        || status.launcher == crate::claude_install::LauncherState::Foreign
    {
        eprintln!(
            "  {} Claude's native updater can reclaim version control here — run `ovm doctor claude --fix`",
            console::style("⚠").yellow()
        );
    }
}

pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

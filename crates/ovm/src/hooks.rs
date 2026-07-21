use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy)]
pub enum Hook {
    PreInstall,
    PostInstall,
    PreSwitch,
    PostSwitch,
    PreUninstall,
    PostUninstall,
}

impl Hook {
    fn filename(self) -> &'static str {
        match self {
            Hook::PreInstall => "pre-install.sh",
            Hook::PostInstall => "post-install.sh",
            Hook::PreSwitch => "pre-switch.sh",
            Hook::PostSwitch => "post-switch.sh",
            Hook::PreUninstall => "pre-uninstall.sh",
            Hook::PostUninstall => "post-uninstall.sh",
        }
    }
}

pub fn run_hook(hooks_dir: &Path, hook: Hook, version: &str) {
    let hook_path = hooks_dir.join(hook.filename());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = match hook_path.metadata() {
            Ok(m) => m,
            Err(_) => return, // Missing or inaccessible
        };
        if meta.permissions().mode() & 0o111 == 0 {
            return;
        }
    }

    #[cfg(not(unix))]
    if !hook_path.exists() {
        return;
    }

    let _ = Command::new(&hook_path)
        .env("OVM_VERSION", version)
        .env("OVM_HOOK", hook.filename())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_hook_skips_nonexistent() {
        let dir = tempdir().unwrap();
        // Should not panic
        run_hook(dir.path(), Hook::PostInstall, "2.1.71");
    }

    #[test]
    fn test_hook_runs_executable() {
        let dir = tempdir().unwrap();
        let hook_path = dir.path().join("post-install.sh");
        let marker = dir.path().join("hook-ran");

        std::fs::write(
            &hook_path,
            format!("#!/bin/sh\ntouch {}\n", marker.display()),
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        run_hook(dir.path(), Hook::PostInstall, "2.1.71");
        assert!(marker.exists());
    }

    #[test]
    fn test_hook_skips_non_executable() {
        let dir = tempdir().unwrap();
        let hook_path = dir.path().join("post-install.sh");
        let marker = dir.path().join("hook-ran");

        std::fs::write(
            &hook_path,
            format!("#!/bin/sh\ntouch {}\n", marker.display()),
        )
        .unwrap();

        // Don't set executable permission
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        run_hook(dir.path(), Hook::PostInstall, "2.1.71");
        assert!(!marker.exists());
    }
}

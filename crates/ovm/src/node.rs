use std::process::Command;

/// Discover npm binary, preferring fnm-managed Node
pub fn find_npm() -> Option<String> {
    // Check if fnm is available and use its npm
    if let Ok(output) = Command::new("fnm")
        .args(["exec", "--", "which", "npm"])
        .output()
    {
        if output.status.success() {
            if let Ok(path) = String::from_utf8(output.stdout) {
                let path = path.trim();
                if !path.is_empty() {
                    return Some(path.to_string());
                }
            }
        }
    }

    // Fall back to npm on PATH
    if let Ok(output) = Command::new("which").arg("npm").output() {
        if output.status.success() {
            if let Ok(path) = String::from_utf8(output.stdout) {
                let path = path.trim();
                if !path.is_empty() {
                    return Some(path.to_string());
                }
            }
        }
    }

    None
}

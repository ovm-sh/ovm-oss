//! Git-style plugin discovery via PATH.
//!
//! Any binary named `ovm-<name>` on the user's PATH is a plugin.
//! `ovm <name>` executes it with all remaining args passed through.

use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Plugin {
    pub name: String,
    pub path: PathBuf,
}

/// Scan PATH for `ovm-*` binaries. Returns a de-duplicated list by name
/// (first occurrence wins — matches shell behavior).
pub fn discover() -> Vec<Plugin> {
    match std::env::var("PATH") {
        Ok(p) => discover_in_path(&p),
        Err(_) => Vec::new(),
    }
}

/// Find a specific plugin by name. Returns the full path if found.
pub fn find(name: &str) -> Option<PathBuf> {
    discover()
        .into_iter()
        .find(|p| p.name == name)
        .map(|p| p.path)
}

/// Find a plugin on PATH, falling back to a sibling of the running `ovm`
/// binary. Distributions bundle plugins (e.g. `ovm-claudex`) next to `ovm`,
/// and this keeps them resolvable even when the install dir isn't on PATH
/// yet (fresh install, absolute-path invocation). PATH stays first so a
/// dev build can shadow the bundled copy.
pub fn find_bundled(name: &str) -> Option<PathBuf> {
    if let Some(path) = find(name) {
        return Some(path);
    }
    let exe = std::env::current_exe().ok()?;
    sibling_plugin(&exe, name)
}

/// Resolve a plugin for generic `ovm <name>` dispatch. Manifest-declared
/// bundled plugins (e.g. `ovm-claudex`) get the sibling fallback, so an
/// absolute-path `ovm` invocation resolves them even when their directory
/// isn't on PATH — matching the dedicated `ovm ccx` alias path. Third-party
/// `ovm-*` plugins stay PATH-only, since only bundled binaries ship beside
/// `ovm`.
pub fn find_for_dispatch(name: &str) -> Option<PathBuf> {
    if is_bundled_plugin(name) {
        find_bundled(name)
    } else {
        find(name)
    }
}

/// Whether `ovm-<name>` is a bundled side binary in the embedded manifest.
fn is_bundled_plugin(name: &str) -> bool {
    let binary = format!("ovm-{name}");
    crate::bundle_manifest::BundleManifest::embedded()
        .map(|manifest| manifest.side_entries().any(|entry| entry.binary == binary))
        .unwrap_or(false)
}

/// The would-be bundled location of plugin `name` for an `ovm` at `exe`,
/// if an executable file exists there.
fn sibling_plugin(exe: &std::path::Path, name: &str) -> Option<PathBuf> {
    let sibling = exe.parent()?.join(format!("ovm-{name}"));
    (is_executable(&sibling)).then_some(sibling)
}

/// Scan a provided PATH-like string for `ovm-*` binaries. Testable.
fn discover_in_path(path_env: &str) -> Vec<Plugin> {
    let mut found: BTreeMap<String, Plugin> = BTreeMap::new();

    for dir in std::env::split_paths(path_env) {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };

        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };

            let Some(plugin_name) = name.strip_prefix("ovm-") else {
                continue;
            };

            if plugin_name.is_empty() || plugin_name.contains('.') {
                continue;
            }

            if !is_executable(&entry.path()) {
                continue;
            }

            found
                .entry(plugin_name.to_string())
                .or_insert_with(|| Plugin {
                    name: plugin_name.to_string(),
                    path: entry.path(),
                });
        }
    }

    found.into_values().collect()
}

/// Execute a discovered plugin binary, inheriting the parent's stdio, and
/// return its exit status. A failure to spawn — the file is not executable, was
/// removed between discovery and dispatch, or is not a valid program — becomes
/// a clear error naming the plugin instead of a raw OS error or a panic.
pub fn dispatch(
    path: &std::path::Path,
    args: &[String],
) -> crate::error::Result<std::process::ExitStatus> {
    std::process::Command::new(path)
        .args(args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|error| {
            crate::error::OvmError::Message(format!(
                "failed to run plugin `{}`: {error}",
                path.display()
            ))
        })
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Create a file at `path`. On Unix, also chmod +x when `executable` is true.
    fn touch(path: &std::path::Path, executable: bool) {
        std::fs::write(path, "#!/bin/sh\necho hi\n").expect("write file");
        #[cfg(unix)]
        if executable {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms).expect("chmod");
        }
    }

    fn find_in(path_env: &str, name: &str) -> Option<PathBuf> {
        discover_in_path(path_env)
            .into_iter()
            .find(|p| p.name == name)
            .map(|p| p.path)
    }

    #[test]
    fn discovers_ovm_prefixed_binaries() {
        let dir = tempdir().expect("tempdir");
        touch(&dir.path().join("ovm-hello"), true);
        touch(&dir.path().join("ovm-world"), true);
        touch(&dir.path().join("unrelated"), true); // should be ignored
        touch(&dir.path().join("xvm-bench"), true); // wrong prefix

        let names: Vec<String> = discover_in_path(&dir.path().to_string_lossy())
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert!(names.contains(&"hello".to_string()));
        assert!(names.contains(&"world".to_string()));
        assert_eq!(names.len(), 2);
    }

    #[test]
    #[cfg(unix)]
    fn skips_non_executable_files() {
        let dir = tempdir().expect("tempdir");
        touch(&dir.path().join("ovm-nonexec"), false);
        touch(&dir.path().join("ovm-exec"), true);

        let names: Vec<String> = discover_in_path(&dir.path().to_string_lossy())
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["exec".to_string()]);
    }

    #[test]
    fn skips_empty_and_dotted_names() {
        let dir = tempdir().expect("tempdir");
        touch(&dir.path().join("ovm-"), true); // empty name after prefix
        touch(&dir.path().join("ovm-foo.bak"), true); // contains dot
        touch(&dir.path().join("ovm-ok"), true);

        let names: Vec<String> = discover_in_path(&dir.path().to_string_lossy())
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["ok".to_string()]);
    }

    #[test]
    fn find_returns_path_for_known_plugin() {
        let dir = tempdir().expect("tempdir");
        let target = dir.path().join("ovm-greet");
        touch(&target, true);

        let path_str = dir.path().to_string_lossy().to_string();
        let found = find_in(&path_str, "greet").expect("plugin should be found");
        assert_eq!(found, target);
        assert!(find_in(&path_str, "missing").is_none());
    }

    #[test]
    fn sibling_plugin_resolves_executable_next_to_ovm() {
        let dir = tempdir().expect("tempdir");
        let ovm = dir.path().join("ovm");
        touch(&ovm, true);
        let claudex = dir.path().join("ovm-claudex");
        touch(&claudex, true);

        assert_eq!(super::sibling_plugin(&ovm, "claudex"), Some(claudex));
        assert_eq!(super::sibling_plugin(&ovm, "missing"), None);
    }

    #[test]
    #[cfg(unix)]
    fn sibling_plugin_ignores_non_executable_files() {
        let dir = tempdir().expect("tempdir");
        let ovm = dir.path().join("ovm");
        touch(&ovm, true);
        touch(&dir.path().join("ovm-claudex"), false);

        assert_eq!(super::sibling_plugin(&ovm, "claudex"), None);
    }

    #[test]
    #[cfg(unix)]
    fn dispatch_reports_helpful_error_for_unexecutable_plugin() {
        // A file that exists but cannot be exec'd (no exec bit) must yield a
        // clear, plugin-named error rather than a panic or a bare OS error.
        let dir = tempdir().expect("tempdir");
        let plugin = dir.path().join("ovm-broken");
        std::fs::write(&plugin, "not a real executable").expect("write file");

        let error = super::dispatch(&plugin, &[]).expect_err("non-executable must fail to run");
        let message = error.to_string();
        assert!(message.contains("failed to run plugin"), "{message}");
        assert!(message.contains("ovm-broken"), "{message}");
    }

    #[test]
    fn only_manifest_bundled_names_get_the_sibling_fallback() {
        // ovm-claudex is a declared side binary → eligible for the sibling
        // fallback; a random third-party plugin name is not.
        assert!(super::is_bundled_plugin("claudex"));
        assert!(super::is_bundled_plugin("codex-skew"));
        assert!(!super::is_bundled_plugin("echoargs"));
        assert!(!super::is_bundled_plugin("greet"));
    }

    #[test]
    fn first_occurrence_wins_for_duplicate_names() {
        let dir_a = tempdir().expect("tempdir a");
        let dir_b = tempdir().expect("tempdir b");
        touch(&dir_a.path().join("ovm-dup"), true);
        touch(&dir_b.path().join("ovm-dup"), true);

        let path = format!("{}:{}", dir_a.path().display(), dir_b.path().display());
        let plugin = find_in(&path, "dup").expect("found");
        assert_eq!(plugin, dir_a.path().join("ovm-dup"));
    }
}

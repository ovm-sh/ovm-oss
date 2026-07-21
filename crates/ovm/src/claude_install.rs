//! Claude install hygiene for OVM-managed setups.
//!
//! OVM owns the Claude binary through `~/.ovm/bin/claude`, which wins on PATH.
//! The failure mode we guard against is Claude's *native* self-updater: when
//! `~/.claude.json` says `"installMethod": "native"`, Claude believes `~/.local`
//! holds a native install it manages. It then re-downloads versions into
//! `~/.local/share/claude/` (hundreds of MB) and repoints `~/.local/bin/claude`
//! out from under OVM. `"autoUpdates": false` alone does NOT stop this — the
//! `native` install method is the trigger.
//!
//! "Clean" therefore means OVM stays authoritative: `installMethod` is anything
//! but `native`, no `~/.local` native install tree is lying around for the
//! updater to chase, and the `~/.local/bin/claude` launcher is one OVM owns — a
//! symlink to the managed binary (`~/.ovm/bin/claude`), never a native-updater
//! foothold. Claude's own `/doctor` will then show a harmless "running native
//! installation but config install method is 'global'" note — purely cosmetic,
//! and it triggers no updates.
//!
//! Why own the launcher instead of deleting it: Claude Code's *interactive*
//! startup runs a `native_check_install` probe of `~/.local/bin/claude`. If the
//! file is gone it prints `⚠ claude command at ~/.local/bin/claude missing or
//! broken · run claude install to repair` on every start. An OVM-owned symlink
//! to the managed binary satisfies that probe while keeping OVM authoritative —
//! safe precisely because a non-`native` install method disarms the updater, so
//! the launcher is no longer a foothold.
//!
//! `ovm doctor claude` reports; `ovm doctor claude --fix` repairs. The cheap,
//! idempotent half — keeping the launcher pointed at the managed binary — also
//! runs automatically on `ovm use claude` / launch via [`ensure_owned_launcher`].

use crate::error::{OvmError, Result};
use console::style;
use std::path::{Path, PathBuf};

const INSTALL_METHOD_KEY: &str = "installMethod";
const NATIVE_METHOD: &str = "native";
/// What we flip a `native` install method to: a value that never invokes the
/// native self-updater path.
const SAFE_METHOD: &str = "global";
const AUTO_UPDATES_KEY: &str = "autoUpdates";

/// State of the `~/.local/bin/claude` launcher relative to OVM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherState {
    /// Nothing at the path.
    Absent,
    /// A symlink OVM owns, resolving to the managed binary.
    OvmOwned,
    /// A real file, or a symlink pointing somewhere other than the managed
    /// binary — e.g. Claude's native-updater launcher.
    Foreign,
}

/// Filesystem locations involved. Held explicitly so tests can point them at a
/// tempdir instead of the real home directory.
pub struct ClaudeHygiene {
    /// `~/.claude.json` — Claude's config file.
    pub claude_json: PathBuf,
    /// `~/.local/bin/claude` — the launcher Claude Code's startup probe checks
    /// (and the native updater recreates). OVM keeps this pointed at the
    /// managed binary.
    pub native_launcher: PathBuf,
    /// `~/.local/share/claude` — the native install tree (the disk hog).
    pub native_install: PathBuf,
    /// `~/.ovm/bin/claude` — the OVM-managed launcher we point the native
    /// launcher at.
    pub managed_launcher: PathBuf,
}

#[derive(Debug)]
pub struct Status {
    pub claude_json_exists: bool,
    pub install_method: Option<String>,
    pub launcher: LauncherState,
    /// `Some(bytes)` when the native install tree exists.
    pub native_install_bytes: Option<u64>,
    /// Whether OVM has a usable managed binary to point the launcher at
    /// (`~/.ovm/bin/claude` resolves). When false there's nothing to link, so
    /// an absent launcher is acceptable.
    pub managed_launcher_available: bool,
}

impl Status {
    pub fn install_method_is_native(&self) -> bool {
        self.install_method.as_deref() == Some(NATIVE_METHOD)
    }

    pub fn is_clean(&self) -> bool {
        if self.install_method_is_native() || self.native_install_bytes.is_some() {
            return false;
        }
        match self.launcher {
            LauncherState::Foreign => false,
            LauncherState::OvmOwned => true,
            // With a managed binary available we want the launcher present and
            // owned; absent leaves Claude Code's startup probe complaining.
            LauncherState::Absent => !self.managed_launcher_available,
        }
    }
}

impl ClaudeHygiene {
    pub fn new(home: &Path) -> Self {
        Self {
            claude_json: home.join(".claude.json"),
            native_launcher: home.join(".local").join("bin").join("claude"),
            native_install: home.join(".local").join("share").join("claude"),
            managed_launcher: home.join(".ovm").join("bin").join("claude"),
        }
    }

    pub fn inspect(&self) -> Status {
        let config = read_json_object(&self.claude_json);
        let (claude_json_exists, install_method) = match config {
            Some(obj) => (
                true,
                obj.get(INSTALL_METHOD_KEY)
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            ),
            None => (false, None),
        };

        let native_install_bytes = if self.native_install.exists() {
            Some(dir_size(&self.native_install))
        } else {
            None
        };

        Status {
            claude_json_exists,
            install_method,
            launcher: launcher_state(&self.native_launcher, &self.managed_launcher),
            native_install_bytes,
            managed_launcher_available: self.managed_launcher.exists(),
        }
    }

    /// Repair anything that lets Claude wrest version control back from OVM.
    /// Returns one human-readable line per action (empty if already clean).
    ///
    /// Order matters: flip the install method *first* so a concurrent Claude
    /// launch won't immediately recreate the native install we're about to
    /// remove.
    pub fn apply(&self) -> Result<Vec<String>> {
        let mut actions = Vec::new();

        if self.claude_json.exists() {
            actions.extend(neutralize_config(&self.claude_json)?);
        } else {
            actions.push(format!(
                "skipped {} (not found — run claude once, then re-run --fix)",
                self.claude_json.display()
            ));
        }

        // Use symlink_metadata (does not follow the final symlink) so a symlinked
        // ~/.local/share/claude is unlinked, never recursively deleted at its
        // target. remove_dir_all is reserved for a real directory.
        if let Ok(meta) = std::fs::symlink_metadata(&self.native_install) {
            if meta.file_type().is_symlink() {
                std::fs::remove_file(&self.native_install)?;
                actions.push(format!(
                    "removed native-install symlink {}",
                    self.native_install.display()
                ));
            } else if meta.is_dir() {
                let freed = dir_size(&self.native_install);
                std::fs::remove_dir_all(&self.native_install)?;
                actions.push(format!(
                    "removed native install {} ({})",
                    self.native_install.display(),
                    human_bytes(freed)
                ));
            }
        }

        // Own the launcher: replace anything foreign with a symlink to the
        // managed binary, or create one if absent. Unlike the auto path this is
        // the explicit, user-invoked repair, so it *will* delete a foreign real
        // file. When OVM has no managed binary to point at, fall back to the old
        // behaviour (just clear the foreign foothold).
        let managed_available = self.managed_launcher.exists();
        match launcher_state(&self.native_launcher, &self.managed_launcher) {
            LauncherState::OvmOwned => {}
            LauncherState::Foreign => {
                std::fs::remove_file(&self.native_launcher)?;
                if managed_available {
                    link_launcher(&self.native_launcher, &self.managed_launcher)?;
                    actions.push(format!(
                        "repointed foreign launcher {} -> {}",
                        self.native_launcher.display(),
                        self.managed_launcher.display()
                    ));
                } else {
                    actions.push(format!(
                        "removed foreign launcher {}",
                        self.native_launcher.display()
                    ));
                }
            }
            LauncherState::Absent => {
                if managed_available {
                    link_launcher(&self.native_launcher, &self.managed_launcher)?;
                    actions.push(format!(
                        "linked {} -> {}",
                        self.native_launcher.display(),
                        self.managed_launcher.display()
                    ));
                }
            }
        }

        Ok(actions)
    }
}

/// Classify `~/.local/bin/claude` relative to the OVM-managed launcher.
fn launcher_state(native_launcher: &Path, managed_launcher: &Path) -> LauncherState {
    match std::fs::read_link(native_launcher) {
        // It's a symlink: ours if it targets the managed launcher (textually or
        // after resolving the chain to the same real binary), else foreign.
        Ok(target) => {
            if target == *managed_launcher || same_real_path(native_launcher, managed_launcher) {
                LauncherState::OvmOwned
            } else {
                LauncherState::Foreign
            }
        }
        // Not a symlink: either a real file (foreign) or nothing (absent).
        Err(_) => {
            if native_launcher.symlink_metadata().is_ok() {
                LauncherState::Foreign
            } else {
                LauncherState::Absent
            }
        }
    }
}

fn same_real_path(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Create `native_launcher` as a symlink to `managed_launcher`, replacing
/// whatever is there. Best-effort `remove_file` first so an existing entry
/// doesn't fail the symlink.
fn link_launcher(native_launcher: &Path, managed_launcher: &Path) -> Result<()> {
    if let Some(parent) = native_launcher.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(native_launcher);
    std::os::unix::fs::symlink(managed_launcher, native_launcher)?;
    Ok(())
}

/// Auto-maintenance for `ovm use claude` and launch: keep `native_launcher`
/// (`~/.local/bin/claude`) an OVM-owned symlink to `managed_launcher`
/// (`~/.ovm/bin/claude`). Safe and idempotent — creates it when absent and
/// repoints a foreign *symlink*, but never deletes a real file (that stays an
/// explicit `ovm doctor claude --fix` decision) and never touches an
/// already-owned launcher. No-ops when OVM has no managed binary to point at.
/// Returns `Some(action)` when it changed something (for tests / verbose logs).
pub fn ensure_owned_launcher(
    native_launcher: &Path,
    managed_launcher: &Path,
) -> Result<Option<String>> {
    if !managed_launcher.exists() {
        return Ok(None);
    }
    match launcher_state(native_launcher, managed_launcher) {
        LauncherState::OvmOwned => Ok(None),
        LauncherState::Absent => {
            link_launcher(native_launcher, managed_launcher)?;
            Ok(Some(format!("linked {}", native_launcher.display())))
        }
        LauncherState::Foreign if native_launcher.is_symlink() => {
            link_launcher(native_launcher, managed_launcher)?;
            Ok(Some(format!("repointed {}", native_launcher.display())))
        }
        LauncherState::Foreign => Ok(None),
    }
}

/// Print the hygiene report. Mirrors the existing `doctor` command's style.
pub fn report(status: &Status) {
    println!("claude install hygiene · OVM-managed");

    if !status.claude_json_exists {
        println!(
            "  {} ~/.claude.json not found — run claude once so it exists",
            style("·").dim()
        );
    } else if status.install_method_is_native() {
        println!(
            "  {} installMethod = native — invites Claude's self-updater to fight OVM",
            style("⚠").yellow()
        );
    } else {
        println!(
            "  {} installMethod = {} (not native)",
            style("✓").green(),
            status.install_method.as_deref().unwrap_or("unset")
        );
    }

    match status.launcher {
        LauncherState::OvmOwned => println!(
            "  {} ~/.local/bin/claude is an OVM-owned launcher",
            style("✓").green()
        ),
        LauncherState::Foreign => println!(
            "  {} a non-OVM Claude launcher sits at ~/.local/bin/claude (native-updater foothold)",
            style("⚠").yellow()
        ),
        LauncherState::Absent => {
            if status.managed_launcher_available {
                println!(
                    "  {} ~/.local/bin/claude is missing — Claude Code warns on startup",
                    style("⚠").yellow()
                );
            } else {
                println!("  {} no stray launcher in ~/.local/bin", style("✓").green());
            }
        }
    }

    match status.native_install_bytes {
        Some(bytes) => println!(
            "  {} native install tree in ~/.local/share/claude ({})",
            style("⚠").yellow(),
            human_bytes(bytes)
        ),
        None => println!(
            "  {} no native install tree in ~/.local/share/claude",
            style("✓").green()
        ),
    }

    if status.is_clean() {
        println!(
            "  {} clean — OVM is authoritative. Claude's /doctor may note the install",
            style("✓").green()
        );
        println!("     method mismatch; that's cosmetic and triggers nothing.");
    }
}

/// Flip a `native` install method to a non-updating one and force
/// `autoUpdates: false`, preserving every other key and its order. Writes via a
/// temp file + rename so an interrupted run can't corrupt the config.
fn neutralize_config(path: &Path) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(path)?;
    let mut value: serde_json::Value = serde_json::from_str(&text).map_err(|source| {
        OvmError::Config(format!(
            "{} is not valid JSON ({source}); leaving it untouched",
            path.display()
        ))
    })?;
    let obj = value.as_object_mut().ok_or_else(|| {
        OvmError::Config(format!(
            "{} is not a JSON object; leaving it untouched",
            path.display()
        ))
    })?;

    let mut changed = Vec::new();
    if obj.get(INSTALL_METHOD_KEY).and_then(|v| v.as_str()) == Some(NATIVE_METHOD) {
        obj.insert(
            INSTALL_METHOD_KEY.to_string(),
            serde_json::Value::String(SAFE_METHOD.to_string()),
        );
        changed.push(format!(
            "set {INSTALL_METHOD_KEY} = \"{SAFE_METHOD}\" (was \"{NATIVE_METHOD}\")"
        ));
    }
    if obj.get(AUTO_UPDATES_KEY).and_then(|v| v.as_bool()) != Some(false) {
        obj.insert(AUTO_UPDATES_KEY.to_string(), serde_json::Value::Bool(false));
        changed.push(format!("set {AUTO_UPDATES_KEY} = false"));
    }

    if changed.is_empty() {
        return Ok(changed);
    }

    // Guard against clobbering a concurrent Claude write. `~/.claude.json` is
    // rewritten frequently by every live `claude` process; if it changed since
    // our initial read, bail rather than overwrite newer state with our stale
    // snapshot. This shrinks the race to the gap between this re-read and the
    // rename below — small enough to ignore for a one-shot manual command.
    if std::fs::read_to_string(path)? != text {
        return Err(OvmError::Config(format!(
            "{} changed while ovm was editing it (claude may be running). \
             Re-run `ovm doctor claude --fix`.",
            path.display()
        )));
    }

    let mut serialized = serde_json::to_string_pretty(&value)?;
    serialized.push('\n');
    write_atomic(path, &serialized)?;
    Ok(changed)
}

fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = parent.join(format!(".ovm-claude-json-{}.tmp", std::process::id()));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn read_json_object(path: &Path) -> Option<serde_json::Map<String, serde_json::Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value.as_object().cloned()
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => total += dir_size(&entry.path()),
            Ok(_) => {
                if let Ok(meta) = entry.metadata() {
                    total += meta.len();
                }
            }
            Err(_) => {}
        }
    }
    total
}

fn human_bytes(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_native_install(h: &ClaudeHygiene, bytes: usize) {
        let versions = h.native_install.join("versions");
        std::fs::create_dir_all(&versions).unwrap();
        std::fs::write(versions.join("2.1.172"), vec![0u8; bytes]).unwrap();
        std::fs::create_dir_all(h.native_launcher.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(versions.join("2.1.172"), &h.native_launcher).unwrap();
    }

    /// Make `~/.ovm/bin/claude` resolve to a real (fake) binary, so OVM "has a
    /// managed binary to point at".
    fn seed_managed_launcher(h: &ClaudeHygiene) {
        std::fs::create_dir_all(h.managed_launcher.parent().unwrap()).unwrap();
        std::fs::write(&h.managed_launcher, "fake-claude").unwrap();
    }

    #[test]
    fn clean_state_is_clean() {
        let dir = tempdir().unwrap();
        let h = ClaudeHygiene::new(dir.path());
        std::fs::write(
            &h.claude_json,
            r#"{"installMethod":"global","autoUpdates":false}"#,
        )
        .unwrap();
        assert!(h.inspect().is_clean());
    }

    #[test]
    fn native_method_is_not_clean() {
        let dir = tempdir().unwrap();
        let h = ClaudeHygiene::new(dir.path());
        std::fs::write(&h.claude_json, r#"{"installMethod":"native"}"#).unwrap();
        let status = h.inspect();
        assert!(status.install_method_is_native());
        assert!(!status.is_clean());
    }

    #[test]
    fn detects_native_install_and_launcher() {
        let dir = tempdir().unwrap();
        let h = ClaudeHygiene::new(dir.path());
        std::fs::write(&h.claude_json, r#"{"installMethod":"native"}"#).unwrap();
        write_native_install(&h, 4096);

        let status = h.inspect();
        assert_eq!(status.launcher, LauncherState::Foreign);
        assert_eq!(status.native_install_bytes, Some(4096));
    }

    #[test]
    fn fix_unlinks_symlinked_native_install_without_following() {
        let dir = tempdir().unwrap();
        let h = ClaudeHygiene::new(dir.path());
        std::fs::write(&h.claude_json, r#"{"installMethod":"global"}"#).unwrap();

        // A directory OUTSIDE ~/.local that ~/.local/share/claude points at.
        let external = dir.path().join("external-claude");
        std::fs::create_dir_all(&external).unwrap();
        std::fs::write(external.join("keep.txt"), "important").unwrap();
        std::fs::create_dir_all(h.native_install.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&external, &h.native_install).unwrap();

        let actions = h.apply().unwrap();
        assert!(actions.iter().any(|a| a.contains("native-install symlink")));
        // Symlink removed, but its target and contents must survive untouched.
        assert!(std::fs::symlink_metadata(&h.native_install).is_err());
        assert!(
            external.join("keep.txt").exists(),
            "must unlink the symlink, never follow it and delete the target"
        );
    }

    #[test]
    fn fix_flips_method_and_removes_strays() {
        let dir = tempdir().unwrap();
        let h = ClaudeHygiene::new(dir.path());
        std::fs::write(
            &h.claude_json,
            r#"{"zeta":1,"installMethod":"native","autoUpdates":true,"alpha":2}"#,
        )
        .unwrap();
        write_native_install(&h, 8192);

        let actions = h.apply().unwrap();
        assert!(actions
            .iter()
            .any(|a| a.contains("installMethod") && a.contains("global")));
        assert!(actions.iter().any(|a| a.contains("autoUpdates")));
        // No managed binary in this tempdir, so the foreign launcher is just
        // cleared, not repointed.
        assert!(actions
            .iter()
            .any(|a| a.contains("removed foreign launcher")));
        assert!(actions.iter().any(|a| a.contains("removed native install")));

        let status = h.inspect();
        assert!(status.is_clean(), "expected clean after fix: {status:?}");

        // Other keys + order preserved.
        let text = std::fs::read_to_string(&h.claude_json).unwrap();
        assert!(text.find("\"zeta\"").unwrap() < text.find("\"alpha\"").unwrap());
        assert!(text.contains("\"installMethod\": \"global\""));
        assert!(text.contains("\"autoUpdates\": false"));
    }

    #[test]
    fn fix_leaves_non_native_method_alone() {
        let dir = tempdir().unwrap();
        let h = ClaudeHygiene::new(dir.path());
        std::fs::write(
            &h.claude_json,
            r#"{"installMethod":"global","autoUpdates":false}"#,
        )
        .unwrap();

        let actions = h.apply().unwrap();
        assert!(actions.is_empty(), "nothing to do: {actions:?}");
        assert_eq!(
            h.inspect().install_method.as_deref(),
            Some("global"),
            "global must not be rewritten"
        );
    }

    #[test]
    fn invalid_json_is_left_untouched() {
        let dir = tempdir().unwrap();
        let h = ClaudeHygiene::new(dir.path());
        std::fs::write(&h.claude_json, "{not valid json").unwrap();

        let err = h.apply().unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
        assert_eq!(
            std::fs::read_to_string(&h.claude_json).unwrap(),
            "{not valid json"
        );
    }

    #[test]
    fn fix_owns_launcher_when_managed_binary_available() {
        let dir = tempdir().unwrap();
        let h = ClaudeHygiene::new(dir.path());
        std::fs::write(&h.claude_json, r#"{"installMethod":"native"}"#).unwrap();
        write_native_install(&h, 4096); // foreign launcher + native tree
        seed_managed_launcher(&h);

        let actions = h.apply().unwrap();
        assert!(actions
            .iter()
            .any(|a| a.contains("repointed foreign launcher")));

        let status = h.inspect();
        assert_eq!(status.launcher, LauncherState::OvmOwned);
        assert!(status.is_clean(), "expected clean after fix: {status:?}");
        // The launcher now resolves to the managed binary.
        assert_eq!(
            std::fs::read_link(&h.native_launcher).unwrap(),
            h.managed_launcher
        );
    }

    #[test]
    fn fix_creates_launcher_when_absent_and_managed_available() {
        let dir = tempdir().unwrap();
        let h = ClaudeHygiene::new(dir.path());
        std::fs::write(&h.claude_json, r#"{"installMethod":"global"}"#).unwrap();
        seed_managed_launcher(&h);

        // Absent launcher with a managed binary available is not yet clean.
        assert!(!h.inspect().is_clean());

        let actions = h.apply().unwrap();
        assert!(actions.iter().any(|a| a.contains("linked")));
        let status = h.inspect();
        assert_eq!(status.launcher, LauncherState::OvmOwned);
        assert!(status.is_clean());
    }

    #[test]
    fn absent_launcher_is_clean_without_managed_binary() {
        let dir = tempdir().unwrap();
        let h = ClaudeHygiene::new(dir.path());
        std::fs::write(&h.claude_json, r#"{"installMethod":"global"}"#).unwrap();
        // No managed binary: nothing to point at, so absent is acceptable.
        let status = h.inspect();
        assert_eq!(status.launcher, LauncherState::Absent);
        assert!(!status.managed_launcher_available);
        assert!(status.is_clean());
    }

    #[test]
    fn ensure_creates_owned_launcher_when_absent() {
        let dir = tempdir().unwrap();
        let h = ClaudeHygiene::new(dir.path());
        seed_managed_launcher(&h);

        let action = ensure_owned_launcher(&h.native_launcher, &h.managed_launcher).unwrap();
        assert!(action.is_some());
        assert_eq!(
            launcher_state(&h.native_launcher, &h.managed_launcher),
            LauncherState::OvmOwned
        );
        // Idempotent: a second call does nothing.
        assert!(
            ensure_owned_launcher(&h.native_launcher, &h.managed_launcher)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn ensure_repoints_foreign_symlink_but_not_real_file() {
        let dir = tempdir().unwrap();
        let h = ClaudeHygiene::new(dir.path());
        seed_managed_launcher(&h);
        std::fs::create_dir_all(h.native_launcher.parent().unwrap()).unwrap();

        // Foreign symlink → repointed.
        let elsewhere = dir.path().join("some-other-claude");
        std::fs::write(&elsewhere, "x").unwrap();
        std::os::unix::fs::symlink(&elsewhere, &h.native_launcher).unwrap();
        assert!(
            ensure_owned_launcher(&h.native_launcher, &h.managed_launcher)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            launcher_state(&h.native_launcher, &h.managed_launcher),
            LauncherState::OvmOwned
        );

        // A real file is left alone — that's a --fix decision, not auto.
        std::fs::remove_file(&h.native_launcher).unwrap();
        std::fs::write(&h.native_launcher, "real-binary").unwrap();
        assert!(
            ensure_owned_launcher(&h.native_launcher, &h.managed_launcher)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            launcher_state(&h.native_launcher, &h.managed_launcher),
            LauncherState::Foreign
        );
    }

    #[test]
    fn ensure_noops_without_managed_binary() {
        let dir = tempdir().unwrap();
        let h = ClaudeHygiene::new(dir.path());
        // No managed launcher seeded.
        assert!(
            ensure_owned_launcher(&h.native_launcher, &h.managed_launcher)
                .unwrap()
                .is_none()
        );
        assert!(h.native_launcher.symlink_metadata().is_err());
    }

    #[test]
    fn human_bytes_formats() {
        assert_eq!(human_bytes(512), "512 bytes");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5 MB");
        assert_eq!(human_bytes(2 * 1024 * 1024 * 1024), "2.0 GB");
    }
}

//! The bits of Claude Code's settings the tour offers to set.
//!
//! Two offers live here because they write the same file, and that file is
//! the reader's: every other key survives both, and anything replaced is
//! named first and copied beside the settings before it goes.
//!
//! Chapter iii says Echo "lives in the statusline now". This is the part that
//! makes that true on the reader's own machine: a small stdlib-only Python
//! script that Claude Code runs on each render, reading the session's real
//! state — context pressure, diff size, spend, model, idleness — and drawing
//! Echo's mood from it.
//!
//! Two files are touched, both deliberately:
//!   * `~/.ovm/statusline/echo.py` — ours, rewritten on every install so an
//!     upgrade ships a newer Echo without the reader doing anything.
//!   * the Claude settings file — theirs. An existing `statusLine` is backed
//!     up beside it before being replaced, and every other key is preserved.

use crate::error::{OvmError, Result};
use std::path::{Path, PathBuf};

/// What claudex already writes into its own isolated home
/// (`ovm-claudex`'s `setup.rs`). Same number here so "forever" means the same
/// thing whichever door the reader came through.
pub const FOREVER_DAYS: u64 = 999_999;

/// Claude Code's own default, for the prompt to say what it is replacing.
pub const DEFAULT_RETENTION_DAYS: u64 = 30;

/// The vendored script. Embedded rather than downloaded so the offer works on
/// a machine that has just been handed a binary and nothing else.
const ECHO_SCRIPT: &str = include_str!("../assets/echo-statusline.py");

/// Claude's settings file, honouring `CLAUDE_CONFIG_DIR` exactly as
/// [`crate::buddy`] does — the tour runs against a sandboxed Claude home and
/// must never write into the real one.
fn settings_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("settings.json"));
        }
    }
    dirs::home_dir().map(|home| home.join(".claude").join("settings.json"))
}

pub fn script_path(base: &Path) -> PathBuf {
    base.join("statusline").join("echo.py")
}

/// Whether the statusline already runs our Echo.
///
/// Keyed on the command pointing at our script rather than on the key merely
/// existing: a reader with their own statusline should be told it will be
/// replaced, not quietly counted as done.
pub fn is_installed(base: &Path) -> bool {
    settings_path().is_some_and(|path| is_installed_at(base, &path))
}

fn is_installed_at(base: &Path, path: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(settings) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    settings
        .get("statusLine")
        .and_then(|line| line.get("command"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|command| command.contains(&script_path(base).to_string_lossy().to_string()))
}

/// A statusline that is set to something that is not ours — worth naming
/// before it is replaced.
pub fn foreign_command(base: &Path) -> Option<String> {
    foreign_command_at(base, &settings_path()?)
}

fn foreign_command_at(base: &Path, settings: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(settings).ok()?;
    let settings = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
    let command = settings
        .get("statusLine")?
        .get("command")?
        .as_str()?
        .to_string();
    if command.contains(&script_path(base).to_string_lossy().to_string()) {
        return None;
    }
    Some(command)
}

/// `ECHO_VERSION = "…"` in a copy of the script, if it carries one. Copies
/// written before 2026-09-27 have none.
pub fn echo_version(script: &str) -> Option<&str> {
    script.lines().find_map(|line| {
        line.trim()
            .strip_prefix("ECHO_VERSION = \"")
            .and_then(|rest| rest.strip_suffix('"'))
    })
}

/// The Echo inside this ovm.
pub fn bundled_version() -> Option<&'static str> {
    echo_version(ECHO_SCRIPT)
}

/// The Echo on disk, if one was ever installed.
pub fn installed_version(base: &Path) -> Option<Option<String>> {
    let raw = std::fs::read_to_string(script_path(base)).ok()?;
    Some(echo_version(&raw).map(str::to_owned))
}

/// Rewrite an installed Echo that differs from the bundled one — the script
/// only, never Claude's settings, and never where Echo was not installed. The
/// answer is whether it was rewritten. Called on every `ovm claude` launch, so
/// an ovm upgrade reaches the statusline without anyone running `ovm
/// statusline` again; the comparison is one small file read.
pub fn refresh_installed(base: &Path) -> Result<bool> {
    let script = script_path(base);
    let Ok(current) = std::fs::read(&script) else {
        return Ok(false);
    };
    if current == ECHO_SCRIPT.as_bytes() {
        return Ok(false);
    }
    write_replacing(&script, ECHO_SCRIPT.as_bytes(), true)?;
    Ok(true)
}

/// Install the script and point Claude's statusline at it.
pub fn install(base: &Path) -> Result<PathBuf> {
    let path = settings_path()
        .ok_or_else(|| OvmError::Message("no home directory for Claude's settings".into()))?;
    install_at(base, &path)
}

fn install_at(base: &Path, path: &Path) -> Result<PathBuf> {
    if which_python().is_none() {
        return Err(OvmError::Message(
            "python3 is not on PATH — Echo's statusline needs it".into(),
        ));
    }
    let script = script_path(base);
    crate::util::ensure_parent_dir(&script)?;
    write_replacing(&script, ECHO_SCRIPT.as_bytes(), true)?;

    crate::util::ensure_parent_dir(path)?;

    // Preserve every other key: this file is the reader's, and it carries
    // hooks, permissions and model choices we have no business dropping.
    let mut settings = match std::fs::read_to_string(path) {
        Ok(raw) if !raw.trim().is_empty() => serde_json::from_str::<serde_json::Value>(&raw)
            .map_err(|error| {
                OvmError::Message(format!(
                    "Claude's settings.json is not valid JSON ({error})"
                ))
            })?,
        _ => serde_json::json!({}),
    };
    if !settings.is_object() {
        return Err(OvmError::Message(
            "Claude's settings.json is not a JSON object".into(),
        ));
    }

    // Back up whatever was there, once, before replacing it.
    if let Some(existing) = settings.get("statusLine") {
        if foreign_command_at(base, path).is_some() {
            let backup = path.with_extension("json.before-echo");
            if !backup.exists() {
                write_replacing(
                    &backup,
                    serde_json::to_string_pretty(existing)?.as_bytes(),
                    false,
                )?;
            }
        }
    }

    settings["statusLine"] = serde_json::json!({
        "type": "command",
        "command": format!("python3 {}", script.display()),
        "padding": 0,
    });
    write_settings(path, &settings)?;
    Ok(script)
}

/// How long Claude Code keeps chat history here, as the settings file says.
/// `None` when the key is absent — which means Claude's own default applies,
/// not that history is kept forever.
pub fn retention_days() -> Option<u64> {
    retention_days_at(&settings_path()?)
}

fn retention_days_at(settings: &Path) -> Option<u64> {
    let raw = std::fs::read_to_string(settings).ok()?;
    serde_json::from_str::<serde_json::Value>(&raw)
        .ok()?
        .get("cleanupPeriodDays")?
        .as_u64()
}

/// Keep chat history indefinitely.
pub fn keep_history_forever() -> Result<()> {
    let path = settings_path()
        .ok_or_else(|| OvmError::Message("no home directory for Claude's settings".into()))?;
    keep_history_forever_at(&path)
}

fn keep_history_forever_at(settings: &Path) -> Result<()> {
    crate::util::ensure_parent_dir(settings)?;
    let mut parsed = read_settings(settings)?;
    parsed["cleanupPeriodDays"] = serde_json::json!(FOREVER_DAYS);
    write_settings(settings, &parsed)
}

/// Read the settings object, treating an absent or empty file as empty.
fn read_settings(path: &Path) -> Result<serde_json::Value> {
    let parsed = match std::fs::read_to_string(path) {
        Ok(raw) if !raw.trim().is_empty() => serde_json::from_str::<serde_json::Value>(&raw)
            .map_err(|error| {
                OvmError::Message(format!(
                    "Claude's settings.json is not valid JSON ({error})"
                ))
            })?,
        _ => serde_json::json!({}),
    };
    if !parsed.is_object() {
        return Err(OvmError::Message(
            "Claude's settings.json is not a JSON object".into(),
        ));
    }
    Ok(parsed)
}

fn write_settings(path: &Path, settings: &serde_json::Value) -> Result<()> {
    write_replacing(
        path,
        (serde_json::to_string_pretty(settings)? + "\n").as_bytes(),
        false,
    )
}

/// Replace a file we own, atomically.
///
/// `util::write_new_file` refuses an existing path on purpose (it is the
/// symlink-safe primitive), and removing the file first to get around that
/// left a window with NO file at all — on `settings.json`, whose other keys
/// this module goes out of its way to preserve, an interruption there costs
/// the reader their hooks, permissions and model choices. Write a fresh temp
/// beside the destination and rename over it instead, the way
/// `claude_install::write_atomic` already does for `~/.claude.json`: the
/// reader sees the old file or the new one, never neither.
///
/// `executable` goes on through the handle that created the file rather than
/// through its name — see `util::make_handle_executable` for why a path-based
/// chmod is the same defect as a path-based create.
fn write_replacing(path: &Path, contents: &[u8], executable: bool) -> Result<()> {
    use std::io::Write;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    // Unique per process *and* per call, so two writers never share a temp.
    static WRITE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = WRITE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".ovm-claude-settings-{}.{seq}.tmp",
        std::process::id()
    ));

    let staged = (|| -> Result<()> {
        let mut file = crate::util::create_new_file(&tmp)?;
        file.write_all(contents)?;
        if executable {
            crate::util::make_handle_executable(&file)?;
        }
        Ok(())
    })();
    if let Err(error) = staged {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&tmp, path) {
        // Never leave scratch behind in the reader's config directory.
        let _ = std::fs::remove_file(&tmp);
        return Err(error.into());
    }
    Ok(())
}

fn which_python() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join("python3"))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_writes_the_script_and_points_the_statusline_at_it() {
        let home = tempfile::tempdir().expect("tempdir");
        let base = home.path().join(".ovm");
        let settings = home.path().join("settings.json");

        let script = install_at(&base, &settings).expect("install");
        assert!(script.is_file(), "the script should be written");
        assert!(
            is_installed_at(&base, &settings),
            "the statusline should point at it"
        );
        assert!(std::fs::read_to_string(&settings)
            .expect("settings")
            .contains("statusLine"));

        // Claude Code executes this file. The bit is set through the handle
        // that created it, so nothing about the write can leave it unset.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&script)
                .expect("script metadata")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o111,
                0o111,
                "the script must be executable: {mode:o}"
            );
        }

        // No scratch left beside either file.
        for dir in [script.parent().expect("script dir"), home.path()] {
            let strays: Vec<_> = std::fs::read_dir(dir)
                .expect("read dir")
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.contains(".tmp"))
                .collect();
            assert!(
                strays.is_empty(),
                "temp files left behind in {dir:?}: {strays:?}"
            );
        }
    }

    #[test]
    fn install_preserves_other_settings_and_backs_up_a_foreign_statusline() {
        let home = tempfile::tempdir().expect("tempdir");
        let base = home.path().join(".ovm");
        let settings = home.path().join("settings.json");
        std::fs::write(
            &settings,
            r#"{"model":"opus","statusLine":{"type":"command","command":"their-own-thing"}}"#,
        )
        .expect("seed settings");

        install_at(&base, &settings).expect("install");

        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).expect("settings"))
                .expect("json");
        assert_eq!(parsed["model"], "opus", "other keys must survive");
        assert!(
            settings.with_extension("json.before-echo").exists(),
            "the statusline it replaced should be recoverable"
        );
    }

    #[test]
    fn a_foreign_statusline_is_reported_before_it_is_replaced() {
        let home = tempfile::tempdir().expect("tempdir");
        let base = home.path().join(".ovm");
        let settings = home.path().join("settings.json");
        std::fs::write(
            &settings,
            r#"{"statusLine":{"type":"command","command":"their-own-thing"}}"#,
        )
        .expect("seed settings");

        assert_eq!(
            foreign_command_at(&base, &settings).as_deref(),
            Some("their-own-thing")
        );
        assert!(!is_installed_at(&base, &settings));
    }

    #[test]
    fn a_stale_installed_echo_is_refreshed_and_a_missing_one_is_left_alone() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path();
        assert!(
            !refresh_installed(base).unwrap(),
            "nothing installed, nothing written"
        );
        assert!(!script_path(base).exists());

        crate::util::ensure_parent_dir(&script_path(base)).unwrap();
        std::fs::write(script_path(base), "print('old echo')\n").unwrap();
        assert_eq!(installed_version(base), Some(None));
        assert!(refresh_installed(base).unwrap());
        assert_eq!(
            std::fs::read_to_string(script_path(base)).unwrap(),
            ECHO_SCRIPT
        );
        assert_eq!(
            installed_version(base).flatten().as_deref(),
            bundled_version()
        );
        assert!(!refresh_installed(base).unwrap(), "already current");
    }

    #[test]
    fn the_bundled_echo_carries_a_version() {
        assert!(bundled_version().is_some_and(|v| !v.is_empty()));
        assert_eq!(echo_version("x\nECHO_VERSION = \"1.2\"\ny"), Some("1.2"));
        assert_eq!(echo_version("no marker"), None);
    }

    #[test]
    fn installing_twice_is_not_an_error_and_leaves_one_backup() {
        let home = tempfile::tempdir().expect("tempdir");
        let base = home.path().join(".ovm");
        let settings = home.path().join("settings.json");
        std::fs::write(
            &settings,
            r#"{"statusLine":{"type":"command","command":"their-own-thing"}}"#,
        )
        .expect("seed settings");

        install_at(&base, &settings).expect("first install");
        install_at(&base, &settings).expect("second install");

        let backup = std::fs::read_to_string(settings.with_extension("json.before-echo"))
            .expect("backup survives");
        assert!(
            backup.contains("their-own-thing"),
            "the backup must still hold THEIR statusline, not ours: {backup}"
        );
    }

    #[test]
    fn retention_reads_what_is_configured_and_absent_is_not_forever() {
        let home = tempfile::tempdir().expect("tempdir");
        let settings = home.path().join("settings.json");
        assert_eq!(retention_days_at(&settings), None, "no file, no claim");

        std::fs::write(&settings, r#"{"cleanupPeriodDays": 7}"#).expect("seed");
        assert_eq!(retention_days_at(&settings), Some(7));

        std::fs::write(&settings, r#"{"model":"opus"}"#).expect("seed");
        assert_eq!(
            retention_days_at(&settings),
            None,
            "an absent key means Claude's default applies, not forever"
        );
    }

    #[test]
    fn keeping_history_forever_preserves_every_other_setting() {
        let home = tempfile::tempdir().expect("tempdir");
        let settings = home.path().join("settings.json");
        std::fs::write(
            &settings,
            r#"{"model":"opus","cleanupPeriodDays":30,"statusLine":{"command":"theirs"}}"#,
        )
        .expect("seed");

        keep_history_forever_at(&settings).expect("set retention");

        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).expect("read")).expect("json");
        assert_eq!(parsed["cleanupPeriodDays"], FOREVER_DAYS);
        assert_eq!(parsed["model"], "opus", "other keys must survive");
        assert_eq!(
            parsed["statusLine"]["command"], "theirs",
            "retention must not disturb their statusline"
        );
    }

    #[test]
    fn keeping_history_forever_works_from_no_settings_file_at_all() {
        let home = tempfile::tempdir().expect("tempdir");
        let settings = home.path().join("nested").join("settings.json");
        keep_history_forever_at(&settings).expect("set retention");
        assert_eq!(retention_days_at(&settings), Some(FOREVER_DAYS));
    }

    /// The number must match what claudex writes into its own home, or
    /// "forever" would mean two different things in one product.
    #[test]
    fn forever_is_the_same_number_claudex_uses() {
        assert_eq!(FOREVER_DAYS, 999_999);
    }
}

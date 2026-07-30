//! Durable feedback correlation for claudex sessions.
//!
//! Claude Code already sends `X-Claude-Code-Session-Id` to CLIProxyAPI, and
//! CLIProxyAPI uses it to select a stable Codex prompt-cache/session identity.
//! We keep a separate, local `cfx_…` identifier keyed to that Claude history
//! session. It exists before any feedback upload and survives resume/relaunch.

use crate::config::write_atomic;
use crate::paths::ClaudexDirs;
use crate::{ClaudexError, Result};
use fs4::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SESSION_HOOK_COMMAND: &str = "ovm claudex __session-start";
const FEEDBACK_ID_ENV: &str = "CLAUDEX_FEEDBACK_ID";
const CLAUDE_SESSION_ID_ENV: &str = "CLAUDE_CODE_SESSION_ID";

#[derive(Debug, Deserialize)]
struct SessionStartInput {
    session_id: String,
    transcript_path: Option<String>,
    source: Option<String>,
    model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct FeedbackSession {
    schema_version: u32,
    pub feedback_id: String,
    pub claude_session_id: String,
    pub transcript_path: Option<String>,
    pub source: Option<String>,
    pub model: Option<String>,
    codex_association: CodexAssociation,
    #[serde(default)]
    pub codex_feedback: Vec<CodexFeedbackRecord>,
    created_at_unix_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct CodexFeedbackRecord {
    pub thread_id: String,
    pub classification: String,
    pub included_logs: bool,
    pub submitted_at_unix_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CodexAssociation {
    bridge: String,
    join_header: String,
    upstream_identity: String,
    /// CLIProxyAPI currently owns this value internally. The archive leaves a
    /// typed slot so a future proxy observer can fill it without a migration.
    upstream_session_id: Option<String>,
}

/// Install the isolated-home SessionStart hook. Existing hooks are preserved,
/// and repeated setup/launches do not duplicate our command.
pub fn install_session_start_hook(dirs: &ClaudexDirs) -> Result<()> {
    let path = dirs.claude_home().join("settings.json");
    let mut value: Value = match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Object(Map::new()),
        Err(error) => return Err(error.into()),
    };
    let settings = value.as_object_mut().ok_or_else(|| {
        ClaudexError::Message(format!(
            "{} is not a JSON object; refusing to overwrite it.",
            path.display()
        ))
    })?;

    if add_session_start_hook(settings)? {
        let mut contents = serde_json::to_string_pretty(&value)?;
        contents.push('\n');
        write_atomic(&path, &contents, None)?;
    }
    Ok(())
}

fn add_session_start_hook(settings: &mut Map<String, Value>) -> Result<bool> {
    let hooks = settings
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| ClaudexError::Message("settings.json `hooks` must be an object".into()))?;
    let session_start = hooks
        .entry("SessionStart".to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| {
            ClaudexError::Message("settings.json `hooks.SessionStart` must be an array".into())
        })?;

    if session_start.iter().any(group_contains_our_hook) {
        return Ok(false);
    }

    session_start.push(json!({
        "matcher": "startup|resume|clear|compact",
        "hooks": [{
            "type": "command",
            "command": SESSION_HOOK_COMMAND,
            "timeout": 5
        }]
    }));
    Ok(true)
}

fn group_contains_our_hook(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command").and_then(Value::as_str) == Some(SESSION_HOOK_COMMAND)
            })
        })
}

/// Hidden SessionStart hook entrypoint. Reads Claude's hook payload on stdin,
/// creates or reuses the association, then exports the ID into Claude's Bash
/// environment without adding anything to model context.
pub fn session_start_hook() -> Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let event: SessionStartInput = serde_json::from_str(&input)?;
    let dirs = ClaudexDirs::new()?;
    dirs.ensure_layout()?;
    let session = register_session(&dirs, event)?;

    if let Ok(env_file) = std::env::var("CLAUDE_ENV_FILE") {
        if !env_file.is_empty() {
            append_feedback_env(Path::new(&env_file), &session.feedback_id)?;
        }
    }
    Ok(())
}

/// Print the stable correlation ID for the current live session. An explicit
/// Claude session ID is accepted for diagnostics outside Claude Code.
pub fn print_feedback_id(explicit_session_id: Option<&str>) -> Result<()> {
    if explicit_session_id.is_none() {
        if let Ok(feedback_id) = std::env::var(FEEDBACK_ID_ENV) {
            if valid_feedback_id(&feedback_id) {
                println!("{feedback_id}");
                return Ok(());
            }
        }
    }

    let dirs = ClaudexDirs::new()?;
    let session = current_session(&dirs, explicit_session_id)?;
    println!("{}", session.feedback_id);
    Ok(())
}

pub(crate) fn current_session(
    dirs: &ClaudexDirs,
    explicit_session_id: Option<&str>,
) -> Result<FeedbackSession> {
    let session_id = explicit_session_id
        .map(str::to_owned)
        .or_else(|| std::env::var(CLAUDE_SESSION_ID_ENV).ok())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            ClaudexError::Message(
                "No live Claude session found. Inside claudex run the command from `!` shell mode."
                    .into(),
            )
        })?;
    load_session(dirs, &session_id)?.ok_or_else(|| {
        ClaudexError::Message(format!(
            "No feedback association is registered for Claude session {session_id}."
        ))
    })
}

pub(crate) fn relationship_path(dirs: &ClaudexDirs, session_id: &str) -> PathBuf {
    session_path(dirs, session_id)
}

pub(crate) fn archive_codex_feedback(
    dirs: &ClaudexDirs,
    session: &FeedbackSession,
    thread_id: String,
    classification: String,
    included_logs: bool,
) -> Result<()> {
    let path = session_path(dirs, &session.claude_session_id);
    // Serialize the whole load -> append -> replace transaction with an
    // exclusive advisory lock. The atomic rename in `write_private` keeps each
    // write crash-safe, but two concurrent submissions for the same resumed
    // session would otherwise both read the pre-append file and the last rename
    // would silently drop the other's thread. The lock is a sibling file so it
    // never becomes part of the archived JSON. (UpdateLock pattern, install.rs.)
    let _lock = lock_relationship(&path)?;
    let mut updated = load_session(dirs, &session.claude_session_id)?.ok_or_else(|| {
        ClaudexError::Message("feedback relationship disappeared before archival".into())
    })?;
    updated.codex_feedback.push(CodexFeedbackRecord {
        thread_id,
        classification,
        included_logs,
        submitted_at_unix_ms: unix_time_ms(),
    });
    let mut contents = serde_json::to_string_pretty(&updated)?;
    contents.push('\n');
    crate::config::write_private(&session_path(dirs, &updated.claude_session_id), &contents)
}

/// Hold an exclusive advisory lock for one relationship's read-modify-write.
/// The OS releases it when the returned handle drops. The lock lives beside the
/// relationship file (`<key>.lock`) so it is never mistaken for archive data.
fn lock_relationship(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = path.with_extension("lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    // `FileExt::lock` is the exclusive, blocking form in this fs4 version.
    FileExt::lock(&file)?;
    Ok(file)
}

fn register_session(dirs: &ClaudexDirs, event: SessionStartInput) -> Result<FeedbackSession> {
    let session_id = event.session_id.trim();
    if session_id.is_empty() {
        return Err(ClaudexError::Message(
            "SessionStart payload has an empty session_id".into(),
        ));
    }
    if let Some(existing) = load_session(dirs, session_id)? {
        return Ok(existing);
    }

    let session = FeedbackSession {
        schema_version: 1,
        feedback_id: generate_feedback_id()?,
        claude_session_id: session_id.to_string(),
        transcript_path: event.transcript_path,
        source: event.source,
        model: event.model,
        codex_association: CodexAssociation {
            bridge: "cliproxyapi".into(),
            join_header: "X-Claude-Code-Session-Id".into(),
            upstream_identity: "prompt_cache_key/Session_id".into(),
            upstream_session_id: None,
        },
        codex_feedback: Vec::new(),
        created_at_unix_ms: unix_time_ms(),
    };
    let path = session_path(dirs, session_id);
    let mut contents = serde_json::to_string_pretty(&session)?;
    contents.push('\n');

    // create_new makes concurrent SessionStart hooks converge on one ID.
    match create_private_new(&path, &contents) {
        Ok(()) => Ok(session),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            load_session(dirs, session_id)?.ok_or_else(|| {
                ClaudexError::Message(format!(
                    "feedback session appeared but could not be read: {}",
                    path.display()
                ))
            })
        }
        Err(error) => Err(error.into()),
    }
}

fn load_session(dirs: &ClaudexDirs, session_id: &str) -> Result<Option<FeedbackSession>> {
    let path = session_path(dirs, session_id);
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(Some(serde_json::from_str(&contents)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn session_path(dirs: &ClaudexDirs, session_id: &str) -> PathBuf {
    let digest = Sha256::digest(session_id.as_bytes());
    let key = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    dirs.history_relationships_dir().join(format!("{key}.json"))
}

fn generate_feedback_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(format!(
        "cfx_{}",
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

fn unix_time_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn valid_feedback_id(value: &str) -> bool {
    value.len() == 36
        && value.starts_with("cfx_")
        && value[4..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn append_feedback_env(path: &Path, feedback_id: &str) -> Result<()> {
    if !valid_feedback_id(feedback_id) {
        return Err(ClaudexError::Message(
            "refusing to export an invalid feedback ID".into(),
        ));
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "export {FEEDBACK_ID_ENV}='{feedback_id}'")?;
    Ok(())
}

fn create_private_new(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(session_id: &str) -> SessionStartInput {
        SessionStartInput {
            session_id: session_id.into(),
            transcript_path: Some("/tmp/transcript.jsonl".into()),
            source: Some("startup".into()),
            model: Some("gpt-test".into()),
        }
    }

    #[test]
    fn registration_is_local_stable_and_distinct_per_history_session() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = ClaudexDirs::at(temp.path().join("claudex"));
        dirs.ensure_layout().expect("layout");

        let first = register_session(&dirs, event("claude-a")).expect("first");
        let resumed = register_session(&dirs, event("claude-a")).expect("resume");
        let other = register_session(&dirs, event("claude-b")).expect("other");

        assert!(valid_feedback_id(&first.feedback_id));
        assert_eq!(resumed.feedback_id, first.feedback_id);
        assert_ne!(other.feedback_id, first.feedback_id);
        assert_eq!(first.claude_session_id, "claude-a");
    }

    #[test]
    #[cfg(unix)]
    fn association_file_is_owner_readable_only() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = ClaudexDirs::at(temp.path().join("claudex"));
        dirs.ensure_layout().expect("layout");
        register_session(&dirs, event("claude-private")).expect("register");

        let mode = std::fs::metadata(session_path(&dirs, "claude-private"))
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn native_codex_feedback_thread_is_archived_on_the_relationship() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = ClaudexDirs::at(temp.path().join("claudex"));
        dirs.ensure_layout().expect("layout");
        let session = register_session(&dirs, event("claude-feedback")).expect("register");

        archive_codex_feedback(
            &dirs,
            &session,
            "no-active-thread-native".into(),
            "bug".into(),
            true,
        )
        .expect("archive");

        let reloaded = load_session(&dirs, "claude-feedback")
            .expect("load")
            .expect("session");
        assert_eq!(reloaded.codex_feedback.len(), 1);
        assert_eq!(
            reloaded.codex_feedback[0].thread_id,
            "no-active-thread-native"
        );
        assert_eq!(reloaded.codex_feedback[0].classification, "bug");
        assert!(reloaded.codex_feedback[0].included_logs);
    }

    #[test]
    fn concurrent_archival_keeps_both_threads() {
        // Two feedback submissions for the same resumed session race the
        // load -> append -> replace. The advisory lock must serialize them so
        // neither successfully submitted thread id is lost.
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = ClaudexDirs::at(temp.path().join("claudex"));
        dirs.ensure_layout().expect("layout");
        let session = register_session(&dirs, event("claude-concurrent")).expect("register");

        let dirs = std::sync::Arc::new(dirs);
        let session = std::sync::Arc::new(session);
        let handles: Vec<_> = ["thread-alpha", "thread-beta"]
            .into_iter()
            .map(|thread_id| {
                let dirs = std::sync::Arc::clone(&dirs);
                let session = std::sync::Arc::clone(&session);
                std::thread::spawn(move || {
                    archive_codex_feedback(
                        &dirs,
                        &session,
                        thread_id.to_string(),
                        "bug".into(),
                        false,
                    )
                    .expect("archive");
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("thread");
        }

        let reloaded = load_session(&dirs, "claude-concurrent")
            .expect("load")
            .expect("session");
        let mut ids: Vec<_> = reloaded
            .codex_feedback
            .iter()
            .map(|record| record.thread_id.clone())
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["thread-alpha", "thread-beta"]);
    }

    #[test]
    fn hook_install_is_idempotent_and_preserves_existing_hooks() {
        let mut settings = Map::new();
        settings.insert(
            "hooks".into(),
            json!({"SessionStart": [{
                "matcher": "startup",
                "hooks": [{"type": "command", "command": "existing-hook"}]
            }]}),
        );

        assert!(add_session_start_hook(&mut settings).expect("add"));
        assert!(!add_session_start_hook(&mut settings).expect("idempotent"));

        let groups = settings["hooks"]["SessionStart"]
            .as_array()
            .expect("groups");
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().any(group_contains_our_hook));
        assert!(groups
            .iter()
            .any(|group| { group["hooks"][0]["command"].as_str() == Some("existing-hook") }));
    }

    #[test]
    fn feedback_id_export_is_shell_safe() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("env");
        append_feedback_env(&path, "cfx_0123456789abcdef0123456789abcdef").expect("append");
        assert_eq!(
            std::fs::read_to_string(path).expect("read"),
            "export CLAUDEX_FEEDBACK_ID='cfx_0123456789abcdef0123456789abcdef'\n"
        );
    }
}

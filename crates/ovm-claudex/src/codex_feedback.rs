//! Optional native Codex feedback submission for a claudex relationship.
//!
//! The command is preview-first. Nothing leaves the machine unless the user
//! explicitly supplies `--send`; logs require the additional
//! `--include-logs` choice and are listed before upload.

use crate::feedback::{self, FeedbackSession};
use crate::paths::ClaudexDirs;
use crate::{ClaudexError, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

const APP_SERVER_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, PartialEq)]
struct FeedbackCommand {
    classification: String,
    note: Option<String>,
    include_logs: bool,
    send: bool,
    session_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FeedbackUploadParams {
    classification: String,
    reason: Option<String>,
    include_logs: bool,
    extra_log_files: Option<Vec<String>>,
    tags: BTreeMap<String, String>,
}

pub fn run(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return Ok(());
    }
    let command = parse_args(args)?;
    let dirs = ClaudexDirs::new()?;
    let session = feedback::current_session(&dirs, command.session_id.as_deref())?;
    let attachments = feedback_attachments(&dirs, &session, command.include_logs);
    print_preview(&session, &command, &attachments);

    if !command.send {
        eprintln!();
        eprintln!("  Not sent. Add --send to this command to submit it to Codex.");
        return Ok(());
    }

    let params = upload_params(&session, &command, &attachments);
    let mut app_server = Command::new("ovm");
    app_server.args(["cx", "app-server"]);
    let thread_id = upload_with_command(app_server, &params)?;
    feedback::archive_codex_feedback(
        &dirs,
        &session,
        thread_id.clone(),
        command.classification,
        command.include_logs,
    )?;

    println!("Codex feedback thread: {thread_id}");
    println!("Claudex relationship: {}", session.feedback_id);
    Ok(())
}

fn parse_args(args: &[String]) -> Result<FeedbackCommand> {
    let mut classification = "other".to_string();
    let mut note = None;
    let mut include_logs = false;
    let mut send = false;
    let mut session_id = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--category" => {
                index += 1;
                classification =
                    canonical_classification(required_value(args, index, "--category")?)?;
            }
            value if value.starts_with("--category=") => {
                classification = canonical_classification(&value["--category=".len()..])?;
            }
            "--note" => {
                index += 1;
                note = Some(required_value(args, index, "--note")?.to_string());
            }
            value if value.starts_with("--note=") => {
                note = Some(value["--note=".len()..].to_string());
            }
            "--session" => {
                index += 1;
                session_id = Some(required_value(args, index, "--session")?.to_string());
            }
            value if value.starts_with("--session=") => {
                session_id = Some(value["--session=".len()..].to_string());
            }
            "--include-logs" => include_logs = true,
            "--send" => send = true,
            unknown => {
                return Err(ClaudexError::Message(format!(
                    "unknown feedback option: {unknown}"
                )));
            }
        }
        index += 1;
    }

    Ok(FeedbackCommand {
        classification,
        note: note.filter(|value| !value.trim().is_empty()),
        include_logs,
        send,
        session_id,
    })
}

fn required_value<'a>(args: &'a [String], index: usize, flag: &str) -> Result<&'a str> {
    args.get(index)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ClaudexError::Message(format!("{flag} requires a value")))
}

fn canonical_classification(value: &str) -> Result<String> {
    let canonical = match value.trim().to_ascii_lowercase().as_str() {
        "bug" => "bug",
        "bad-result" | "bad_result" => "bad_result",
        "good-result" | "good_result" => "good_result",
        "safety-check" | "safety_check" => "safety_check",
        "other" => "other",
        _ => {
            return Err(ClaudexError::Message(
                "category must be bug, bad-result, good-result, safety-check, or other".into(),
            ));
        }
    };
    Ok(canonical.to_string())
}

fn feedback_attachments(
    dirs: &ClaudexDirs,
    session: &FeedbackSession,
    include_logs: bool,
) -> Vec<PathBuf> {
    if !include_logs {
        return Vec::new();
    }
    let candidates = [
        feedback::relationship_path(dirs, &session.claude_session_id),
        session
            .transcript_path
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_default(),
        dirs.proxy_log_file(),
    ];
    candidates
        .into_iter()
        .filter(|path| path.is_file())
        .collect()
}

fn print_preview(session: &FeedbackSession, command: &FeedbackCommand, attachments: &[PathBuf]) {
    eprintln!();
    eprintln!("Codex feedback preview");
    eprintln!("  Relationship: {}", session.feedback_id);
    eprintln!("  Category: {}", command.classification);
    eprintln!("  Claude session: {}", session.claude_session_id);
    if let Some(note) = command.note.as_deref() {
        eprintln!("  Note: {note}");
    }
    if command.include_logs {
        eprintln!("  Files and diagnostics sent:");
        eprintln!("    • codex-logs.log");
        eprintln!("    • codex-doctor-report.json (when available)");
        eprintln!("    • codex-connectivity-diagnostics.txt (when available)");
        for path in attachments {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_else(|| path.as_os_str().to_string_lossy());
            eprintln!("    • {name}");
        }
    } else {
        eprintln!("  Logs: not included");
    }
}

fn upload_params(
    session: &FeedbackSession,
    command: &FeedbackCommand,
    attachments: &[PathBuf],
) -> FeedbackUploadParams {
    let mut tags = BTreeMap::new();
    tags.insert("source".into(), "claudex".into());
    tags.insert("claudex_feedback_id".into(), session.feedback_id.clone());
    tags.insert(
        "claude_session_id".into(),
        session.claude_session_id.clone(),
    );
    FeedbackUploadParams {
        classification: command.classification.clone(),
        reason: command.note.clone(),
        include_logs: command.include_logs,
        extra_log_files: command.include_logs.then(|| {
            attachments
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect()
        }),
        tags,
    }
}

fn upload_with_command(command: Command, params: &FeedbackUploadParams) -> Result<String> {
    let mut client = AppServerClient::spawn(command)?;
    client.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "clientInfo": {
                "name": "ovm-claudex",
                "version": env!("CARGO_PKG_VERSION")
            },
            "capabilities": { "experimentalApi": true }
        }
    }))?;
    client.response(1)?;
    client.send(&json!({"jsonrpc": "2.0", "method": "initialized"}))?;
    client.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "feedback/upload",
        "params": params
    }))?;
    let response = client.response(2)?;
    response
        .get("result")
        .and_then(|result| result.get("threadId"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ClaudexError::Message("Codex feedback response omitted threadId".into()))
}

struct AppServerClient {
    child: Child,
    stdin: ChildStdin,
    output: Receiver<std::result::Result<String, String>>,
}

impl AppServerClient {
    fn spawn(mut command: Command) -> Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ClaudexError::Message("Codex app-server stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ClaudexError::Message("Codex app-server stdout unavailable".into()))?;
        let (sender, output) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let message = line.map_err(|error| error.to_string());
                if sender.send(message).is_err() {
                    return;
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            output,
        })
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        serde_json::to_writer(&mut self.stdin, message)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn response(&self, request_id: i64) -> Result<Value> {
        loop {
            let line = self
                .output
                .recv_timeout(APP_SERVER_TIMEOUT)
                .map_err(|_| ClaudexError::Message("Codex app-server response timed out".into()))?
                .map_err(ClaudexError::Message)?;
            let message: Value = serde_json::from_str(&line)?;
            if message.get("id").and_then(Value::as_i64) != Some(request_id) {
                continue;
            }
            if let Some(error) = message.get("error") {
                return Err(ClaudexError::Message(format!(
                    "Codex feedback failed: {error}"
                )));
            }
            return Ok(message);
        }
    }
}

impl Drop for AppServerClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn print_help() {
    println!("Usage: ovm claudex feedback [options]");
    println!();
    println!("Options:");
    println!("  --category <kind>  bug, bad-result, good-result, safety-check, or other");
    println!("  --note <text>      Optional explanation");
    println!("  --include-logs     Include the listed claudex/Codex diagnostics");
    println!("  --send             Explicitly submit to Codex (otherwise preview only)");
    println!("  --session <id>     Select a Claude history session outside a live shell");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parsing_is_preview_first_and_normalizes_categories() {
        let args = vec![
            "--category".into(),
            "bad-result".into(),
            "--note=missed the tool result".into(),
            "--include-logs".into(),
        ];
        let parsed = parse_args(&args).expect("parse");
        assert_eq!(parsed.classification, "bad_result");
        assert_eq!(parsed.note.as_deref(), Some("missed the tool result"));
        assert!(parsed.include_logs);
        assert!(!parsed.send, "upload must require explicit --send");
    }

    #[test]
    #[cfg(unix)]
    fn app_server_upload_returns_native_thread_id_and_carries_relationship_tags() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let script = temp.path().join("fake-app-server");
        let capture = temp.path().join("request.json");
        fs::write(
            &script,
            "#!/bin/sh\nread _init\nprintf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}'\nread _initialized\nread feedback\nprintf '%s\\n' \"$feedback\" > \"$CAPTURE\"\nprintf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"threadId\":\"no-active-thread-test\"}}'\n",
        )
        .expect("script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");

        let params = FeedbackUploadParams {
            classification: "bug".into(),
            reason: Some("broken".into()),
            include_logs: false,
            extra_log_files: None,
            tags: BTreeMap::from([
                ("source".into(), "claudex".into()),
                ("claudex_feedback_id".into(), "cfx_test".into()),
            ]),
        };
        let mut command = Command::new(script);
        command.env("CAPTURE", &capture);
        let thread_id = upload_with_command(command, &params).expect("upload");
        assert_eq!(thread_id, "no-active-thread-test");

        let request: Value =
            serde_json::from_str(&fs::read_to_string(capture).expect("capture")).expect("json");
        assert_eq!(request["method"], "feedback/upload");
        assert_eq!(request["params"]["tags"]["claudex_feedback_id"], "cfx_test");
        assert!(request["params"].get("threadId").is_none());
    }
}

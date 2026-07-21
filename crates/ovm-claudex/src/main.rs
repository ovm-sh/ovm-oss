//! claudex — Claude Code as the harness, GPT-5.6 as the model.
//!
//! An OVM plugin (`ovm claudex …`) that launches the OVM-managed Claude Code
//! binary against a local CLIProxyAPI sidecar, which translates Anthropic's
//! API shape to the ChatGPT/Codex backend using the user's own subscription
//! OAuth. Everything claudex owns lives under `~/.ovm/claudex/`, including an
//! isolated Claude home (`CLAUDE_CONFIG_DIR`) so normal `claude` history,
//! settings, and login are never touched.
//!
//! Origin: https://x.com/thsottiaux/status/2076119366647894371

mod codex_feedback;
mod config;
mod doctor;
mod feedback;
mod install;
mod launch;
mod paths;
mod proxy;
mod setup;
mod uninstall;

use console::style;

pub(crate) type Result<T> = std::result::Result<T, ClaudexError>;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ClaudexError {
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Known subcommands are handled here; anything else is passed through to
    // the launch path so `claudex --continue`, `claudex -p "…"` etc. behave
    // exactly like the corresponding `claude` invocation.
    let result = match args.first().map(String::as_str) {
        Some("setup") => setup::run(),
        Some("doctor") => doctor::run(),
        Some("feedback") => codex_feedback::run(&args[1..]),
        Some("feedback-id") => feedback::print_feedback_id(args.get(1).map(String::as_str)),
        Some("__session-start") => feedback::session_start_hook(),
        Some("stop") => proxy::stop_command(),
        Some("uninstall") => uninstall::run(args.iter().any(|arg| arg == "--purge")),
        Some("update") => install::update_command(args.get(1).map(String::as_str)),
        Some("help") | Some("--help") | Some("-h") => {
            print_help();
            Ok(())
        }
        Some("launch") => launch::run(&args[1..]),
        _ => launch::run(&args),
    };

    if let Err(error) = result {
        eprintln!("  {} {error}", style("✗").red().bold());
        std::process::exit(1);
    }
}

fn print_help() {
    println!(
        "claudex {} — Claude Code on GPT-5.6",
        env!("CARGO_PKG_VERSION")
    );
    println!();
    println!("Usage: ovm claudex [command] [claude args…]");
    println!();
    println!("Commands:");
    println!("  setup    Interactive first-time setup (proxy, OAuth, isolated Claude home)");
    println!("  launch   Launch Claude Code against the proxy (default when no command given)");
    println!("  doctor   Check proxy, config, and isolation health");
    println!("  feedback Preview or explicitly send correlated native Codex feedback");
    println!("  feedback-id  Print this history session's local feedback correlation ID");
    println!("  stop     Stop the background proxy");
    println!("  uninstall  Stop the proxy and remove shims (--purge deletes all data)");
    println!("  update   Install/update the managed proxy binary (optionally: update <version>)");
    println!();
    println!("Anything else is passed through to Claude Code, e.g. `claudex --continue`.");
    println!("Launch flags: --fast (priority tier), --yolo (skip permissions).");
}

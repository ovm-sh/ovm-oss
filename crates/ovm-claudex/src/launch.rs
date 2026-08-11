//! Launch: assemble the environment, make sure the proxy is up, show which
//! (Claude Code, proxy) pair is about to run, then exec `ovm cc` so claudex
//! inherits OVM's version resolution, auto-update, and yolo handling. The
//! shared proxy-session lock descriptor is handed to OVM across that exec.

use crate::config::ClaudexConfig;
use crate::paths::{display, ClaudexDirs};
use crate::{proxy, ClaudexError, Result};
use console::style;
use std::io::IsTerminal;
use std::process::Command;
use std::time::Duration;

pub fn run(args: &[String]) -> Result<()> {
    // `--fast` selects the `<model>-fast` proxy aliases (OpenAI priority
    // service tier — the same wire field Codex CLI's fast toggle sets).
    // Only the flags region counts: after a bare `--` it's positional text.
    let flags_end = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let fast = args[..flags_end].iter().any(|arg| arg == "--fast");
    // Resolved before `--fast` is filtered out below, while the flag positions
    // still line up with `flags_end`.
    let resume_launcher = resume_launcher(&args[..flags_end], fast);
    let args: Vec<String> = args
        .iter()
        .enumerate()
        .filter(|(index, arg)| !(*index < flags_end && arg.as_str() == "--fast"))
        .map(|(_, arg)| arg.clone())
        .collect();
    let args = args.as_slice();

    let dirs = ClaudexDirs::new()?;
    // First run: walk through setup (intro, proxy, OAuth, isolated home)
    // instead of erroring, so `ovm switch` → claudex just works.
    let config = match ClaudexConfig::load(&dirs.config_file())? {
        Some(config) => config,
        None => {
            crate::setup::run()?;
            match ClaudexConfig::load(&dirs.config_file())? {
                Some(config) => config,
                None => return Ok(()), // setup was declined
            }
        }
    };
    if !dirs.claude_home().is_dir() {
        return Err(ClaudexError::Message(
            "claudex's isolated Claude home is missing. Run: ovm claudex setup".into(),
        ));
    }
    // Without an OAuth grant the proxy can never authenticate a request — a
    // launched session would hang on its first prompt (seen in RC: 40s+ of
    // silence instead of an error). Fail fast with the same fix doctor gives.
    if !proxy::has_oauth_grant(&dirs) {
        return Err(ClaudexError::Message(
            "no Codex OAuth grant — the proxy cannot authenticate any request. Run: ovm claudex setup".into(),
        ));
    }

    // Upgrade existing isolated homes in place. The hook creates a durable,
    // local feedback correlation as soon as Claude reports its real history
    // session ID (including resumes and /clear).
    crate::feedback::install_session_hooks(&dirs)?;

    if let Err(error) = crate::install::maybe_prepare_auto_update(&dirs, &config) {
        eprintln!(
            "  {} Could not check/update cliproxyapi; continuing with the installed proxy ({error}).",
            style("!").yellow()
        );
    }

    // Every operation that can stop, start, or replace the proxy takes the
    // update lock first, then the session lock. A launcher must wait here even
    // when it would receive the exclusive session lease: otherwise `stop` can
    // kill the old proxy, the launcher can publish a new pidfile, and `stop`
    // can then unlink that new record by pathname.
    let runtime_lock = crate::install::acquire_update_lock(&dirs)?;
    let session_guard = proxy::SessionGuard::acquire(&dirs)?;
    if session_guard.is_exclusive() {
        if let Err(error) = proxy::activate_pending_update(&dirs, &config, false) {
            eprintln!(
                "  {} Prepared proxy update could not be activated; continuing with the previous version ({error}).",
                style("!").yellow()
            );
        }
    }
    proxy::ensure_running_for_session(&dirs, &config)?;
    let session_guard = session_guard.downgrade()?;
    drop(runtime_lock);

    let claude_version = active_claude_version();
    let proxy_label = proxy::running_version_label(&dirs, &config);
    print_banner(
        &config,
        &dirs,
        claude_version.as_deref(),
        &proxy_label,
        fast,
    );
    if let Some(hold) = banner_hold(&dirs) {
        std::thread::sleep(hold);
    }

    let claude_args = build_claude_args(&config, args, fast)?;

    let mut command = Command::new("ovm");
    command.args(&claude_args);
    // Ambient credentials must never reach the claudex child: a real
    // ANTHROPIC_API_KEY could route requests around the proxy (credential
    // precedence) or hand a live Anthropic key to the local sidecar.
    for key in SCRUBBED_ENV {
        command.env_remove(key);
    }
    for (key, value) in launch_env(&config, &dirs, fast) {
        command.env(key, value);
    }

    #[cfg(unix)]
    {
        let session_lock_fd = session_guard.make_inheritable()?;
        command.env("OVM_CLAUDEX_SESSION_LOCK_FD", session_lock_fd.to_string());

        // Wait for Claude Code rather than exec'ing into it, so claudex is
        // still alive to sign off when the session ends.
        //
        // A SessionEnd hook cannot do this job: measured on 2.1.226, SessionEnd
        // fires for `-p` runs (reason "other") and NOT AT ALL when an
        // interactive TUI session ends — which is every real session. The
        // launcher is the only thing guaranteed to outlive the session.
        //
        // SIGINT is ignored here, not blocked for the child: Ctrl-C is
        // delivered to the whole foreground process group, so Claude Code still
        // receives and handles it. Ignoring it in the parent only stops claudex
        // dying first and taking the sign-off with it.
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
        }
        let status = command.status()?;
        drop(session_guard);
        sign_off(&dirs, &resume_launcher);
        std::process::exit(status.code().unwrap_or(1));
    }
    #[cfg(not(unix))]
    {
        let status = command.status()?;
        drop(session_guard);
        sign_off(&dirs, &resume_launcher);
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// The claudex sign-off: a sleeping Mochi and the command that resumes what
/// just ended.
///
/// Best-effort throughout — a session that has already finished must never fail
/// because of its own goodbye. Nothing prints when stderr isn't a terminal, so
/// `-p`, pipes and CI stay clean.
fn sign_off(dirs: &ClaudexDirs, launcher: &str) {
    if !std::io::stderr().is_terminal() {
        return;
    }
    let Some(session_id) = crate::feedback::latest_session_id(dirs) else {
        return;
    };
    let magenta = console::Style::new().magenta();
    let dim = console::Style::new().dim();
    let cyan = console::Style::new().cyan();
    eprintln!();
    eprintln!(
        "{}  {}",
        magenta.apply_to(r"  /\_/\ "),
        dim.apply_to("thanks for using claudex via OVM")
    );
    eprintln!(
        "{}  {}",
        magenta.apply_to(" ( -.- )"),
        cyan.apply_to(format!("{launcher} --resume {session_id}"))
    );
    eprintln!();
}

/// Active Claude Code version via OVM's script-friendly interface.
pub(crate) fn active_claude_version() -> Option<String> {
    let output = Command::new("ovm")
        .args(["current", "claude"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!version.is_empty()).then_some(version)
}

/// Which installed shim reproduces this session, for the sign-off's resume
/// line. The shim family is `claudex`/`ccx` plus `y`=yolo and `f`=fast
/// suffixes, so the flags a session was launched with name their own command.
/// `--yolo` is OVM's flag, passed straight through, so it is read here rather
/// than parsed out.
fn resume_launcher(flags: &[String], fast: bool) -> String {
    let yolo = flags.iter().any(|arg| arg == "--yolo");
    match (yolo, fast) {
        (false, false) => "claudex",
        (true, false) => "ccxy",
        (false, true) => "ccxf",
        (true, true) => "ccxyf",
    }
    .to_string()
}

/// `<model>` → its fast-tier proxy alias.
fn fast_model(model: &str) -> String {
    format!("{model}-fast")
}

fn pick_model(model: &str, fast: bool) -> String {
    if fast {
        fast_model(model)
    } else {
        model.to_string()
    }
}

fn print_banner(
    config: &ClaudexConfig,
    dirs: &ClaudexDirs,
    claude_version: Option<&str>,
    proxy_label: &str,
    fast: bool,
) {
    // Mochi the Cat, full size — same mascot OVM shows on installs/switches.
    // Three padded lines; info lines beyond the cat get matching indentation.
    const CAT: [&str; 3] = ["  /\\_/\\ ", " ( ^.^ )", "  > ^ < "];
    const CAT_BLANK: &str = "        ";

    let claude = claude_version.unwrap_or("?");
    let model_label = if fast {
        format!("{} (priority tier)", fast_model(&config.models.default))
    } else {
        config.models.default.clone()
    };
    let mut info = vec![
        style("claudex").magenta().bold().to_string(),
        format!(
            "Claude Code {} · model {}",
            style(claude).green().bold(),
            style(model_label).cyan().bold()
        ),
        format!(
            "proxy cliproxyapi {} @ 127.0.0.1:{}",
            style(proxy_label).green(),
            config.proxy.port
        ),
        format!(
            "history isolated → {}",
            style(display(&dirs.claude_home())).dim()
        ),
    ];
    if let Some(pin) = &config.pin {
        info.push(format!(
            "{} pinned pair: claude {} + proxy {}",
            style("⚲").yellow(),
            pin.claude,
            pin.proxy
        ));
    }

    eprintln!();
    for (index, line) in info.iter().enumerate() {
        // The title line stands alone; the cat sits beside the detail lines
        // below it, ending level with "history isolated".
        let face = match index {
            0 => CAT_BLANK,
            _ => CAT.get(index - 1).copied().unwrap_or(CAT_BLANK),
        };
        eprintln!("{}  {}", style(face).magenta(), line);
    }
    eprintln!();
}

/// How long the banner stays readable before Claude Code's TUI paints over it.
///
/// On the FIRST launch in this isolated home the banner is the one moment the
/// user can see that their history is isolated — hold it a beat. Every later
/// launch skips the pause so the daily path stays instant. The heuristic for
/// "first": Claude Code has not yet written a session ledger into the isolated
/// home. OVM_CLAUDEX_BANNER_HOLD_MS overrides in both directions (0 disables,
/// any value forces that hold — useful for recording demos); a non-terminal
/// stderr never holds, so scripts and pipes are unaffected.
fn banner_hold(dirs: &ClaudexDirs) -> Option<Duration> {
    let override_ms = std::env::var("OVM_CLAUDEX_BANNER_HOLD_MS").ok();
    let first_launch = !dirs.claude_home().join("history.jsonl").exists();
    banner_hold_policy(
        override_ms.as_deref(),
        std::io::stderr().is_terminal(),
        first_launch,
    )
}

fn banner_hold_policy(
    override_ms: Option<&str>,
    stderr_is_tty: bool,
    first_launch: bool,
) -> Option<Duration> {
    if let Some(raw) = override_ms {
        return raw
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis);
    }
    if !stderr_is_tty {
        return None;
    }
    first_launch.then(|| Duration::from_millis(2500))
}

/// The `ovm` argument list: `cc`, the pinned Claude version when one is set,
/// a default `--model` unless the user chose their own, then the user's args.
/// In fast mode the injected default becomes its `-fast` alias; an explicit
/// user `--model` is always respected verbatim.
fn build_claude_args(
    config: &ClaudexConfig,
    user_args: &[String],
    fast: bool,
) -> Result<Vec<String>> {
    let mut args = vec!["cc".to_string()];
    let mut user_args: Vec<String> = user_args.to_vec();

    if let Some(pin) = &config.pin {
        // OVM keeps the LAST --ovm-version it sees, so a user-supplied one
        // would silently override the pin. A pin is a contract: strip the
        // override (flag and its value, both forms) loudly instead — but
        // only in the flags region; after `--` everything is positional.
        let strip_end = user_args
            .iter()
            .position(|arg| arg == "--")
            .unwrap_or(user_args.len());
        let mut cleaned = Vec::with_capacity(user_args.len());
        let mut stripped = false;
        let mut iter = user_args.into_iter().enumerate().peekable();
        while let Some((index, arg)) = iter.next() {
            if index < strip_end && arg == "--ovm-version" {
                stripped = true;
                // Drop the separate value too — but reject a missing or
                // option-like one (e.g. `--ovm-version --model x`) instead of
                // silently swallowing the following application option or the
                // `--` delimiter as if it were the version.
                match iter.peek() {
                    Some((_, next)) if !next.starts_with('-') => {
                        let _ = iter.next();
                    }
                    _ => {
                        return Err(ClaudexError::Message(
                            "--ovm-version requires a version.".into(),
                        ));
                    }
                }
            } else if index < strip_end && arg.starts_with("--ovm-version=") {
                stripped = true;
            } else {
                cleaned.push(arg);
            }
        }
        user_args = cleaned;
        if stripped {
            eprintln!(
                "  {} --ovm-version is ignored while the pair is pinned (claude {}). \
                 Clear the pin in ~/.ovm/claudex/config.json to choose versions manually.",
                console::style("!").yellow(),
                pin.claude
            );
        }
        args.push(format!("--ovm-version={}", pin.claude));
    }

    // Everything after a bare `--` is positional (a prompt, a path), not
    // flags — never let it suppress the default model injection.
    let flags_end = user_args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(user_args.len());
    let user_picked_model = user_args[..flags_end]
        .iter()
        .any(|arg| arg == "--model" || arg.starts_with("--model="));
    if !user_picked_model {
        args.push("--model".to_string());
        args.push(pick_model(&config.models.default, fast));
    }

    args.extend(user_args);
    Ok(args)
}

/// Inherited credential/profile vars scrubbed from the child before claudex
/// injects its own. `ANTHROPIC_AUTH_TOKEN` is re-set to the local proxy key.
/// Every ambient provider credential and provider-routing variable that must
/// not reach the claudex child or the proxy sidecar. Beyond raw keys, the
/// Bedrock/Vertex/custom-header vars could otherwise route the child around
/// the proxy or hand a real key to the local sidecar. One list, both spawns.
pub(crate) const SCRUBBED_ENV: [&str; 9] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_PROFILE",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "OPENAI_API_KEY",
    "ANTHROPIC_CUSTOM_HEADERS",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "ANTHROPIC_BEDROCK_BASE_URL",
];

/// Every env var claudex injects. The static parts are also seeded into the
/// isolated settings.json by setup; repeating them here keeps launches correct
/// even if the user edits that file. Fast mode remaps every tier slot to its
/// `-fast` alias so `/model` switching stays fast too.
fn launch_env(config: &ClaudexConfig, dirs: &ClaudexDirs, fast: bool) -> Vec<(String, String)> {
    let mut env = vec![
        (
            "CLAUDE_CONFIG_DIR".to_string(),
            dirs.claude_home().to_string_lossy().into_owned(),
        ),
        (
            "ANTHROPIC_BASE_URL".to_string(),
            format!("http://127.0.0.1:{}", config.proxy.port),
        ),
        (
            "ANTHROPIC_AUTH_TOKEN".to_string(),
            config.proxy.api_key.clone(),
        ),
        (
            "ANTHROPIC_DEFAULT_OPUS_MODEL".to_string(),
            pick_model(&config.models.opus, fast),
        ),
        (
            "ANTHROPIC_DEFAULT_SONNET_MODEL".to_string(),
            pick_model(&config.models.sonnet, fast),
        ),
        (
            "ANTHROPIC_DEFAULT_HAIKU_MODEL".to_string(),
            pick_model(&config.models.haiku, fast),
        ),
        (
            "CLAUDE_CODE_SUBAGENT_MODEL".to_string(),
            pick_model(&config.models.subagent, fast),
        ),
        (
            "CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY".to_string(),
            config.tuning.max_tool_use_concurrency.to_string(),
        ),
        (
            "ENABLE_TOOL_SEARCH".to_string(),
            config.tuning.enable_tool_search.to_string(),
        ),
        // Without this Claude Code cannot size a model it doesn't know, assumes
        // 200K, and warns on every launch. It honours the variable only for
        // model IDs that do not start with `claude-`, which every proxied
        // GPT-5.6 ID satisfies. Older Claude Code builds (before ~2.1.193)
        // ignore the variable entirely rather than misreading it.
        (
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS".to_string(),
            config.tuning.max_context_tokens.to_string(),
        ),
    ];
    if config.tuning.always_enable_effort {
        env.push((
            "CLAUDE_CODE_ALWAYS_ENABLE_EFFORT".to_string(),
            "1".to_string(),
        ));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PinnedPair;
    use std::path::PathBuf;

    fn env_value<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn banner_holds_only_on_the_first_interactive_launch() {
        // First launch at a terminal: hold so "history isolated" is readable.
        assert_eq!(
            banner_hold_policy(None, true, true),
            Some(Duration::from_millis(2500))
        );
        // Every later launch is instant; pipes and scripts never hold.
        assert_eq!(banner_hold_policy(None, true, false), None);
        assert_eq!(banner_hold_policy(None, false, true), None);
    }

    #[test]
    fn banner_hold_override_wins_in_both_directions() {
        assert_eq!(
            banner_hold_policy(Some("4000"), false, false),
            Some(Duration::from_millis(4000))
        );
        assert_eq!(banner_hold_policy(Some("0"), true, true), None);
        // Garbage in the override disables rather than panicking or holding.
        assert_eq!(banner_hold_policy(Some("soon"), true, true), None);
    }

    #[test]
    fn launch_env_wires_proxy_isolation_and_model_registry() {
        let mut config = ClaudexConfig::default();
        config.proxy.api_key = "secret".into();
        let dirs = ClaudexDirs::at(PathBuf::from("/tmp/claudex"));

        let env = launch_env(&config, &dirs, false);

        assert_eq!(
            env_value(&env, "CLAUDE_CONFIG_DIR"),
            Some("/tmp/claudex/claude")
        );
        assert_eq!(
            env_value(&env, "ANTHROPIC_BASE_URL"),
            Some("http://127.0.0.1:8317")
        );
        assert_eq!(env_value(&env, "ANTHROPIC_AUTH_TOKEN"), Some("secret"));
        assert_eq!(
            env_value(&env, "ANTHROPIC_DEFAULT_OPUS_MODEL"),
            Some("gpt-5.6-sol")
        );
        assert_eq!(
            env_value(&env, "ANTHROPIC_DEFAULT_SONNET_MODEL"),
            Some("gpt-5.6-terra")
        );
        assert_eq!(
            env_value(&env, "ANTHROPIC_DEFAULT_HAIKU_MODEL"),
            Some("gpt-5.6-luna")
        );
        assert_eq!(
            env_value(&env, "CLAUDE_CODE_SUBAGENT_MODEL"),
            Some("gpt-5.6-terra")
        );
        assert_eq!(
            env_value(&env, "CLAUDE_CODE_ALWAYS_ENABLE_EFFORT"),
            Some("1")
        );
        assert_eq!(
            env_value(&env, "CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY"),
            Some("3")
        );
        assert_eq!(env_value(&env, "ENABLE_TOOL_SEARCH"), Some("false"));
    }

    /// Claude Code reads CLAUDE_CODE_MAX_CONTEXT_TOKENS only for model IDs that
    /// do not start with `claude-`; every other ID silently keeps the 200K
    /// default. Pinning both halves here means a future model rename that
    /// reintroduces a `claude-` prefix fails this test instead of quietly
    /// halving everyone's usable context.
    #[test]
    fn launch_env_declares_the_context_window_for_a_non_claude_model_id() {
        let mut config = ClaudexConfig::default();
        config.proxy.api_key = "secret".into();
        let dirs = ClaudexDirs::at(PathBuf::from("/tmp/claudex"));

        let env = launch_env(&config, &dirs, false);

        // 272K is OpenAI's 2x-pricing boundary, not the 1,050,000 raw window.
        assert_eq!(
            env_value(&env, "CLAUDE_CODE_MAX_CONTEXT_TOKENS"),
            Some("272000")
        );
        for key in [
            "ANTHROPIC_DEFAULT_OPUS_MODEL",
            "ANTHROPIC_DEFAULT_SONNET_MODEL",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL",
            "CLAUDE_CODE_SUBAGENT_MODEL",
        ] {
            let model = env_value(&env, key).unwrap_or_default();
            assert!(
                !model.starts_with("claude-"),
                "{key} = {model} starts with `claude-`, so Claude Code ignores \
                 CLAUDE_CODE_MAX_CONTEXT_TOKENS and falls back to 200K"
            );
        }
    }

    /// The resume line is only useful if it reproduces the session the user
    /// just left — a yolo session that suggests the plain shim would come back
    /// asking permission for everything it stopped asking about.
    #[test]
    fn resume_launcher_names_the_shim_that_reproduces_the_session() {
        let flags =
            |args: &[&str]| -> Vec<String> { args.iter().map(|arg| (*arg).to_string()).collect() };
        assert_eq!(resume_launcher(&flags(&[]), false), "claudex");
        assert_eq!(resume_launcher(&flags(&["--yolo"]), false), "ccxy");
        assert_eq!(resume_launcher(&flags(&[]), true), "ccxf");
        assert_eq!(resume_launcher(&flags(&["--yolo"]), true), "ccxyf");
        // Every name has to be a shim setup actually installs, or the line
        // prints a command that does not exist.
        for launcher in ["claudex", "ccxy", "ccxf", "ccxyf"] {
            assert!(
                crate::setup::CLAUDEX_SHIMS.contains(&launcher),
                "{launcher} is not an installed shim"
            );
        }
    }

    #[test]
    fn scrub_list_covers_ambient_credentials_and_reinjects_only_ours() {
        for critical in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_PROFILE",
            "CLAUDE_CODE_OAUTH_TOKEN",
        ] {
            assert!(
                SCRUBBED_ENV.contains(&critical),
                "{critical} must be scrubbed from the claudex child"
            );
        }
        // ANTHROPIC_AUTH_TOKEN is scrubbed then re-set to the proxy key.
        assert!(SCRUBBED_ENV.contains(&"ANTHROPIC_AUTH_TOKEN"));
        let mut config = ClaudexConfig::default();
        config.proxy.api_key = "local-key".into();
        let env = launch_env(&config, &ClaudexDirs::at(PathBuf::from("/tmp/x")), false);
        assert_eq!(env_value(&env, "ANTHROPIC_AUTH_TOKEN"), Some("local-key"));
    }

    #[test]
    fn effort_flag_is_omitted_when_disabled() {
        let mut config = ClaudexConfig::default();
        config.tuning.always_enable_effort = false;
        let dirs = ClaudexDirs::at(PathBuf::from("/tmp/claudex"));

        let env = launch_env(&config, &dirs, false);
        assert_eq!(env_value(&env, "CLAUDE_CODE_ALWAYS_ENABLE_EFFORT"), None);
    }

    #[test]
    fn default_model_is_injected_for_plain_launches() {
        let args = build_claude_args(&ClaudexConfig::default(), &[], false).expect("build args");
        assert_eq!(args, vec!["cc", "--model", "gpt-5.6-sol"]);
    }

    #[test]
    fn user_model_choice_wins_over_the_default() {
        let user = vec!["--model".to_string(), "gpt-5.6-luna".to_string()];
        let args = build_claude_args(&ClaudexConfig::default(), &user, false).expect("build args");
        assert_eq!(args, vec!["cc", "--model", "gpt-5.6-luna"]);

        let user_eq = vec!["--model=gpt-5.6-luna".to_string()];
        let args_eq =
            build_claude_args(&ClaudexConfig::default(), &user_eq, false).expect("build args");
        assert_eq!(args_eq, vec!["cc", "--model=gpt-5.6-luna"]);
    }

    #[test]
    fn passthrough_args_survive_after_the_model() {
        let user = vec!["--continue".to_string()];
        let args = build_claude_args(&ClaudexConfig::default(), &user, false).expect("build args");
        assert_eq!(args, vec!["cc", "--model", "gpt-5.6-sol", "--continue"]);
    }

    #[test]
    fn fast_mode_selects_fast_aliases_everywhere() {
        let config = ClaudexConfig::default();
        let dirs = ClaudexDirs::at(PathBuf::from("/tmp/claudex"));

        let args = build_claude_args(&config, &[], true).expect("build args");
        assert_eq!(args, vec!["cc", "--model", "gpt-5.6-sol-fast"]);

        let env = launch_env(&config, &dirs, true);
        assert_eq!(
            env_value(&env, "ANTHROPIC_DEFAULT_OPUS_MODEL"),
            Some("gpt-5.6-sol-fast")
        );
        assert_eq!(
            env_value(&env, "ANTHROPIC_DEFAULT_SONNET_MODEL"),
            Some("gpt-5.6-terra-fast")
        );
        assert_eq!(
            env_value(&env, "ANTHROPIC_DEFAULT_HAIKU_MODEL"),
            Some("gpt-5.6-luna-fast")
        );
        assert_eq!(
            env_value(&env, "CLAUDE_CODE_SUBAGENT_MODEL"),
            Some("gpt-5.6-terra-fast")
        );
    }

    #[test]
    fn pin_strips_user_ovm_version_in_both_forms() {
        let config = ClaudexConfig {
            pin: Some(PinnedPair {
                claude: "2.1.207".into(),
                proxy: "7.2.70".into(),
            }),
            ..ClaudexConfig::default()
        };

        let separate = vec!["--ovm-version".to_string(), "2.1.100".to_string()];
        let args = build_claude_args(&config, &separate, false).expect("build args");
        assert_eq!(
            args,
            vec!["cc", "--ovm-version=2.1.207", "--model", "gpt-5.6-sol"],
            "flag AND its value must both be stripped"
        );

        let equals = vec!["--ovm-version=2.1.100".to_string(), "-p".to_string()];
        let args = build_claude_args(&config, &equals, false).expect("build args");
        assert_eq!(
            args,
            vec![
                "cc",
                "--ovm-version=2.1.207",
                "--model",
                "gpt-5.6-sol",
                "-p"
            ]
        );
    }

    #[test]
    fn pinned_ovm_version_rejects_option_like_or_missing_value() {
        let config = ClaudexConfig {
            pin: Some(PinnedPair {
                claude: "2.1.207".into(),
                proxy: "7.2.70".into(),
            }),
            ..ClaudexConfig::default()
        };

        // `claudex --ovm-version --model x` must NOT silently strip `--model`
        // as the version — it errors, matching the underlying `ovm cc` path.
        for tail in [
            vec![
                "--ovm-version".to_string(),
                "--model".to_string(),
                "x".to_string(),
            ],
            vec!["--ovm-version".to_string(), "-p".to_string()],
            vec!["--ovm-version".to_string()],
            vec![
                "--ovm-version".to_string(),
                "--".to_string(),
                "prompt".to_string(),
            ],
        ] {
            let error = build_claude_args(&config, &tail, false).expect_err("bad version value");
            assert_eq!(error.to_string(), "--ovm-version requires a version.");
        }
    }

    #[test]
    fn model_after_double_dash_is_positional_not_a_flag() {
        let user = vec!["--".to_string(), "--model".to_string()];
        let args = build_claude_args(&ClaudexConfig::default(), &user, false).expect("build args");
        assert_eq!(
            args,
            vec!["cc", "--model", "gpt-5.6-sol", "--", "--model"],
            "a positional --model must not suppress the default injection"
        );
    }

    #[test]
    fn fast_mode_never_rewrites_an_explicit_user_model() {
        let user = vec!["--model".to_string(), "gpt-5.4-mini".to_string()];
        let args = build_claude_args(&ClaudexConfig::default(), &user, true).expect("build args");
        assert_eq!(args, vec!["cc", "--model", "gpt-5.4-mini"]);
    }

    #[test]
    fn pinned_claude_version_is_passed_to_ovm() {
        let config = ClaudexConfig {
            pin: Some(PinnedPair {
                claude: "2.1.207".into(),
                proxy: "7.2.55".into(),
            }),
            ..ClaudexConfig::default()
        };
        let args = build_claude_args(&config, &[], false).expect("build args");
        assert_eq!(
            args,
            vec!["cc", "--ovm-version=2.1.207", "--model", "gpt-5.6-sol"]
        );
    }
}
